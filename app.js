// app.js — view router, state store, daemon supervisor.
import { t64, onT64, daemonJson, daemonWS, emit, on } from "./bridge.js";
import { mountProjects } from "./views/projects.js";
import { mountActive } from "./views/active.js";
import { mountConflicts } from "./views/conflicts.js";
import { mountSettings } from "./views/settings.js";
import { mountOverwriteModal } from "./views/overwrite.js";
import { mountPreviewModal } from "./views/preview.js";
import {
  PLATFORM, IS_WINDOWS,
  BINARY_REL, WIDGET_DIR_SHELL,
  shQuote,
  pidAliveCmd, parsePidAlive,
  killPidCmd, killDaemonOnPortCmd,
  tailLogCmd, portOwnerCmd,
  launchDaemonCmd, tmpLogPath,
  joinShell,
} from "./platform.js";

// ---------- State store ----------
// Persisted shape:
//   {
//     projects: [{ id, name, path, addedAt, gameId, placeIds }],
//     activeProjectId,
//     daemonPid, daemonPort, daemonProject,
//     lastView,
//   }
const DEFAULT_STATE = {
  projects: [],
  activeProjectId: null,
  daemonPid: null,
  daemonPort: null,
  daemonProject: null,
  lastView: "projects",
};

const app = {
  state: { ...DEFAULT_STATE },
  daemonBase: null,     // http://127.0.0.1:<port>
  daemonOk: false,
  currentView: null,
  unmountCurrent: null,
};

function saveState() {
  return t64("t64:set-state", { key: "state", value: app.state }).catch((e) =>
    console.warn("t64:set-state failed", e)
  );
}

async function loadState() {
  try {
    const res = await t64("t64:get-state", { key: "state" });
    const value = res && typeof res === "object" ? res.value : null;
    if (value && typeof value === "object") {
      app.state = { ...DEFAULT_STATE, ...value };
    }
  } catch {
    // No stored state yet — that's fine.
  }
}

export function getState() { return app.state; }
export function getDaemonBase() { return app.daemonOk ? app.daemonBase : null; }
export function setState(patch) {
  app.state = { ...app.state, ...patch };
  saveState();
  emit("state", app.state);
}

// ---------- Daemon supervision ----------
// The daemon is single-project: launched with --project <path> --port <p>.
// Switching projects requires kill + relaunch.

const DEFAULT_PORT = 7878;
const PORT_SCAN_MAX = 7890;   // inclusive — scan 7878..7890 before giving up

// ---------- Sessions registry (per-user; mirrors Argon's src/sessions.rs) ----------
// Persisted via t64:get-state/set-state under key "sessions". Shape:
//   [{ port, pid, project, startedAt }]
// On boot we `kill -0 <pid>` each entry; dead entries are dropped before
// ensureDaemon() runs so we never try to reuse a stale record.
async function loadSessions() {
  try {
    const res = await t64("t64:get-state", { key: "sessions" });
    const v = res && typeof res === "object" ? res.value : null;
    return Array.isArray(v) ? v : [];
  } catch { return []; }
}

async function saveSessions(list) {
  try {
    await t64("t64:set-state", { key: "sessions", value: list });
  } catch (e) { console.warn("t64:set-state sessions failed", e); }
}

async function pidAlive(pid) {
  const n = parseInt(pid, 10);
  if (!Number.isFinite(n) || n <= 0) return false;
  try {
    const res = await t64("t64:exec", { command: pidAliveCmd(n) });
    return parsePidAlive(res && res.stdout);
  } catch { return false; }
}

async function pruneDeadSessions() {
  const list = await loadSessions();
  if (!list.length) return [];
  const alive = [];
  for (const s of list) {
    if (await pidAlive(s && s.pid)) alive.push(s);
  }
  if (alive.length !== list.length) await saveSessions(alive);
  return alive;
}

async function upsertSession(entry) {
  const list = await loadSessions();
  const next = list.filter((s) => s && s.port !== entry.port && s.pid !== entry.pid);
  next.push(entry);
  await saveSessions(next);
}

async function removeSession(match) {
  const list = await loadSessions();
  const next = list.filter((s) => {
    if (!s) return false;
    if (match.pid && s.pid === match.pid) return false;
    if (match.port && s.port === match.port) return false;
    return true;
  });
  if (next.length !== list.length) await saveSessions(next);
}

async function probePort(port) {
  try {
    const r = await fetch(`http://127.0.0.1:${port}/hello`, {
      method: "GET",
      signal: AbortSignal.timeout(500),
    });
    if (!r.ok) return null;
    const info = await r.json().catch(() => ({}));
    return { port, info };
  } catch {
    return null;
  }
}

