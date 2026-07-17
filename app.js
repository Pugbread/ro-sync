// app.js — view router, state store, daemon supervisor.
import {
  t64,
  onT64,
  host,
  HOST_KIND,
  IS_DESKTOP_HOST,
  daemonJson,
  daemonWS,
  daemonURL,
  setDaemonAuthToken,
  emit,
  on,
} from "./bridge.js";
import { mountProjects } from "./views/projects.js";
import { mountActive } from "./views/active.js";
import { mountConflicts } from "./views/conflicts.js";
import { mountDocs } from "./views/docs.js";
import { mountSettings } from "./views/settings.js";
import { mountOverwriteModal } from "./views/overwrite.js";
import { canStopDesktopDaemon, isDesktopManagedStatus } from "./lifecycle-policy.js";
import {
  PLATFORM, IS_WINDOWS,
  BINARY_REL, WIDGET_DIR_SHELL,
  shQuote,
  pidAliveCmd, parsePidAlive,
  killPidCmd,
  tailLogCmd, portOwnerCmd,
  launchDaemonCmd, tmpLogPath,
  joinShell,
} from "./platform.js";

// ---------- State store ----------
// Persisted shape:
//   {
//     projects: [{ id, name, path, addedAt, gameId, groupId, placeIds }],
//     activeProjectId,
//     daemonPid, daemonPort, daemonProject, daemonBootId, daemonOwnerToken,
//     lastView,
//   }
const DEFAULT_STATE = {
  projects: [],
  activeProjectId: null,
  daemonPid: null,
  daemonPort: null,
  daemonProject: null,
  daemonBootId: null,
  daemonOwnerToken: null,
  lastView: "projects",
};

const app = {
  state: { ...DEFAULT_STATE },
  daemonBase: null,     // http://127.0.0.1:<port>
  daemonOk: false,
  currentView: null,
  unmountCurrent: null,
};

let stateSaveChain = Promise.resolve();

function saveState() {
  // setState can fire several times while a daemon is stopping, scanning
  // fallback ports, and relaunching. Terminal 64 state writes are async, so
  // unconstrained calls can complete out of order and resurrect an older
  // daemonProject/daemonPort snapshot. Queue immutable snapshots in call
  // order; a failed write is logged without breaking later saves.
  const value = { ...app.state };
  stateSaveChain = stateSaveChain.then(
    () => host.stateSet("state", value),
    () => host.stateSet("state", value),
  ).catch((e) => {
    console.warn("t64:set-state failed", e);
  });
  return stateSaveChain;
}

async function loadState() {
  try {
    const value = await host.stateGet("state");
    if (value && typeof value === "object") {
      app.state = { ...DEFAULT_STATE, ...value };
    }
  } catch {
    // No stored state yet — that's fine.
  }
  setDaemonAuthToken(app.state.daemonOwnerToken);
}

export function getState() { return app.state; }
export function getDaemonBase() { return app.daemonOk ? app.daemonBase : null; }
export function setState(patch) {
  app.state = { ...app.state, ...patch };
  setDaemonAuthToken(app.state.daemonOwnerToken);
  saveState();
  emit("state", app.state);
}

// ---------- Daemon supervision ----------
// The daemon is single-project: launched with --project <path> --port <p>.
// Switching projects requires kill + relaunch.

const DEFAULT_PORT = 7878;
const PORT_SCAN_MAX = 7890;   // inclusive — scan 7878..7890 before giving up
const DAEMON_HEARTBEAT_INTERVAL_MS = 5000;

let daemonHeartbeatTimer = null;
let widgetCloseSent = false;
let lastHeartbeatFailureNoticeAt = 0;

// ---------- Sessions registry (per-user; mirrors Argon's src/sessions.rs) ----------
// Persisted via t64:get-state/set-state under key "sessions". Shape:
//   [{ port, pid, project, startedAt }]
// On boot we `kill -0 <pid>` each entry; dead entries are dropped before
// ensureDaemon() runs so we never try to reuse a stale record.
async function loadSessions() {
  try {
    const v = await host.stateGet("sessions");
    return Array.isArray(v) ? v : [];
  } catch { return []; }
}

