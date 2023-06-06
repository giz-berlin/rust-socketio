let createServer = require("http").createServer;
let server = createServer();
const io = require("socket.io")(server);
const port = 4206;
const timeout = 2000;

const sockets = new Set();
server.on('connection', (socket) => {
  sockets.add(socket);
});

console.log("Started");
var callback = (client) => {
  console.log("Connected!");
  client.emit("message", "test");
  client.on("force_disconnect", () => {
    console.log("will disconnect in ", timeout, "ms");
    setTimeout(() => {
      for (const socket of sockets) {
        socket.destroy();
        sockets.delete(socket);
      }
      console.log("forcefully disconnected clients");
    }, timeout);
  });
};
io.on("connection", callback);
server.listen(port);
