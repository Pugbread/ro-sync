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
  fetchJsonWithDeadline,
  fetchWithDeadline,
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
import { createChoiceReplayEpochGuard } from "./views/initial-selection.js";
import { createLastEditedStore, projectMemoryKey } from "./views/last-edited.js";
import { mergeProjectInitEvent } from "./project-init.js";
import {
  canStopDesktopDaemon,
  desktopStartOwnership,
  desktopStopPlan,
  exactLifecycleCloseIdentity,
  isDesktopManagedStatus,
} from "./lifecycle-policy.js";
import {
  PLATFORM, IS_WINDOWS,
  BINARY_REL, WIDGET_DIR_SHELL,
  daemonIdentityMatchesTrackedSession,
  projectPathsEqual,
  shQuote,
  pidAliveCmd, parsePidAlive,
  tailLogCmd, portOwnerCmd,
  launchDaemonCmd, tmpLogPath,
  joinShell,
} from "./platform.js";
import {
  applyAppearanceTheme,
  defaultAppearanceTheme,
  normalizeAppearanceTheme,
  resolveAppearanceTheme,
} from "./views/theme.js";

// Debug tracing for daemon lifecycle diagnosis. Shows in the webview console
// (right-click → Inspect Element → Console in dev builds).
function dbg(...args) {
  console.log("[rosync-dbg]", ...args);
}

const APPEARANCE_CONTEXT = Object.freeze({
  supportsHost: host.supports.hostTheme,
  isDesktop: IS_DESKTOP_HOST,
});
const DEFAULT_APPEARANCE_THEME = defaultAppearanceTheme(APPEARANCE_CONTEXT);

// Per-project last-edited memory. The app-level stream feeds it from
// sanitized sync-activity frames; the initial-divergence picker sorts by it.
const lastEditedStore = createLastEditedStore({
  stateGet: (key) => host.stateGet(key),
  stateSet: (key, value) => host.stateSet(key, value),
});
window.addEventListener("pagehide", () => { void lastEditedStore.flush(); });
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "hidden") void lastEditedStore.flush();
});

// ---------- State store ----------
// Persisted shape:
//   {
//     projects: [{ id, name, path, addedAt, gameId, groupId, placeIds }],
//     activeProjectId, servedProjectIds, daemonSessions, projectsRoot,
//     daemonPid, daemonPort, daemonProject, daemonCanonicalProject,
//     daemonBootId, daemonOwnerToken,
//     appearanceTheme, lastView,
//   }
const DEFAULT_STATE = {
  projects: [],
  projectsRoot: null,
  activeProjectId: null,
  servedProjectIds: [],
  daemonSessions: {},
  daemonPid: null,
  daemonPort: null,
  daemonProject: null,
  daemonCanonicalProject: null,
  daemonBootId: null,
  daemonOwnerToken: null,
  appearanceTheme: DEFAULT_APPEARANCE_THEME,
  lastView: "projects",
};

function validProjectIdSet(state) {
  return new Set((state.projects || []).map((project) => project?.id).filter(Boolean));
}

function normalizeDaemonSession(session, project = null) {
  if (!session || typeof session !== "object" || Array.isArray(session)) return null;
  const path = session.project || project?.path || null;
  if (!path) return null;
  return {
    project: path,
    canonicalProject: session.canonicalProject || null,
    port: session.port ?? null,
    pid: session.pid ?? null,
    base: session.base || (session.port ? `http://127.0.0.1:${session.port}` : null),
    bootId: session.bootId || null,
    ownerToken: session.ownerToken || null,
    ok: session.ok === true ? true : (session.ok === false ? false : null),
    error: session.error || null,
  };
}

function normalizePersistedState(value) {
  const incoming = value && typeof value === "object" && !Array.isArray(value) ? value : {};
  const next = { ...DEFAULT_STATE, ...incoming };
  next.projects = Array.isArray(incoming.projects) ? incoming.projects.filter(Boolean) : [];
  next.projectsRoot = typeof incoming.projectsRoot === "string" && incoming.projectsRoot.trim()
    ? incoming.projectsRoot.trim()
    : null;
  next.appearanceTheme = normalizeAppearanceTheme(
    incoming.appearanceTheme,
    APPEARANCE_CONTEXT,
  );

  const validIds = validProjectIdSet(next);
  const hasServedProjectIds = Array.isArray(incoming.servedProjectIds);
  const served = hasServedProjectIds
    ? incoming.servedProjectIds.filter((id) => validIds.has(id))
    : [];
  // Version-1 state represented the only served project as activeProjectId.
  // Seed the new collection once, while keeping activeProjectId as the
  // selected/detail focus for existing installs.
  if (!hasServedProjectIds && incoming.activeProjectId && validIds.has(incoming.activeProjectId)) {
    served.push(incoming.activeProjectId);
  }
  next.servedProjectIds = [...new Set(served)];

  const sessions = {};
  const incomingSessions = incoming.daemonSessions;
  if (incomingSessions && typeof incomingSessions === "object" && !Array.isArray(incomingSessions)) {
    for (const [projectId, session] of Object.entries(incomingSessions)) {
      if (!validIds.has(projectId)) continue;
      const project = next.projects.find((item) => item.id === projectId);
      const normalized = normalizeDaemonSession(session, project);
      if (normalized) sessions[projectId] = normalized;
    }
  }
  // Migrate the singleton daemon claim into the matching project session.
  const legacyProject = incoming.daemonProject
    ? next.projects.find((project) => projectPathsEqual(project.path, incoming.daemonProject))
    : next.projects.find((project) => project.id === incoming.activeProjectId);
  if (
    !hasServedProjectIds
    && !next.servedProjectIds.length
    && legacyProject
    && incoming.daemonBootId
    && incoming.daemonOwnerToken
  ) {
    next.servedProjectIds = [legacyProject.id];
  }
  if (legacyProject && !sessions[legacyProject.id] && (
    incoming.daemonProject || incoming.daemonPort || incoming.daemonBootId || incoming.daemonOwnerToken
  )) {
    sessions[legacyProject.id] = normalizeDaemonSession({
      project: incoming.daemonProject || legacyProject.path,
      canonicalProject: incoming.daemonCanonicalProject || null,
      port: incoming.daemonPort,
      pid: incoming.daemonPid,
      bootId: incoming.daemonBootId,
      ownerToken: incoming.daemonOwnerToken,
      ok: null,
    }, legacyProject);
  }
  next.daemonSessions = sessions;
  return next;
}

const app = {
  state: { ...DEFAULT_STATE },
  daemonBase: null,     // http://127.0.0.1:<port>
  daemonOk: false,
  currentView: null,
  unmountCurrent: null,
  projectBrokerStatus: null,
};

let currentHostTheme = null;
let appearanceStateLoaded = false;
let appearanceRevealTimer = null;
const systemThemeQuery = typeof globalThis.matchMedia === "function"
  ? globalThis.matchMedia("(prefers-color-scheme: dark)")
  : null;

function systemPrefersDark() {
  return systemThemeQuery ? systemThemeQuery.matches : true;
}

export function getAppearanceTheme() {
  return resolveAppearanceTheme(app.state.appearanceTheme, {
    ...APPEARANCE_CONTEXT,
    hostTheme: currentHostTheme,
    systemDark: systemPrefersDark(),
  });
}

function appearanceCanReveal() {
  if (!appearanceStateLoaded) return false;
  return app.state.appearanceTheme !== "host" || !!currentHostTheme;
}

function applyCurrentAppearanceTheme({ forceReveal = false } = {}) {
  return applyAppearanceTheme(document.documentElement, app.state.appearanceTheme, {
    ...APPEARANCE_CONTEXT,
    hostTheme: currentHostTheme,
    systemDark: systemPrefersDark(),
    reveal: forceReveal || appearanceCanReveal(),
  });
}

function scheduleAppearanceRevealFallback() {
  clearTimeout(appearanceRevealTimer);
  if (!document.documentElement.classList.contains("theme-pending")) return;
  appearanceRevealTimer = setTimeout(() => {
    appearanceRevealTimer = null;
    applyCurrentAppearanceTheme({ forceReveal: true });
  }, 800);
}

let stateSaveChain = Promise.resolve();

function saveState() {
  // setState can fire several times while a daemon is stopping, scanning
  // fallback ports, and relaunching. Terminal 64 state writes are async, so
  // unconstrained calls can complete out of order and resurrect an older
  // daemonProject/daemonPort snapshot. Queue immutable snapshots in call
  // order; a failed write is logged without breaking later saves.
  const value = { ...app.state };
  const write = stateSaveChain.then(
    () => host.stateSet("state", value),
    () => host.stateSet("state", value),
  );
  stateSaveChain = write.catch((e) => {
    console.warn("t64:set-state failed", e);
  });
  return write;
}

async function loadState() {
  try {
    const value = await host.stateGet("state");
    if (value && typeof value === "object") {
      app.state = normalizePersistedState(value);
    }
  } catch {
    // No stored state yet — that's fine.
  }
  setDaemonAuthToken(app.state.daemonOwnerToken);
  for (const session of Object.values(app.state.daemonSessions || {})) {
    if (session?.base && session?.ownerToken) {
      setDaemonAuthToken(session.ownerToken, session.base);
    }
  }
}

export function getState() { return app.state; }
export function getDaemonBase(projectId = null) {
  if (IS_DESKTOP_HOST) {
    const id = projectId || app.state.activeProjectId || app.state.servedProjectIds?.[0] || null;
    const session = id && app.state.daemonSessions?.[id];
    return session?.ok === true ? session.base || null : null;
  }
  return app.daemonOk ? app.daemonBase : null;
}
export function getDaemonSession(projectId = null) {
  const id = projectId || app.state.activeProjectId || app.state.servedProjectIds?.[0] || null;
  return id ? app.state.daemonSessions?.[id] || null : null;
}
export function isProjectServed(projectId) {
  return !!projectId && (app.state.servedProjectIds || []).includes(projectId);
}
export function setState(patch) {
  const nextPatch = patch && typeof patch === "object" ? { ...patch } : {};
  if (Object.hasOwn(nextPatch, "appearanceTheme")) {
    nextPatch.appearanceTheme = normalizeAppearanceTheme(
      nextPatch.appearanceTheme,
      APPEARANCE_CONTEXT,
    );
  }
  app.state = { ...app.state, ...nextPatch };
  if (Object.hasOwn(nextPatch, "appearanceTheme")) applyCurrentAppearanceTheme();
  setDaemonAuthToken(app.state.daemonOwnerToken);
  const persisted = saveState();
  emit("state", app.state);
  return persisted;
}

export function setAppearanceTheme(value) {
  const appearanceTheme = normalizeAppearanceTheme(value, APPEARANCE_CONTEXT);
  setState({ appearanceTheme });
  return getAppearanceTheme();
}

// ---------- Daemon supervision ----------
// The daemon is single-project: launched with --project <path> --port <p>.
// Switching projects requires kill + relaunch.

const DEFAULT_PORT = 7878;
const PORT_SCAN_MAX = 7890;   // inclusive — scan 7878..7890 before giving up
const DAEMON_HEARTBEAT_INTERVAL_MS = 5000;