function activeProject() {
  const s = app.state;
  return (s.projects || []).find((x) => x.id === s.activeProjectId) || null;
}

function activeProjectPath() {
  const p = activeProject();
  return p ? p.path : null;
}

async function launchDaemon(projectPath, port) {
  const proj = activeProject();
  // Shell-level path — host expands $HOME / %USERPROFILE% at command time.
  const binaryPath = joinShell(WIDGET_DIR_SHELL, BINARY_REL);

  // Raw (unquoted) args — launchDaemonCmd applies platform-native quoting.
  const args = [
    "--project", projectPath,
    "--port",    String(port),
  ];
  if (proj && proj.gameId) {
    args.push("--game-id", String(proj.gameId));
  }
  if (proj && Array.isArray(proj.placeIds)) {
    for (const pid of proj.placeIds) {
      const v = String(pid).trim();
      if (!v) continue;
      args.push("--place-id", v);
    }
  }

  const logPath = tmpLogPath(`rosync-${port}.log`);
  const command = launchDaemonCmd({ binaryPath, args, logPath, port });

  try {
    const res = await t64("t64:exec", { command });
    const stdout = (res && typeof res.stdout === "string" ? res.stdout : "").trim();
    // Parse the structured response: `---\n<pid>` on success, `---\nERROR: <msg>`
    // on failure. PS may prepend warning lines (ignored — we key off the sep).
    const lines = stdout.split(/\r?\n/);
    const sepIdx = lines.lastIndexOf("---");
    const payload = sepIdx >= 0 ? (lines[sepIdx + 1] || "").trim() : "";
    const pid = parseInt(payload, 10);
    if (Number.isFinite(pid) && pid > 0) {
      setState({ daemonPid: pid, daemonPort: port, daemonProject: projectPath });
      await upsertSession({ port, pid, project: projectPath, startedAt: Date.now() });
      return pid;
    }

    // If the PS try/catch caught it, `payload` starts with "ERROR:" — use it
    // directly. Otherwise fall through to log/port/stderr hints.
    let hint = "";
    if (payload.startsWith("ERROR:")) {
      hint = payload.slice(6).trim();
    } else {
      let logTail = "";
      try {
        const logRes = await t64("t64:exec", { command: tailLogCmd(logPath) });
        logTail = (logRes && logRes.stdout) ? logRes.stdout.trim() : "";
      } catch {}
      let portOwner = "";
      try {
        const own = await t64("t64:exec", { command: portOwnerCmd(port) });
        portOwner = (own && own.stdout) ? own.stdout.trim() : "";
      } catch {}
      hint =
        logTail ||
        (portOwner ? `port ${port} held by ${portOwner}` : "") ||
        cleanPsStderr(res && res.stderr) ||
        "no pid returned";
    }
    console.error("daemon launch failed", { stdout, payload, stderr: res?.stderr });
    setStatus(`daemon launch failed — ${hint.slice(0, 240)}`, "err");
  } catch (e) {
    console.error("launch daemon failed", e);
    setStatus(`daemon launch failed: ${e.message}`, "err");
  }
  return null;
}

