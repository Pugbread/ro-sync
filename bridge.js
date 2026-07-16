// bridge.js — minimal postMessage + SSE helpers for the Ro Sync widget.
//
// Exports:
//   t64(type, payload) -> Promise<any>     postMessage RPC to the T64 host
//   onT64(type, fn)    -> unsubscribe      subscribe to host-pushed events
//   daemonFetch(base, path, init) -> Promise<Response>
//   daemonSSE(base, path, handlers)        EventSource-style wrapper
//   emit(name, detail) / on(name, fn)      intra-widget event bus

// T64 protocol (per the widget-host docs):
//   request : { type, payload: { ...args, id } }
//   reply   : { type: "<...>-result" (or similar), payload: { id, ...fields } }
// Matching is by payload.id — NOT a top-level id.
const pending = new Map();          // id -> {resolve, reject, timer}
const listeners = new Map();        // t64 event type -> Set<fn>
let daemonAuthToken = null;

function nextId() {
  return "r" + Math.random().toString(36).slice(2) + Date.now().toString(36);
}

let setStateQueue = Promise.resolve();

// `payload.timeoutMs` — optional override for the default 30s timeout, for
// long-running ops like `cargo build`. The timeoutMs key is NOT forwarded to
// the host; it only controls our local pending-promise expiry.
function postT64(type, payload = {}) {
  return new Promise((resolve, reject) => {
    const id = nextId();
    const { timeoutMs = 30000, ...forwarded } = payload || {};
    const timer = setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id);
        reject(new Error(`t64 ${type} timed out`));
      }
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
    try {
      window.parent.postMessage({ type, payload: { ...forwarded, id } }, "*");
    } catch (err) {
      clearTimeout(timer);
      pending.delete(id);
      reject(err);
    }
  });
}

export function t64(type, payload = {}) {
  if (type !== "t64:set-state") return postT64(type, payload);

  // Terminal 64 persists widget keys into one state file. Serializing writes
  // from this iframe prevents rapid `state` saves from racing separate
  // `secrets` saves and making top-level keys appear/disappear.
  const run = () => postT64(type, payload);
  const result = setStateQueue.then(run, run);
  setStateQueue = result.catch(() => {});
  return result;
}

export function onT64(type, fn) {
  let set = listeners.get(type);
  if (!set) { set = new Set(); listeners.set(type, set); }
  set.add(fn);
  return () => set.delete(fn);
}

window.addEventListener("message", (ev) => {
  const msg = ev.data;
  if (!msg || typeof msg !== "object") return;
  const replyId = msg.payload && msg.payload.id;
  if (replyId && pending.has(replyId)) {
    const { resolve, reject, timer } = pending.get(replyId);
    pending.delete(replyId);
    clearTimeout(timer);
    if (msg.payload.error && !msg.payload.ok && msg.payload.stdout == null) {
      reject(new Error(msg.payload.error));
    } else {
      resolve(msg.payload);
    }
    return;
  }
  if (msg.type) {
    const set = listeners.get(msg.type);
    if (set) for (const fn of set) { try { fn(msg.payload ?? msg); } catch (e) { console.error(e); } }
  }
});

// --------- Daemon HTTP helpers ---------

// Browser-backed widgets carry an Origin header and therefore authenticate to
// the localhost daemon with the same unguessable owner token used for process
// lifecycle control. Native Studio/CLI requests have no browser Origin and do
// not use this query capability.
export function setDaemonAuthToken(token) {
  daemonAuthToken = typeof token === "string" && token.length ? token : null;
}

export function daemonURL(base, path = "") {
  if (!base) throw new Error("daemon not running");
  const url = new URL(base.replace(/\/+$/, "") + path);
  if (!["127.0.0.1", "localhost", "::1"].includes(url.hostname)) {
    throw new Error("daemon URL must use a loopback host");
  }
  if (daemonAuthToken) url.searchParams.set("widgetToken", daemonAuthToken);
  return url.toString();
}

export async function daemonFetch(base, path, init = {}) {
  const url = daemonURL(base, path);
  const res = await fetch(url, {
    ...init,
    headers: { "content-type": "application/json", ...(init.headers || {}) },
  });
  return res;
}