let daemonHeartbeatTimer = null;
const desktopHeartbeatTimers = new Map();
const heartbeatInFlightByProject = new Set();
const heartbeatFailureNoticeByProject = new Map();
const daemonFailureByProject = new Map();
let widgetCloseSent = false;
const pendingDesktopOwnershipByProject = new Map();
let lastDaemonLaunchError = "";

function getDaemonFailure(projectId) {
  const sessionError = getDaemonSession(projectId)?.error;
  return sessionError || daemonFailureByProject.get(projectId) || null;
}

function reportDaemonFailure(projectId, error) {
  if (!projectId) return;
  if (error) daemonFailureByProject.set(projectId, String(error));
  else daemonFailureByProject.delete(projectId);
}

function runtimeShutdownDiagnostic(frame = {}) {
  const code = typeof frame.code === "string" && frame.code.trim()
    ? `${frame.code.trim()}: `
    : "";
  const reason = typeof frame.reason === "string" && frame.reason.trim()
    ? frame.reason.trim()
    : "Ro Sync stopped this project connection.";
  return `Error: ${code}${reason}`;
}

// ---------- Sessions registry (per-user; mirrors Argon's src/sessions.rs) ----------
// Persisted via t64:get-state/set-state under key "sessions". Shape:
//   [{ port, pid, project, canonicalProject, bootId, startedAt }]
// On boot we use PID liveness only to prune obvious dead entries. Every
// destructive cleanup independently re-authenticates the exact PID, port,
// project, and boot ID so PID reuse can never authorize a kill.
async function loadSessions() {
  try {
    const v = await host.stateGet("sessions");
    if (!Array.isArray(v)) return [];
    return v
      .filter((session) => session && typeof session === "object")
      .map((session) => {
        const port = parseInt(session.port, 10);
        const pid = parseInt(session.pid, 10);
        return {
          ...session,
          port: Number.isFinite(port) && port > 0 ? port : null,
          pid: Number.isFinite(pid) && pid > 0 ? pid : null,
        };
      });
  } catch { return []; }
}

async function saveSessions(list) {
  try {
    await host.stateSet("sessions", list);
  } catch (e) { console.warn("t64:set-state sessions failed", e); }
}

const PID_STATUS_ALIVE = "alive";
const PID_STATUS_DEAD = "dead";
const PID_STATUS_UNKNOWN = "unknown";

async function pidStatus(pid) {
  const n = parseInt(pid, 10);
  if (!Number.isFinite(n) || n <= 0) return PID_STATUS_DEAD;
  try {
    const res = await t64("t64:exec", { command: pidAliveCmd(n) });
    const stdout = String(res?.stdout || "");
    if (/\bdead\b/i.test(stdout)) return PID_STATUS_DEAD;
    if (parsePidAlive(stdout)) return PID_STATUS_ALIVE;
    console.warn("PID probe returned an ambiguous result; preserving tracked state", {
      pid: n,
      stdout,
    });
    return PID_STATUS_UNKNOWN;
  } catch (error) {
    console.warn("PID probe failed; preserving tracked state", { pid: n, error });
    return PID_STATUS_UNKNOWN;
  }
}

async function pruneDeadSessions() {
  const list = await loadSessions();
  const retained = [];
  const statusByPid = new Map();
  for (const s of list) {
    const status = await pidStatus(s?.pid);
    const pid = parseInt(s?.pid, 10);
    if (Number.isFinite(pid) && pid > 0) statusByPid.set(pid, status);
    if (status !== PID_STATUS_DEAD) retained.push(s);
  }
  if (retained.length !== list.length) await saveSessions(retained);
  // If we still hold a daemonPid in widget state but its process is gone (the
  // user rebooted, killed it manually, widget reloaded after Studio crashed),
  // clear it so ensureDaemon doesn't try to reuse a dead pid on relaunch. A
  // missing/failed host probe is deliberately not evidence that it is gone.
  const pid = parseInt(app.state?.daemonPid, 10);
  if (Number.isFinite(pid) && pid > 0 && !retained.some((s) => parseInt(s?.pid, 10) === pid)) {
    const status = statusByPid.get(pid) || await pidStatus(pid);
    if (status === PID_STATUS_DEAD) {
      // A manager capability belongs to the authenticated boot even though it
      // is not itself the boot identity. Rotate it after confirmed death so a
      // stale/leaked capability cannot authorize the replacement process.
      setState({
        daemonPid: null,
        daemonCanonicalProject: null,
        daemonBootId: null,
        daemonOwnerToken: null,
      });
    }
  } else if (app.state?.daemonPid && (!Number.isFinite(pid) || pid <= 0)) {
    setState({
      daemonPid: null,
      daemonCanonicalProject: null,
      daemonBootId: null,
      daemonOwnerToken: null,
    });
  }
  return retained;
}

async function upsertSession(entry) {
  const list = await loadSessions();
  const entryPort = parseInt(entry?.port, 10);
  const entryPid = parseInt(entry?.pid, 10);
  const next = list.filter((s) => {
    if (!s) return false;
    if (Number.isFinite(entryPort) && parseInt(s.port, 10) === entryPort) return false;
    if (Number.isFinite(entryPid) && entryPid > 0 && parseInt(s.pid, 10) === entryPid) return false;
    return true;
  });
  next.push({
    ...entry,
    port: Number.isFinite(entryPort) && entryPort > 0 ? entryPort : null,
    pid: Number.isFinite(entryPid) && entryPid > 0 ? entryPid : null,
  });
  await saveSessions(next);
}

async function removeSession(match) {
  const list = await loadSessions();
  const matchPort = parseInt(match?.port, 10);
  const matchPid = parseInt(match?.pid, 10);
  const matchBootId = typeof match?.bootId === "string" && match.bootId
    ? match.bootId
    : null;
  const hasPort = Number.isFinite(matchPort) && matchPort > 0;
  const hasPid = Number.isFinite(matchPid) && matchPid > 0;
  const hasBootId = !!matchBootId;
  if (!hasPort && !hasPid && !hasBootId) return;
  const next = list.filter((s) => {
    if (!s) return false;
    // Every supplied field is part of one identity. Single-field cleanup
    // remains available for deliberately forgetting whichever process owns a
    // port, while exact shutdown cleanup cannot erase a replacement boot.
    const matches = (
      (!hasPid || parseInt(s.pid, 10) === matchPid)
      && (!hasPort || parseInt(s.port, 10) === matchPort)
      && (!hasBootId || s.bootId === matchBootId)
    );
    return !matches;
  });
  if (next.length !== list.length) await saveSessions(next);
}

async function inspectTrackedSessionOwnership(session) {
  if (!session?.port || !session?.pid || !session?.bootId) {
    return { owned: false, status: "incomplete" };
  }
  const hit = await probePort(session.port);
  if (!hit) return { owned: false, status: "unreachable" };
  if (!daemonIdentityMatchesTrackedSession(session, hit.info)) {
    return { owned: false, status: "identity-mismatch", hit };
  }
  if (!isWidgetOwnedDaemon(hit.info) || !app.state.daemonOwnerToken) {
    return { owned: false, status: "ownership-unavailable", hit };
  }
  if (!(await verifyDaemonOwnership(`http://127.0.0.1:${hit.port}`))) {
    return { owned: false, status: "ownership-rejected", hit };
  }
  return { owned: true, status: "owned", hit };
}

async function trackedSessionStillOwnsProcess(session) {
  return (await inspectTrackedSessionOwnership(session)).owned;
}

async function stopTrackedSession(session) {
  if (!session) return false;
  try {
    // Authenticate the full daemon identity first. This remains authoritative
    // when the host process probe is unavailable or PowerShell fails.
    const ownership = await inspectTrackedSessionOwnership(session);
    if (ownership.owned) {
      const base = `http://127.0.0.1:${session.port}`;
      const identity = exactLifecycleCloseIdentity(session);
      if (!identity) {
        console.warn("tracked daemon has no complete close identity", session);
        return false;
      }
      const result = await daemonLifecycleRequest(
        base,
        "/manager-close",
        "duplicate daemon cleanup",
        app.state.daemonOwnerToken,
        identity,
      );
      if (!result.ok || !(await waitForPortRelease(session.port, 4000, session.pid))) {
        console.warn("tracked daemon remained running after authenticated cleanup", session, result);
        return false;
      }
      await removeSession({ pid: session.pid, port: session.port, bootId: session.bootId });
      return true;
    }

    const status = await pidStatus(session.pid);
    if (status === PID_STATUS_DEAD) {
      await removeSession({ pid: session.pid, port: session.port, bootId: session.bootId });
      return true;
    }

    if (status === PID_STATUS_UNKNOWN) {
      console.warn("could not verify tracked PID; preserving session for a later retry", session);
      return false;
    }

    if (ownership.status === "identity-mismatch" || ownership.status === "incomplete") {
      console.warn("discarding stale tracked identity without stopping its live PID", {
        session,
        status: ownership.status,
      });
      await removeSession({ pid: session.pid, port: session.port, bootId: session.bootId });
      return false;
    }

    // A busy daemon can miss a short /hello timeout while its PID remains
    // healthy. Preserve the exact boot record for a later retry instead of
    // turning a transient transport or ownership check into a permanent
    // orphan.
    console.warn("tracked daemon ownership could not be re-authenticated; preserving session", {
      session,
      status: ownership.status,
    });
    return false;
  } catch (e) {
    console.warn("stopTrackedSession failed", session, e);
    return false;
  }
}

async function stopDuplicateTrackedSessions(keepProject, keepPort) {
  const keepPortN = parseInt(keepPort, 10);
  const sessions = await pruneDeadSessions();
  for (const session of sessions) {
    const sessionPort = parseInt(session && session.port, 10);
    if (
      Number.isFinite(keepPortN)
      && sessionPort === keepPortN
      && projectPathsEqual(session?.project, keepProject)
    ) continue;

    // The widget serves one project at a time. Stop only daemons this widget
    // launched/tracked; manually launched daemons are not in this registry.
    if (session && session.project) {
      await stopTrackedSession(session);
    }
  }
}

