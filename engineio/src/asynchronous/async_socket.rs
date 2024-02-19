use std::{
    borrow::BorrowMut,
    fmt::Debug,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::SystemTime,
};

use async_stream::try_stream;
use bytes::Bytes;
use futures_util::{Future, FutureExt, Stream, StreamExt};
use tokio::{runtime::Handle, sync::Mutex, time::Duration, time::Instant, time::Timeout};

use crate::{
    asynchronous::{callback::OptionalCallback, transport::AsyncTransportType},
    error::Result,
    packet::{HandshakePacket, Payload},
    Error, Packet, PacketId,
};

use super::generator::StreamGenerator;

#[derive(Clone)]
pub struct Socket {
    handle: Handle,
    transport: Arc<Mutex<AsyncTransportType>>,
    on_close: OptionalCallback<()>,
    on_data: OptionalCallback<Bytes>,
    on_error: OptionalCallback<String>,
    on_open: OptionalCallback<()>,
    on_packet: OptionalCallback<Packet>,
    connected: Arc<AtomicBool>,
    last_ping: Arc<AtomicU64>,
    last_pong: Arc<AtomicU64>,
    connection_data: Arc<HandshakePacket>,
    generator: StreamGenerator<Packet>,
    max_ping_timeout: u64,
    sleep: Arc<Pin<Box<tokio::time::Sleep>>>,
}

impl Socket {
    pub(crate) fn new(
        transport: AsyncTransportType,
        handshake: HandshakePacket,
        on_close: OptionalCallback<()>,
        on_data: OptionalCallback<Bytes>,
        on_error: OptionalCallback<String>,
        on_open: OptionalCallback<()>,
        on_packet: OptionalCallback<Packet>,
    ) -> Self {
        // let max_ping_timeout = handshake.ping_interval + handshake.ping_timeout;
        let max_ping_timeout = 10;

        let last_ping = Arc::new(AtomicU64::new(current_time_in_seconds()));
        let last_pong = Arc::new(AtomicU64::new(current_time_in_seconds()));
        let connected = Arc::new(AtomicBool::default());
        let handle = Handle::current();

        Socket {
            handle,
            on_close,
            on_data,
            on_error,
            on_open,
            on_packet,
            transport: Arc::new(Mutex::new(transport.clone())),
            connected,
            last_ping,
            last_pong,
            connection_data: Arc::new(handshake),
            generator: StreamGenerator::new(Self::stream(transport)), // TODO: what do I fill in here?
            max_ping_timeout: max_ping_timeout,
            sleep: Arc::new(Box::pin(tokio::time::sleep(
                tokio::time::Duration::from_secs(max_ping_timeout),
            ))),
        }
    }

    /// Returns the packet stream for the client.
    // pub(crate) fn as_stream<'a>(
    //     &'a self,
    //     transport: AsyncTransportType,
    //     max_ping_timeout: u64,
    // ) -> Pin<Box<dyn Stream<Item = Result<Packet>> + Send + 'a>> {
    //     // let max_ping_timeout = Arc::new(max_ping_timeout);
    //     futures_util::stream::unfold(Self::stream(transport.clone()), |mut stream| async {
    //         // Wait for the next payload or until we should have received the next ping.
    //         match tokio::time::timeout(
    //             std::time::Duration::from_secs(Self::time_to_next_ping(self.last_ping.clone(), 64)),
    //             stream.next(),
    //         )
    //         .await
    //         {
    //             Ok(result) => result.map(|result| (result, stream)),
    //             // We didn't receive a ping in time and now consider the connection as closed.
    //             Err(_) => {
    //                 // Be nice and disconnect properly.
    //                 if let Err(e) = self.disconnect().await {
    //                     Some((Err(e), stream))
    //                 } else {
    //                     Some((Err(Error::PingTimeout()), stream))
    //                 }
    //             }
    //         }
    //     })
    //     .boxed()
    // }