export async function daemonJson(base, path, init) {
  const res = await daemonFetch(base, path, init);
  if (!res.ok) throw new Error(`${path} -> ${res.status}`);
  const ct = res.headers.get("content-type") || "";
  return ct.includes("json") ? res.json() : res.text();
}

// Thin SSE wrapper. handlers = { open, message, error, [customEventName]: fn }.
// Returns { close }.
export function daemonSSE(base, path, handlers = {}) {
  const url = daemonURL(base, path);
  const es = new EventSource(url);
  es.onopen = (e) => handlers.open && handlers.open(e);
  es.onerror = (e) => handlers.error && handlers.error(e);
  es.onmessage = (e) => handlers.message && handlers.message(parseMaybe(e.data), e);
  for (const [name, fn] of Object.entries(handlers)) {
    if (["open", "error", "message"].includes(name)) continue;
    es.addEventListener(name, (e) => fn(parseMaybe(e.data), e));
  }
  return { close: () => es.close(), source: es };
}

function parseMaybe(s) {
  if (typeof s !== "string") return s;
  try { return JSON.parse(s); } catch { return s; }
}

// WebSocket wrapper for daemon realtime channel (replaces SSE /events).
// Opens ws://<host>/ws (derived from http base). Auto-reconnects with 1s→30s
// exponential backoff on close/error.
// handlers = { open, message, error, close, skipRaw }.
// skipRaw(raw, event) can return true to avoid JSON parsing hot-path frames.
// message receives a JSON-decoded frame (or raw string on parse failure).
// Returns { close, send }. close() stops reconnects and shuts the socket.
export function daemonWS(base, path = "/ws", handlers = {}) {
  if (!base) throw new Error("daemon not running");
  const wsUrl = daemonURL(base, path).replace(/^http/i, "ws");

  let ws = null;
  let stopped = false;
  let backoff = 1000;
  let reconnectTimer = null;

  function scheduleReconnect() {
    if (stopped) return;
    if (reconnectTimer) return;
    const delay = backoff;
    backoff = Math.min(30_000, Math.max(1000, backoff * 2));
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, delay);
  }

  function connect() {
    if (stopped) return;
    try {
      ws = new WebSocket(wsUrl);
    } catch (e) {
      if (handlers.error) { try { handlers.error(e); } catch {} }
      scheduleReconnect();
      return;
    }
    ws.onopen = (e) => {
      backoff = 1000;
      // Identify this socket before subscribing to privileged daemon traffic.
      // The daemon rejects request/push/response frames from unidentified or
      // role-mismatched peers; widget streams are read-only clients.
      try {
        ws.send(JSON.stringify({
          type: "hello",
          clientId: "terminal64-widget",
          role: "watch",
          protocol: 2,
        }));
      } catch {}
      if (handlers.open) { try { handlers.open(e); } catch (err) { console.error(err); } }
    };
    ws.onmessage = (e) => {
      if (handlers.skipRaw) {
        try {
          if (handlers.skipRaw(e.data, e)) return;
        } catch (err) {
          console.error(err);
        }
      }
      if (!handlers.message) return;
      const data = parseMaybe(e.data);
      try { handlers.message(data, e); } catch (err) { console.error(err); }
    };
    ws.onerror = (e) => {
      if (handlers.error) { try { handlers.error(e); } catch (err) { console.error(err); } }
    };
    ws.onclose = (e) => {
      if (handlers.close) { try { handlers.close(e); } catch (err) { console.error(err); } }
      ws = null;
      scheduleReconnect();
    };
  }

  connect();

  return {
    close: () => {
      stopped = true;
      if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
      if (ws) { try { ws.close(); } catch {} ws = null; }
    },
    send: (data) => {
      if (!ws || ws.readyState !== 1) return false;
      try {
        ws.send(typeof data === "string" ? data : JSON.stringify(data));
        return true;
      } catch { return false; }
    },
    get socket() { return ws; },
  };
}

// --------- Intra-widget event bus ---------

const bus = new EventTarget();
export function emit(name, detail) { bus.dispatchEvent(new CustomEvent(name, { detail })); }
export function on(name, fn) {
  const h = (e) => fn(e.detail);
  bus.addEventListener(name, h);
  return () => bus.removeEventListener(name, h);
}
