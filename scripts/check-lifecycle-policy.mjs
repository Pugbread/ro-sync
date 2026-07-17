import assert from "node:assert/strict";
import fs from "node:fs";
import {
  canStopDesktopDaemon,
  desktopStartOwnership,
  desktopStopPlan,
  desktopTrackedOwnership,
  isDesktopManagedStatus,
} from "../lifecycle-policy.js";

const exactStatus = {
  running: true,
  managed: true,
  externallyManaged: false,
  managedBy: "desktop",
  project: "/game",
  canonicalProject: "/game",
  pid: 4100,
  port: 7878,
  bootId: "desktop-boot",
};
const exactHello = {
  managed: true,
  managedBy: "desktop",
  project: "/game",
  pid: 4100,
  port: 7878,
  bootId: "desktop-boot",
};
const policy = (status, hello, ownershipAuthenticated = true) => canStopDesktopDaemon({
  status,
  hello,
  ownershipAuthenticated,
  expectedProjects: ["/game"],
});

assert.equal(isDesktopManagedStatus(exactStatus), true);
assert.equal(policy(exactStatus, exactHello), true, "exact authenticated Desktop boot may stop");
assert.equal(policy(exactStatus, exactHello, false), false, "unauthenticated Desktop boot must survive");
assert.equal(policy(exactStatus, { ...exactHello, bootId: "replacement" }), false, "replacement boot must survive");

for (const external of [
  { ...exactStatus, managedBy: "cli" },
  { ...exactStatus, managedBy: "other-manager" },
  { ...exactStatus, externallyManaged: true },
  { ...exactStatus, managed: false, managedBy: "manual" },
]) {
  assert.equal(isDesktopManagedStatus(external), false);
  assert.equal(policy(external, exactHello), false, "external daemon must never be stoppable");
}

const completeTracking = {
  daemonProject: "/game",
  daemonPid: 4100,
  daemonPort: 7878,
  daemonBootId: "desktop-boot",
  daemonOwnerToken: "desktop-owner-token",
};
assert.deepEqual(desktopTrackedOwnership(completeTracking), {
  project: "/game",
  pid: 4100,
  port: 7878,
  bootId: "desktop-boot",
  ownerToken: "desktop-owner-token",
});
assert.deepEqual(desktopStopPlan(completeTracking), {
  kind: "stop-owned",
  spec: desktopTrackedOwnership(completeTracking),
});

for (const field of [
  "daemonProject",
  "daemonBootId",
  "daemonOwnerToken",
]) {
  const incomplete = { ...completeTracking, [field]: null };
  assert.equal(desktopTrackedOwnership(incomplete), null, `${field} is required for Desktop ownership`);
  assert.deepEqual(
    desktopStopPlan(incomplete),
    { kind: "clear-local" },
    "incomplete Desktop tracking must never issue a remote stop",
  );
}
for (const field of ["daemonPid", "daemonPort"]) {
  const recoverable = { ...completeTracking, [field]: null };
  const claim = desktopTrackedOwnership(recoverable);
  assert.ok(claim, `${field} may be recovered from authenticated daemon status`);
  assert.equal(claim[field === "daemonPid" ? "pid" : "port"], null);
  assert.equal(desktopStopPlan(recoverable).kind, "stop-owned");
}
for (const invalid of [
  { daemonOwnerToken: "too-short" },
  { daemonOwnerToken: "invalid token with spaces" },
]) {
  assert.equal(
    desktopTrackedOwnership({ ...completeTracking, ...invalid }),
    null,
    "malformed Desktop ownership fields must be rejected locally",
  );
}

const orphanTracking = {
  daemonProject: null,
  daemonPid: null,
  daemonPort: null,
  daemonBootId: null,
  daemonOwnerToken: "orphan-owner-token",
};
const orphanBefore = structuredClone(orphanTracking);
assert.deepEqual(
  desktopStartOwnership(orphanTracking, "/game", "fresh-token"),
  { token: "fresh-token", reusedClaim: false, reusedPending: false },
  "an orphan capability must not be reused or persisted by Desktop startup",
);
assert.deepEqual(orphanTracking, orphanBefore, "Desktop start planning must not mutate state");
assert.deepEqual(
  desktopStartOwnership(completeTracking, "/game", "fresh-token"),
  { token: "desktop-owner-token", reusedClaim: true, reusedPending: false },
  "a complete claim must remain reusable across Desktop reconnects",
);
assert.deepEqual(
  desktopStartOwnership(completeTracking, "/other-game", "fresh-token"),
  { token: "fresh-token", reusedClaim: false, reusedPending: false },
  "a claim for another project must never authorize a fresh start",
);
assert.deepEqual(
  desktopStartOwnership(
    orphanTracking,
    "/game",
    "fresh-token",
    { project: "/game", token: "pending-owner-token" },
  ),
  { token: "pending-owner-token", reusedClaim: false, reusedPending: true },
  "a same-project pending capability must survive transient verification failure",
);
assert.deepEqual(
  desktopStartOwnership(
    orphanTracking,
    "/game",
    "fresh-token",
    { project: "/other-game", token: "pending-owner-token" },
  ),
  { token: "fresh-token", reusedClaim: false, reusedPending: false },
  "a pending capability must never cross project boundaries",
);