    /// Wraps the underlying stream in a different stream that respects max_timeout
    fn enforce_timeout<'a, S: Stream<Item = Result<Packet>> + Send + Unpin + 'a>(
        stream: S,
        last_ping: Arc<AtomicU64>,
        max_ping_timeout: u64,
        connected: Arc<AtomicBool>,
        on_close: OptionalCallback<()>,
        handle: Handle,
    ) -> Pin<Box<dyn Stream<Item = Result<Packet>> + Send + 'a>> {
        let max_ping_timeout = Arc::new(max_ping_timeout);
        futures_util::stream::unfold(
            (
                stream,
                last_ping,
                max_ping_timeout,
                connected,
                on_close,
                handle,
            ),
            |(mut stream, last_ping, max_ping_timeout, connected, on_close, handle)| async {
                // Wait for the next payload or until we should have received the next ping.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(Self::time_to_next_ping(
                        last_ping.clone(),
                        *max_ping_timeout.as_ref(),
                    )),
                    stream.next(),
                )
                .await
                {
                    Ok(result) => result.map(|result| {
                        (
                            result,
                            (
                                stream,
                                last_ping,
                                max_ping_timeout,
                                connected,
                                on_close,
                                handle,
                            ),
                        )
                    }),
                    // We didn't receive a ping in time and now consider the connection as closed.
                    Err(_) => {
                        // FIXME: Don't love the duplication of implementation of self.disconnect...
                        // Be nice and disconnect properly.
                        connected.clone().store(false, Ordering::Relaxed);
                        if let Some(callback) = on_close.clone().as_ref() {
                            let on_close = callback.clone();
                            handle.clone().spawn(async move { on_close(()).await });
                        }
                        Some((
                            Err(Error::PingTimeout()),
                            (
                                stream,
                                last_ping,
                                max_ping_timeout,
                                connected,
                                on_close,
                                handle,
                            ),
                        ))
                    }
                }
            },
        )
        .boxed()
    }

    /// Opens the connection to a specified server. The first Pong packet is sent
    /// to the server to trigger the Ping-cycle.
    pub async fn connect(&self) -> Result<()> {
        // SAFETY: Has valid handshake due to type
        self.connected.store(true, Ordering::Release);

        if let Some(on_open) = self.on_open.as_ref() {
            let on_open = on_open.clone();
            self.handle.spawn(async move { on_open(()).await });
        }

        // set the last ping to now and set the connected state
        self.last_ping
            .store(current_time_in_seconds(), Ordering::Relaxed);

        // emit a pong packet to keep trigger the ping cycle on the server
        self.emit(Packet::new(PacketId::Pong, Bytes::new())).await?;

        Ok(())
    }

    /// A helper method that distributes
    pub(super) async fn handle_incoming_packet(&self, packet: Packet) -> Result<()> {
        // check for the appropriate action or callback
        self.handle_packet(packet.clone());
        match packet.packet_id {
            PacketId::MessageBinary => {
                self.handle_data(packet.data.clone());
            }
            PacketId::Message => {
                self.handle_data(packet.data.clone());
            }
            PacketId::Close => {
                self.handle_close();
            }
            PacketId::Upgrade => {
                // this is already checked during the handshake, so just do nothing here
            }
            PacketId::Ping => {
                self.pinged().await;
                self.emit(Packet::new(PacketId::Pong, Bytes::new())).await?;
            }
            PacketId::Pong | PacketId::Open => {
                // this will never happen as the pong and open
                // packets are only sent by the client
                return Err(Error::InvalidPacket());
            }
            PacketId::Noop => (),
        }
        Ok(())
    }

    /// Helper method that parses bytes and returns an iterator over the elements.
    fn parse_payload(bytes: Bytes) -> impl Stream<Item = Result<Packet>> {
        try_stream! {
            let payload = Payload::try_from(bytes);

            for elem in payload?.into_iter() {
                yield elem;
            }
        }
    }

    /// Creates a stream over the incoming packets, uses the streams provided by the
    /// underlying transport types.
    fn stream(
        mut transport: AsyncTransportType,
    ) -> Pin<Box<impl Stream<Item = Result<Packet>> + 'static + Send>> {
        // map the byte stream of the underlying transport
        // to a packet stream
        Box::pin(try_stream! {
            for await payload in transport.as_pin_box() {
                for await packet in Self::parse_payload(payload?) {
                    yield packet?;
                }
            }
        })
    }

    pub async fn disconnect(&self) -> Result<()> {
        if let Some(on_close) = self.on_close.as_ref() {
            let on_close = on_close.clone();
            self.handle.spawn(async move { on_close(()).await });
        }

        self.emit(Packet::new(PacketId::Close, Bytes::new()))
            .await?;

        self.connected.store(false, Ordering::Release);

        Ok(())
    }

    /// Sends a packet to the server.
    pub async fn emit(&self, packet: Packet) -> Result<()> {
        if !self.connected.load(Ordering::Acquire) {
            let error = Error::IllegalActionBeforeOpen();
            self.call_error_callback(format!("{}", error));
            return Err(error);
        }

        let is_binary = packet.packet_id == PacketId::MessageBinary;

        // send a post request with the encoded payload as body
        // if this is a binary attachment, then send the raw bytes
        let data: Bytes = if is_binary {
            packet.data
        } else {
            packet.into()
        };

        let lock = self.transport.lock().await;
        let fut = lock.as_transport().emit(data, is_binary);

        if let Err(error) = fut.await {
            self.call_error_callback(error.to_string());
            return Err(error);
        }

        Ok(())
    }

    /// Calls the error callback with a given message.
    #[inline]
    fn call_error_callback(&self, text: String) {
        if let Some(on_error) = self.on_error.as_ref() {
            let on_error = on_error.clone();
            self.handle.spawn(async move { on_error(text).await });
        }
    }

    // Check if the underlying transport client is connected.
    pub(crate) fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub(crate) async fn pinged(&self) {
        self.last_ping
            .store(current_time_in_seconds(), Ordering::Relaxed);
    }

    /// Returns the time in seconds that is left until a new ping must be received.
    /// This is used to detect whether we have been disconnected from the server.
    /// See https://socket.io/docs/v4/how-it-works/#disconnection-detection
    fn time_to_next_ping(last_ping: Arc<AtomicU64>, max_ping_timeout: u64) -> u64 {
        let current_time = current_time_in_seconds();
        let last_ping = last_ping.load(Ordering::Relaxed);

        let since_last_ping = current_time - last_ping;
        if since_last_ping > max_ping_timeout {
            0
        } else {
            max_ping_timeout - since_last_ping
        }
    }

    pub(crate) fn handle_packet(&self, packet: Packet) {
        if let Some(on_packet) = self.on_packet.as_ref() {
            let on_packet = on_packet.clone();
            self.handle.spawn(async move { on_packet(packet).await });
        }
    }

    pub(crate) fn handle_data(&self, data: Bytes) {
        if let Some(on_data) = self.on_data.as_ref() {
            let on_data = on_data.clone();
            self.handle.spawn(async move { on_data(data).await });
        }
    }

    pub(crate) fn handle_close(&self) {
        if let Some(on_close) = self.on_close.as_ref() {
            let on_close = on_close.clone();
            self.handle.spawn(async move { on_close(()).await });
        }

        self.connected.store(false, Ordering::Release);
    }
}

