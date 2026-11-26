// The WebSocket to `ursa serve`, in a worker of its own (URSA-7): what
// socket-transport.js's SocketTransport runs, so that a reply's parts are
// received and assembled off the page's thread, and reach the page in one
// postMessage with their buffers transferred, uncopied.
//
// From the page: {type: "open", url}, {type: "send", message} (its parts'
// buffers transferred here), {type: "close"}.
// To the page: {type: "open"} once the socket is open, or {type:
// "unreachable", text} if it never opened; {type: "reply", message} (its
// parts' buffers transferred) and {type: "event", message}, in the order the
// engine sent them; {type: "fail", text} once, if the engine stops.

import { framesOf, reader } from "./socket-transport.js";

let socket = null;

self.onmessage = ({ data }) => {
  if (data.type === "open") open(data.url);
  else if (data.type === "send") for (const frame of framesOf(data.message)) socket.send(frame);
  else if (data.type === "close") socket?.close();
};

function open(url) {
  socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  // Once: the engine is gone, or what it sent cannot be read.
  let stopped = false;
  const fail = (text) => {
    if (stopped) return;
    stopped = true;
    self.postMessage({ type: "fail", text });
    socket.close();
  };
  const receive = reader({
    reply: (message) =>
      self.postMessage(
        { type: "reply", message },
        message.parts.map((p) => p.bytes),
      ),
    event: (message) => self.postMessage({ type: "event", message }),
    fail,
  });
  socket.onmessage = ({ data }) => {
    if (!stopped) receive(data);
  };
  let opened = false;
  socket.onopen = () => {
    opened = true;
    self.postMessage({ type: "open" });
  };
  socket.onerror = () => {
    if (!opened) self.postMessage({ type: "unreachable", text: "cannot reach the engine (is ursa serve running, and was this page opened from the link it printed?)" });
  };
  socket.onclose = (e) => {
    if (opened) fail(`the connection to the engine closed${e.reason ? ` (${e.reason})` : ""}`);
    // Nothing more to do here: the worker ends with its socket.
    self.close();
  };
}
