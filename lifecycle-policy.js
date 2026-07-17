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