async function saveSessions(list) {
  try {
    await host.stateSet("sessions", list);
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
  const alive = [];
  for (const s of list) {
    if (await pidAlive(s && s.pid)) alive.push(s);
  }
  if (alive.length !== list.length) await saveSessions(alive);
  // If we still hold a daemonPid in widget state but its process is gone (the
  // user rebooted, killed it manually, widget reloaded after Studio crashed),
  // clear it so ensureDaemon doesn't try to reuse a dead pid on relaunch.
  const pid = app.state && app.state.daemonPid;
  if (pid && !alive.some((s) => s.pid === pid)) {
    setState({ daemonPid: null });
  }
  return alive;
}

async function upsertSession(entry) {
  const list = await loadSessions();
  const next = list.filter((s) => {
    if (!s) return false;
    if (s.port === entry.port) return false;
    if (entry.pid && s.pid === entry.pid) return false;
    return true;
  });
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

async function stopTrackedSession(session) {
  if (!session) return;
  try {
    if (session.pid && await pidAlive(session.pid)) {
      await t64("t64:exec", { command: killPidCmd(session.pid) });
    }
  } catch (e) {
    console.warn("stopTrackedSession failed", session, e);
  }
  await removeSession({ pid: session.pid, port: session.port });
}

async function stopDuplicateTrackedSessions(keepProject, keepPort) {
  const keepPortN = parseInt(keepPort, 10);
  const sessions = await pruneDeadSessions();
  for (const session of sessions) {
    const sessionPort = parseInt(session && session.port, 10);
    if (Number.isFinite(keepPortN) && sessionPort === keepPortN) continue;

    // The widget serves one project at a time. Kill only daemons this widget
    // launched/tracked; manually launched daemons are not in this registry.
    if (session && session.project) {
      await stopTrackedSession(session);
    }
  }

  const remaining = await loadSessions();
  const activeOnly = remaining.filter((session) => {
    const sessionPort = parseInt(session && session.port, 10);
    return session && session.project === keepProject && sessionPort === keepPortN;
  });
  if (activeOnly.length !== remaining.length) {
    await saveSessions(activeOnly);
  }
}

async function probePort(port) {
  try {
    const r = await fetch(daemonURL(`http://127.0.0.1:${port}`, "/hello"), {
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

async function getPortOwner(port) {
  try {
    const own = await t64("t64:exec", { command: portOwnerCmd(port) });
    return (own && own.stdout) ? own.stdout.trim() : "";
  } catch {
    return "";
  }
}

function parsePortOwnerPid(owner) {
  const match = String(owner || "").match(/\((\d+)\)\s*$/);
  if (!match) return null;
  const pid = parseInt(match[1], 10);
  return Number.isFinite(pid) && pid > 0 ? pid : null;
}

async function getPortOwnerPid(port) {
  return parsePortOwnerPid(await getPortOwner(port));
}

async function waitForPidExit(pid, timeoutMs = 4000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!(await pidAlive(pid))) return true;
    await sleep(100);
  }
  return !(await pidAlive(pid));
}

async function waitForPortRelease(port, timeoutMs = 4000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!(await getPortOwner(port))) return true;
    await sleep(100);
  }
  return !(await getPortOwner(port));
}

function makeOwnerToken() {
  const bytes = new Uint8Array(24);
  if (globalThis.crypto && typeof globalThis.crypto.getRandomValues === "function") {
    globalThis.crypto.getRandomValues(bytes);
  } else {
    for (let i = 0; i < bytes.length; i++) bytes[i] = Math.floor(Math.random() * 256);
  }
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

function ensureOwnerToken() {
  if (app.state.daemonOwnerToken) return app.state.daemonOwnerToken;
  const token = makeOwnerToken();
  setState({ daemonOwnerToken: token });
  return token;
}

async function daemonLifecycleRequest(
  base,
  path,
  reason,
  token = app.state.daemonOwnerToken,
) {
  if (!base || !token) return { sent: false, ok: false, error: "missing daemon owner token" };
  const url = daemonURL(base, path);
  const body = JSON.stringify({ token, reason });
  try {
    const res = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body,
      keepalive: true,
    });
    const json = await res.json().catch(() => null);
    return {
      sent: true,
      ok: !!(res.ok && json && json.ok !== false),
      status: res.status,
      error: json && json.error,
      data: json,
    };
  } catch (e) {
    return { sent: false, ok: false, error: e && e.message ? e.message : String(e) };
  }
}

function daemonLifecyclePost(path, reason, preferBeacon = false) {
  const base = app.daemonBase;
  const token = app.state.daemonOwnerToken;
  if (!base || !token) return false;
  const url = daemonURL(base, path);
  const body = JSON.stringify({ token, reason });
  if (preferBeacon && navigator.sendBeacon) {
    try {
      const blob = new Blob([body], { type: "application/json" });
      if (navigator.sendBeacon(url, blob)) return true;
    } catch {}
  }
  fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body,
    keepalive: true,
  }).catch(() => {});
  return true;
}

