// bridge.js — shared host adapter plus daemon transport helpers.
//
// The same renderer runs in two hosts:
//   - Terminal 64, via the existing postMessage RPC protocol
//   - the packaged Tauri app, via a deliberately narrow invoke surface
//
// Keep host-specific privileges in this file. Views consume `host` methods and
// never receive a raw shell primitive.

import {
  PLATFORM,
  BINARY_REL,
  WIDGET_DIR_SHELL,
  PLUGIN_DIR_DISPLAY,
  PLUGIN_DIR_SHELL,
  buildDaemonCmd,
  checkBinaryCmd,
  checkCargoCmd,
  openFolderEnsuredCmd,
  pickFolderCmd,
  pluginInstallCmd,
  readFileCmd,
  secureWidgetStateCmd,
  wallyInstallCmd,
  writeFileFromB64Cmd,
  joinShell,
} from "./platform.js";
//
// Exports:
//   host                         narrow cross-host native capability adapter
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
const daemonAuthTokensByBase = new Map();

function daemonBaseKey(base) {
  if (!base) return null;
  try {
    const url = new URL(String(base));
    return `${url.protocol}//${url.host}`.toLowerCase();
  } catch {
    return String(base).replace(/\/+$/, "").toLowerCase();
  }
}

function findTauriInvoke() {
  const globalApi = globalThis.__TAURI__;
  if (globalApi && globalApi.core && typeof globalApi.core.invoke === "function") {
    return globalApi.core.invoke.bind(globalApi.core);
  }
  return null;
}

const tauriInvoke = findTauriInvoke();
export const HOST_KIND = tauriInvoke ? "tauri" : "terminal64";
export const IS_DESKTOP_HOST = HOST_KIND === "tauri";

async function invokeDesktop(command, args = {}) {
  if (!tauriInvoke) throw new Error(`${command} is only available in the desktop app`);
  try {
    return await tauriInvoke(command, args);
  } catch (error) {
    const message = typeof error === "string"
      ? error
      : (error && error.message) || String(error);
    throw new Error(message);
  }
}

function unwrapValue(value) {
  if (value && typeof value === "object" && Object.hasOwn(value, "value")) {
    return value.value;
  }
  return value;
}

function unwrapText(value) {
  if (typeof value === "string") return value;
  if (!value || typeof value !== "object") return "";
  return value.content ?? value.text ?? value.data ?? value.stdout ?? "";
}

function encodeTextBase64(value) {
  return btoa(unescape(encodeURIComponent(String(value))));
}

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

async function widgetExec(command, options = {}) {
  if (IS_DESKTOP_HOST) throw new Error("raw shell execution is unavailable in the desktop app");
  return t64("t64:exec", { command, ...options });
}

/**
 * Privileged host operations used by the shared renderer.
 *
 * Terminal 64 implementations preserve the established RPC/command behavior.
 * Tauri implementations map one-to-one to allowlisted native commands; there
 * is intentionally no `exec` method on this public object.
 */