async function probePort(port) {
  try {
    const { response: r, json } = await fetchJsonWithDeadline(
      daemonURL(`http://127.0.0.1:${port}`, "/hello"),
      {
        method: "GET",
        timeoutMs: 500,
      },
    );
    if (!r.ok) return null;
    if (
      !json
      || typeof json !== "object"
      || Array.isArray(json)
      || typeof json.version !== "string"
      || typeof json.project !== "string"
      || typeof json.bootId !== "string"
      || Number(json.port) !== Number(port)
      || !Number.isInteger(Number(json.pluginProtocol))
    ) return null;
    const info = json;
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

async function waitForPortRelease(port, timeoutMs = 4000, expectedPid = null) {
  const deadline = Date.now() + timeoutMs;
  let consecutiveHelloMisses = 0;
  let checkedOwnerAfterMiss = false;
  while (Date.now() < deadline) {
    if (await probePort(port)) {
      consecutiveHelloMisses = 0;
    } else {
      consecutiveHelloMisses += 1;
      // Avoid launching a PowerShell/Get-NetTCPConnection process on every
      // 100 ms poll. Two failed daemon probes establish the shutdown edge;
      // inspect the OS listener once there, then once more at the deadline.
      if (consecutiveHelloMisses >= 2 && !checkedOwnerAfterMiss) {
        checkedOwnerAfterMiss = true;
        const owner = await getPortOwner(port);
        if (!owner && (
          !expectedPid || await pidStatus(expectedPid) === PID_STATUS_DEAD
        )) return true;
      }
    }
    await sleep(100);
  }
  const owner = await getPortOwner(port);
  if (owner) return false;
  return !expectedPid || await pidStatus(expectedPid) === PID_STATUS_DEAD;
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
  exactIdentity = null,
  timeoutMs = 4000,
) {
  if (!base || !token) return { sent: false, ok: false, error: "missing daemon owner token" };
  const destructive = path === "/manager-close" || path === "/widget-close";
  if (destructive && !exactIdentity) {
    return { sent: false, ok: false, error: "missing exact daemon close identity" };
  }
  const url = daemonURL(base, path, token);
  const body = JSON.stringify({ token, reason, ...(exactIdentity || {}) });
  try {
    const { response: res, json } = await fetchJsonWithDeadline(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body,
      keepalive: true,
      timeoutMs,
    });
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

function daemonLifecyclePost(
  path,
  reason,
  preferBeacon = false,
  override = null,
  exactIdentity = null,
) {
  const base = override?.base || app.daemonBase;
  const token = override?.token || app.state.daemonOwnerToken;
  if (!base || !token) return false;
  const destructive = path === "/manager-close" || path === "/widget-close";
  if (destructive && !exactIdentity) return false;
  const url = daemonURL(base, path, token);
  const body = JSON.stringify({ token, reason, ...(exactIdentity || {}) });
  if (preferBeacon && navigator.sendBeacon) {
    try {
      const blob = new Blob([body], { type: "application/json" });
      if (navigator.sendBeacon(url, blob)) return true;
    } catch {}
  }
  fetchWithDeadline(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body,
    keepalive: true,
    timeoutMs: 4000,
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

function stopDaemonHeartbeat(projectId = null) {
  if (IS_DESKTOP_HOST) {
    if (projectId) {
      const timer = desktopHeartbeatTimers.get(projectId);
      if (timer) clearInterval(timer);
      desktopHeartbeatTimers.delete(projectId);
      heartbeatFailureNoticeByProject.delete(`project:${projectId}`);
    } else {
      for (const timer of desktopHeartbeatTimers.values()) clearInterval(timer);
      desktopHeartbeatTimers.clear();
      for (const key of heartbeatFailureNoticeByProject.keys()) {
        if (key.startsWith("project:")) heartbeatFailureNoticeByProject.delete(key);
      }
    }
    return;
  }
  if (daemonHeartbeatTimer) {
    clearInterval(daemonHeartbeatTimer);
    daemonHeartbeatTimer = null;
  }
  heartbeatFailureNoticeByProject.delete("terminal64");
}

async function sendDaemonHeartbeat(projectId = null) {
  const session = IS_DESKTOP_HOST && projectId
    ? app.state.daemonSessions?.[projectId]
    : null;
  const base = session?.base || app.daemonBase;
  const ownerToken = session?.ownerToken || app.state.daemonOwnerToken;
  if (!base || !ownerToken) return;
  const requestKey = IS_DESKTOP_HOST ? `project:${projectId}` : "terminal64";
  if (heartbeatInFlightByProject.has(requestKey)) return;
  heartbeatInFlightByProject.add(requestKey);
  const endpoint = IS_DESKTOP_HOST ? "/manager-heartbeat" : "/widget-heartbeat";
  const reason = IS_DESKTOP_HOST ? "desktop heartbeat" : "widget heartbeat";
  try {
    const result = await daemonLifecycleRequest(base, endpoint, reason, ownerToken);
    if (result.ok) return;
    const now = Date.now();
    const lastNotice = heartbeatFailureNoticeByProject.get(requestKey) || 0;
    if (now - lastNotice > 30_000) {
      heartbeatFailureNoticeByProject.set(requestKey, now);
      console.warn("daemon heartbeat failed", result);
    }
  } finally {
    heartbeatInFlightByProject.delete(requestKey);
  }
}

function startDaemonHeartbeat(projectId = null) {
  if (IS_DESKTOP_HOST) {
    if (!projectId) return;
    stopDaemonHeartbeat(projectId);
    const session = app.state.daemonSessions?.[projectId];
    if (!session?.base || !session?.ownerToken) return;
    void sendDaemonHeartbeat(projectId);
    desktopHeartbeatTimers.set(
      projectId,
      setInterval(() => { void sendDaemonHeartbeat(projectId); }, DAEMON_HEARTBEAT_INTERVAL_MS),
    );
    return;
  }
  stopDaemonHeartbeat();
  if (!app.daemonBase || !app.state.daemonOwnerToken) return;
  void sendDaemonHeartbeat();
  daemonHeartbeatTimer = setInterval(() => { void sendDaemonHeartbeat(); }, DAEMON_HEARTBEAT_INTERVAL_MS);
}

function notifyWidgetClosing() {
  // Desktop daemon ownership is native process state. A renderer navigation,
  // reload, WebView suspension, or page teardown must never stop it; only
  // Tauri RunEvent::Exit closes exact native claims. Terminal 64 retains its
  // historical widget-close behavior.
  if (IS_DESKTOP_HOST) return;
  if (widgetCloseSent) return;
  widgetCloseSent = true;
  if (projectBrokerTimer) {
    clearTimeout(projectBrokerTimer);
    projectBrokerTimer = null;
  }
  stopDaemonHeartbeat();
  const identity = exactLifecycleCloseIdentity(app.state);
  if (!identity) return;
  daemonLifecyclePost("/widget-close", "widget closed", true, null, identity);
}

function activeProject() {
  const s = app.state;
  return (s.projects || []).find((x) => x.id === s.activeProjectId) || null;
}

function projectById(projectId) {
  return (app.state.projects || []).find((project) => project.id === projectId) || null;
}

function servedProjectIds() {
  const validIds = validProjectIdSet(app.state);
  return (app.state.servedProjectIds || []).filter((id) => validIds.has(id));
}

function projectIdForPath(path) {
  return (app.state.projects || []).find((project) => projectPathsEqual(project.path, path))?.id || null;
}

function sessionPolicyState(session) {
  return {
    daemonPid: session?.pid ?? null,
    daemonPort: session?.port ?? null,
    daemonProject: session?.project ?? null,
    daemonCanonicalProject: session?.canonicalProject ?? null,
    daemonBootId: session?.bootId ?? null,
    daemonOwnerToken: session?.ownerToken ?? null,
  };
}

function legacyDaemonPatch(sessions, preferredProjectId = app.state.activeProjectId) {
  const fallbackId = servedProjectIds().find((id) => sessions[id])
    || Object.keys(sessions)[0]
    || null;
  const session = sessions[preferredProjectId] || sessions[fallbackId] || null;
  return {
    daemonPid: session?.pid ?? null,
    daemonPort: session?.port ?? null,
    daemonProject: session?.project ?? null,
    daemonCanonicalProject: session?.canonicalProject ?? null,
    daemonBootId: session?.bootId ?? null,
    daemonOwnerToken: session?.ownerToken ?? null,
  };
}

function setDesktopDaemonSession(projectId, session) {
  if (!projectId) return Promise.resolve();
  const previous = app.state.daemonSessions?.[projectId] || null;
  const sessions = { ...(app.state.daemonSessions || {}) };
  if (session) sessions[projectId] = normalizeDaemonSession(session, projectById(projectId));
  else delete sessions[projectId];
  if (previous?.base && (
    !session
    || previous.base !== sessions[projectId]?.base
    || previous.ownerToken !== sessions[projectId]?.ownerToken
  )) {
    setDaemonAuthToken(null, previous.base);
  }
  const next = sessions[projectId];
  if (next?.base && next?.ownerToken) setDaemonAuthToken(next.ownerToken, next.base);
  return setState({ daemonSessions: sessions, ...legacyDaemonPatch(sessions, projectId) });
}

function patchDesktopDaemonSession(projectId, patch) {
  const current = app.state.daemonSessions?.[projectId]
    || { project: projectById(projectId)?.path || null };
  return setDesktopDaemonSession(projectId, { ...current, ...patch });
}

function activeProjectPath() {
  const p = activeProject();
  return p ? p.path : null;
}

async function launchDaemon(projectPath, port) {
  const proj = activeProject();
  lastDaemonLaunchError = "";
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
      lastDaemonLaunchError = "";
      setState({ daemonPid: pid, daemonPort: port, daemonProject: projectPath });
      return { pid, logPath };
    }

    // If the PS try/catch caught it, `payload` starts with "ERROR:" — use it
    // directly. Otherwise fall through to log/port/stderr hints.
    let hint = "";
    if (payload.startsWith("ERROR:")) {
      hint = payload.slice(6).trim();
    } else {
      const logTail = await readDaemonLaunchTail(logPath);
      const portOwner = await getPortOwner(port);
      hint =
        logTail ||
        (portOwner ? `port ${port} held by ${portOwner}` : "") ||
        cleanPsStderr(res && res.stderr) ||
        "no pid returned";
    }
    lastDaemonLaunchError = hint;
    console.error("daemon launch failed", { stdout, payload, stderr: res?.stderr });
    setStatus(`daemon launch failed — ${hint.slice(0, 240)}`, "err");
  } catch (e) {
    lastDaemonLaunchError = e?.message || String(e);
    console.error("launch daemon failed", e);
    setStatus(`daemon launch failed: ${e.message}`, "err");
  }
  return null;
}

async function scanFallbackPorts(project, preferred) {
  const trackedSessions = await loadSessions();
  let attemptedFreePortLaunch = false;
  let fallbackLaunchError = "";
  for (let p = preferred + 1; p <= PORT_SCAN_MAX; p++) {
    // Skip ports already occupied by a non-ours daemon.
    const occ = await probePort(p);
    const ours = occ && isOwnDaemon(occ.info, project, trackedSessions);
    if (occ && !ours) continue;
    if (ours) {
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
    // Browser probing cannot identify a non-Ro-Sync listener. Confirm the OS
    // port is actually free before counting this as a daemon launch attempt.
    if (await getPortOwner(p)) continue;
    attemptedFreePortLaunch = true;
    const hit = await launchAndWait(project, p);
    if (hit) {
      toast(`Port ${preferred} busy — started daemon on :${p}`);
      return hit;
    }
    fallbackLaunchError = lastDaemonLaunchError
      || `The daemon process on port ${p} did not complete its authenticated startup handshake.`;
    // A filesystem/configuration/startup failure is independent of the port.
    // Preserve that exact error instead of retrying every free port and
    // replacing it with noise. Only a genuine bind race should continue.
    if (!daemonLaunchErrorLooksPortBusy(fallbackLaunchError)) break;
  }
  if (attemptedFreePortLaunch) {
    lastDaemonLaunchError = fallbackLaunchError;
    toast("Daemon startup failed on the available fallback ports. Open project details for the exact error.");
    return null;
  }
  lastDaemonLaunchError = `All ports ${preferred}–${PORT_SCAN_MAX} are busy. Stop another daemon and retry.`;
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

// Probes a port and decides whether the daemon serves this exact filesystem
// project. A universe/game ID is intentionally insufficient: two local clones
// of the same Roblox game are distinct sync roots.
function isOwnDaemon(info, project, trackedSessions = []) {
  if (!info || typeof info !== "object") return false;
  // Project-path match against the currently active project.
  if (projectPathsEqual(info.project, project)) return true;
  // Windows canonicalization can change the spelling beyond prefix/separator
  // normalization (junctions, symlinks, 8.3 aliases, or component case).
  // Reattach only
  // through a session that pairs the current UI path with the exact daemon
  // PID, port, boot ID, and canonical project captured after authentication.
  if (trackedSessions.some((session) =>
    projectPathsEqual(session?.project, project)
      && daemonIdentityMatchesTrackedSession(session, info)
  )) return true;
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
  const launch = await launchDaemon(project, port);
  if (!launch) return null;
  const launchedPid = launch.pid;
  for (let i = 0; i < 20; i++) {
    await sleep(200);
    const hit = await probePort(port);
    if (hit) {
      const helloPid = Number.parseInt(hit.info?.pid, 10);
      if (helloPid === launchedPid) return hit;
      // A listener can win the preferred-port race after our launch attempt.
      // Never attach the just-created ownership token or PID to that process.
      console.warn("ignoring daemon launch race with a different listener PID", {
        expectedPid: launchedPid,
        actualPid: helloPid,
        port,
      });
    }
  }
  // This process never produced an authenticated identity. Do not persist it
  // or force-kill a bare PID: if it already exited, the OS may have reused that
  // PID for an unrelated process. A responsive owned daemon is cleaned up only
  // after its exact boot identity has been recorded below.
  if (app.state.daemonPid === launchedPid) {
    setState({
      daemonPid: null,
      daemonProject: null,
      daemonCanonicalProject: null,
      daemonBootId: null,
    });
  }
  const logTail = await readDaemonLaunchTail(launch.logPath);
  lastDaemonLaunchError = logTail
    || lastDaemonLaunchError
    || `The daemon process on port ${port} exited or stopped responding before its authenticated startup handshake.`;
  return null;
}

async function readDaemonLaunchTail(logPath) {
  let logTail = "";
  try {
    const logRes = await t64("t64:exec", { command: tailLogCmd(logPath) });
    logTail = (logRes && logRes.stdout) ? logRes.stdout.trim() : "";
  } catch {}
  // On Windows launchDaemonCmd redirects stderr to `<logPath>.err`, so
  // daemon startup crashes never land in the main .log. Tail the .err file
  // too — it is where panics and DLL-load failures surface.
  try {
    const errRes = await t64("t64:exec", { command: tailLogCmd(logPath + ".err") });
    const errTail = (errRes && errRes.stdout) ? errRes.stdout.trim() : "";
    if (errTail) logTail = logTail ? `${logTail}\n${errTail}` : errTail;
  } catch {}
  return logTail;
}

function daemonLaunchErrorLooksPortBusy(error) {
  return /address already in use|port\s+\d+.*(?:busy|in use|held by)|eaddrinuse/i.test(String(error || ""));
}

let ensureDaemonPromise = null;
let ensureDaemonQueued = false;
const desktopEnsurePromises = new Map();

async function ensureDaemon(projectId = null) {
  if (IS_DESKTOP_HOST) {
    const id = projectId || app.state.activeProjectId || servedProjectIds()[0] || null;
    const project = projectById(id);
    if (!project || !isProjectServed(id)) return;
    if (desktopEnsurePromises.has(id)) return desktopEnsurePromises.get(id);
    // Reserve the per-project slot before ensureDesktopDaemon can synchronously
    // publish an intermediate session state. Without this microtask boundary,
    // that state event can re-enter ensureDaemon with a fresh owner token before
    // this promise reaches the map, leaving the renderer unable to authenticate
    // the exact daemon boot that the first call just created.
    const promise = Promise.resolve()
      .then(() => ensureDesktopDaemon(id, project.path))
      .finally(() => {
        if (desktopEnsurePromises.get(id) === promise) {
          desktopEnsurePromises.delete(id);
        }
      });
    desktopEnsurePromises.set(id, promise);
    return promise;
  }
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

function desktopOwnershipError(message, status = null, reason = "external-manager") {
  const error = new Error(message);
  error.code = "EXTERNAL_DAEMON";
  error.status = status;
  error.reason = reason;
  return error;
}

function shouldClearDesktopClaim(error) {
  return !!error && [
    "incomplete-tracking",
    "external-manager",
    "ownership-rejected",
  ].includes(error.reason);
}

async function inspectOwnedDesktopDaemon(spec) {
  const project = spec?.project;
  const token = spec?.ownerToken;
  if (!project || !token) {
    throw desktopOwnershipError(
      "The daemon has no usable Desktop ownership capability; it was left running.",
      null,
      "incomplete-tracking",
    );
  }

  const status = lifecycleValue(await host.daemonStatus(project));
  if (!status.running) return { running: false, status };
  if (!isDesktopManagedStatus(status)) {
    const manager = status.managedBy ? ` by ${status.managedBy}` : " outside Ro Sync Desktop";
    throw desktopOwnershipError(
      `The daemon is managed${manager}; it was left running.`,
      status,
      "external-manager",
    );
  }

  const port = Number(status.port || spec.port);
  if (!Number.isFinite(port) || port <= 0) {
    throw desktopOwnershipError(
      "The managed daemon reported no verifiable port; it was left running.",
      status,
      "invalid-status",
    );
  }
  const base = status.base || status.baseUrl || spec.base || `http://127.0.0.1:${port}`;

  // CORS and the lifecycle endpoint both require the exact manager token. A
  // manager label or a persisted PID is never treated as proof of ownership.
  setDaemonAuthToken(token, base);
  let info;
  try {
    info = await daemonJson(base, "/hello");
  } catch {
    throw desktopOwnershipError(
      "Desktop could not authenticate the managed daemon; it was left running.",
      status,
      "transport",
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
  if (!proof.ok) {
    throw desktopOwnershipError(
      `Desktop could not authenticate the exact daemon boot${proof.error ? `: ${proof.error}` : ""}; it was left running.`,
      status,
      proof.sent ? "ownership-rejected" : "transport",
    );
  }
  if (!canStopDesktopDaemon({
    status,
    hello: info,
    ownershipAuthenticated: true,
    expectedProjects: validProjects,
  })) {
    throw desktopOwnershipError(
      "Desktop could not match the authenticated daemon to the expected boot; it was left running.",
      status,
      "identity-mismatch",
    );
  }
  return { running: true, status, info, base, port, token, bootId: info.bootId };
}

async function stopOwnedDesktopDaemon(spec, reason) {
  dbg("stopOwnedDesktopDaemon:", { project: spec?.project, port: spec?.port, reason });
  const owned = await inspectOwnedDesktopDaemon(spec);
  if (!owned.running) {
    if (spec?.bootId && spec?.ownerToken) {
      await host.daemonStop({
        project: spec.project,
        bootId: spec.bootId,
        ownerToken: spec.ownerToken,
        reason,
      });
    }
    return { stopped: true, alreadyStopped: true, status: owned.status };
  }
  const response = lifecycleValue(await host.daemonStop({
    project: spec.project,
    bootId: owned.bootId,
    ownerToken: owned.token,
    reason,
  }));
  if (response.ok === false || response.stopped === false) {
    throw new Error(response.error || "the authenticated daemon stop was rejected");
  }
  return { stopped: true, status: response, replacementRunning: false };
}

function clearDesktopDaemonTracking(projectId, { preserveTarget = false } = {}) {
  const session = app.state.daemonSessions?.[projectId] || null;
  const retained = preserveTarget && session?.project
    ? {
      project: session.project,
      canonicalProject: session.canonicalProject || null,
      port: session.port || null,
      pid: null,
      base: null,
      bootId: null,
      ownerToken: null,
      ok: null,
      error: null,
    }
    : null;
  setDesktopDaemonSession(projectId, retained);
  if (app.state.activeProjectId === projectId) {
    app.daemonBase = null;
    app.daemonOk = false;
  }
}

async function ensureDesktopDaemon(projectId, project) {
  const projectInfo = projectById(projectId);
  if (!projectInfo || projectInfo.path !== project || !isProjectServed(projectId)) {
    dbg("ensureDesktopDaemon: skipped", {
      projectId,
      project,
      known: !!projectInfo,
      pathMatch: projectInfo?.path === project,
      served: isProjectServed(projectId),
    });
    return;
  }
  dbg("ensureDesktopDaemon: starting", { projectId, project });
  let ownedCandidate = null;
  let requestedToken = null;
  const existingSession = app.state.daemonSessions?.[projectId] || null;
  const preferredPort = projectPathsEqual(existingSession?.project, project) && existingSession?.port
    ? existingSession.port
    : DEFAULT_PORT;
  let pending = pendingDesktopOwnershipByProject.get(projectId) || null;

  try {
    const startOwnership = desktopStartOwnership(
      sessionPolicyState(existingSession),
      project,
      makeOwnerToken(),
      pending,
    );
    requestedToken = startOwnership.token;
    if (!startOwnership.reusedClaim && !startOwnership.reusedPending) {
      // Fresh capabilities stay memory-only until the exact daemon boot proves
      // ownership. A failed start never turns a partial state record into stop
      // authority.
      clearDesktopDaemonTracking(projectId, { preserveTarget: true });
      pending = {
        project,
        token: requestedToken,
        base: preferredPort ? `http://127.0.0.1:${preferredPort}` : null,
      };
      pendingDesktopOwnershipByProject.set(projectId, pending);
    } else if (startOwnership.reusedClaim && !pending) {
      pending = {
        project,
        token: requestedToken,
        base: preferredPort ? `http://127.0.0.1:${preferredPort}` : null,
        port: preferredPort || null,
      };
      pendingDesktopOwnershipByProject.set(projectId, pending);
    }

    let result = await host.daemonEnsure({
      project,
      projectsRoot: app.state.projectsRoot || null,
      preferredPort,
      gameId: projectInfo.gameId || null,
      groupId: projectInfo.groupId || null,
      placeIds: projectInfo.placeIds || [],
      ownerToken: requestedToken,
    });
    result = lifecycleValue(result);
    dbg("ensureDesktopDaemon: daemonEnsure result", {
      ok: result.ok,
      running: result.running,
      port: result.port,
      pid: result.pid,
      managedBy: result.managedBy,
      error: result.error || null,
    });
    if (result.ok === false || result.running === false) {
      throw new Error(result.error || "managed daemon did not start");
    }
    if (result.externallyManaged || result.managedBy !== "desktop") {
      const manager = result.managedBy ? ` (${result.managedBy})` : "";
      throw desktopOwnershipError(
        `A daemon for this project is already externally managed${manager}; Desktop left it running.`,
        result,
        "external-manager",
      );
    }

    const port = Number(result.port || preferredPort);
    if (!Number.isFinite(port) || port <= 0) throw new Error("managed daemon returned an invalid port");
    const base = result.base || result.baseUrl || `http://127.0.0.1:${port}`;
    setDaemonAuthToken(requestedToken, base);
    pending = { project, token: requestedToken, base, port };
    pendingDesktopOwnershipByProject.set(projectId, pending);
    ownedCandidate = await inspectOwnedDesktopDaemon({
      project,
      canonicalProject: result.canonicalProject,
      port,
      base,
      ownerToken: requestedToken,
    });
    if (!ownedCandidate.running) throw new Error("managed daemon stopped during startup");

    // Stop only this exact authenticated boot if its own toggle was switched
    // off while the native sidecar handshake was in flight. Other served
    // projects are independent and remain untouched.
    if (!isProjectServed(projectId)) {
      await stopOwnedDesktopDaemon({
        project,
        port: ownedCandidate.port,
        pid: ownedCandidate.status.pid,
        bootId: ownedCandidate.bootId,
        ownerToken: requestedToken,
      }, "desktop startup was superseded");
      pendingDesktopOwnershipByProject.delete(projectId);
      clearDesktopDaemonTracking(projectId);
      emit("daemon:down", { projectId, project, host: HOST_KIND });
      refreshDesktopDaemonPresentation();
      return;
    }

    await setDesktopDaemonSession(projectId, {
      project,
      canonicalProject: ownedCandidate.status.canonicalProject || ownedCandidate.info.project || project,
      pid: ownedCandidate.status.pid || null,
      port: ownedCandidate.port,
      base: ownedCandidate.base,
      bootId: ownedCandidate.bootId,
      ownerToken: requestedToken,
      ok: true,
      error: null,
    });
    pendingDesktopOwnershipByProject.delete(projectId);
    if (app.state.activeProjectId === projectId) {
      app.daemonBase = ownedCandidate.base;
      app.daemonOk = true;
    }
    emit("daemon:up", {
      projectId,
      base: ownedCandidate.base,
      info: ownedCandidate.info,
      project,
      host: HOST_KIND,
    });
    refreshDesktopDaemonPresentation();
  } catch (error) {
    let retainedOwnedCandidate = false;
    if (ownedCandidate?.running) {
      try {
        await stopOwnedDesktopDaemon({
          project,
          port: ownedCandidate.port,
          bootId: ownedCandidate.bootId,
          ownerToken: ownedCandidate.token,
        }, "desktop startup did not complete");
        pendingDesktopOwnershipByProject.delete(projectId);
        clearDesktopDaemonTracking(projectId, { preserveTarget: isProjectServed(projectId) });
      } catch (cleanupError) {
        console.warn("owned daemon cleanup failed", cleanupError);
        setDesktopDaemonSession(projectId, {
          project,
          pid: ownedCandidate.status.pid || null,
          port: ownedCandidate.port,
          base: ownedCandidate.base,
          bootId: ownedCandidate.bootId,
          ownerToken: ownedCandidate.token,
          ok: false,
          error: cleanupError.message,
        });
        pendingDesktopOwnershipByProject.set(projectId, {
          project,
          token: ownedCandidate.token,
          base: ownedCandidate.base,
          port: ownedCandidate.port,
        });
        retainedOwnedCandidate = true;
      }
    }
    if (!retainedOwnedCandidate && shouldClearDesktopClaim(error)) {
      const currentPending = pendingDesktopOwnershipByProject.get(projectId);
      if (!requestedToken || currentPending?.token === requestedToken) {
        pendingDesktopOwnershipByProject.delete(projectId);
      }
      clearDesktopDaemonTracking(projectId, { preserveTarget: isProjectServed(projectId) });
    }
    if (!retainedOwnedCandidate) {
      const session = app.state.daemonSessions?.[projectId];
      if (session?.base) setDaemonAuthToken(null, session.base);
      patchDesktopDaemonSession(projectId, { ok: false, error: error.message });
    }
    setStatus(`managed daemon failed for ${projectInfo.name || project}: ${error.message}`, "err");
    emit("daemon:down", { projectId, project, error: error.message, host: HOST_KIND });
    refreshDesktopDaemonPresentation();
  }
}

async function ensureDaemonInner() {
  const project = activeProjectPath();
  const preferred =
    projectPathsEqual(app.state.daemonProject, project) && app.state.daemonPort
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
    await ensureDesktopDaemon(app.state.activeProjectId, project);
    return;
  }

  // 1. Probe preferred port.
  let hit = await probePort(preferred);
  const trackedSessions = hit ? await loadSessions() : [];

  // 2. If someone is on preferred port: is it ours?
  if (hit) {
    const ours = isOwnDaemon(hit.info, project, trackedSessions);
    const pointedAtOurProject = projectPathsEqual(hit.info?.project, project);
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
    } else if (app.state.daemonProject && !projectPathsEqual(app.state.daemonProject, project)) {
      // It's our own prior daemon but for a different project — stop and relaunch here.
      if (!(await killDaemon())) {
        app.daemonOk = false;
        app.daemonBase = null;
        setDaemonDot("idle", "previous project still connected");
        emit("daemon:down", { project, error: "previous project daemon is still running" });
        return;
      }
      hit = await launchAndWait(project, preferred);
      // The preferred port may belong to the unrelated listener that led us
      // into this branch; stopping our previous-project daemon does not free
      // that port. Continue through the normal fallback range instead of
      // leaving the newly selected project offline.
      if (!hit && await getPortOwner(preferred)) {
        hit = await scanFallbackPorts(project, preferred);
      }
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
    lastDaemonLaunchError = "";
    app.daemonBase = `http://127.0.0.1:${hit.port}`;
    app.daemonOk = true;
    setDaemonDot("ok", `:${hit.port}`);
    if (app.state.daemonPort !== hit.port) setState({ daemonPort: hit.port });
    if (!projectPathsEqual(app.state.daemonProject, project)) setState({ daemonProject: project });
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
      const helloPid = parseInt(hit.info?.pid, 10);
      const ownerPid = Number.isFinite(helloPid) && helloPid > 0
        ? helloPid
        : await getPortOwnerPid(hit.port);
      const daemonPid = ownerPid || app.state.daemonPid || null;
      if (daemonPid && app.state.daemonPid !== daemonPid) setState({ daemonPid });
      if (app.state.daemonCanonicalProject !== hit.info?.project) {
        setState({ daemonCanonicalProject: hit.info?.project || null });
      }
      await upsertSession({
        port: hit.port,
        pid: daemonPid,
        project,
        canonicalProject: hit.info?.project || project,
        bootId: hit.info?.bootId || null,
        startedAt: Date.now(),
      });
      await stopDuplicateTrackedSessions(project, hit.port);
    } else {
      // Forget any stale record for this listener. Keeping a token or PID here
      // would make killDaemon()/heartbeat treat an external process as ours.
      await removeSession({ port: hit.port });
      if (
        app.state.daemonPid
        || app.state.daemonCanonicalProject
        || app.state.daemonBootId
        || app.state.daemonOwnerToken
      ) {
        setState({
          daemonPid: null,
          daemonCanonicalProject: null,
          daemonBootId: null,
          daemonOwnerToken: null,
        });
      }
    }
    emit("daemon:up", { base: app.daemonBase, info: hit.info, project });
  } else {
    app.daemonOk = false;
    app.daemonBase = null;
    setDaemonDot("err", "daemon down");
    emit("daemon:down", {
      projectId: projectIdForPath(project) || app.state.activeProjectId,
      project,
      error: lastDaemonLaunchError || "The daemon did not become ready.",
    });
  }
}

async function killDaemon(projectIdOrOptions = null, maybeOptions = {}) {
  const options = projectIdOrOptions && typeof projectIdOrOptions === "object"
    ? projectIdOrOptions
    : maybeOptions;
  const preserveTarget = options?.preserveTarget === true;
  const requestedProjectId = typeof projectIdOrOptions === "string" ? projectIdOrOptions : null;
  const pid = app.state.daemonPid;
  const port = app.state.daemonPort;
  if (IS_DESKTOP_HOST) {
    const projectId = requestedProjectId || app.state.activeProjectId || servedProjectIds()[0] || null;
    if (!projectId) return true;
    const project = projectById(projectId);
    const pending = pendingDesktopOwnershipByProject.get(projectId) || null;
    if (pending) {
      try {
        await stopOwnedDesktopDaemon(
          {
            project: pending.project,
            ownerToken: pending.token,
          },
          "desktop cancelled a pending startup",
        );
        pendingDesktopOwnershipByProject.delete(projectId);
      } catch (error) {
        if (shouldClearDesktopClaim(error)) {
          pendingDesktopOwnershipByProject.delete(projectId);
        } else {
          toast(`Could not stop pending daemon safely: ${error.message}`);
          return false;
        }
      }
    }
    const session = app.state.daemonSessions?.[projectId] || null;
    const plan = desktopStopPlan(sessionPolicyState(session));
    dbg("killDaemon: stop plan", { projectId, kind: plan.kind, port: plan.spec?.port });
    if (plan.kind === "clear-local") {
      clearDesktopDaemonTracking(projectId, { preserveTarget });
      stopDaemonHeartbeat(projectId);
      emit("daemon:down", { projectId, project: project?.path || session?.project, host: HOST_KIND });
      refreshDesktopDaemonPresentation();
      return true;
    }
    try {
      const result = await stopOwnedDesktopDaemon(plan.spec, "desktop daemon stopped");
      if (!result.stopped) throw new Error("managed daemon did not stop");
      clearDesktopDaemonTracking(projectId, { preserveTarget });
      stopDaemonHeartbeat(projectId);
      emit("daemon:down", { projectId, project: project?.path || session?.project, host: HOST_KIND });
      refreshDesktopDaemonPresentation();
      return true;
    } catch (error) {
      if (shouldClearDesktopClaim(error)) {
        clearDesktopDaemonTracking(projectId, { preserveTarget });
      }
      toast(`Could not stop managed daemon: ${error.message}`);
      patchDesktopDaemonSession(projectId, { ok: false, error: error.message });
      refreshDesktopDaemonPresentation();
      return false;
    }
  }
  const trackedSessions = await loadSessions();
  const registeredStop = trackedSessions.find((session) =>
    app.state.daemonBootId
      && session?.bootId === app.state.daemonBootId
      && parseInt(session?.port, 10) === parseInt(port, 10)
      && projectPathsEqual(session?.project, app.state.daemonProject)
      && (!pid || parseInt(session?.pid, 10) === parseInt(pid, 10))
  );
  if (!registeredStop || !(await trackedSessionStillOwnsProcess(registeredStop))) {
    toast("The daemon could not be matched to an exact authenticated session and was left running.");
    return false;
  }
  const base = `http://127.0.0.1:${port}`;
  const identity = exactLifecycleCloseIdentity(registeredStop);
  if (!identity) {
    toast("The daemon session has no complete close identity and was left running.");
    return false;
  }
  const graceful = await daemonLifecycleRequest(
    base,
    "/manager-close",
    "daemon stopped",
    app.state.daemonOwnerToken,
    identity,
  );
  if (!graceful.ok) {
    toast(`Could not stop daemon safely: ${graceful.error || "ownership rejected"}`);
    return false;
  }
  if (!(await waitForPortRelease(port, 4000, registeredStop.pid))) {
    toast(`Daemon did not release port ${port}; it was left tracked.`);
    return false;
  }
  await removeSession({ pid: registeredStop.pid, port, bootId: registeredStop.bootId });
  setState({
    daemonPid: null,
    daemonProject: preserveTarget ? app.state.daemonProject : null,
    daemonCanonicalProject: null,
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
const desktopHealthInFlightByProject = new Set();

async function healthTickDesktopProject(projectId, nativeClaims, nativeByProject) {
  if (desktopHealthInFlightByProject.has(projectId)) return;
  desktopHealthInFlightByProject.add(projectId);
  try {
    const project = projectById(projectId);
    if (!project) return;
    const session = app.state.daemonSessions?.[projectId] || null;
    const cfg = project.settings || {};
    if (!session?.base || !session?.ownerToken || !session?.bootId) {
      if (!pendingDesktopOwnershipByProject.has(projectId)
          && !desktopEnsurePromises.has(projectId)
          && cfg.AutoReconnect !== "off") {
        await ensureDaemon(projectId);
      }
      return;
    }

    // A renderer reload can restore an authenticated persisted session before
    // the native in-memory claim registry has seen that boot. Re-run the
    // idempotent ensure with the same capability so native exit cleanup and
    // daemon_stop regain the exact project-keyed claim.
    const nativeClaim = nativeByProject.get(session.canonicalProject)
      || nativeByProject.get(session.project)
      || nativeByProject.get(project.path)
      || null;
    if (!nativeClaim) {
      if (nativeClaims) {
        await ensureDaemon(projectId);
        return;
      }
    }

    try {
      setDaemonAuthToken(session.ownerToken, session.base);
      const info = await daemonJson(session.base, "/hello");
      const expectedProjects = new Set([
        project.path,
        session.project,
        session.canonicalProject,
        nativeClaim?.project,
        nativeClaim?.canonicalProject,
      ].filter(Boolean));
      if (
        info.bootId !== session.bootId
        || !expectedProjects.has(info.project)
        || info.managed !== true
        || info.managedBy !== "desktop"
        || (nativeClaim && nativeClaim.bootId !== session.bootId)
      ) {
        throw new Error("managed daemon identity changed");
      }
      if (session.ok !== true || session.error) {
        await patchDesktopDaemonSession(projectId, { ok: true, error: null });
        emit("daemon:up", { projectId, project: project.path, base: session.base, info, host: HOST_KIND });
      }
    } catch (error) {
      if (session.ok !== false || session.error !== error.message) {
        patchDesktopDaemonSession(projectId, { ok: false, error: error.message });
        emit("daemon:down", { projectId, project: project.path, error: error.message, host: HOST_KIND });
      }
      if (cfg.AutoReconnect !== "off") await ensureDaemon(projectId);
    }
  } finally {
    desktopHealthInFlightByProject.delete(projectId);
  }
}

async function healthTickDesktop() {
  const ids = servedProjectIds();
  const nativeList = await host.daemonList().catch(() => null);
  const nativeClaims = Array.isArray(nativeList?.daemons) ? nativeList.daemons : null;
  const nativeByProject = new Map(
    (nativeClaims || [])
      .filter((item) => item?.project)
      .map((item) => [item.project, item]),
  );
  await Promise.allSettled(
    ids.map((projectId) => healthTickDesktopProject(projectId, nativeClaims, nativeByProject)),
  );
  refreshDesktopDaemonPresentation();
}

async function healthTick() {
  if (IS_DESKTOP_HOST) {
    await healthTickDesktop();
    return;
  }
  if (!app.daemonBase) {
    return;
  }
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
const $projectsTab = document.getElementById("nav-projects");
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
    if (context && info?.version) {
      const commit = typeof info.buildCommit === "string" && info.buildCommit
        ? ` · ${info.buildCommit}${info.buildDirty ? " dirty" : ""}`
        : "";
      context.textContent = `Studio bridge · v${info.version}${commit}`;
      if (info.target) context.title = `Build target: ${info.target}`;
      document.title = `Ro Sync v${info.version}${commit}`;
    }
  } catch {
    document.documentElement.dataset.platform = PLATFORM;
  }
}

function wireDesktopTitlebar() {
  const titlebar = document.getElementById("desktop-titlebar");
  if (!titlebar || !IS_DESKTOP_HOST || !host.supports.windowDrag) return;
  titlebar.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || event.buttons !== 1 || event.ctrlKey) return;
    if (event.target.closest("button, a, input, select, textarea, [role='button']")) return;
    host.startWindowDrag().catch((error) => {
      console.warn("window drag failed", error);
    });
  });
}

wireDesktopTitlebar();

const CONFLICT_BADGE_POLL_MS = 20_000;

function wireConflictBadge() {
  const badge = document.getElementById("conflicts-badge");
  if (!badge) {
    return {
      refresh() {},
      report() {},
      invalidate() {},
    };
  }

  const counts = new Map();
  let refreshInFlight = false;
  let refreshQueued = false;

  const targets = () => servedProjectIds().map((projectId) => ({
    projectId,
    base: getDaemonBase(projectId),
  }));

  const syncTargets = () => {
    const current = targets();
    const currentIds = new Set(current.map(({ projectId }) => projectId));
    for (const projectId of counts.keys()) {
      if (!currentIds.has(projectId)) counts.delete(projectId);
    }
    for (const { projectId, base } of current) {
      const previous = counts.get(projectId);
      if (!previous || previous.base !== base) {
        counts.set(projectId, {
          base,
          state: base ? "loading" : "unavailable",
          count: null,
        });
      }
    }
    return current;
  };

  const render = () => {
    const current = syncTargets();
    if (!current.length) {
      badge.hidden = true;
      badge.textContent = "—";
      badge.className = "badge badge-unknown sidenav-badge";
      badge.title = "No projects are being served";
      badge.setAttribute("aria-label", "Conflict count unavailable; no projects are being served");
      return;
    }

    const entries = current.map(({ projectId }) => counts.get(projectId));
    const known = entries.filter((entry) => entry?.state === "known");
    const unknownCount = entries.length - known.length;
    const total = known.reduce((sum, entry) => sum + entry.count, 0);
    const projectLabel = `${current.length} served ${current.length === 1 ? "project" : "projects"}`;

    badge.hidden = false;
    badge.className = "badge sidenav-badge";
    if (unknownCount === 0) {
      badge.textContent = String(total);
      badge.classList.toggle("badge-ok", total === 0);
      badge.title = total === 0
        ? `No unresolved conflicts across ${projectLabel}`
        : `${total} unresolved conflict${total === 1 ? "" : "s"} across ${projectLabel}`;
    } else if (total > 0) {
      badge.textContent = `${total}+`;
      badge.classList.add("badge-partial");
      badge.title = `At least ${total} unresolved conflict${total === 1 ? "" : "s"}; ${unknownCount} ${unknownCount === 1 ? "project has" : "projects have"} not reported yet`;
    } else {
      badge.textContent = "—";
      badge.classList.add("badge-unknown");
      badge.title = `Conflict count unavailable for ${unknownCount} of ${projectLabel}`;
    }
    badge.setAttribute("aria-label", badge.title);
  };

  async function refresh() {
    if (refreshInFlight) {
      refreshQueued = true;
      return;
    }
    refreshInFlight = true;
    try {
      do {
        refreshQueued = false;
        const current = syncTargets();
        render();
        await Promise.all(current.map(async ({ projectId, base }) => {
          if (!base) return;
          try {
            const data = await daemonJson(base, "/resolve");
            if (!servedProjectIds().includes(projectId) || getDaemonBase(projectId) !== base) return;
            const conflicts = Array.isArray(data)
              ? data
              : (Array.isArray(data?.conflicts) ? data.conflicts : null);
            if (!conflicts) throw new Error("invalid conflict response");
            counts.set(projectId, { base, state: "known", count: conflicts.length });
          } catch {
            if (!servedProjectIds().includes(projectId) || getDaemonBase(projectId) !== base) return;
            counts.set(projectId, { base, state: "unavailable", count: null });
          }
        }));
        render();
      } while (refreshQueued);
    } finally {
      refreshInFlight = false;
    }
  }

  const report = (projectId, value) => {
    const count = Number(value);
    if (!servedProjectIds().includes(projectId) || !Number.isInteger(count) || count < 0) return;
    const base = getDaemonBase(projectId);
    if (!base) return;
    counts.set(projectId, { base, state: "known", count });
    render();
  };

  const invalidate = (projectId = null) => {
    const current = syncTargets();
    for (const target of current) {
      if (projectId && target.projectId !== projectId) continue;
      counts.set(target.projectId, {
        base: target.base,
        state: target.base ? "loading" : "unavailable",
        count: null,
      });
    }
    render();
    void refresh();
  };

  setInterval(() => {
    if (!document.hidden) void refresh();
  }, CONFLICT_BADGE_POLL_MS);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) void refresh();
  });

  render();
  return { refresh, report, invalidate };
}

const DESKTOP_UPDATE_RECHECK_MS = 6 * 60 * 60 * 1000;
const DESKTOP_UPDATE_RETRY_MS = 5 * 60 * 1000;

function wireDesktopUpdater() {
  const button = document.getElementById("desktop-update");
  const overlay = document.getElementById("desktop-update-confirm");
  const description = document.getElementById("desktop-update-confirm-description");
  const cancelButton = document.getElementById("desktop-update-cancel");
  const proceedButton = document.getElementById("desktop-update-proceed");
  const root = document.getElementById("root");
  if (!button || !overlay || !description || !cancelButton || !proceedButton || !root
      || !IS_DESKTOP_HOST || !host.supports.updates) return { check() {} };

  let checking = false;
  let installing = false;
  let availableVersion = null;
  let lastCheckedAt = 0;
  let lastAttemptAt = 0;
  let lastFocused = null;

  const closeConfirmation = ({ restoreFocus = true } = {}) => {
    if (overlay.hidden) return;
    overlay.hidden = true;
    root.inert = false;
    if (restoreFocus && lastFocused && document.contains(lastFocused)) lastFocused.focus();
    lastFocused = null;
  };

  const showConfirmation = (count) => {
    const projects = count === 1 ? "the project currently being served" : `${count} projects currently being served`;
    description.textContent = `Updating will stop Ro Sync and disconnect ${projects} from Roblox Studio. You can start them again after Ro Sync restarts.`;
    lastFocused = document.activeElement;
    root.inert = true;
    overlay.hidden = false;
    requestAnimationFrame(() => cancelButton.focus());
  };

  const renderAvailable = () => {
    if (!availableVersion) {
      button.hidden = true;
      closeConfirmation({ restoreFocus: false });
      return;
    }
    button.hidden = false;
    button.disabled = installing;
    button.setAttribute("aria-busy", installing ? "true" : "false");
    button.title = installing
      ? `Installing Ro Sync ${availableVersion}`
      : `Download and install Ro Sync ${availableVersion}`;
    button.querySelector("span").textContent = installing
      ? "Updating…"
      : `Update v${availableVersion.replace(/^v/i, "")}`;
  };

  async function check({ force = false } = {}) {
    if (checking || installing || document.hidden) return;
    if (!force && Date.now() - lastCheckedAt < DESKTOP_UPDATE_RECHECK_MS) return;
    if (!force && Date.now() - lastAttemptAt < DESKTOP_UPDATE_RETRY_MS) return;
    checking = true;
    lastAttemptAt = Date.now();
    try {
      const result = await host.updateCheck();
      lastCheckedAt = Date.now();
      availableVersion = result?.configured && result?.available && result?.version
        ? String(result.version)
        : null;
      renderAvailable();
    } catch (error) {
      console.warn("application update check failed", error);
    } finally {
      checking = false;
    }
  }

  async function installAvailableUpdate() {
    if (!availableVersion || installing) return;
    installing = true;
    renderAvailable();
    try {
      await host.updateInstall(availableVersion);
    } catch (error) {
      installing = false;
      renderAvailable();
      toast(`Update failed: ${error.message}`);
    }
  }

  button.addEventListener("click", () => {
    if (!availableVersion || installing) return;
    const count = servedProjectIds().length;
    if (count) showConfirmation(count);
    else void installAvailableUpdate();
  });
  cancelButton.addEventListener("click", () => closeConfirmation());
  proceedButton.addEventListener("click", () => {
    closeConfirmation({ restoreFocus: false });
    void installAvailableUpdate();
  });
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) closeConfirmation();
  });
  document.addEventListener("keydown", (event) => {
    if (overlay.hidden || installing) return;
    if (event.key === "Escape") {
      event.preventDefault();
      closeConfirmation();
      return;
    }
    if (event.key !== "Tab") return;
    if (event.shiftKey && document.activeElement === cancelButton) {
      event.preventDefault();
      proceedButton.focus();
    } else if (!event.shiftKey && document.activeElement === proceedButton) {
      event.preventDefault();
      cancelButton.focus();
    }
  });

  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) void check();
  });
  return { check };
}