async function verifyDaemonOwnership(base) {
  const endpoint = IS_DESKTOP_HOST ? "/manager-heartbeat" : "/widget-heartbeat";
  const reason = IS_DESKTOP_HOST ? "desktop heartbeat" : "widget heartbeat";
  const result = await daemonLifecycleRequest(base, endpoint, reason);
  if (!result.ok) {
    console.warn("daemon heartbeat rejected", result);
  }
  return result.ok;
}

function stopDaemonHeartbeat() {
  if (daemonHeartbeatTimer) {
    clearInterval(daemonHeartbeatTimer);
    daemonHeartbeatTimer = null;
  }
}

function sendDaemonHeartbeat() {
  const base = app.daemonBase;
  if (!base || !app.state.daemonOwnerToken) return;
  const endpoint = IS_DESKTOP_HOST ? "/manager-heartbeat" : "/widget-heartbeat";
  const reason = IS_DESKTOP_HOST ? "desktop heartbeat" : "widget heartbeat";
  daemonLifecycleRequest(base, endpoint, reason).then((result) => {
    if (result.ok) return;
    const now = Date.now();
    if (now - lastHeartbeatFailureNoticeAt > 30_000) {
      lastHeartbeatFailureNoticeAt = now;
      console.warn("daemon heartbeat failed", result);
    }
  });
}

function startDaemonHeartbeat() {
  stopDaemonHeartbeat();
  if (!app.daemonBase || !app.state.daemonOwnerToken) return;
  sendDaemonHeartbeat();
  daemonHeartbeatTimer = setInterval(sendDaemonHeartbeat, DAEMON_HEARTBEAT_INTERVAL_MS);
}

