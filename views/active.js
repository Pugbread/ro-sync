// views/active.js — live WS log tail, op counter, plugin connection state.
import { daemonWS, daemonJson } from "../bridge.js";

const MAX_LINES = 200;
const MAX_PENDING_LINES = 400;
const MAX_FLUSH_LINES = 80;
const MAX_PARSED_OPS_PER_SECOND = 20;
const RAW_OP_RE = /"type"\s*:\s*"op"/;

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
      <button id="act-live">Stop live log</button>
      <button id="act-snapshot">Refresh snapshot</button>
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
  const $live = root.querySelector("#act-live");
  const $snap = root.querySelector("#act-snapshot");
  const $hint = root.querySelector("#act-hint");
  const $unsynced = root.querySelector("#act-unsynced");

  let opCount = 0;
  let lastSync = null;
  let lastSyncTimer = null;
  let statsRaf = 0;
  let unsyncedRaf = 0;
  let logRaf = 0;
  let droppedNoticeTimer = 0;
  let rawWindowStart = 0;
  let parsedOpsInWindow = 0;
  let skippedRawOps = 0;
  let droppedLogFrames = 0;
  let liveEnabled = true;
  let ws = null;
  const pendingLogFrames = [];

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
    scheduleUnsyncedBadge();
  }
  function scheduleUnsyncedBadge() {
    if (unsyncedRaf) return;
    unsyncedRaf = requestAnimationFrame(() => {
      unsyncedRaf = 0;
      updateUnsyncedBadge();
    });
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
    const meta = ["filesystem watcher"];
    const node = innerOp.node && typeof innerOp.node === "object" ? innerOp.node : null;
    if (node && node.class) meta.push(`class ${node.class}`);
    if (kind === "rename") {
      const from = Array.isArray(innerOp.from) ? innerOp.from.join("/") : "?";
      const to = Array.isArray(innerOp.to) ? innerOp.to.join("/") : "?";
      return { kind, cls: "lv-fs", title: "Renamed synced path", path: to, meta: [`from ${from}`] };
    }
    if (kind === "delete") return { kind, cls: "lv-fs", title: "Deleted synced path", path: pathStr, meta };
    if (kind === "set") return { kind, cls: "lv-fs", title: "Created or replaced synced path", path: pathStr, meta };
    if (kind === "update") return { kind, cls: "lv-fs", title: "Updated synced path", path: pathStr, meta };
    return { kind: kind || "op", cls: "lv-info", title: "Daemon operation", path: pathStr, meta: [JSON.stringify(innerOp)] };
  }

  function eventLine(frame) {
    const t = String(frame.type || "").toLowerCase();
    const cls = t.includes("error") || t.includes("conflict") ? "lv-err"
      : t.includes("sync") ? "lv-ok"
      : t.includes("plugin") ? "lv-studio"
      : "lv-info";
    // Short one-liner for known events; fall back to stringified frame.
    let title = "";
    let path = "";
    const meta = [];
    if (t === "initial-choice-needed") title = "Initial sync needs a source choice";
    else if (t === "initial-choice-made") {
      title = "Initial sync choice applied";
      meta.push(`choice ${frame.choice || "?"}`);
    } else if (t === "config-changed") title = "Project config reloaded";
    else if (t === "conflict") {
      title = "Sync conflict detected";
      path = frame.path || "";
    } else if (t === "busy") title = frame.message || frame.msg || "Daemon event burst collapsed";
    else title = frame.message || frame.msg || JSON.stringify(frame);
    return { kind: t, cls, title, path, meta };
  }

  function renderLine(frame) {
    const time = new Date(Date.now()).toLocaleTimeString();
    const rendered = frame && frame.type === "op"
      ? opLine(frame)
      : eventLine(frame || {});
    if (!rendered) return;
    return renderLogCard(rendered, time);
  }

  function droppedLine(count) {
    return renderLogCard({
      kind: "busy",
      cls: "lv-info",
      title: `Collapsed ${count} daemon events while the log was saturated`,
      path: "",
      meta: ["log throttle"],
    }, new Date(Date.now()).toLocaleTimeString());
  }

  function renderLogCard(rendered, time) {
    const card = document.createElement("article");
    card.className = `log-card ${rendered.cls}`;
    const meta = Array.isArray(rendered.meta) ? rendered.meta.filter(Boolean) : [];
    card.innerHTML = `
      <div class="log-card-head">
        <span class="log-kind">${escape(rendered.kind || "event")}</span>
        <span class="log-time">${escape(time)}</span>
      </div>
      <div class="log-title">${escape(rendered.title || "Daemon event")}</div>
      ${rendered.path ? `
        <div class="log-path-row">
          <div class="log-path">${escape(rendered.path)}</div>
          <button class="log-copy" data-copy-path="${escape(rendered.path)}">Copy path</button>
        </div>
      ` : ""}
      ${meta.length ? `<div class="log-meta">${meta.map((item) => `<span>${escape(item)}</span>`).join("")}</div>` : ""}
    `;
    return card;
  }

  function isNearBottom() {
    return $log.scrollHeight - $log.scrollTop - $log.clientHeight < 32;
  }

  function flushLogLines() {
    logRaf = 0;
    if (!$log.isConnected) {
      pendingLogFrames.length = 0;
      droppedLogFrames = 0;
      return;
    }

    const stickToBottom = isNearBottom();
    const fragment = document.createDocumentFragment();
    if (droppedLogFrames > 0) {
      fragment.appendChild(droppedLine(droppedLogFrames));
      droppedLogFrames = 0;
    }

    const batch = pendingLogFrames.splice(0, MAX_FLUSH_LINES);
    for (const frame of batch) {
      const line = renderLine(frame);
      if (line) fragment.appendChild(line);
    }

    if (fragment.childNodes.length > 0) {
      $log.appendChild(fragment);
      while ($log.childElementCount > MAX_LINES) {
        const first = $log.firstElementChild || $log.firstChild;
        if (!first) break;
        $log.removeChild(first);
      }
      if (stickToBottom) $log.scrollTop = $log.scrollHeight;
    }

    if (pendingLogFrames.length > 0 && !logRaf) {
      logRaf = requestAnimationFrame(flushLogLines);
    }
  }

  function addLine(frame) {
    if (document.hidden) {
      droppedLogFrames++;
      return;
    }
    pendingLogFrames.push(frame);
    if (pendingLogFrames.length > MAX_PENDING_LINES) {
      const drop = pendingLogFrames.length - Math.floor(MAX_PENDING_LINES / 2);
      pendingLogFrames.splice(0, drop);
      droppedLogFrames += drop;
    }
    if (!logRaf) logRaf = requestAnimationFrame(flushLogLines);
  }

  function flushSkippedRawNotice() {
    droppedNoticeTimer = 0;
    if (skippedRawOps <= 0) return;
    droppedLogFrames += skippedRawOps;
    skippedRawOps = 0;
    if (!logRaf) logRaf = requestAnimationFrame(flushLogLines);
  }

  function scheduleSkippedRawNotice() {
    if (droppedNoticeTimer) return;
    droppedNoticeTimer = setTimeout(flushSkippedRawNotice, 1000);
  }

  function shouldSkipRawFrame(raw) {
    if (typeof raw !== "string" || !RAW_OP_RE.test(raw)) return false;
    const now = Date.now();
    if (now - rawWindowStart >= 1000) {
      rawWindowStart = now;
      parsedOpsInWindow = 0;
      flushSkippedRawNotice();
    }
    if (parsedOpsInWindow < MAX_PARSED_OPS_PER_SECOND) {
      parsedOpsInWindow++;
      return false;
    }

    skippedRawOps++;
    opCount++;
    lastSync = now;
    scheduleStatsUpdate();
    scheduleUnsyncedBadge();
    scheduleSkippedRawNotice();
    return true;
  }

  function scheduleStatsUpdate() {
    if (statsRaf) return;
    statsRaf = requestAnimationFrame(() => {
      statsRaf = 0;
      $ops.textContent = String(opCount);
      updateLastSyncDisplay();
    });
  }

  function bumpOp() {
    opCount++;
    lastSync = Date.now();
    scheduleStatsUpdate();
  }

  function openStream() {
    if (!liveEnabled) {
      $hint.textContent = "live log paused";
      return;
    }
    const base = api.getDaemonBase();
    if (!base) {
      setPluginStatus("daemon offline", "err");
      $hint.textContent = "waiting for daemon…";
      return;
    }
    if (ws) return;
    try {
      ws = daemonWS(base, "/ws", {
        skipRaw: shouldSkipRawFrame,
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

  function closeStream() {
    if (ws) {
      try { ws.close(); } catch {}
      ws = null;
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

  $clear.addEventListener("click", () => {
    pendingLogFrames.length = 0;
    droppedLogFrames = 0;
    $log.innerHTML = "";
    opCount = 0;
    $ops.textContent = "0";
  });
  $log.addEventListener("click", async (event) => {
    const btn = event.target.closest("[data-copy-path]");
    if (!btn) return;
    const path = btn.dataset.copyPath || "";
    if (!path) return;
    try {
      await navigator.clipboard.writeText(path);
      api.toast && api.toast("Path copied");
    } catch {
      api.toast && api.toast("Could not copy path");
    }
  });
  $live.addEventListener("click", () => {
    liveEnabled = !liveEnabled;
    $live.textContent = liveEnabled ? "Stop live log" : "Start live log";
    if (liveEnabled) openStream();
    else {
      closeStream();
      $hint.textContent = "live log paused";
    }
  });
  $snap.addEventListener("click", async () => {
    const base = api.getDaemonBase();
    const s = api.getState();
    const proj = (s.projects || []).find((p) => p.id === s.activeProjectId);
    if (!base || !proj) { api.toast("No active project"); return; }
    try {
      await daemonJson(base, "/snapshot");
      await refreshHeader();
      api.toast("Snapshot refreshed");
    } catch (e) { api.toast(`snapshot failed: ${e.message}`); }
  });

  const offUp = api.onBus("daemon:up", () => { clearUnsynced(); openStream(); refreshHeader(); });
  const offDown = api.onBus("daemon:down", () => {
    closeStream();
    setPluginStatus("daemon offline", "err");
    $hint.textContent = liveEnabled ? "waiting for daemon…" : "live log paused";
  });
  const offState = api.onBus("state", () => { refreshHeader(); openStream(); });

  $hint.textContent = "waiting for daemon…";
  openStream();
  refreshHeader();

  return () => {
    offUp(); offDown(); offState();
    clearInterval(lastSyncTimer);
    clearInterval(unsyncedTimer);
    if (statsRaf) cancelAnimationFrame(statsRaf);
    if (unsyncedRaf) cancelAnimationFrame(unsyncedRaf);
    if (logRaf) cancelAnimationFrame(logRaf);
    if (droppedNoticeTimer) clearTimeout(droppedNoticeTimer);
    pendingLogFrames.length = 0;
    closeStream();
  };
}

function escape(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}
