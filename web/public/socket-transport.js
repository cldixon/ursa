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
// The page's token travels in the cookie the link ursa serve printed set
// (HttpOnly: this script never sees it).

/** Whether this page was served by `ursa serve`. */
export function servedOverSocket() {
  return document.querySelector('meta[name="ursa-transport"]')?.content === "socket";
}

/** `ursa serve`'s native engine, over a WebSocket at /session. */
export class SocketTransport {
  constructor(url = sessionUrl()) {
    this.url = url;
  }

  /** Open the socket; resolves once it is open. */
  open({ reply, event, fail }) {
    const socket = (this.socket = new WebSocket(this.url));
    socket.binaryType = "arraybuffer";
    // The reply being read: its JSON, and its parts as they arrive.
    let pending = null;
    const deliver = () => {
      if (pending && pending.parts.every((p) => p.bytes instanceof ArrayBuffer)) {
        const done = pending;
        pending = null;
        reply(done);
      }
    };
    socket.onmessage = ({ data }) => {
      if (typeof data === "string") {
        const message = JSON.parse(data);
        if ("event" in message) return event(message);
        if (pending) return fail("the engine sent a reply before the last one's parts");
        pending = { ...message, parts: message.parts ?? [] };
        return deliver();
      }
      const head = new DataView(data, 0, 8);
      const id = head.getUint32(0, true);
      const index = head.getUint16(4, true);
      if (!pending || pending.id !== id || !pending.parts[index]) return fail("the engine sent a part of no reply");
      pending.parts[index] = { ...pending.parts[index], bytes: data.slice(8) };
      deliver();
    };
    return new Promise((resolve, reject) => {
      let opened = false;
      socket.onopen = () => {
        opened = true;
        resolve({ transport: "socket" });
      };
      socket.onerror = () => {
        if (!opened) reject(new Error("cannot reach the engine (is ursa serve running, and was this page opened from the link it printed?)"));
      };
      socket.onclose = (e) => {
        if (opened) fail(`the connection to the engine closed${e.reason ? ` (${e.reason})` : ""}`);
      };
    });
  }

  /** Send a request: its JSON, then each part in a frame of its own. */
  send(message) {
    const parts = message.parts ?? [];
    this.socket.send(JSON.stringify({ ...message, parts: parts.map(({ kind, name, bytes }) => ({ kind, name, bytes: bytes.byteLength })) }));
    parts.forEach((part, index) => {
      const frame = new Uint8Array(8 + part.bytes.byteLength);
      const head = new DataView(frame.buffer);
      head.setUint32(0, message.id, true);
      head.setUint16(4, index, true);
      frame.set(new Uint8Array(part.bytes), 8);
      this.socket.send(frame);
    });
  }

  close() {
    this.socket?.close();
  }
}

/** ws://this-host/session, beside the page. */
function sessionUrl() {
  const url = new URL("/session", location.href);
  url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
  return url.href;
}