function notifyWidgetClosing() {
  if (widgetCloseSent) return;
  widgetCloseSent = true;
  stopDaemonHeartbeat();
  const endpoint = IS_DESKTOP_HOST ? "/manager-close" : "/widget-close";
  const reason = IS_DESKTOP_HOST ? "desktop app closed" : "widget closed";
  daemonLifecyclePost(endpoint, reason, true);
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

  // Persist the widget capability before launch. The daemon reads this one
  // narrow key from Terminal 64's ignored state file, so the secret never
  // appears in the t64:exec command string or in the daemon's argv.
  ensureOwnerToken();
  await saveState();

  // Raw (unquoted) args — launchDaemonCmd applies platform-native quoting.
  const args = [
    "serve",
    "--project", projectPath,
    "--port",    String(port),
    "--widget-owned",
  ];
  if (proj && proj.gameId) {
    args.push("--game-id", String(proj.gameId));
  }
  if (proj && proj.groupId) {
    args.push("--group-id", String(proj.groupId));
  }
  if (proj && Array.isArray(proj.placeIds)) {
    for (const pid of proj.placeIds) {
      const v = String(pid).trim();
      if (!v) continue;
      args.push("--place-id", v);
    }
  }

  const logPath = tmpLogPath(`rosync-${port}.log`);
  const command = launchDaemonCmd({
    binaryPath,
    args,
    logPath,
    port,
    ownerTokenStatePath: joinShell(WIDGET_DIR_SHELL, "state.json"),
  });

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
      // On Windows launchDaemonCmd redirects stderr to `<logPath>.err`, so
      // daemon startup crashes never land in the main .log. Tail the .err
      // file too — it's where panics / dll-load failures actually surface.
      try {
        const errRes = await t64("t64:exec", { command: tailLogCmd(logPath + ".err") });
        const errTail = (errRes && errRes.stdout) ? errRes.stdout.trim() : "";
        if (errTail) logTail = logTail ? `${logTail}\n${errTail}` : errTail;
      } catch {}
      const portOwner = await getPortOwner(port);
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

async function scanFallbackPorts(project, preferred) {
  for (let p = preferred + 1; p <= PORT_SCAN_MAX; p++) {
    // Skip ports already occupied by a non-ours daemon.
    const occ = await probePort(p);
    if (occ && !isOwnDaemon(occ.info, project)) continue;
    if (occ && isOwnDaemon(occ.info, project)) {
      if (!isWidgetOwnedDaemon(occ.info) || !app.state.daemonOwnerToken) {
        toast(`Port ${preferred} busy — using existing daemon on :${p}`);
        return occ;
      }
      if (!(await verifyDaemonOwnership(`http://127.0.0.1:${occ.port}`))) {
        setState({ daemonOwnerToken: null });
        toast(`Port ${preferred} busy — using existing daemon on :${p}`);
        return occ;
      }
      toast(`Port ${preferred} busy — started daemon on :${p}`);
      return occ;
    }
    const hit = await launchAndWait(project, p);
    if (hit) {
      toast(`Port ${preferred} busy — started daemon on :${p}`);
      return hit;
    }
  }
  toast(`All ports ${preferred}–${PORT_SCAN_MAX} busy; stop another daemon first.`);
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

function isWidgetOwnedDaemon(info) {
  return !!(info && typeof info === "object" && info.widgetOwned === true);
}

async function launchAndWait(project, port) {
  const launchedPid = await launchDaemon(project, port);
  if (!launchedPid) return null;
  for (let i = 0; i < 20; i++) {
    await sleep(200);
    const hit = await probePort(port);
    if (hit) return hit;
  }
  // A process that launched but never passed the authenticated browser probe
  // must not leak into the fallback scan. Stop only the exact PID we just
  // created; manually managed daemons are never touched here.
  await stopTrackedSession({ pid: launchedPid, port, project });
  if (app.state.daemonPid === launchedPid) {
    setState({ daemonPid: null, daemonProject: null, daemonBootId: null });
  }
  return null;
}

let ensureDaemonPromise = null;
let ensureDaemonQueued = false;

async function ensureDaemon() {
  if (ensureDaemonPromise) {
    ensureDaemonQueued = true;
    return ensureDaemonPromise;
  }
  ensureDaemonPromise = (async () => {
    do {
      ensureDaemonQueued = false;
      await ensureDaemonInner();
    } while (ensureDaemonQueued);
  })().finally(() => {
    ensureDaemonPromise = null;
  });
  return ensureDaemonPromise;
}

function lifecycleValue(value) {
  return value?.status || value || {};
}

function desktopOwnershipError(message, status = null) {
  const error = new Error(message);
  error.code = "EXTERNAL_DAEMON";
  error.status = status;
  return error;
}

async function inspectOwnedDesktopDaemon(spec) {
  const project = spec?.project;
  const token = spec?.ownerToken;
  if (!project || !token) {
    throw desktopOwnershipError(
      "The daemon has no usable Desktop ownership capability; it was left running.",
    );
  }

  const status = lifecycleValue(await host.daemonStatus(project));
  if (!status.running) return { running: false, status };
  if (!isDesktopManagedStatus(status)) {
    const manager = status.managedBy ? ` by ${status.managedBy}` : " outside Ro Sync Desktop";
    throw desktopOwnershipError(`The daemon is managed${manager}; it was left running.`, status);
  }

  const port = Number(status.port || spec.port);
  if (!Number.isFinite(port) || port <= 0) {
    throw desktopOwnershipError("The managed daemon reported no verifiable port; it was left running.", status);
  }
  const base = status.base || status.baseUrl || spec.base || `http://127.0.0.1:${port}`;

  // CORS and the lifecycle endpoint both require the exact manager token. A
  // manager label or a persisted PID is never treated as proof of ownership.
  setDaemonAuthToken(token);
  let info;
  try {
    info = await daemonJson(base, "/hello");
  } catch {
    throw desktopOwnershipError(
      "Desktop could not authenticate the managed daemon; it was left running.",
      status,
    );
  }
  const validProjects = new Set([
    project,
    status.project,
    status.canonicalProject,
    spec.canonicalProject,
  ].filter(Boolean));
  const proof = await daemonLifecycleRequest(
    base,
    "/manager-heartbeat",
    "desktop ownership check",
    token,
  );
  if (!canStopDesktopDaemon({
    status,
    hello: info,
    ownershipAuthenticated: proof.ok,
    expectedProjects: validProjects,
  })) {
    throw desktopOwnershipError(
      `Desktop could not authenticate the exact daemon boot${proof.error ? `: ${proof.error}` : ""}; it was left running.`,
      status,
    );
  }
  return { running: true, status, info, base, port, token, bootId: info.bootId };
}

async function stopOwnedDesktopDaemon(spec, reason) {
  const owned = await inspectOwnedDesktopDaemon(spec);
  if (!owned.running) return { stopped: true, alreadyStopped: true, status: owned.status };

  const response = await daemonLifecycleRequest(
    owned.base,
    "/manager-close",
    reason || "desktop daemon stop requested",
    owned.token,
  );
  if (!response.ok) {
    throw new Error(response.error || "the authenticated daemon stop was rejected");
  }

  // The HTTP acknowledgement means the exact owned process accepted shutdown;
  // only clear persisted tracking once the lifecycle record confirms that boot
  // is gone. A replacement external/CLI daemon is reported but never stopped.
  const deadline = Date.now() + 6000;
  while (Date.now() < deadline) {
    await sleep(100);
    const status = lifecycleValue(await host.daemonStatus(spec.project));
    if (!status.running || status.bootId !== owned.bootId) {
      return {
        stopped: true,
        status,
        replacementRunning: !!status.running,
      };
    }
  }
  throw new Error(`owned daemon boot ${owned.bootId} did not stop before the timeout`);
}

function clearDesktopDaemonTracking({ preserveTarget = false } = {}) {
  setDaemonAuthToken(null);
  app.daemonBase = null;
  app.daemonOk = false;
  setState({
    daemonPid: null,
    daemonProject: preserveTarget ? app.state.daemonProject : null,
    daemonBootId: null,
    daemonOwnerToken: null,
  });
}

async function ensureDesktopDaemon(project) {
  const projectInfo = activeProject();
  let ownedCandidate = null;
  let preferredPort =
    app.state.daemonProject === project && app.state.daemonPort
      ? app.state.daemonPort
      : DEFAULT_PORT;
  try {
    const previousProject = app.state.daemonProject;
    if (previousProject && previousProject !== project) {
      try {
        await stopOwnedDesktopDaemon({
          project: previousProject,
          port: app.state.daemonPort,
          pid: app.state.daemonPid,
          bootId: app.state.daemonBootId,
          ownerToken: app.state.daemonOwnerToken,
        }, "desktop switched projects");
        clearDesktopDaemonTracking();
      } catch (error) {
        if (error?.code !== "EXTERNAL_DAEMON") throw error;
        // The tracked boot was replaced or adopted elsewhere. Forget only our
        // stale local claim, leave that listener untouched, and let lifecycle
        // discovery choose a different free port for the new project.
        toast(error.message);
        clearDesktopDaemonTracking();
        preferredPort = null;
      }
    }

    const requestedToken = ensureOwnerToken();
    let result = await host.daemonEnsure({
      project,
      preferredPort,
      gameId: projectInfo?.gameId || null,
      groupId: projectInfo?.groupId || null,
      placeIds: projectInfo?.placeIds || [],
      ownerToken: requestedToken,
    });
    result = lifecycleValue(result);
    if (result.ok === false || result.running === false) {
      throw new Error(result.error || "managed daemon did not start");
    }

    if (result.externallyManaged || result.managedBy !== "desktop") {
      const manager = result.managedBy ? ` (${result.managedBy})` : "";
      throw desktopOwnershipError(
        `A daemon for this project is already externally managed${manager}; Desktop left it running.`,
        result,
      );
    }

    const port = Number(result.port || app.state.daemonPort || DEFAULT_PORT);
    if (!Number.isFinite(port) || port <= 0) throw new Error("managed daemon returned an invalid port");
    const token = requestedToken;
    const base = result.base || `http://127.0.0.1:${port}`;
    ownedCandidate = await inspectOwnedDesktopDaemon({
      project,
      canonicalProject: result.canonicalProject,
      port,
      base,
      ownerToken: token,
    });
    if (!ownedCandidate.running) throw new Error("managed daemon stopped during startup");
    const info = ownedCandidate.info;

    app.daemonBase = ownedCandidate.base;
    app.daemonOk = true;
    setState({
      daemonPid: ownedCandidate.status.pid || null,
      daemonPort: ownedCandidate.port,
      daemonProject: project,
      daemonBootId: ownedCandidate.bootId,
      daemonOwnerToken: token,
    });
    setDaemonDot("ok", `:${ownedCandidate.port}`);
    emit("daemon:up", { base: ownedCandidate.base, info, project, host: HOST_KIND });
  } catch (error) {
    // Cleanup is allowed only after an authenticated ownership proof. A CLI,
    // manual, or other Desktop manager remains untouched on every failure.
    if (ownedCandidate?.running) {
      try {
        const stopped = await stopOwnedDesktopDaemon(
          {
            project,
            port: ownedCandidate.port,
            bootId: ownedCandidate.bootId,
            ownerToken: ownedCandidate.token,
          },
          "desktop startup did not complete",
        );
        if (stopped.stopped) clearDesktopDaemonTracking();
      } catch (cleanupError) {
        console.warn("owned daemon cleanup failed", cleanupError);
      }
    }
    setDaemonAuthToken(null);
    app.daemonOk = false;
    app.daemonBase = null;
    setDaemonDot("err", "daemon down");
    setStatus(`managed daemon failed: ${error.message}`, "err");
    emit("daemon:down", { error: error.message, host: HOST_KIND });
  }
}

async function ensureDaemonInner() {
  const project = activeProjectPath();
  const preferred =
    app.state.daemonProject === project && app.state.daemonPort
      ? app.state.daemonPort
      : DEFAULT_PORT;

  if (!project) {
    app.daemonOk = false;
    app.daemonBase = null;
    setDaemonDot("idle", "no active project");
    emit("daemon:down", {});
    return;
  }

  if (IS_DESKTOP_HOST) {
    await ensureDesktopDaemon(project);
    return;
  }

  // 1. Probe preferred port.
  let hit = await probePort(preferred);

  // 2. If someone is on preferred port: is it ours?
  if (hit) {
    const ours = isOwnDaemon(hit.info, project);
    const pointedAtOurProject = hit.info && hit.info.project === project;
    if (ours && (!isWidgetOwnedDaemon(hit.info) || !app.state.daemonOwnerToken)) {
      // A manually started daemon, or a widget daemon whose ownership token
      // is no longer available, is external to this widget. Reuse it without
      // claiming lifecycle ownership; never kill a process by command pattern.
    } else if (ours && !(await verifyDaemonOwnership(`http://127.0.0.1:${hit.port}`))) {
      // Lost/invalid ownership is not permission to kill the listener.
      setState({ daemonOwnerToken: null });
    } else if (ours) {
      // Already have a daemon for our project — great, use it.
    } else if (pointedAtOurProject) {
      // Daemon IS serving our current project path, but gameId/groupId/placeIds don't
      // match. The daemon hot-reloads ro-sync.json, so keep the existing
      // process instead of risking a pattern-based kill of a manual daemon.
    } else if (app.state.daemonProject && app.state.daemonProject !== project) {
      // It's our own prior daemon but for a different project — stop and relaunch here.
      await killDaemon();
      hit = await launchAndWait(project, preferred);
    } else {
      // Occupied by someone we don't own — fall back to port scan.
      hit = await scanFallbackPorts(project, preferred);
    }
  } else {
    // 3. No one on preferred port — just launch.
    hit = await launchAndWait(project, preferred);
    // Browser fetch cannot distinguish a free port from a listener that is not
    // Ro Sync (non-HTTP service, CORS-blocked HTTP, etc.). If launch failed and
    // the OS reports a listener, scan the remaining widget port range.
    if (!hit && await getPortOwner(preferred)) {
      hit = await scanFallbackPorts(project, preferred);
    }
  }

  if (hit) {
    app.daemonBase = `http://127.0.0.1:${hit.port}`;
    app.daemonOk = true;
    setDaemonDot("ok", `:${hit.port}`);
    if (app.state.daemonPort !== hit.port) setState({ daemonPort: hit.port });
    if (app.state.daemonProject !== project) setState({ daemonProject: project });
    if (hit.info?.bootId && app.state.daemonBootId !== hit.info.bootId) {
      setState({ daemonBootId: hit.info.bootId });
    }

    // A matching /hello response only proves that this is a Ro Sync daemon;
    // it does not give the widget lifecycle ownership. Claim and persist its
    // PID only after the widget-owned token has been authenticated. Otherwise
    // a manually started daemon would enter our sessions registry and a later
    // project switch/widget close could kill that external process by PID.
    const ownsLifecycle =
      isWidgetOwnedDaemon(hit.info) &&
      !!app.state.daemonOwnerToken &&
      await verifyDaemonOwnership(app.daemonBase);
    if (ownsLifecycle) {
      const ownerPid = await getPortOwnerPid(hit.port);
      const daemonPid = ownerPid || app.state.daemonPid || null;
      if (daemonPid && app.state.daemonPid !== daemonPid) setState({ daemonPid });
      await upsertSession({
        port: hit.port,
        pid: daemonPid,
        project,
        startedAt: Date.now(),
      });
      await stopDuplicateTrackedSessions(project, hit.port);
    } else {
      // Forget any stale record for this listener. Keeping a token or PID here
      // would make killDaemon()/heartbeat treat an external process as ours.
      await removeSession({ port: hit.port });
      if (app.state.daemonPid || app.state.daemonOwnerToken) {
        setState({ daemonPid: null, daemonBootId: null, daemonOwnerToken: null });
      }
    }
    emit("daemon:up", { base: app.daemonBase, info: hit.info, project });
  } else {
    app.daemonOk = false;
    app.daemonBase = null;
    setDaemonDot("err", "daemon down");
    emit("daemon:down", {});
  }
}

async function killDaemon({ preserveTarget = false } = {}) {
  const pid = app.state.daemonPid;
  const port = app.state.daemonPort;
  if (IS_DESKTOP_HOST) {
    try {
      const result = await stopOwnedDesktopDaemon({
        project: app.state.daemonProject,
        port,
        pid,
        bootId: app.state.daemonBootId,
        ownerToken: app.state.daemonOwnerToken,
      }, "desktop daemon stopped");
      if (!result.stopped) throw new Error("managed daemon did not stop");
      clearDesktopDaemonTracking({ preserveTarget });
      setDaemonDot("idle", "daemon stopped");
      emit("daemon:down", { host: HOST_KIND });
      return true;
    } catch (error) {
      toast(`Could not stop managed daemon: ${error.message}`);
      return false;
    }
  }
  if (!pid) {
    // No tracked PID means the process may have been started manually. Only
    // the authenticated lifecycle endpoint is safe to use in this case.
    if (!app.daemonBase || !app.state.daemonOwnerToken) {
      toast("Daemon is externally managed; it was left running.");
      return false;
    }
    const result = await daemonLifecycleRequest(app.daemonBase, "/widget-close", "daemon stopped");
    if (!result.ok) {
      toast(`Could not stop daemon safely: ${result.error || "ownership rejected"}`);
      return false;
    }
    if (result.data && result.data.keptAlive) {
      toast("Daemon kept running because Studio is connected.");
      return false;
    }
    if (port && !(await waitForPortRelease(port))) {
      toast(`Daemon did not release port ${port}; restart was cancelled.`);
      return false;
    }
    await removeSession({ port });
    setState({
      daemonPid: null,
      daemonProject: preserveTarget ? app.state.daemonProject : null,
      daemonBootId: null,
      daemonOwnerToken: null,
    });
    app.daemonOk = false;
    app.daemonBase = null;
    setDaemonDot("idle", "daemon stopped");
    emit("daemon:down", {});
    return true;
  }
  try {
    await t64("t64:exec", { command: killPidCmd(pid) });
  } catch (e) {
    console.warn("kill failed", e);
  }
  if (!(await waitForPidExit(pid))) {
    toast(`Daemon PID ${pid} did not exit; restart was cancelled.`);
    return false;
  }
  await removeSession({ pid, port });
  setState({
    daemonPid: null,
    daemonProject: preserveTarget ? app.state.daemonProject : null,
    daemonBootId: null,
    daemonOwnerToken: null,
  });
  app.daemonOk = false;
  app.daemonBase = null;
  setDaemonDot("idle", "daemon stopped");
  emit("daemon:down", {});
  return true;
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

let healthTimer = null;
let healthInFlight = false;
const HEALTH_INTERVAL_MS = 30000;

function scheduleHealthTick() {
  if (healthTimer) clearTimeout(healthTimer);
  healthTimer = setTimeout(() => { void runHealthTick(); }, HEALTH_INTERVAL_MS);
}

async function runHealthTick() {
  if (document.hidden || healthInFlight) {
    scheduleHealthTick();
    return;
  }
  healthInFlight = true;
  try {
    await healthTick();
  } finally {
    healthInFlight = false;
    scheduleHealthTick();
  }
}

scheduleHealthTick();

// ---------- UI wiring ----------
const $view = document.getElementById("view");
const $tabs = document.querySelectorAll(".tab");
const $daemonDot = document.getElementById("daemon-dot");
const $statusLeft = document.getElementById("status-left");
const $statusRight = document.getElementById("status-right");
document.documentElement.dataset.host = HOST_KIND;
document.body.dataset.host = HOST_KIND;
document.documentElement.dataset.platform = PLATFORM;
document.body.dataset.platform = PLATFORM;

async function hydrateHostPresentation() {
  try {
    const info = await host.appInfo();
    const platform = info?.platform || PLATFORM;
    document.documentElement.dataset.platform = platform;
    document.body.dataset.platform = platform;
    const context = document.querySelector(".desktop-titlebar-context");
    if (context && info?.version) context.textContent = `Studio bridge · v${info.version}`;
  } catch {
    document.documentElement.dataset.platform = PLATFORM;
  }
}

const ROUTES = {
  projects: mountProjects,
  active: mountActive,
  conflicts: mountConflicts,
  docs: mountDocs,
  settings: mountSettings,
};

function setDaemonDot(kind, label) {
  $daemonDot.className = "dot dot-" + kind;
  $daemonDot.title = `Daemon: ${label}`;
  $statusRight.textContent = `daemon: ${label}`;
  document.getElementById("root").dataset.connection = kind;
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
    const selected = t.dataset.route === route;
    t.setAttribute("aria-selected", selected ? "true" : "false");
    t.tabIndex = selected ? 0 : -1;
    if (selected) $view.setAttribute("aria-labelledby", t.id);
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
    host,
  };
  app.unmountCurrent = ROUTES[route]($view, api) || null;
  setState({ lastView: route });
}

for (const [index, t] of [...$tabs].entries()) {
  t.addEventListener("click", () => navigate(t.dataset.route));
  t.addEventListener("keydown", (event) => {
    let next = null;
    if (event.key === "ArrowRight" || event.key === "ArrowDown") next = (index + 1) % $tabs.length;
    else if (event.key === "ArrowLeft" || event.key === "ArrowUp") next = (index - 1 + $tabs.length) % $tabs.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = $tabs.length - 1;
    if (next == null) return;
    event.preventDefault();
    $tabs[next].focus();
    navigate($tabs[next].dataset.route);
  });
}

window.addEventListener("pagehide", (event) => {
  if (event && event.persisted) return;
  notifyWidgetClosing();
});
window.addEventListener("beforeunload", notifyWidgetClosing);

// Re-render active view on daemon state changes (cheap).
on("daemon:up", () => emit("view:refresh", app.currentView));
on("daemon:down", () => emit("view:refresh", app.currentView));
on("daemon:up", startDaemonHeartbeat);
on("daemon:down", stopDaemonHeartbeat);

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
    el.setAttribute("role", "status");
    el.setAttribute("aria-live", "polite");
    el.setAttribute("aria-atomic", "true");
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
    app.state = {
      ...DEFAULT_STATE,
      ...payload.state,
      daemonOwnerToken: payload.state.daemonOwnerToken || app.state.daemonOwnerToken || null,
    };
    setDaemonAuthToken(app.state.daemonOwnerToken);
    emit("state", app.state);
  }
});