function wireProjectsRootPrompt() {
  const overlay = document.getElementById("projects-root-prompt");
  const choose = document.getElementById("projects-root-choose");
  const skip = document.getElementById("projects-root-skip");
  const error = document.getElementById("projects-root-error");
  const root = document.getElementById("root");
  if (!overlay || !choose || !skip || !error || !root || !IS_DESKTOP_HOST) {
    return { showIfNeeded() {} };
  }

  let lastFocused = null;
  let choosing = false;
  const focusable = [choose, skip];

  const close = () => {
    if (overlay.hidden || choosing) return;
    overlay.hidden = true;
    root.inert = false;
    if (lastFocused && document.contains(lastFocused)) lastFocused.focus();
    lastFocused = null;
  };

  const showIfNeeded = () => {
    if (app.state.projectsRoot || !overlay.hidden) return;
    lastFocused = document.activeElement;
    error.hidden = true;
    error.textContent = "";
    root.inert = true;
    overlay.hidden = false;
    requestAnimationFrame(() => choose.focus());
  };

  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) close();
  });
  skip.addEventListener("click", close);
  choose.addEventListener("click", async () => {
    if (choosing) return;
    choosing = true;
    choose.disabled = true;
    skip.disabled = true;
    error.hidden = true;
    try {
      const path = String(await host.pickFolder("Choose Ro Sync projects folder") || "").trim();
      if (!path) return;
      setState({ projectsRoot: path });
      choosing = false;
      choose.disabled = false;
      skip.disabled = false;
      close();
      toast("Projects folder saved");
    } catch (pickError) {
      error.textContent = pickError.message || String(pickError);
      error.hidden = false;
    } finally {
      choosing = false;
      choose.disabled = false;
      skip.disabled = false;
    }
  });
  document.addEventListener("keydown", (event) => {
    if (overlay.hidden || choosing) return;
    if (event.key === "Escape") {
      event.preventDefault();
      close();
      return;
    }
    if (event.key !== "Tab") return;
    const current = focusable.indexOf(document.activeElement);
    if (event.shiftKey && current <= 0) {
      event.preventDefault();
      focusable[focusable.length - 1].focus();
    } else if (!event.shiftKey && current === focusable.length - 1) {
      event.preventDefault();
      focusable[0].focus();
    }
  });

  return { showIfNeeded };
}