export const host = Object.freeze({
  kind: HOST_KIND,
  isDesktop: IS_DESKTOP_HOST,
  supports: Object.freeze({
    buildDaemon: !IS_DESKTOP_HOST,
    multiDaemon: IS_DESKTOP_HOST,
    spawnSession: !IS_DESKTOP_HOST,
    hostTheme: !IS_DESKTOP_HOST,
    windowDrag: IS_DESKTOP_HOST,
    updates: IS_DESKTOP_HOST,
  }),

  async startWindowDrag() {
    if (!IS_DESKTOP_HOST) return { supported: false };
    const getCurrentWindow = globalThis.__TAURI__?.window?.getCurrentWindow;
    if (typeof getCurrentWindow !== "function") {
      throw new Error("the Tauri window API is unavailable");
    }
    const currentWindow = getCurrentWindow();
    if (!currentWindow || typeof currentWindow.startDragging !== "function") {
      throw new Error("window dragging is unavailable");
    }
    await currentWindow.startDragging();
    return { supported: true };
  },

  async appInfo() {
    if (IS_DESKTOP_HOST) {
      return (await invokeDesktop("app_info")) || { kind: "tauri", platform: PLATFORM };
    }
    return {
      kind: "terminal64",
      platform: PLATFORM,
      widgetDir: WIDGET_DIR_SHELL,
      daemonPath: joinShell(WIDGET_DIR_SHELL, BINARY_REL),
      pluginDir: PLUGIN_DIR_DISPLAY,
    };
  },

  async updateCheck() {
    if (!IS_DESKTOP_HOST) {
      return { configured: false, available: false, currentVersion: null, version: null };
    }
    return invokeDesktop("update_check");
  },

  async updateInstall(expectedVersion) {
    if (!IS_DESKTOP_HOST) throw new Error("application updates are only available in the desktop app");
    return invokeDesktop("update_install", { expectedVersion: String(expectedVersion || "") });
  },

  async stateGet(key) {
    if (IS_DESKTOP_HOST) return unwrapValue(await invokeDesktop("state_get", { key }));
    return unwrapValue(await t64("t64:get-state", { key }));
  },

  async stateSet(key, value) {
    if (IS_DESKTOP_HOST) return invokeDesktop("state_set", { key, value });
    const result = await t64("t64:set-state", { key, value });
    if (PLATFORM !== "windows") {
      const secured = await widgetExec(secureWidgetStateCmd());
      if (secured && secured.code != null && secured.code !== 0) {
        throw new Error("Terminal 64 state was saved but could not be restricted to mode 0600");
      }
    }
    return result;
  },

  async secretGet(key) {
    if (IS_DESKTOP_HOST) return unwrapValue(await invokeDesktop("secret_get", { key }));
    const secrets = await this.stateGet("secrets");
    return secrets && typeof secrets === "object" ? (secrets[key] ?? null) : null;
  },

  async secretSet(key, value) {
    if (IS_DESKTOP_HOST) return invokeDesktop("secret_set", { key, value });
    const current = await this.stateGet("secrets");
    const secrets = current && typeof current === "object" && !Array.isArray(current)
      ? { ...current }
      : {};
    secrets[key] = value;
    return this.stateSet("secrets", secrets);
  },

  async secretDelete(key) {
    if (IS_DESKTOP_HOST) return invokeDesktop("secret_delete", { key });
    const current = await this.stateGet("secrets");
    const secrets = current && typeof current === "object" && !Array.isArray(current)
      ? { ...current }
      : {};
    delete secrets[key];
    return this.stateSet("secrets", secrets);
  },

  async readTextFile(path) {
    if (IS_DESKTOP_HOST) return unwrapText(await invokeDesktop("read_project_file", { path }));
    return unwrapText(await widgetExec(readFileCmd(path)));
  },

  async writeTextFile(path, text) {
    if (IS_DESKTOP_HOST) {
      return invokeDesktop("write_project_file", { path, content: String(text) });
    }
    return widgetExec(writeFileFromB64Cmd(path, encodeTextBase64(text)));
  },

  async readResourceText(path) {
    try {
      const response = await fetch(path);
      if (response.ok) return await response.text();
    } catch {}
    if (IS_DESKTOP_HOST) {
      return unwrapText(await invokeDesktop("read_resource_file", { path }));
    }
    return unwrapText(await t64("t64:read-file", { path: `{widgetDir}/${path}` }));
  },

  async pickFolder(prompt = "Pick a folder") {
    if (IS_DESKTOP_HOST) return unwrapValue(await invokeDesktop("pick_folder", { prompt }));
    const result = await widgetExec(pickFolderCmd(prompt));
    return unwrapText(result).replace(/^﻿/, "").trim() || null;
  },

  async openPath(path) {
    if (IS_DESKTOP_HOST) return invokeDesktop("open_path", { path });
    return widgetExec(openFolderEnsuredCmd(path));
  },

  async clipboardWrite(text) {
    if (IS_DESKTOP_HOST) return invokeDesktop("clipboard_write", { text: String(text) });
    return t64("t64:clipboard-write", { text: String(text), timeoutMs: 5000 });
  },

  async pluginInstall() {
    if (IS_DESKTOP_HOST) return invokeDesktop("plugin_install");
    const command = pluginInstallCmd({
      srcFile: joinShell(WIDGET_DIR_SHELL, "plugin/Plugin.rbxm"),
      destDir: PLUGIN_DIR_SHELL,
      destName: "RoSync.rbxm",
      staleNames: ["RoSync.lua", "RoSync.luau"],
    });
    const result = await widgetExec(command);
    if (result && result.code != null && result.code !== 0) {
      throw new Error(result.stderr?.trim() || `plugin install exited ${result.code}`);
    }
    return { ok: true, path: joinShell(PLUGIN_DIR_DISPLAY, "RoSync.rbxm"), restartRequired: true };
  },

  async binaryStatus() {
    if (IS_DESKTOP_HOST) {
      const info = await this.appInfo();
      return { present: !!info.daemonPath, cargo: false, bundled: true, path: info.daemonPath || null };
    }
    const [binary, cargo] = await Promise.all([
      widgetExec(checkBinaryCmd()),
      widgetExec(checkCargoCmd()),
    ]);
    return {
      present: unwrapText(binary).trim().toLowerCase() === "yes",
      cargo: unwrapText(cargo).trim().toLowerCase() === "yes",
      bundled: false,
      path: joinShell(WIDGET_DIR_SHELL, BINARY_REL),
    };
  },

  async buildDaemon(options = {}) {
    if (IS_DESKTOP_HOST) throw new Error("The desktop app ships with a managed daemon");
    return widgetExec(buildDaemonCmd(), { timeoutMs: options.timeoutMs ?? 5 * 60_000 });
  },

  async wallyInstall(cwd) {
    if (IS_DESKTOP_HOST) return invokeDesktop("wally_install", { cwd });
    return widgetExec(wallyInstallCmd(cwd), { timeoutMs: 2 * 60_000 });
  },

  async windowBounds() {
    if (IS_DESKTOP_HOST) return null;
    return t64("t64:get-bounds", { timeoutMs: 1000 });
  },

  async createSession(payload) {
    if (IS_DESKTOP_HOST) return { supported: false };
    return t64("t64:create-session", { ...payload, timeoutMs: 10_000 });
  },

  async daemonEnsure(spec) {
    return invokeDesktop("daemon_ensure", {
      spec: {
        project: spec.project,
        projectsRoot: spec.projectsRoot ?? null,
        preferredPort: spec.preferredPort,
        gameId: spec.gameId ?? null,
        groupId: spec.groupId ?? null,
        placeIds: Array.isArray(spec.placeIds) ? spec.placeIds : [],
        ownerToken: spec.ownerToken ?? null,
      },
    });
  },

  async daemonStatus(project = null) {
    return invokeDesktop("daemon_status", { project });
  },

  async daemonList() {
    if (!IS_DESKTOP_HOST) return { ok: true, daemons: [] };
    return invokeDesktop("daemon_list");
  },

  async daemonStop(spec) {
    if (!IS_DESKTOP_HOST) throw new Error("managed daemon stop is only available in the desktop app");
    return invokeDesktop("daemon_stop", {
      spec: {
        project: spec.project,
        bootId: spec.bootId ?? null,
        ownerToken: spec.ownerToken ?? null,
      },
    });
  },

  async projectBrokerStatus() {
    if (!IS_DESKTOP_HOST) return { ok: false, supported: false };
    return invokeDesktop("project_broker_status");
  },

  async projectInitDrain() {
    if (!IS_DESKTOP_HOST) return [];
    const events = await invokeDesktop("project_init_drain");
    return Array.isArray(events) ? events : [];
  },

  ready() {
    if (IS_DESKTOP_HOST) return Promise.resolve({ ok: true });
    return t64("t64:ready", { app: "ro-sync", version: 1 });
  },
});

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
export function setDaemonAuthToken(token, base = null) {
  const value = typeof token === "string" && token.length ? token : null;
  const key = daemonBaseKey(base);
  if (!key) {
    daemonAuthToken = value;
    return;
  }
  if (value) daemonAuthTokensByBase.set(key, value);
  else daemonAuthTokensByBase.delete(key);
}

export function daemonURL(base, path = "", authToken) {
  if (!base) throw new Error("daemon not running");
  const url = new URL(base.replace(/\/+$/, "") + path);
  if (!["127.0.0.1", "localhost", "::1"].includes(url.hostname)) {
    throw new Error("daemon URL must use a loopback host");
  }
  const token = authToken === undefined
    ? (daemonAuthTokensByBase.get(daemonBaseKey(base)) || daemonAuthToken)
    : authToken;
  if (token) url.searchParams.set("widgetToken", token);
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
          protocol: 5,
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