// ---------- App-level WS relay ----------
// Opens a single WebSocket per daemon so events (e.g. initial-choice-needed)
// can fan out to modal/overlay components regardless of the current view.
//
// The stream stays connected for control events, but raw op frames are handled
// before JSON.parse so large file bursts do not stall the Terminal 64 host.
const ENABLE_APP_REALTIME_STREAM = true;
// Server frames are serde-tagged with `type`:
//   {type:"op", op:{op:"set"|"delete"|"update"|"rename", path:[...], ...}}
//       → skipped here; the plugin consumes sync ops directly
//   {type:"<event-name>", ...}   ← daemon forwards state.events frames with
//       their ORIGINAL top-level type ("initial-choice-needed",
//       "initial-choice-made", "config-changed", "conflict")
//       → emit(type, frame)
//   {type:"ping"} / {type:"pong"} / {type:"lagged"} / {type:"push-result"} /
//   {type:"error"} → transport-only, ignored here
let appWS = null;
const RAW_OP_RE = /"type"\s*:\s*"op"/;

// Op frames are intentionally skipped on the app-level stream so large file
// bursts do not force the Terminal 64 host to JSON-parse every source payload.
function shouldSkipRawAppFrame(raw) {
  if (typeof raw !== "string" || !RAW_OP_RE.test(raw)) return false;
  return true;
}