const conflictBadge = wireConflictBadge();
const desktopUpdater = wireDesktopUpdater();
const projectsRootPrompt = wireProjectsRootPrompt();

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
  refreshProjectsServingIndicator();
}

function refreshDesktopDaemonPresentation() {
  if (!IS_DESKTOP_HOST) return;
  const ids = servedProjectIds();
  const sessions = ids.map((id) => app.state.daemonSessions?.[id]).filter(Boolean);
  const healthy = sessions.filter((session) => session.ok === true);
  const failed = sessions.filter((session) => session.ok === false);
  const pending = Math.max(0, ids.length - healthy.length - failed.length);
  const focused = getDaemonSession(app.state.activeProjectId);
  app.daemonBase = focused?.ok === true ? focused.base : null;
  app.daemonOk = !!app.daemonBase;

  if (!ids.length) {
    setDaemonDot("idle", "no projects served");
  } else if (healthy.length === ids.length) {
    const ports = healthy.filter((session) => session.port).map((session) => `:${session.port}`).join(", ");
    setDaemonDot("ok", `${healthy.length} ${healthy.length === 1 ? "project" : "projects"}${ports ? ` · ${ports}` : ""}`);
  } else if (failed.length) {
    setDaemonDot("err", `${healthy.length}/${ids.length} projects online`);
  } else {
    setDaemonDot("warn", `${pending || ids.length} ${ids.length === 1 ? "project" : "projects"} starting…`);
  }
}

