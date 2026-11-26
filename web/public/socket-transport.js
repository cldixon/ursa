// The session protocol over a WebSocket: `ursa serve`'s native engine
// (design/console.md §4.2). engine-client.js picks it when the page was
// served by `ursa serve`, which marks its index with
// <meta name="ursa-transport" content="socket">.
//
// A message is a JSON text frame, then one binary frame per part it lists, in
// order. A binary frame starts with 8 bytes: the request's id (u32), the
// part's index (u16) and two reserved bytes, zero, little-endian. Requests and
// replies are framed alike; the server never interleaves two replies' frames.
//
// The socket is opened in a worker of its own (socket-worker.js), not on the
// page (URSA-7): a large part (`.graph rmat` is 8.4 MB of UGR) is received
// and assembled there, off the page's thread, and each reply comes to the
// page in one postMessage with its parts' buffers transferred. No part is
// copied on the way: a part is the binary frame's own buffer, given with
// `offset: 8` past the frame's header (engine-client.js's header). The
// requests' parts are transferred to the worker, which frames them.
//
// The page's token travels in the cookie the link ursa serve printed set
// (HttpOnly: no script sees it; the worker's handshake carries it, as the
// page's would). It is a transport as engine-client.js's header defines one:
// open({reply, event, fail}), send, close.

/** Whether this page was served by `ursa serve`. */
export function servedOverSocket() {
  return document.querySelector('meta[name="ursa-transport"]')?.content === "socket";
}

/** `ursa serve`'s native engine, over a WebSocket at /session, opened in socket-worker.js. */
export class SocketTransport {
  constructor(url = sessionUrl(), worker = new URL("./socket-worker.js", import.meta.url)) {
    this.url = url;
    this.workerUrl = worker;
  }

  /** Open the socket; resolves once it is open. */
  open({ reply, event, fail }) {
    const worker = (this.worker = new Worker(this.workerUrl, { type: "module" }));
    return new Promise((resolve, reject) => {
      let opened = false;
      worker.onmessage = ({ data }) => {
        if (data.type === "reply") reply(data.message);
        else if (data.type === "event") event(data.message);
        else if (data.type === "fail") fail(data.text);
        else if (data.type === "open") {
          opened = true;
          resolve({ transport: "socket" });
        } else if (data.type === "unreachable") reject(new Error(data.text));
      };
      // An error the worker didn't catch: its socket may be in any state.
      worker.onerror = (e) => {
        e.preventDefault();
        if (opened) fail(e.message || "the connection to the engine failed");
        else reject(new Error(e.message || "cannot start the connection to the engine"));
      };
      worker.postMessage({ type: "open", url: this.url });
    });
  }

  /** Send a request: the worker frames it, its parts' buffers transferred there. */
  send(message) {
    const parts = message.parts ?? [];
    this.worker.postMessage(
      { type: "send", message: { ...message, parts } },
      parts.map((p) => p.bytes),
    );
  }

  close() {
    this.worker?.postMessage({ type: "close" });
  }
}

/**
 * The protocol's framing, for whoever holds the socket (socket-worker.js):
 * `receive(data)` for each message the socket gets (a string or an
 * ArrayBuffer) calls reply(message) once a reply's parts have all come,
 * event(message) for an event, and fail(text) if the engine sent what cannot
 * be read. A part is its frame's buffer, at `offset` 8: nothing is copied.
 */
export function reader({ reply, event, fail }) {
  // The reply being read: its JSON, and its parts as they arrive.
  let pending = null;
  let arrived = 0;
  const deliver = () => {
    if (pending && arrived === pending.parts.length) {
      const done = pending;
      pending = null;
      reply(done);
    }
  };
  return (data) => {
    if (typeof data === "string") {
      const message = JSON.parse(data);
      if ("event" in message) return event(message);
      if (pending) return fail("the engine sent a reply before the last one's parts");
      pending = { ...message, parts: message.parts ?? [] };
      arrived = 0;
      return deliver();
    }
    const head = new DataView(data, 0, 8);
    const id = head.getUint32(0, true);
    const index = head.getUint16(4, true);
    const part = pending?.parts[index];
    if (!pending || pending.id !== id || !part || part.bytes instanceof ArrayBuffer) return fail("the engine sent a part of no reply");
    pending.parts[index] = { ...part, bytes: data, offset: 8 };
    arrived++;
    deliver();
  };
}

/** A request's frames: its JSON, then each part in a frame of its own. */
export function framesOf(message) {
  const parts = message.parts ?? [];
  const frames = [JSON.stringify({ ...message, parts: parts.map(({ kind, name, bytes }) => ({ kind, name, bytes: bytes.byteLength })) })];
  parts.forEach((part, index) => {
    const frame = new Uint8Array(8 + part.bytes.byteLength);
    const head = new DataView(frame.buffer);
    head.setUint32(0, message.id, true);
    head.setUint16(4, index, true);
    frame.set(new Uint8Array(part.bytes), 8);
    frames.push(frame);
  });
  return frames;
}

/** ws://this-host/session, beside the page. */
function sessionUrl() {
  const url = new URL("/session", location.href);
  url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
  return url.href;
}