function openAppStream() {
  if (!ENABLE_APP_REALTIME_STREAM) {
    return;
  }
  if (!app.daemonBase) return;
  if (appWS) { try { appWS.close(); } catch {} appWS = null; }
  try {
    appWS = daemonWS(app.daemonBase, "/ws", {
      skipRaw: shouldSkipRawAppFrame,
      message: (data) => {
        if (!data || typeof data !== "object") return;
        const t = data.type;
        if (!t) return;
        if (t === "op") {
          // Sync ops are applied by the plugin. The app-level stream only
          // exists for control events, so never turn op volume into a prompt.
          return;
        }
        // Transport-only frames — not surfaced to views.
        if (t === "ping" || t === "pong" || t === "lagged"
            || t === "push-result" || t === "error") {
          return;
        }
        // Everything else is a state.events passthrough carrying its
        // original top-level type. Fan out to the bus so app-level controls
        // can react.
        if (t === "initial-choice-needed" || t === "initial-choice-made"
            || t === "config-changed" || t === "conflict") {
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
  void hydrateHostPresentation();
  await loadState();
  // Reap dead-PID sessions before we try to reuse any recorded port.
  if (!IS_DESKTOP_HOST) await pruneDeadSessions();
  // Signal readiness so host can send t64:init.
  host.ready().catch(() => {});
  navigate(app.state.lastView || "projects");
  // Mount the blocking overwrite-choice modal at app-level.
  mountOverwriteModal({
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
