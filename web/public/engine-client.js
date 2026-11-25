// The page's one way to the engine: the session protocol (design/console.md
// §4), over one of two transports.
//
//   const client = connect(defaultTransport());
//   await client.ready;                        // hello's result
//   const { out, err } = await client.call("feed", { line: "SELECT 1;" });
//
// - WorkerTransport: the browser demo's engine, ursa-wasm in a Web Worker
//   (engine-worker.js), over postMessage. Binary parts are transferred.
//
// A transport carries these messages:
//
//   request  {v: 1, id, op, args, parts: [{kind, name, bytes}]}
//   reply    {id, ok: true, result, parts: [{kind, name, bytes}]}
//            {id, ok: false, error: {kind, message}}
//   event    {event, ...}
//
// `call(op, args, parts)` resolves to the reply's result with each part put
// in it under its name: a UGR graph or a file as a Uint8Array, `f32` parts as
// a Float32Array and `u32` parts as a Uint32Array. It rejects with an Error
// whose `kind` is the protocol's (`sql`, `engine`, `protocol`, …) and whose
// `crash` is set when the engine stopped (`crash`, or the transport closed):
// the page then connects again, to a fresh session.
//
// Requests run one at a time, in the order they were sent.

const PROTOCOL = 1;

/** A client of the session on the other end of `transport`. */
export function connect(transport, client = { name: "shell", kind: "human" }) {
  let nextId = 0;
  const calls = new Map();
  const listeners = new Map();
  let closed = null;

  const fail = (error) => {
    closed ??= error;
    for (const call of calls.values()) call.reject(error);
    calls.clear();
  };
  const opened = transport.open({
    reply(message) {
      const call = calls.get(message.id);
      calls.delete(message.id);
      if (!call) return;
      if (message.ok) call.resolve(flatten(message));
      else call.reject(problem(message.error));
    },
    event(message) {
      for (const fn of listeners.get(message.event) ?? []) fn(message);
    },
    fail: (message) => fail(stopped(message)),
  });

  function send(op, args = {}, parts = []) {
    if (closed) return Promise.reject(closed);
    return new Promise((resolve, reject) => {
      const id = nextId++;
      calls.set(id, { resolve, reject });
      transport.send({ v: PROTOCOL, id, op, args, parts });
    });
  }

  const ready = opened.then(() => send("hello", { client }));
  ready.catch(() => {});
  return {
    ready,
    /** Run `op`; `parts` are [{kind, name, bytes: ArrayBuffer}], transferred. */
    call: (op, args, parts) => ready.then(() => send(op, args, parts)),
    on(event, fn) {
      if (!listeners.has(event)) listeners.set(event, new Set());
      listeners.get(event).add(fn);
      return () => listeners.get(event).delete(fn);
    },
    close() {
      transport.close();
      fail(stopped("the engine stopped"));
    },
  };
}

/** The transport this page was served for. */
export function defaultTransport() {
  return new WorkerTransport();
}

/** The reply's result, with its parts in it by name. */
function flatten({ result = {}, parts = [] }) {
  const out = { ...result };
  for (const part of parts) {
    const buffer = part.bytes;
    out[part.name] = part.kind === "f32" ? new Float32Array(buffer) : part.kind === "u32" ? new Uint32Array(buffer) : new Uint8Array(buffer);
  }
  return out;
}

function problem({ kind, message }) {
  return Object.assign(new Error(message), { kind, crash: kind === "crash" });
}

function stopped(message) {
  return Object.assign(new Error(message || "the engine stopped"), { kind: "crash", crash: true });
}

/** The engine in a Web Worker: engine-worker.js, ursa-wasm. */
export class WorkerTransport {
  constructor(url = new URL("./engine-worker.js", import.meta.url)) {
    this.url = url;
  }

  /** Start the worker; resolves once the engine has loaded. */
  open({ reply, event, fail }) {
    this.worker = new Worker(this.url, { type: "module" });
    return new Promise((resolve, reject) => {
      this.worker.onmessage = ({ data }) => {
        if ("ready" in data) {
          if (data.ready) resolve(data);
          else reject(new Error(data.error));
        } else if ("event" in data) event(data);
        else reply(data);
      };
      // An error the worker didn't catch: it may be in any state.
      this.worker.onerror = (e) => {
        e.preventDefault();
        fail(e.message);
      };
    });
  }

  send(message) {
    this.worker.postMessage(
      message,
      message.parts.map((p) => p.bytes),
    );
  }

  close() {
    this.worker?.terminate();
  }
}
