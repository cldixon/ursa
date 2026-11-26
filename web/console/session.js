// The console's session: one engine client (engine-client.js) on the shared
// session (`hello {session: "shared"}`), kept connected, and the requests the
// console has running, which the status bar shows and can cancel.
//
//   import { session } from "/console/session.js";
//   session.on("connect", (hello) => …);          // after every (re)connect
//   session.onEvent("catalog", () => …);          // a protocol event (§4.4)
//   const r = await session.run("query", { sql }, { label: "query" });
//   session.cancel();                             // the running request
//
// Over `ursa serve` it is a SocketTransport; served anywhere else (the
// preview), the wasm engine in a worker, which has no `cancel`. When the
// connection goes (the server stopped or restarted, the engine crashed), it
// connects again, sooner at first and then every 10 s, and says why it can't
// when it can tell: the server is not running, or it was restarted and this
// page's token is the last run's.
//
// States: "connecting" (the first time), "connected", "reconnecting" (the
// connection went; `retryAt` says when the next try is, `reason` why).
import { connect, defaultTransport } from "/engine-client.js";
import { servedOverSocket } from "/socket-transport.js";

const CLIENT = { name: "console", kind: "human" };
/** The waits between tries, in ms; the last repeats. */
const BACKOFF = [500, 1000, 2000, 4000, 7000, 10000];

class Session {
  constructor() {
    this.state = "connecting";
    /** hello's result, while connected. */
    this.hello = null;
    /** Why the last connection failed or went, in words, or null. */
    this.reason = null;
    /** When the next try is (performance.now()), while reconnecting. */
    this.retryAt = null;
    /** The console's requests not yet answered, oldest first: {op, label, started, reply, cancelling}. */
    this.requests = [];
    this.client = null;
    this.listeners = new Map();
    this.eventListeners = new Map();
    this.tries = 0;
    this.timer = null;
  }

  /** Listen for "state", "connect" (hello) or "requests"; returns an unsubscribe. */
  on(name, fn) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(fn);
    return () => this.listeners.get(name).delete(fn);
  }

  /** Listen for a protocol event (catalog, progress, notice, graph, activity), on every connection. */
  onEvent(event, fn) {
    if (!this.eventListeners.has(event)) this.eventListeners.set(event, new Set());
    this.eventListeners.get(event).add(fn);
    // On the connection, or the one being opened (whose listeners were
    // added before this one came).
    const off = (this.client ?? this.opening)?.on(event, fn);
    return () => {
      this.eventListeners.get(event).delete(fn);
      off?.();
    };
  }

  emit(name, value) {
    for (const fn of this.listeners.get(name) ?? []) {
      try {
        fn(value);
      } catch (e) {
        console.error(e);
      }
    }
  }

  setState(state) {
    this.state = state;
    this.emit("state", this);
  }

  /** Whether the session can cancel a running request (native sessions). */
  get canCancel() {
    return Boolean(this.hello?.capabilities?.includes("cancel"));
  }

  /** Connect, and keep connecting. */
  start() {
    if (!this.client && !this.timer) this.open();
  }

  /** Try again now, instead of when the wait is over. */
  reconnect() {
    clearTimeout(this.timer);
    this.timer = null;
    if (this.state !== "connected") this.open();
  }

  async open() {
    this.retryAt = null;
    this.setState(this.tries === 0 && !this.hello ? "connecting" : "reconnecting");
    let client;
    const transport = watched(defaultTransport(), (message) => this.lost(client, message));
    client = connect(transport, CLIENT, { session: "shared" });
    this.opening = client;
    for (const [event, fns] of this.eventListeners) for (const fn of fns) client.on(event, fn);
    try {
      const hello = await client.ready.finally(() => {
        if (this.opening === client) this.opening = null;
      });
      this.client = client;
      this.hello = hello;
      this.reason = null;
      this.tries = 0;
      this.setState("connected");
      this.emit("connect", hello);
    } catch (e) {
      client.close();
      this.retry(await explain(e.message));
    }
  }

  /** The connection `client` went. */
  async lost(client, message) {
    if (!client || client !== this.client) return;
    this.client = null;
    // Its requests have failed (engine-client rejects them): the status bar
    // lets them go as they settle.
    this.retry(await explain(message, true));
  }

  retry(reason) {
    this.reason = reason;
    const wait = BACKOFF[Math.min(this.tries, BACKOFF.length - 1)];
    this.tries++;
    this.retryAt = performance.now() + wait;
    this.setState("reconnecting");
    clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      this.open();
    }, wait);
  }

  /**
   * Run `op` on the session, shown in the status bar as `label` (the op's
   * name by default) with a Cancel button until it is answered. Resolves to
   * the reply's result (engine-client.js's call()); rejects with the
   * protocol's error, kind `cancelled` if it was cancelled.
   */
  run(op, args = {}, { label = op, parts } = {}) {
    if (!this.client) return Promise.reject(Object.assign(new Error("not connected to the engine"), { kind: "crash", crash: true }));
    const reply = this.client.call(op, args, parts);
    const request = { op, label, started: performance.now(), reply, client: this.client, cancelling: false };
    this.requests.push(request);
    this.emit("requests", this.requests);
    const done = () => {
      this.requests = this.requests.filter((r) => r !== request);
      this.emit("requests", this.requests);
    };
    reply.then(done, done);
    return reply;
  }

  /** Cancel `reply` (a run()'s promise), or the oldest request still running. */
  async cancel(reply) {
    const request = reply ? this.requests.find((r) => r.reply === reply) : this.requests.find((r) => !r.cancelling);
    if (!request || !this.canCancel) return;
    request.cancelling = true;
    this.emit("requests", this.requests);
    await request.client.cancel(request.reply).catch(() => {});
  }
}

/** `transport`, telling `lost` when it fails once open. */
function watched(transport, lost) {
  return {
    open: (handlers) => transport.open({ ...handlers, fail: (message) => (handlers.fail(message), lost(message)) }),
    send: (message) => transport.send(message),
    close: () => transport.close(),
  };
}

/**
 * Why the connection failed, in words. Over the socket, ask the server: no
 * answer means it isn't running; 401 for its data means it is, with another
 * token than this page's (it was restarted: its new link sets the new one).
 */
async function explain(message, wasOpen = false) {
  if (!servedOverSocket()) return message || "the engine stopped";
  const stopped = /the server stopped/.test(message ?? "");
  try {
    const r = await fetch("/data/.ursa-console", { method: "HEAD", cache: "no-store" });
    if (r.status === 401) return "ursa serve was restarted: open the link it printed";
    return stopped ? "ursa serve stopped" : wasOpen ? "the connection to ursa serve closed" : "cannot reach the engine";
  } catch {
    return stopped ? "ursa serve stopped" : "ursa serve is not running";
  }
}

export const session = new Session();