function refreshProjectsServingIndicator() {
  const count = servedProjectIds().length;
  let state = "idle";
  if (count) {
    if (IS_DESKTOP_HOST) {
      const sessions = servedProjectIds().map((id) => app.state.daemonSessions?.[id]);
      state = sessions.some((session) => session?.ok === false)
        ? "err"
        : sessions.every((session) => session?.ok === true) ? "ok" : "warn";
    } else {
      const connection = document.getElementById("root").dataset.connection;
      state = app.daemonOk ? "ok" : connection === "err" ? "err" : "warn";
    }
  }
  const action = state === "ok" ? "serving" : state === "warn" ? "starting" : "needs attention";
  const label = `${count} ${count === 1 ? "project" : "projects"} ${action}`;
  document.getElementById("root").dataset.projectsServing = state;
  $projectsTab.setAttribute("aria-label", count ? `Projects, ${label}` : "Projects");
  $projectsTab.title = count ? label : "";
}

async function serveProject(projectId) {
  const project = projectById(projectId);
  if (!project) return false;
  if (IS_DESKTOP_HOST) {
    const next = [...new Set([...servedProjectIds(), projectId])];
    setState({ servedProjectIds: next, activeProjectId: projectId });
    refreshDesktopDaemonPresentation();
    await ensureDaemon(projectId);
    return getDaemonSession(projectId)?.ok === true;
  }
  setState({ servedProjectIds: [projectId], activeProjectId: projectId });
  await ensureDaemon();
  return app.daemonOk;
}