fn current_time_in_seconds() -> u64 {
    // Safety: Current time is after the EPOCH
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

impl Stream for Socket {
    type Item = Result<Packet>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let me = self.get_mut();
        let ttnp = Self::time_to_next_ping(me.last_ping.clone(), me.max_ping_timeout);
        println!("polling");
        // Poll the generator first
        match me.generator.next().poll_unpin(cx) {
            std::task::Poll::Ready(Some(value)) => {
                println!("value from stream some");
                return std::task::Poll::Ready(Some(value));
            }
            std::task::Poll::Ready(None) => {
                println!("value from stream none");
                // Generator finished, return None
                return std::task::Poll::Ready(None);
            }
            std::task::Poll::Pending => {
                println!("pending ttnp {ttnp}");
            }
        };

        println!("sleeping ttnp {ttnp}");

        let timeout = tokio::time::Instant::now()
            .checked_add(tokio::time::Duration::from_secs(ttnp))
            .unwrap()
        

        me.sleep.then(|| => {
            
        })
        std::task::Poll::Pending

        // match timeout.poll(cx) {
        //     std::task::Poll::Ready(timeout) => {
        //         println!("timeout ready");
        //         match timeout {
        //             // Stream / generator has new value.
        //             Ok(value) => {
        //                 println!("message from generator");
        //                 return std::task::Poll::Ready(value);
        //             }
        //             Err(elapsed) => {
        //                 // Be nice and disconnect properly.
        //                 // if let Err(e) = self.disconnect().await {
        //                 //     return std::task::Poll::Ready(Some(Err(e)));
        //                 // } else {
        //                 //     return std::task::Poll::Ready(Some(Err(Error::PingTimeout())));
        //                 // }
        //                 println!("timeout elapsed: {elapsed}");
        //                 // TODO: remove
        //                 return std::task::Poll::Pending;
        //             }
        //         }
        //     }
        //     std::task::Poll::Pending => {
        //         println!("timeout pending");
        //         std::task::Poll::Pending
        //     }
        // }
    }
}

#[cfg_attr(tarpaulin, ignore)]
impl Debug for Socket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socket")
            .field("transport", &self.transport)
            .field("on_close", &self.on_close)
            .field("on_data", &self.on_data)
            .field("on_error", &self.on_error)
            .field("on_open", &self.on_open)
            .field("on_packet", &self.on_packet)
            .field("connected", &self.connected)
            .field("last_ping", &self.last_ping)
            .field("last_pong", &self.last_pong)
            .field("connection_data", &self.connection_data)
            .finish()
    }
}
