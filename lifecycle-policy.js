// Pure lifecycle predicates shared by the renderer and contract checks.
// Labels, PIDs, and ports are descriptive only: an exact authenticated boot
// identity is required before Desktop may ask a daemon to stop.

export function isDesktopManagedStatus(status) {
  return !!(
    status &&
    status.running === true &&
    status.externallyManaged !== true &&
    status.managed === true &&
    status.managedBy === "desktop"
  );
}

function nonBlankString(value) {
  return typeof value === "string" && value.trim() ? value : null;
}

function positiveInteger(value, maximum = Number.MAX_SAFE_INTEGER) {
  const number = Number(value);
  return Number.isInteger(number) && number > 0 && number <= maximum ? number : null;
}

function validOwnerToken(value) {
  const token = nonBlankString(value);
  return token &&
    token.length >= 16 &&
    token.length <= 512 &&
    /^[A-Za-z0-9._~-]+$/.test(token)
    ? token
    : null;
}

// Desktop persists one lifecycle session per project ID. Accepting the legacy
// daemon* field names here keeps migration and Terminal 64 callers compatible,
// but authorization always resolves to one selected project record; claims can
// never bleed between entries in daemonSessions.
function projectSession(state, projectId = null) {
  if (projectId != null) {
    const sessions = state?.daemonSessions;
    if (!sessions || typeof sessions !== "object" || Array.isArray(sessions)) return null;
    return sessions[projectId] || null;
  }
  if (state && (
    Object.hasOwn(state, "project") ||
    Object.hasOwn(state, "bootId") ||
    Object.hasOwn(state, "ownerToken")
  )) {
    return state;
  }
  return state ? {
    project: state.daemonProject,
    bootId: state.daemonBootId,
    ownerToken: state.daemonOwnerToken,
    pid: state.daemonPid,
    port: state.daemonPort,
  } : null;
}

// A persisted Desktop token is authority, not a launch preference. Treat it
// as usable only when it was committed with the complete authenticated boot
// identity. PID and port remain recoverable descriptions, never authority.
export function desktopTrackedOwnership(state, projectId = null) {
  const session = projectSession(state, projectId);
  const project = nonBlankString(session?.project);
  const bootId = nonBlankString(session?.bootId);
  const ownerToken = validOwnerToken(session?.ownerToken);
  const pid = positiveInteger(session?.pid, 0xffff_ffff);
  const port = positiveInteger(session?.port, 0xffff);
  if (!project || !bootId || !ownerToken) return null;
  return { project, bootId, ownerToken, pid, port };
}

export function desktopStopPlan(state, projectId = null) {
  const spec = desktopTrackedOwnership(state, projectId);
  return spec ? { kind: "stop-owned", spec } : { kind: "clear-local" };
}

// Fresh Desktop capabilities stay in memory until the daemon proves exact
// ownership. A complete prior claim may reuse its token to reattach/relaunch.
export function desktopStartOwnership(state, project, freshToken, pending = null, projectId = null) {
  const claim = desktopTrackedOwnership(state, projectId);
  if (claim && claim.project === project) {
    return { token: claim.ownerToken, reusedClaim: true, reusedPending: false };
  }
  const pendingProject = nonBlankString(pending?.project);
  const pendingToken = validOwnerToken(pending?.token);
  if (pendingProject === project && pendingToken) {
    return { token: pendingToken, reusedClaim: false, reusedPending: true };
  }
  return { token: freshToken, reusedClaim: false, reusedPending: false };
}

export function canStopDesktopDaemon({
  status,
  hello,
  ownershipAuthenticated,
  expectedProjects = [],
}) {
  if (!isDesktopManagedStatus(status) || ownershipAuthenticated !== true) return false;
  if (!hello || hello.managed !== true || hello.managedBy !== "desktop") return false;
  const projects = new Set(Array.from(expectedProjects || []).filter(Boolean));
  const statusProject = nonBlankString(status.canonicalProject || status.project);
  const helloProject = nonBlankString(hello.project);
  const statusBootId = nonBlankString(status.bootId);
  const helloBootId = nonBlankString(hello.bootId);
  const statusPid = positiveInteger(status.pid, 0xffff_ffff);
  const helloPid = positiveInteger(hello.pid, 0xffff_ffff);
  const statusPort = positiveInteger(status.port, 0xffff);
  const helloPort = positiveInteger(hello.port, 0xffff);
  return (
    !!statusProject &&
    helloProject === statusProject &&
    projects.has(statusProject) &&
    !!statusBootId &&
    helloBootId === statusBootId &&
    !!statusPid &&
    helloPid === statusPid &&
    !!statusPort &&
    helloPort === statusPort
  );
}