async function stopProject(projectId) {
  if (!projectById(projectId)) return true;
  if (IS_DESKTOP_HOST) {
    const previous = servedProjectIds();
    setState({ servedProjectIds: previous.filter((id) => id !== projectId) });
    const stopped = await killDaemon(projectId);
    if (!stopped) setState({ servedProjectIds: previous });
    refreshDesktopDaemonPresentation();
    return stopped;
  }
  if (app.state.activeProjectId !== projectId) return true;
  setState({ servedProjectIds: [], activeProjectId: null });
  return killDaemon();
}

async function restartProject(projectId) {
  if (!projectById(projectId) || !isProjectServed(projectId)) return false;
  const stopped = await killDaemon(projectId, { preserveTarget: true });
  if (!stopped) return false;
  await ensureDaemon(projectId);
  return !!getDaemonBase(projectId);
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
    getAppearanceTheme,
    setAppearanceTheme,
    getDaemonBase,
    getDaemonFailure,
    getDaemonSession,
    reportDaemonFailure,
    isProjectServed,
    ensureDaemon,
    killDaemon,
    serveProject,
    stopProject,
    restartProject,
    reportConflictCount: conflictBadge.report,
    invalidateConflictCount: conflictBadge.invalidate,
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

if (!IS_DESKTOP_HOST) {
  window.addEventListener("pagehide", (event) => {
    if (event && event.persisted) return;
    notifyWidgetClosing();
  });
  window.addEventListener("beforeunload", notifyWidgetClosing);
}

// Re-render active view on daemon state changes (cheap).
on("daemon:up", (event = {}) => {
  const projectId = event.projectId || projectIdForPath(event.project) || app.state.activeProjectId;
  reportDaemonFailure(projectId, null);
  emit("view:refresh", app.currentView);
});
on("daemon:down", (event = {}) => {
  const projectId = event.projectId || projectIdForPath(event.project) || app.state.activeProjectId;
  reportDaemonFailure(projectId, event.error || null);
  emit("view:refresh", app.currentView);
});
on("daemon:up", (event) => startDaemonHeartbeat(event?.projectId || null));
on("daemon:down", (event) => stopDaemonHeartbeat(event?.projectId || null));
on("daemon:up", (event) => conflictBadge.invalidate(event?.projectId || null));
on("daemon:down", (event) => conflictBadge.invalidate(event?.projectId || null));
on("conflict", (event) => conflictBadge.invalidate(event?.projectId || null));

// Desktop treats activeProjectId as the focused project and serves the
// independent servedProjectIds set. Terminal 64 retains its historical
// single-daemon active-project behavior.
let lastActiveProject = null;
let lastServedProjects = new Set();
on("state", () => {
  refreshProjectsServingIndicator();
  void conflictBadge.refresh();
  if (IS_DESKTOP_HOST) {
    const next = new Set(servedProjectIds());
    const newlyServed = [...next].filter((projectId) => !lastServedProjects.has(projectId));
    // Commit the observation before starting anything. Daemon startup itself
    // emits state while recording pending ownership, so callbacks must see this
    // project as already observed rather than recursively starting it again.
    lastServedProjects = next;
    for (const projectId of newlyServed) void ensureDaemon(projectId);
    refreshDesktopDaemonPresentation();
    return;
  }
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
  const text = String(msg || "");
  el.textContent = text;
  el.classList.add("show");
  clearTimeout(toastT);
  const readingTime = Math.min(6500, Math.max(2400, 1400 + text.length * 32));
  toastT = setTimeout(() => el.classList.remove("show"), readingTime);
}

function applyHostThemePayload(payload) {
  const ui = payload?.theme?.ui;
  if (ui && typeof ui === "object" && !Array.isArray(ui)) {
    currentHostTheme = ui;
    applyCurrentAppearanceTheme();
    if (!document.documentElement.classList.contains("theme-pending")) {
      clearTimeout(appearanceRevealTimer);
      appearanceRevealTimer = null;
    }
  }
}

onT64("t64:init", (payload) => {
  applyHostThemePayload(payload);
  if (payload && payload.state) {
    app.state = normalizePersistedState({
      ...payload.state,
      daemonOwnerToken: payload.state.daemonOwnerToken || app.state.daemonOwnerToken || null,
    });
    setDaemonAuthToken(app.state.daemonOwnerToken);
    for (const session of Object.values(app.state.daemonSessions || {})) {
      if (session?.base && session?.ownerToken) setDaemonAuthToken(session.ownerToken, session.base);
    }
    emit("state", app.state);
  }
  applyCurrentAppearanceTheme();
  void conflictBadge.refresh();
});
onT64("t64:state", applyHostThemePayload);

function onSystemAppearanceChanged() {
  if (app.state.appearanceTheme === "system") applyCurrentAppearanceTheme();
}
if (systemThemeQuery?.addEventListener) {
  systemThemeQuery.addEventListener("change", onSystemAppearanceChanged);
} else if (systemThemeQuery?.addListener) {
  systemThemeQuery.addListener(onSystemAppearanceChanged);
}

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
const appStreams = new Map();
const RAW_OP_RE = /"type"\s*:\s*"op"/;
const INITIAL_CHOICE_REPLAY = Object.freeze({
  REPLAYED: "replayed",
  RESOLVED: "resolved",
  UNAVAILABLE: "unavailable",
});
const initialChoiceReplayEpoch = createChoiceReplayEpochGuard();

async function replayPendingInitialChoice(projectId, base, event = {}) {
  const replayEpoch = initialChoiceReplayEpoch.begin(projectId, event.choiceId);
  try {
    const pending = await daemonJson(base, "/initial-choice");
    if (!initialChoiceReplayEpoch.accepts(replayEpoch, pending?.choiceId)) {
      return INITIAL_CHOICE_REPLAY.RESOLVED;
    }
    if (!pending?.pending || pending.choice || !pending.choiceId) {
      return INITIAL_CHOICE_REPLAY.RESOLVED;
    }
    emit("initial-choice-needed", {
      ...pending,
      projectId: projectId === "__single" ? app.state.activeProjectId : projectId,
      projectPath: event.project || projectById(projectId)?.path || null,
    });
    return INITIAL_CHOICE_REPLAY.REPLAYED;
  } catch {
    if (!initialChoiceReplayEpoch.accepts(replayEpoch, event.choiceId || null)) {
      return INITIAL_CHOICE_REPLAY.RESOLVED;
    }
    // The realtime stream will still deliver a newly-created decision. Replay
    // is best-effort recovery for a decision broadcast before this UI attached.
    return INITIAL_CHOICE_REPLAY.UNAVAILABLE;
  }
}

// Op frames are intentionally skipped on the app-level stream so large file
// bursts do not force the Terminal 64 host to JSON-parse every source payload.
function shouldSkipRawAppFrame(raw) {
  if (typeof raw !== "string" || !RAW_OP_RE.test(raw)) return false;
  return true;
}

function openAppStream(event = {}) {
  if (!ENABLE_APP_REALTIME_STREAM) {
    return;
  }
  const projectId = event.projectId
    || projectIdForPath(event.project)
    || app.state.activeProjectId
    || "__single";
  const base = event.base || getDaemonBase(projectId === "__single" ? null : projectId);
  if (!base) return;
  initialChoiceReplayEpoch.invalidate(projectId);
  const previous = appStreams.get(projectId);
  if (previous) { try { previous.close(); } catch {} }
  // Resolved lazily: in single-daemon mode the active project can change
  // between frames, and last-edited stamps must land on the right project.
  const resolveMemoryKey = () => {
    const resolvedId = projectId === "__single" ? app.state.activeProjectId : projectId;
    return projectMemoryKey(
      event.project || projectById(resolvedId)?.path || null,
      resolvedId,
    );
  };
  const resolveProjectId = () => (
    projectId === "__single" ? app.state.activeProjectId : projectId
  );
  const resolveProjectPath = () => (
    event.project || projectById(resolveProjectId())?.path || null
  );
  const publishPluginState = (connected) => {
    const resolvedId = resolveProjectId();
    if (connected) reportDaemonFailure(resolvedId, null);
    emit("plugin:state", {
      type: "plugin",
      connected: connected === true,
      projectId: resolvedId,
      projectPath: resolveProjectPath(),
    });
  };
  try {
    const stream = daemonWS(base, "/ws", {
      skipRaw: shouldSkipRawAppFrame,
      open: () => {
        void replayPendingInitialChoice(projectId, base, event);
        void daemonJson(base, "/hello")
          .then((info) => publishPluginState(info?.pluginConnected === true))
          .catch(() => {});
      },
      message: (data) => {
        if (!data || typeof data !== "object") return;
        const t = data.type;
        if (!t) return;
        if (t === "op") {
          // Sync ops are applied by the plugin. The app-level stream only
          // exists for control events, so never turn op volume into a prompt.
          return;
        }
        if (t === "sync-activity") {
          lastEditedStore.record(resolveMemoryKey(), data);
          return;
        }
        if (t === "plugin") {
          publishPluginState(data.connected === true);
          return;
        }
        if (t === "shutdown") {
          const resolvedId = resolveProjectId();
          const shutdown = {
            ...data,
            projectId: resolvedId,
            projectPath: resolveProjectPath(),
          };
          if (data.retryable === false) {
            reportDaemonFailure(resolvedId, runtimeShutdownDiagnostic(data));
          }
          emit("plugin:shutdown", shutdown);
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
        if (t === "initial-choice-needed") {
          // The realtime event and this status replay are both deliberately
          // bounded. The modal opens from the summary returned here, then
          // pages immutable stable-ID details itself; this relay never pulls a
          // tens-of-thousands-path payload into the app event hot path.
          initialChoiceReplayEpoch.invalidate(projectId);
          void replayPendingInitialChoice(projectId, base, {
            ...event,
            project: data.project || event.project,
            choiceId: data.choiceId,
          }).then((outcome) => {
            // An authoritative "not pending" status means this WS event raced
            // with resolution. Only a transport failure may fall back to the
            // bounded event summary; otherwise a stale modal would reopen.
            if (outcome !== INITIAL_CHOICE_REPLAY.UNAVAILABLE) return;
            emit(t, {
              ...data,
              projectId: projectId === "__single" ? app.state.activeProjectId : projectId,
              projectPath: event.project || projectById(projectId)?.path || null,
            });
          });
          return;
        }
        if (t === "initial-choice-made" || t === "config-changed" || t === "conflict") {
          if (t === "initial-choice-made") initialChoiceReplayEpoch.invalidate(projectId);
          emit(t, {
            ...data,
            projectId: projectId === "__single" ? app.state.activeProjectId : projectId,
            projectPath: event.project || projectById(projectId)?.path || null,
          });
          return;
        }
        if (t === "project-init") {
          adoptProjectInitEvents([data]);
          return;
        }
        // Unknown event — no-op.
      },
      error: () => { /* daemonWS handles reconnect */ },
    });
    appStreams.set(projectId, stream);
    void replayPendingInitialChoice(projectId, base, event);
  } catch (e) {
    console.warn("app WS failed", e);
  }
}
on("daemon:up", openAppStream);
on("daemon:down", (event = {}) => {
  const projectId = event.projectId || projectIdForPath(event.project) || null;
  if (projectId) {
    initialChoiceReplayEpoch.invalidate(projectId);
    const stream = appStreams.get(projectId);
    if (stream) { try { stream.close(); } catch {} }
    appStreams.delete(projectId);
    return;
  }
  // Terminal 64 single-daemon flows emit daemon:down without a project
  // identity, while streams are keyed by the project that was active when
  // they opened. Only one daemon can exist there, so every remaining stream
  // is stale — leaving one open lets it re-attach to the next daemon on the
  // same port and attribute that project's frames (including last-edited
  // stamps) to the old project.
  for (const stream of appStreams.values()) { try { stream.close(); } catch {} }
  appStreams.clear();
  initialChoiceReplayEpoch.invalidateAll();
});

// ---------- Native Studio project-initialization broker ----------
let projectBrokerTimer = null;
let projectBrokerInFlight = false;

function adoptProjectInitEvents(events) {
  let nextState = app.state;
  const mergedEvents = [];
  for (const event of events) {
    try {
      const merged = mergeProjectInitEvent(nextState, event);
      nextState = { ...nextState, ...merged.patch };
      mergedEvents.push({ event, ...merged });
    } catch (error) {
      console.warn("project initialization event rejected", event, error);
    }
  }
  if (!mergedEvents.length) return [];
  setState({
    projects: nextState.projects,
    servedProjectIds: nextState.servedProjectIds,
    activeProjectId: nextState.activeProjectId,
  });
  for (const merged of mergedEvents) {
    emit("project:init", { ...merged.event, projectId: merged.project.id, created: merged.created });
  }
  const last = mergedEvents[mergedEvents.length - 1];
  toast(last.created
    ? `Created and started ${last.project.name}`
    : `Connected ${last.project.name}`);
  return mergedEvents;
}

async function drainProjectInitEvents() {
  if (!IS_DESKTOP_HOST || projectBrokerInFlight) return;
  projectBrokerInFlight = true;
  try {
    const events = await host.projectInitDrain();
    if (!events.length) return;
    adoptProjectInitEvents(events);
  } finally {
    projectBrokerInFlight = false;
  }
}

async function pollProjectBroker() {
  if (!IS_DESKTOP_HOST) return;
  try {
    app.projectBrokerStatus = await host.projectBrokerStatus();
    emit("broker:status", app.projectBrokerStatus);
    await drainProjectInitEvents();
  } catch (error) {
    app.projectBrokerStatus = { ok: false, error: error.message };
    emit("broker:status", app.projectBrokerStatus);
  } finally {
    if (!widgetCloseSent) projectBrokerTimer = setTimeout(pollProjectBroker, 1200);
  }
}

// ---------- Boot ----------
function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

(async function boot() {
  void hydrateHostPresentation();
  await loadState();
  await lastEditedStore.load();
  appearanceStateLoaded = true;
  // Appearance is persisted independently of the last route. Apply it before
  // mounting a view so restored sessions never render a page in the wrong
  // preset and then flash to the chosen one.
  applyCurrentAppearanceTheme();
  void conflictBadge.refresh();
  projectsRootPrompt.showIfNeeded();
  void desktopUpdater.check({ force: true });
  // Reap dead-PID sessions before we try to reuse any recorded port.
  if (!IS_DESKTOP_HOST) await pruneDeadSessions();
  // Signal readiness so host can send t64:init.
  host.ready().catch(() => {});
  scheduleAppearanceRevealFallback();
  if (IS_DESKTOP_HOST) void pollProjectBroker();
  navigate(app.state.lastView || "projects");
  // Mount the blocking overwrite-choice modal at app-level.
  mountOverwriteModal({
    onBus: on,
    getDaemonBase,
    getState,
    toast,
    lastEdited: lastEditedStore,
  });
  setDaemonDot("warn", "connecting…");
  await ensureDaemon();
})();

// Expose for debugging from devtools.
window.__rosync = {
  getState,
  setState,
  getAppearanceTheme,
  setAppearanceTheme,
  getDaemonBase,
  getDaemonFailure,
  getDaemonSession,
  reportDaemonFailure,
  serveProject,
  stopProject,
  restartProject,
  ensureDaemon,
  killDaemon,
};
