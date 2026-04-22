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

function nextId() {
  return "r" + Math.random().toString(36).slice(2) + Date.now().toString(36);
}

export function t64(type, payload = {}) {
  return new Promise((resolve, reject) => {
    const id = nextId();
    const timer = setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id);
        reject(new Error(`t64 ${type} timed out`));
      }
    }, 30000);
    pending.set(id, { resolve, reject, timer });
    try {
      window.parent.postMessage({ type, payload: { ...payload, id } }, "*");
    } catch (err) {
      clearTimeout(timer);
      pending.delete(id);
      reject(err);
    }
  });
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

export async function daemonFetch(base, path, init = {}) {
  if (!base) throw new Error("daemon not running");
  const url = base.replace(/\/+$/, "") + path;
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
  if (!base) throw new Error("daemon not running");
  const url = base.replace(/\/+$/, "") + path;
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
// exponential backoff on close/error. handlers = { open, message, error, close }.
// message receives a JSON-decoded frame (or raw string on parse failure).
// Returns { close, send }. close() stops reconnects and shuts the socket.
export function daemonWS(base, path = "/ws", handlers = {}) {
  if (!base) throw new Error("daemon not running");
  const wsUrl = base.replace(/^http/i, "ws").replace(/\/+$/, "") + path;

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
      if (handlers.open) { try { handlers.open(e); } catch (err) { console.error(err); } }
    };
    ws.onmessage = (e) => {
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