// PowerShell serializes errors as CLIXML when piping to a non-PS consumer.
// The blob is unreadable to humans — strip it down to the inner <S> message
// text, or drop it entirely if we can't recover anything useful.
function cleanPsStderr(s) {
  if (!s || typeof s !== "string") return "";
  const trimmed = s.trim();
  if (!trimmed.startsWith("#< CLIXML")) return trimmed;
  // Try to pull the first <S …>message</S> payload out of the XML.
  const m = trimmed.match(/<S[^>]*>([^<]+)<\/S>/);
  if (m) return m[1].replace(/&#x[0-9A-Fa-f]+;/g, "").trim();
  return "PowerShell error (see devtools console for full CLIXML)";
}

// Probes a port and, if a daemon responds, decides whether it's OURS for the
// currently-active project. Matches on gameId when we have one, otherwise on
// daemonProject history — mirrors plugin-side port-probe behavior.
function isOwnDaemon(info, project) {
  if (!info || typeof info !== "object") return false;
  const proj = activeProject();
  // GameId match against the CURRENTLY active project — authoritative.
  if (proj && proj.gameId && info.gameId && String(info.gameId) === String(proj.gameId)) return true;
  // Project-path match against the currently active project.
  if (info.project && project && info.project === project) return true;
  // NOTE: the old third check (`daemonProject === info.project`) was removed:
  // it claimed ownership based on *prior* daemonProject state, so after a
  // project switch the stale daemon would be treated as ours, skipping the
  // kill-and-relaunch branch and causing the plugin to see "wrong game".
  return false;
}

async function launchAndWait(project, port) {
  await launchDaemon(project, port);
  for (let i = 0; i < 20; i++) {
    await sleep(200);
    const hit = await probePort(port);
    if (hit) return hit;
  }
  return null;
}

async function ensureDaemon() {
  const project = activeProjectPath();
  const preferred = app.state.daemonPort || DEFAULT_PORT;

  if (!project) {
    app.daemonOk = false;
    app.daemonBase = null;
    setDaemonDot("idle", "no active project");
    emit("daemon:down", {});
    return;
  }

  // 1. Probe preferred port.
  let hit = await probePort(preferred);

  // 2. If someone is on preferred port: is it ours?
  if (hit) {
    const ours = isOwnDaemon(hit.info, project);
    const pointedAtOurProject = hit.info && hit.info.project === project;
    if (ours) {
      // Already have a daemon for our project — great, use it.
    } else if (pointedAtOurProject) {
      // Daemon IS serving our current project path, but gameId/placeIds don't
      // match. It was launched with stale CLI args (before the user set the
      // gameId, or edited them while serving in an older widget build that
      // didn't auto-restart). Kill and relaunch with current settings.
      await killStaleDaemonAt(preferred);
      hit = await launchAndWait(project, preferred);
    } else if (app.state.daemonProject && app.state.daemonProject !== project) {
      // It's our own prior daemon but for a different project — stop and relaunch here.
      await killDaemon();
      hit = await launchAndWait(project, preferred);
    } else {
      // Occupied by someone we don't own — fall back to port scan.
      hit = null;
      let scannedPort = null;
      for (let p = preferred + 1; p <= PORT_SCAN_MAX; p++) {
        // Skip ports already occupied by a non-ours daemon.
        const occ = await probePort(p);
        if (occ && !isOwnDaemon(occ.info, project)) continue;
        if (occ && isOwnDaemon(occ.info, project)) { hit = occ; scannedPort = p; break; }
        hit = await launchAndWait(project, p);
        if (hit) { scannedPort = p; break; }
      }
      if (hit && scannedPort) {
        toast(`Port ${preferred} busy — started daemon on :${scannedPort}`);
      } else if (!hit) {
        toast(`All ports ${preferred}–${PORT_SCAN_MAX} busy; stop another daemon first.`);
      }
    }
  } else {
    // 3. No one on preferred port — just launch.
    hit = await launchAndWait(project, preferred);
  }

  if (hit) {
    app.daemonBase = `http://127.0.0.1:${hit.port}`;
    app.daemonOk = true;
    setDaemonDot("ok", `:${hit.port}`);
    if (app.state.daemonPort !== hit.port) setState({ daemonPort: hit.port });
    if (app.state.daemonProject !== project) setState({ daemonProject: project });
    emit("daemon:up", { base: app.daemonBase, info: hit.info, project });
  } else {
    app.daemonOk = false;
    app.daemonBase = null;
    setDaemonDot("err", "daemon down");
    emit("daemon:down", {});
  }
}

async function killDaemon() {
  const pid = app.state.daemonPid;
  const port = app.state.daemonPort;
  if (!pid) {
    // No tracked pid (e.g. widget was reloaded). Try to kill by port so we
    // don't leave a zombie daemon running.
    if (port) await killStaleDaemonAt(port);
    return;
  }
  try {
    await t64("t64:exec", { command: killPidCmd(pid) });
  } catch (e) {
    console.warn("kill failed", e);
  }
  await removeSession({ pid, port });
  setState({ daemonPid: null, daemonProject: null });
  app.daemonOk = false;
  app.daemonBase = null;
  setDaemonDot("idle", "daemon stopped");
  emit("daemon:down", {});
}

// Kill whatever rosync daemon is bound to `port`, even if we don't have its
// PID in state (common after a widget reload or after an older build launched
// it with different CLI args). Platform-aware — see killDaemonOnPortCmd.
async function killStaleDaemonAt(port) {
  const portN = parseInt(port, 10);
  if (!Number.isFinite(portN)) return;
  try {
    await t64("t64:exec", { command: killDaemonOnPortCmd(portN) });
  } catch (e) {
    console.warn("killStaleDaemonAt failed", e);
  }
  if (app.state.daemonPort === portN) {
    await removeSession({ port: portN });
    setState({ daemonPid: null, daemonProject: null });
  }
  app.daemonOk = false;
  app.daemonBase = null;
}

// ---------- Health loop ----------
async function healthTick() {
  if (!app.daemonBase) return;
  try {
    await daemonJson(app.daemonBase, "/hello");
    if (!app.daemonOk) {
      app.daemonOk = true;
      setDaemonDot("ok", `:${app.state.daemonPort}`);
      emit("daemon:up", { base: app.daemonBase });
    }
  } catch {
    if (app.daemonOk) {
      app.daemonOk = false;
      emit("daemon:down", {});
      const proj = activeProject();
      const cfg = (proj && proj.settings) || {};
      if (cfg.AutoReconnect === "off") {
        setDaemonDot("err", "daemon down");
      } else {
        setDaemonDot("warn", "reconnecting…");
        await ensureDaemon();
      }
    }
  }
}
setInterval(healthTick, 5000);

// ---------- UI wiring ----------
const $view = document.getElementById("view");
const $tabs = document.querySelectorAll(".tab");
const $daemonDot = document.getElementById("daemon-dot");
const $statusLeft = document.getElementById("status-left");
const $statusRight = document.getElementById("status-right");

const ROUTES = {
  projects: mountProjects,
  active: mountActive,
  conflicts: mountConflicts,
  settings: mountSettings,
};

function setDaemonDot(kind, label) {
  $daemonDot.className = "dot dot-" + kind;
  $daemonDot.title = `Daemon: ${label}`;
  $statusRight.textContent = `daemon: ${label}`;
}

function setStatus(msg, kind) {
  $statusLeft.textContent = msg || "—";
  $statusLeft.dataset.kind = kind || "";
}

function navigate(route) {
  if (!ROUTES[route]) route = "projects";
  if (app.currentView === route) return;
  if (typeof app.unmountCurrent === "function") {
    try { app.unmountCurrent(); } catch (e) { console.error(e); }
  }
  app.currentView = route;
  $view.innerHTML = "";
  for (const t of $tabs) {
    t.setAttribute("aria-selected", t.dataset.route === route ? "true" : "false");
  }
  document.getElementById("root").dataset.view = route;
  const api = {
    getState,
    setState,
    getDaemonBase,
    ensureDaemon,
    killDaemon,
    setStatus,
    toast,
    onBus: on,
    emitBus: emit,
    t64,
  };
  app.unmountCurrent = ROUTES[route]($view, api) || null;
  setState({ lastView: route });
}

for (const t of $tabs) {
  t.addEventListener("click", () => navigate(t.dataset.route));
}

// Re-render active view on daemon state changes (cheap).
on("daemon:up", () => emit("view:refresh", app.currentView));
on("daemon:down", () => emit("view:refresh", app.currentView));

// When the active project changes, (re)launch the daemon against it.
let lastActiveProject = null;
on("state", () => {
  const p = activeProjectPath();
  if (p !== lastActiveProject) {
    lastActiveProject = p;
    ensureDaemon();
  }
});

// Toast helper
let toastT;
function toast(msg) {
  let el = document.querySelector(".toast");
  if (!el) {
    el = document.createElement("div");
    el.className = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toastT);
  toastT = setTimeout(() => el.classList.remove("show"), 1800);
}

// Theme from t64:init -> CSS custom properties.
function applyTheme(theme) {
  if (!theme || typeof theme !== "object") return;
  const map = {
    bg: "--bg", fg: "--fg", foreground: "--fg", background: "--bg",
    accent: "--accent", border: "--border",
    surface: "--surface", muted: "--muted",
    danger: "--danger", warn: "--warn", ok: "--ok",
  };
  const root = document.documentElement;
  for (const [k, v] of Object.entries(theme)) {
    const css = map[k] || (k.startsWith("--") ? k : null);
    if (css && typeof v === "string") root.style.setProperty(css, v);
  }
}

onT64("t64:init", (payload) => {
  if (payload && payload.theme) applyTheme(payload.theme);
  if (payload && payload.state) {
    app.state = { ...DEFAULT_STATE, ...payload.state };
    emit("state", app.state);
  }
});

// ---------- App-level WS relay ----------
// Opens a single WebSocket per daemon so events (e.g. initial-choice-needed)
// can fan out to modal/overlay components regardless of the current view.
// Server frames are serde-tagged with `type`:
//   {type:"op", op:{op:"set"|"delete"|"update"|"rename", path:[...], ...}}
//       → bufferOp(innerOp) for the burst-preview heuristic
//   {type:"<event-name>", ...}   ← daemon forwards state.events frames with
//       their ORIGINAL top-level type ("initial-choice-needed",
//       "initial-choice-made", "config-changed", "conflict", "batch-preview")
//       → emit(type, frame)
//   {type:"ping"} / {type:"pong"} / {type:"lagged"} / {type:"push-result"} /
//   {type:"error"} → transport-only, ignored here
let appWS = null;

// Heuristic collector: any op burst where >5 ops arrive within 500ms triggers
// a synthetic "batch-preview" bus event, so the preview modal can gate large
// auto-applied changes even before the daemon emits batch-preview natively.
// Only {type:"op"} frames feed this — control/event frames are routed directly
// to the bus without passing through here.
const opBuffer = [];
let opFlushTimer = null;
// While an initial sync is in flight, hundreds-to-thousands of ops flood the
// stream legitimately. The >5-ops-in-500ms heuristic would pop the preview
// modal every burst, creating an infinite accept loop. Suppress until ops
// quiet down for ~3s after the user's initial-choice.
let suppressHeuristicUntil = 0;
let lastOpTs = 0;
function suppressHeuristic(ms) {
  suppressHeuristicUntil = Math.max(suppressHeuristicUntil, Date.now() + ms);
}
on("initial-choice-needed", () => suppressHeuristic(60_000));
on("initial-choice-made", () => suppressHeuristic(60_000));
function bufferOp(data) {
  lastOpTs = Date.now();
  // During bootstrap, extend suppression with a rolling 5s trailing window
  // so the last burst doesn't immediately pop a preview modal.
  if (Date.now() < suppressHeuristicUntil) {
    suppressHeuristicUntil = Math.max(suppressHeuristicUntil, Date.now() + 5_000);
    return;
  }
  opBuffer.push(data);
  if (!opFlushTimer) opFlushTimer = setTimeout(flushOpBuffer, 500);
}
function flushOpBuffer() {
  opFlushTimer = null;
  const ops = opBuffer.splice(0);
  if (ops.length <= 5) return;
  if (Date.now() < suppressHeuristicUntil) return;
  let added = 0, updated = 0, removed = 0;
  for (const op of ops) {
    // bufferOp stores the INNER plugin-shape op, where `op.op` is the kind
    // string ("set" / "delete" / "update" / "rename"). "set" covers both
    // additions and property updates; treat it as "added" for display.
    const k = String((op && op.op) || "").toLowerCase();
    if (k === "delete" || k === "remove") removed++;
    else if (k === "set" || k === "add" || k === "create") added++;
    else updated++;
  }
  emit("batch-preview", {
    source: "heuristic",
    summary: { added, updated, removed },
    ops,
  });
}

function openAppStream() {
  if (!app.daemonBase) return;
  if (appWS) { try { appWS.close(); } catch {} appWS = null; }
  try {
    appWS = daemonWS(app.daemonBase, "/ws", {
      message: (data) => {
        if (!data || typeof data !== "object") return;
        const t = data.type;
        if (!t) return;
        if (t === "op") {
          // Store the INNER plugin-shape op so downstream consumers look at
          // `op.op` (kind), `op.path` (array), etc. without re-unwrapping.
          if (data.op && typeof data.op === "object") bufferOp(data.op);
          return;
        }
        // Transport-only frames — not surfaced to views.
        if (t === "ping" || t === "pong" || t === "lagged"
            || t === "push-result" || t === "error" || t === "hello") {
          return;
        }
        // Everything else is a state.events passthrough carrying its
        // original top-level type. Fan out to the bus so modals / previews
        // can react.
        if (t === "initial-choice-needed" || t === "initial-choice-made"
            || t === "batch-preview" || t === "config-changed"
            || t === "conflict") {
          emit(t, data);
          return;
        }
        // Unknown event — no-op.
      },
      error: () => { /* daemonWS handles reconnect */ },
    });
  } catch (e) {
    console.warn("app WS failed", e);
  }
}
on("daemon:up", openAppStream);
on("daemon:down", () => {
  if (appWS) { try { appWS.close(); } catch {} appWS = null; }
});

// ---------- Boot ----------
function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

(async function boot() {
  await loadState();
  // Reap dead-PID sessions before we try to reuse any recorded port.
  await pruneDeadSessions();
  // Signal readiness so host can send t64:init.
  t64("t64:ready", { app: "ro-sync", version: 1 }).catch(() => {});
  navigate(app.state.lastView || "projects");
  // Mount the blocking overwrite-choice modal at app-level.
  mountOverwriteModal({
    onBus: on,
    getDaemonBase,
    getState,
    toast,
  });
  // Mount the batch-preview threshold modal at app-level.
  mountPreviewModal({
    onBus: on,
    getDaemonBase,
    getState,
    toast,
  });
  setDaemonDot("warn", "connecting…");
  await ensureDaemon();
})();

// Expose for debugging from devtools.
window.__rosync = { getState, setState, getDaemonBase, ensureDaemon, killDaemon };
