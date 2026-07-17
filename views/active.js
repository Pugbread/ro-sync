// views/active.js — readable live activity timeline and connection summary.
import { daemonWS, daemonJson } from "../bridge.js";
import { copyText } from "./runtime.js";
import { isCountableActivity, pushActivity, renderActivityFeed } from "./activity-format.js";

const MAX_ACTIVITIES = 200;
const MAX_PENDING = 400;
const MAX_FLUSH = 80;
const MAX_PARSED_LEGACY_OPS_PER_SECOND = 20;
const RAW_OP_RE = /"type"\s*:\s*"op"/;

export function mountActive(root, api) {
  root.innerHTML = `
    <header class="activity-page-head">
      <div>
        <h1 class="page-title">Activity</h1>
        <p class="page-sub">Understand what Ro Sync is doing in Studio and why.</p>
      </div>
      <span id="act-hint" class="activity-live-state" role="status" aria-live="polite">Connecting…</span>
    </header>
    <div class="active-grid">
      <div class="stat"><div class="label">Actions</div><div class="value" id="s-ops">0</div></div>
      <div class="stat"><div class="label">Last sync</div><div class="value" id="s-last">—</div></div>
      <div class="stat"><div class="label">Plugin</div><div class="value" id="s-plugin">Unknown</div></div>
      <div class="stat"><div class="label">Project</div><div class="value stat-project" id="s-project">—</div></div>
    </div>
    <div class="activity-toolbar">
      <div class="activity-toolbar-actions">
        <button id="act-clear" type="button">Clear activity</button>
        <button id="act-live" type="button">Pause updates</button>
        <button id="act-snapshot" type="button">Refresh snapshot</button>
      </div>
      <span id="act-unsynced" class="badge badge-warn" hidden></span>
    </div>
    <div id="act-log" class="activity-feed activity-feed--full" aria-live="polite" aria-label="Live project activity"></div>
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

  let actionCount = 0;
  let lastSync = null;
  let liveEnabled = true;
  let ws = null;
  let streamBase = "";
  let streamEpoch = 0;
  let activityProjectId = api.getState().activeProjectId || null;
  let statsRaf = 0;
  let logRaf = 0;
  let droppedNoticeTimer = 0;
  let rawWindowStart = 0;
  let parsedLegacyOps = 0;
  let skippedLegacyOps = 0;
  let droppedFrames = 0;
  const activities = [];
  const pendingFrames = [];

  const UNSYNCED_WINDOW_MS = 10_000;
  const UNSYNCED_THRESHOLD = 10;
  const recentSyncs = [];

  function setPluginStatus(label, kind) {
    $plugin.textContent = label;
    $plugin.dataset.kind = kind || "";
  }

  function noteRecentSync() {
    const now = Date.now();
    recentSyncs.push(now);
    updateRecentSyncBadge();
  }

  function updateRecentSyncBadge() {
    const cutoff = Date.now() - UNSYNCED_WINDOW_MS;
    while (recentSyncs.length && recentSyncs[0] < cutoff) recentSyncs.shift();
    if (recentSyncs.length > UNSYNCED_THRESHOLD) {
      $unsynced.hidden = false;
      $unsynced.textContent = `${recentSyncs.length} recent sync changes`;
    } else {
      $unsynced.hidden = true;
    }
  }

  function clearRecentSyncs() {
    recentSyncs.length = 0;
    $unsynced.hidden = true;
  }

  function resetProjectActivity() {
    pendingFrames.length = 0;
    activities.length = 0;
    droppedFrames = 0;
    actionCount = 0;
    lastSync = null;
    rawWindowStart = 0;
    parsedLegacyOps = 0;
    skippedLegacyOps = 0;
    if (droppedNoticeTimer) clearTimeout(droppedNoticeTimer);
    droppedNoticeTimer = 0;
    clearRecentSyncs();
    $ops.textContent = "0";
    $last.textContent = "—";
    setPluginStatus("Unknown", "");
    renderActivityFeed($log, activities, {
      emptyMessage: "Studio and filesystem actions will appear here.",
    });
  }

  function syncActivityProject() {
    const nextProjectId = api.getState().activeProjectId || null;
    if (nextProjectId === activityProjectId) return false;
    activityProjectId = nextProjectId;
    resetProjectActivity();
    return true;
  }

  function updateLastSyncDisplay() {
    const timestamp = normalizeTimestamp(lastSync);
    if (!timestamp) {
      $last.textContent = "—";
      return;
    }
    const seconds = Math.max(0, Math.floor((Date.now() - timestamp) / 1000));
    if (seconds < 5) $last.textContent = "Just now";
    else if (seconds < 60) $last.textContent = `${seconds}s ago`;
    else if (seconds < 3600) $last.textContent = `${Math.floor(seconds / 60)}m ago`;
    else $last.textContent = `${Math.floor(seconds / 3600)}h ago`;
  }

  function scheduleStatsUpdate() {
    if (statsRaf) return;
    statsRaf = requestAnimationFrame(() => {
      statsRaf = 0;
      $ops.textContent = String(actionCount);
      updateLastSyncDisplay();
    });
  }

  function recordFrame(frame, at = Date.now()) {
    pendingFrames.push({ frame, at });
    if (pendingFrames.length > MAX_PENDING) {
      const count = pendingFrames.length - Math.floor(MAX_PENDING / 2);
      pendingFrames.splice(0, count);
      droppedFrames += count;
    }
    if (!logRaf) logRaf = requestAnimationFrame(flushFrames);
  }

  function flushFrames() {
    logRaf = 0;
    if (!$log.isConnected) {
      pendingFrames.length = 0;
      droppedFrames = 0;
      return;
    }
    const stickToBottom = isNearBottom($log);
    if (droppedFrames) {
      pushActivity(activities, { type: "busy", count: droppedFrames }, Date.now(), MAX_ACTIVITIES);
      droppedFrames = 0;
    }
    for (const entry of pendingFrames.splice(0, MAX_FLUSH)) {
      pushActivity(activities, entry.frame, entry.at, MAX_ACTIVITIES);
    }
    renderActivityFeed($log, activities, {
      emptyMessage: liveEnabled
        ? "Studio and filesystem actions will appear here."
        : "Updates are paused. Resume to see new actions.",
    });
    if (stickToBottom) $log.scrollTop = $log.scrollHeight;
    if (pendingFrames.length && !logRaf) logRaf = requestAnimationFrame(flushFrames);
  }

  function processFrame(data) {
    if (!data || typeof data !== "object") return;
    const type = data.type;
    if (["ping", "pong", "lagged", "push-result", "error"].includes(type)) return;

    if (type === "plugin") {
      setPluginStatus(data.connected ? "Connected" : "Disconnected", data.connected ? "ok" : "warn");
      recordFrame(data);
      return;
    }
    if (isCountableActivity(data)) {
      actionCount++;
      if (type === "sync-activity") {
        lastSync = Date.now();
        noteRecentSync();
      }
      scheduleStatsUpdate();
    }
    recordFrame(data);
  }

  function shouldSkipRawFrame(raw) {
    if (typeof raw !== "string" || !RAW_OP_RE.test(raw)) return false;
    const now = Date.now();
    if (now - rawWindowStart >= 1000) {
      rawWindowStart = now;
      parsedLegacyOps = 0;
      flushSkippedLegacyNotice();
    }
    if (parsedLegacyOps < MAX_PARSED_LEGACY_OPS_PER_SECOND) {
      parsedLegacyOps++;
      return false;
    }
    skippedLegacyOps++;
    actionCount++;
    lastSync = now;
    scheduleStatsUpdate();
    scheduleSkippedLegacyNotice();
    return true;
  }

  function scheduleSkippedLegacyNotice() {
    if (droppedNoticeTimer) return;
    droppedNoticeTimer = setTimeout(flushSkippedLegacyNotice, 1000);
  }

  function flushSkippedLegacyNotice() {
    if (droppedNoticeTimer) clearTimeout(droppedNoticeTimer);
    droppedNoticeTimer = 0;
    if (!skippedLegacyOps) return;
    recordFrame({ type: "busy", count: skippedLegacyOps });
    skippedLegacyOps = 0;
  }

  function openStream() {
    if (syncActivityProject()) closeStream();
    const base = api.getDaemonBase();
    if (!liveEnabled) {
      $hint.textContent = "Updates paused";
      $hint.dataset.state = "paused";
      return;
    }
    if (!base) {
      closeStream();
      setPluginStatus("Daemon offline", "err");
      $hint.textContent = "Waiting for daemon";
      $hint.dataset.state = "waiting";
      return;
    }
    if (ws && streamBase === base) return;
    closeStream();
    streamBase = base;
    const epoch = streamEpoch;
    try {
      ws = daemonWS(base, "/ws", {
        skipRaw: shouldSkipRawFrame,
        open: () => {
          if (epoch !== streamEpoch) return;
          $hint.textContent = "Live updates";
          $hint.dataset.state = "live";
        },
        error: () => {
          if (epoch !== streamEpoch) return;
          $hint.textContent = "Reconnecting";
          $hint.dataset.state = "waiting";
        },
        close: () => {
          if (epoch !== streamEpoch) return;
          $hint.textContent = "Reconnecting";
          $hint.dataset.state = "waiting";
        },
        message: (data) => {
          if (epoch === streamEpoch) processFrame(data);
        },
      });
    } catch {
      recordFrame({ type: "stream-error", attempts: 1 });
      $hint.textContent = "Reconnecting";
      $hint.dataset.state = "waiting";
    }
  }

  function closeStream() {
    streamEpoch++;
    if (ws) {
      try { ws.close(); } catch {}
      ws = null;
    }
    streamBase = "";
  }

  async function refreshHeader() {
    const state = api.getState();
    const projectId = state.activeProjectId || null;
    const project = (state.projects || []).find((item) => item.id === projectId);
    $project.textContent = project ? project.name : "—";
    const base = api.getDaemonBase();
    if (!base || !project) return;
    try {
      const info = await daemonJson(base, "/snapshot");
      if (api.getState().activeProjectId !== projectId || api.getDaemonBase() !== base) return;
      if (info.lastSync) {
        lastSync = info.lastSync;
        updateLastSyncDisplay();
      }
      if (typeof info.pluginConnected === "boolean") {
        setPluginStatus(info.pluginConnected ? "Connected" : "Disconnected", info.pluginConnected ? "ok" : "warn");
      }
    } catch {}
  }

  $clear.addEventListener("click", () => {
    pendingFrames.length = 0;
    activities.length = 0;
    droppedFrames = 0;
    actionCount = 0;
    $ops.textContent = "0";
    renderActivityFeed($log, activities, { emptyMessage: "New Studio and filesystem actions will appear here." });
  });

  $log.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-copy-path]");
    if (!button) return;
    const path = button.dataset.copyPath || "";
    if (!path) return;
    try {
      await copyText(api, path);
      api.toast?.("Path copied");
    } catch {
      api.toast?.("Could not copy path");
    }
  });

  $live.addEventListener("click", () => {
    liveEnabled = !liveEnabled;
    $live.textContent = liveEnabled ? "Pause updates" : "Resume updates";
    if (liveEnabled) openStream();
    else {
      closeStream();
      $hint.textContent = "Updates paused";
      $hint.dataset.state = "paused";
    }
  });

  $snap.addEventListener("click", async () => {
    const base = api.getDaemonBase();
    const state = api.getState();
    const project = (state.projects || []).find((item) => item.id === state.activeProjectId);
    if (!base || !project) {
      api.toast("No active project");
      return;
    }
    try {
      await daemonJson(base, "/snapshot");
      await refreshHeader();
      api.toast("Snapshot refreshed");
    } catch (error) {
      api.toast(`Snapshot failed: ${error.message}`);
    }
  });

  const offUp = api.onBus("daemon:up", () => {
    clearRecentSyncs();
    openStream();
    refreshHeader();
  });
  const offDown = api.onBus("daemon:down", () => {
    closeStream();
    setPluginStatus("Daemon offline", "err");
    $hint.textContent = liveEnabled ? "Waiting for daemon" : "Updates paused";
    $hint.dataset.state = liveEnabled ? "waiting" : "paused";
  });
  const offState = api.onBus("state", () => {
    refreshHeader();
    openStream();
  });

  const relativeTimer = setInterval(updateLastSyncDisplay, 1000);
  const recentSyncTimer = setInterval(updateRecentSyncBadge, 1000);
  renderActivityFeed($log, activities);
  openStream();
  refreshHeader();

  return () => {
    offUp(); offDown(); offState();
    clearInterval(relativeTimer);
    clearInterval(recentSyncTimer);
    if (statsRaf) cancelAnimationFrame(statsRaf);
    if (logRaf) cancelAnimationFrame(logRaf);
    if (droppedNoticeTimer) clearTimeout(droppedNoticeTimer);
    pendingFrames.length = 0;
    closeStream();
  };
}

function isNearBottom(element) {
  return element.scrollHeight - element.scrollTop - element.clientHeight < 32;
}

function normalizeTimestamp(value) {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : 0;
}