const appSource = fs.readFileSync(new URL("../app.js", import.meta.url), "utf8");
const bridgeSource = fs.readFileSync(new URL("../bridge.js", import.meta.url), "utf8");
const prepareSource = fs.readFileSync(new URL("../desktop/scripts/prepare.mjs", import.meta.url), "utf8");
const tauriLibSource = fs.readFileSync(new URL("../desktop/src-tauri/src/lib.rs", import.meta.url), "utf8");
const tauriDaemonSource = fs.readFileSync(new URL("../desktop/src-tauri/src/daemon.rs", import.meta.url), "utf8");
assert.equal(
  appSource.includes("host.daemonStop("),
  false,
  "renderer must not bypass authenticated HTTP ownership with native record-based stop",
);
assert.equal(
  appSource.includes('"--owner-token", ensureOwnerToken()'),
  false,
  "Terminal 64 launch must not put the owner token in argv",
);
assert.equal(
  bridgeSource.includes("async daemonStop("),
  false,
  "shared renderer host must not expose the unauthenticated native stop command",
);
assert.equal(
  tauriLibSource.includes("daemon::daemon_stop"),
  false,
  "Tauri invoke surface must not expose record-based native stop",
);
assert.match(
  tauriDaemonSource,
  /for port in attempts[\s\S]*?run_lifecycle[\s\S]*?Ok\(value\) => \{[\s\S]*?managed_daemon\.remember\(&value, &owner_token\);[\s\S]*?return Ok\(value\);/,
  "Desktop must retry managed startup through the daemon's free-port scan",
);
assert.match(
  tauriDaemonSource,
  /fn preferred_port_attempts[\s\S]*?Some\(port\) => vec!\[Some\(port\), None\]/,
  "Desktop must preserve a preferred listener first and fall back without stopping its owner",
);
assert.match(
  tauriDaemonSource,
  /local_json_request\(claim\.port, "GET", "\/hello", &\[\]\)[\s\S]*?bootId[\s\S]*?local_json_request\(claim\.port, "POST", "\/manager-close", &body\)/,
  "Native exit cleanup must revalidate the exact daemon boot before sending its ownership capability",
);
assert.match(
  tauriLibSource,
  /managed_daemon\.mark_exiting\(\);[\s\S]*?lifecycle_children\.terminate_all\(\);[\s\S]*?managed_daemon\.terminate\(\);/,
  "Desktop exit must reject late daemon claims before terminating in-flight lifecycle children",
);
const tauriEnsureSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("pub(crate) async fn daemon_ensure"),
  tauriDaemonSource.indexOf("fn preferred_port_attempts"),
);
const tauriStatusSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("pub(crate) async fn daemon_status"),
  tauriDaemonSource.indexOf("async fn run_lifecycle"),
);
assert.equal(
  tauriEnsureSource.includes('"--parent-stdin-lease"'),
  true,
  "Desktop daemon start must hold a parent-stdin lease",
);
assert.equal(
  tauriStatusSource.includes('"--parent-stdin-lease"'),
  true,
  "Desktop daemon status must hold a parent-stdin lease",
);
assert.equal(
  prepareSource.includes('"lifecycle-policy.js"'),
  true,
  "Desktop packaging must include the shared lifecycle policy module",
);
const desktopEnsureSource = appSource.slice(
  appSource.indexOf("async function ensureDesktopDaemon"),
  appSource.indexOf("async function ensureDaemonInner"),
);
assert.equal(
  desktopEnsureSource.includes("ensureOwnerToken()"),
  false,
  "Desktop must not persist a fresh owner token before authenticated ownership proof",
);
assert.equal(
  desktopEnsureSource.includes("activeProjectPath() !== project"),
  true,
  "Desktop must cancel a daemon start superseded by a newer UI intent",
);
assert.match(
  desktopEnsureSource,
  /if \(!startOwnership\.reusedClaim && !startOwnership\.reusedPending\) \{[\s\S]*?clearDesktopDaemonTracking\(\);/,
  "Desktop must clear partial tracking before a fresh memory-only capability is used",
);
assert.match(
  desktopEnsureSource,
  /pendingDesktopOwnership = \{[\s\S]*?project,[\s\S]*?token: startOwnership\.token,[\s\S]*?base:/,
  "Desktop must retain a fresh capability in memory until ownership is proven",
);
assert.match(
  desktopEnsureSource,
  /startOwnership\.reusedClaim && !pendingDesktopOwnership[\s\S]*?pendingDesktopOwnership = \{[\s\S]*?token: startOwnership\.token/,
  "Desktop must retain a persisted claim as a close target while reattaching",
);
assert.match(
  desktopEnsureSource,
  /pendingDesktopOwnership = \{ project, token, base, port \};[\s\S]*?ownedCandidate = await inspectOwnedDesktopDaemon/,
  "Desktop must replace a provisional target with the exact fallback listener before verification",
);
assert.match(
  desktopEnsureSource,
  /owned daemon cleanup failed[\s\S]*?daemonOwnerToken: ownedCandidate\.token[\s\S]*?pendingDesktopOwnership = \{[\s\S]*?base: ownedCandidate\.base/,
  "Desktop must retain proven ownership and a close target when startup cleanup is uncertain",
);
const desktopKillSource = appSource.slice(
  appSource.indexOf("async function killDaemon"),
  appSource.indexOf("async function healthTick"),
);
assert.equal(
  desktopKillSource.includes("desktopStopPlan(app.state)"),
  true,
  "Desktop stop must reject incomplete ownership before any remote lifecycle request",
);
assert.match(
  desktopKillSource,
  /if \(pendingDesktopOwnership\)[\s\S]*?stopOwnedDesktopDaemon\([\s\S]*?desktop cancelled a pending startup/,
  "explicit Desktop Stop must clean up an in-flight authenticated capability immediately",
);
const closeSource = appSource.slice(
  appSource.indexOf("function notifyWidgetClosing"),
  appSource.indexOf("function activeProject"),
);
assert.match(
  closeSource,
  /pendingDesktopOwnership\?\.base[\s\S]*?daemonLifecyclePost\(endpoint, reason, true, pending\)/,
  "Desktop close must use a pending startup capability when committed ownership is not available",
);
assert.match(
  appSource,
  /function daemonLifecyclePost[\s\S]*?daemonURL\(base, path, token\)/,
  "lifecycle beacons must authenticate their URL with an explicit pending or committed token",
);
const healthSource = appSource.slice(
  appSource.indexOf("async function healthTick"),
  appSource.indexOf("async function runHealthTick"),
);
assert.match(
  healthSource,
  /if \(!app\.daemonBase\)[\s\S]*?await ensureDaemon\(\)/,
  "an active Desktop project must retry after an external daemon leaves the port",
);
assert.match(
  healthSource,
  /if \(!IS_DESKTOP_HOST\) return;/,
  "Desktop no-base recovery must not change Terminal 64 lifecycle behavior",
);
const settingsSource = fs.readFileSync(new URL("../views/settings.js", import.meta.url), "utf8");
const settingsStopSource = settingsSource.slice(
  settingsSource.indexOf('$stop.addEventListener("click"'),
  settingsSource.indexOf('$restart.addEventListener("click"'),
);
assert.match(
  settingsStopSource,
  /activeProjectId: null[\s\S]*?active\?\.path \|\| s\.daemonProject[\s\S]*?killDaemon\(\{ preserveTarget: true \}\)/,
  "Settings Stop must preserve its active target while clearing desired-serving state",
);
const settingsStartSource = settingsSource.slice(
  settingsSource.indexOf('$start.addEventListener("click"'),
  settingsSource.indexOf('$stop.addEventListener("click"'),
);
assert.match(
  settingsStartSource,
  /p\.path === s\.daemonProject[\s\S]*?activeProjectId: target\.id[\s\S]*?ensureDaemon\(\)/,
  "Settings Start must restore the explicitly paused project target",
);

console.log("lifecycle policy checks passed");
