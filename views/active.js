// views/active.js — live WS log tail, op counter, plugin connection state.
import { daemonWS, daemonJson } from "../bridge.js";

const MAX_LINES = 500;

export function mountActive(root, api) {
  root.innerHTML = `
    <div class="active-grid">
      <div class="stat"><div class="label">Ops</div><div class="value" id="s-ops">0</div></div>
      <div class="stat"><div class="label">Last sync</div><div class="value" id="s-last">—</div></div>
      <div class="stat"><div class="label">Plugin</div><div class="value" id="s-plugin">unknown</div></div>
      <div class="stat"><div class="label">Project</div><div class="value" id="s-project">—</div></div>
    </div>
    <div class="row" style="margin-bottom:8px">
      <button id="act-clear">Clear log</button>
      <button id="act-snapshot">Re-snapshot</button>
      <span class="status-left" id="act-hint" style="color:var(--muted)"></span>
      <span id="act-unsynced" class="badge badge-warn" hidden></span>
    </div>
    <div id="act-log" class="log" aria-live="polite"></div>
  `;

  const $ops = root.querySelector("#s-ops");
  const $last = root.querySelector("#s-last");
  const $plugin = root.querySelector("#s-plugin");
  const $project = root.querySelector("#s-project");
  const $log = root.querySelector("#act-log");
  const $clear = root.querySelector("#act-clear");
  const $snap = root.querySelector("#act-snapshot");
  const $hint = root.querySelector("#act-hint");
  const $unsynced = root.querySelector("#act-unsynced");

  let opCount = 0;
  let lastSync = null;
  let lastSyncTimer = null;
  let ws = null;

  // Rolling-window unsynced-ops tracker: if >10 ops hit in the last 10s we
  // surface a yellow "unsynced changes" badge (Argon's queue.rs threshold).
  // Cleared on daemon reconnect.
  const UNSYNCED_WINDOW_MS = 10_000;
  const UNSYNCED_THRESHOLD = 10;
  const recentOps = [];
  function noteOp() {
    const now = Date.now();
    recentOps.push(now);
    const cutoff = now - UNSYNCED_WINDOW_MS;
    while (recentOps.length && recentOps[0] < cutoff) recentOps.shift();
    updateUnsyncedBadge();
  }
  function updateUnsyncedBadge() {
    const cutoff = Date.now() - UNSYNCED_WINDOW_MS;
    while (recentOps.length && recentOps[0] < cutoff) recentOps.shift();
    const n = recentOps.length;
    if (n > UNSYNCED_THRESHOLD) {
      $unsynced.hidden = false;
      $unsynced.textContent = `!! ${n} unsynced changes — check for errors`;
    } else {
      $unsynced.hidden = true;
    }
  }
  function clearUnsynced() {
    recentOps.length = 0;
    $unsynced.hidden = true;
  }
  const unsyncedTimer = setInterval(updateUnsyncedBadge, 1000);

  function setPluginStatus(label, kind) {
    $plugin.textContent = label;
    $plugin.style.color = kind === "ok" ? "var(--ok)" : kind === "warn" ? "var(--warn)" : kind === "err" ? "var(--danger)" : "";
  }

  function updateLastSyncDisplay() {
    if (!lastSync) { $last.textContent = "—"; return; }
    const s = Math.max(0, Math.floor((Date.now() - lastSync) / 1000));
    if (s < 5) $last.textContent = "just now";
    else if (s < 60) $last.textContent = `${s}s ago`;
    else if (s < 3600) $last.textContent = `${Math.floor(s / 60)}m ago`;
    else $last.textContent = `${Math.floor(s / 3600)}h ago`;
  }
  lastSyncTimer = setInterval(updateLastSyncDisplay, 1000);

  function opLine(frame) {
    const innerOp = frame && frame.op;
    if (!innerOp || typeof innerOp !== "object") return null;
    const kind = String(innerOp.op || "").toLowerCase();
    const pathArr = Array.isArray(innerOp.path) ? innerOp.path : [];
    const pathStr = pathArr.join("/");
    if (kind === "rename") {
      const from = Array.isArray(innerOp.from) ? innerOp.from.join("/") : "?";
      const to = Array.isArray(innerOp.to) ? innerOp.to.join("/") : "?";
      return { kind, cls: "lv-fs", chip: "", msg: `rename ${from} → ${to}` };
    }
    if (kind === "delete") return { kind, cls: "lv-fs", chip: "", msg: `delete ${pathStr}` };
    if (kind === "set")    return { kind, cls: "lv-fs", chip: "", msg: `set ${pathStr}` };
    if (kind === "update") return { kind, cls: "lv-fs", chip: "", msg: `update ${pathStr}` };
    return { kind: kind || "op", cls: "lv-info", chip: "", msg: pathStr || JSON.stringify(innerOp) };
  }

  function eventLine(frame) {
    const t = String(frame.type || "").toLowerCase();
    const cls = t.includes("error") || t.includes("conflict") ? "lv-err"
      : t.includes("sync") ? "lv-ok"
      : t.includes("plugin") ? "lv-studio"
      : "lv-info";
    // Short one-liner for known events; fall back to stringified frame.
    let msg = "";
    if (t === "initial-choice-needed") msg = "initial sync needs a choice";
    else if (t === "initial-choice-made") msg = `initial sync: ${frame.choice || "?"}`;
    else if (t === "config-changed") msg = "project config reloaded";
    else if (t === "conflict") msg = `conflict @ ${frame.path || "?"}`;
    else msg = frame.message || frame.msg || JSON.stringify(frame);
    return { kind: t, cls, chip: "", msg };
  }

  function addLine(frame) {
    const line = document.createElement("span");
    line.className = "line";
    const time = new Date(Date.now()).toLocaleTimeString();
    const rendered = frame && frame.type === "op"
      ? opLine(frame)
      : eventLine(frame || {});
    if (!rendered) return;
    line.innerHTML =
      `<span class="t">${time}</span> ` +
      `<span class="${rendered.cls}">[${escape(rendered.kind)}]</span>${rendered.chip} ` +
      `<span>${escape(rendered.msg)}</span>`;
    $log.appendChild(line);
    while ($log.childElementCount > MAX_LINES) $log.removeChild($log.firstChild);
    $log.scrollTop = $log.scrollHeight;
  }

  function bumpOp() {
    opCount++;
    $ops.textContent = String(opCount);
    lastSync = Date.now();
    updateLastSyncDisplay();
  }

  function openStream() {
    const base = api.getDaemonBase();
    if (!base) {
      setPluginStatus("daemon offline", "err");
      $hint.textContent = "waiting for daemon…";
      return;
    }
    if (ws) { try { ws.close(); } catch {} ws = null; }
    try {
      ws = daemonWS(base, "/ws", {
        open: () => { $hint.textContent = "streaming /ws"; },
        error: () => { $hint.textContent = "stream error — retrying"; },
        close: () => { $hint.textContent = "stream closed — reconnecting"; },
        message: (data) => {
          if (!data || typeof data !== "object") return;
          const t = data.type;
          // Transport-only frames — not log-worthy.
          if (t === "ping" || t === "pong" || t === "hello" || t === "lagged"
              || t === "push-result" || t === "error") return;

          if (t === "op") {
            addLine(data);
            bumpOp();
            noteOp();
            return;
          }
          // state.events passthrough: render the event with its original
          // top-level type (no "event" wrapper).
          if (t === "plugin") {
            setPluginStatus(data.connected ? "connected" : "disconnected",
              data.connected ? "ok" : "warn");
            return;
          }
          addLine(data);
        },
      });
    } catch (e) {
      $hint.textContent = `stream failed: ${e.message}`;
    }
  }

  async function refreshHeader() {
    const s = api.getState();
    const proj = (s.projects || []).find((p) => p.id === s.activeProjectId);
    $project.textContent = proj ? proj.name : "—";
    const base = api.getDaemonBase();
    if (!base || !proj) return;
    try {
      const info = await daemonJson(base, "/snapshot");
      if (info.lastSync) { lastSync = info.lastSync; updateLastSyncDisplay(); }
      if (typeof info.pluginConnected === "boolean") {
        setPluginStatus(info.pluginConnected ? "connected" : "disconnected",
          info.pluginConnected ? "ok" : "warn");
      }
    } catch {}
  }

  $clear.addEventListener("click", () => { $log.innerHTML = ""; opCount = 0; $ops.textContent = "0"; });
  $snap.addEventListener("click", async () => {
    const base = api.getDaemonBase();
    const s = api.getState();
    const proj = (s.projects || []).find((p) => p.id === s.activeProjectId);
    if (!base || !proj) { api.toast("No active project"); return; }
    try {
      await daemonJson(base, "/snapshot", {
        method: "POST",
        body: JSON.stringify({ force: true }),
      });
      api.toast("Re-snapshot queued");
    } catch (e) { api.toast(`snapshot failed: ${e.message}`); }
  });

  const offUp = api.onBus("daemon:up", () => { clearUnsynced(); openStream(); refreshHeader(); });
  const offDown = api.onBus("daemon:down", () => setPluginStatus("daemon offline", "err"));
  const offState = api.onBus("state", () => { refreshHeader(); openStream(); });

  openStream();
  refreshHeader();

  return () => {
    offUp(); offDown(); offState();
    clearInterval(lastSyncTimer);
    clearInterval(unsyncedTimer);
    if (ws) { try { ws.close(); } catch {} }
  };
}

function escape(s) {
  return String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
}
