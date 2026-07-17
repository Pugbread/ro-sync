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

// A persisted Desktop token is authority, not a launch preference. Treat it
// as usable only when it was committed with the complete authenticated boot
// identity. Partial migration/startup state must never trigger a remote stop.
export function desktopTrackedOwnership(state) {
  const project = nonBlankString(state?.daemonProject);
  const bootId = nonBlankString(state?.daemonBootId);
  const ownerToken = validOwnerToken(state?.daemonOwnerToken);
  const pid = positiveInteger(state?.daemonPid, 0xffff_ffff);
  const port = positiveInteger(state?.daemonPort, 0xffff);
  if (!project || !bootId || !ownerToken) return null;
  return { project, bootId, ownerToken, pid, port };
}

export function desktopStopPlan(state) {
  const spec = desktopTrackedOwnership(state);
  return spec ? { kind: "stop-owned", spec } : { kind: "clear-local" };
}

// Fresh Desktop capabilities stay in memory until the daemon proves exact
// ownership. A complete prior claim may reuse its token to reattach/relaunch.
export function desktopStartOwnership(state, project, freshToken, pending = null) {
  const claim = desktopTrackedOwnership(state);
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
  return (
    projects.has(hello.project) &&
    !!status.bootId &&
    hello.bootId === status.bootId &&
    Number.isFinite(Number(status.pid)) &&
    Number(hello.pid) === Number(status.pid) &&
    Number.isFinite(Number(status.port)) &&
    Number(hello.port) === Number(status.port)
  );
}
