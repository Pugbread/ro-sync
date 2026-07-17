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
for (const mismatch of [
  { ...exactHello, project: "/other-game" },
  { ...exactHello, bootId: "replacement" },
  { ...exactHello, pid: 4101 },
  { ...exactHello, port: 7879 },
]) {
  assert.equal(policy(exactStatus, mismatch), false, "every hello identity field must match status");
}
assert.equal(
  policy({ ...exactStatus, canonicalProject: "/other-game" }, exactHello),
  false,
  "status and hello must identify the same expected project",
);

for (const external of [
  { ...exactStatus, managedBy: "cli" },
  { ...exactStatus, managedBy: "other-manager" },
  { ...exactStatus, externallyManaged: true },
  { ...exactStatus, managed: false, managedBy: "manual" },
]) {
  assert.equal(isDesktopManagedStatus(external), false);
  assert.equal(policy(external, exactHello), false, "external daemon must never be stoppable");
}

const alphaSession = {
  project: "/game",
  pid: 4100,
  port: 7878,
  bootId: "desktop-boot",
  ownerToken: "desktop-owner-token",
};
const betaSession = {
  project: "/other-game",
  pid: 4200,
  port: 7879,
  bootId: "other-desktop-boot",
  ownerToken: "other-desktop-owner-token",
};
const projectState = {
  servedProjectIds: ["alpha", "beta"],
  daemonSessions: { alpha: alphaSession, beta: betaSession },
};
assert.deepEqual(desktopTrackedOwnership(projectState, "alpha"), {
  project: "/game",
  pid: 4100,
  port: 7878,
  bootId: "desktop-boot",
  ownerToken: "desktop-owner-token",
});
assert.deepEqual(desktopTrackedOwnership(projectState, "beta"), {
  project: "/other-game",
  pid: 4200,
  port: 7879,
  bootId: "other-desktop-boot",
  ownerToken: "other-desktop-owner-token",
});
assert.equal(desktopTrackedOwnership(projectState, "missing"), null);
assert.deepEqual(desktopStopPlan(projectState, "alpha"), {
  kind: "stop-owned",
  spec: desktopTrackedOwnership(projectState, "alpha"),
});

for (const field of ["project", "bootId", "ownerToken"]) {
  const incomplete = {
    ...projectState,
    daemonSessions: { ...projectState.daemonSessions, alpha: { ...alphaSession, [field]: null } },
  };
  assert.equal(
    desktopTrackedOwnership(incomplete, "alpha"),
    null,
    `${field} is required for one project's Desktop ownership`,
  );
  assert.deepEqual(
    desktopStopPlan(incomplete, "alpha"),
    { kind: "clear-local" },
    "incomplete project tracking must never issue a remote stop",
  );
  assert.deepEqual(
    desktopTrackedOwnership(incomplete, "beta"),
    desktopTrackedOwnership(projectState, "beta"),
    "one incomplete project must not alter another project's claim",
  );
}
for (const field of ["pid", "port"]) {
  const recoverable = { ...alphaSession, [field]: null };
  const claim = desktopTrackedOwnership(recoverable);
  assert.ok(claim, `${field} may be recovered from authenticated daemon status`);
  assert.equal(claim[field], null);
  assert.equal(desktopStopPlan(recoverable).kind, "stop-owned");
}
for (const invalid of ["too-short", "invalid token with spaces"]) {
  assert.equal(
    desktopTrackedOwnership({ ...alphaSession, ownerToken: invalid }),
    null,
    "malformed Desktop ownership tokens must be rejected locally",
  );
}

const emptyState = { daemonSessions: {} };
const emptyBefore = structuredClone(emptyState);
assert.deepEqual(
  desktopStartOwnership(emptyState, "/game", "fresh-token", null, "alpha"),
  { token: "fresh-token", reusedClaim: false, reusedPending: false },
  "a missing project claim must receive a fresh memory-only capability",
);
assert.deepEqual(emptyState, emptyBefore, "Desktop start planning must not mutate state");
assert.deepEqual(
  desktopStartOwnership(projectState, "/game", "fresh-token", null, "alpha"),
  { token: "desktop-owner-token", reusedClaim: true, reusedPending: false },
  "a complete same-project claim remains reusable across Desktop reloads",
);
assert.deepEqual(
  desktopStartOwnership(projectState, "/other-game", "fresh-token", null, "alpha"),
  { token: "fresh-token", reusedClaim: false, reusedPending: false },
  "a selected project's capability must never authorize a different project",
);
assert.deepEqual(
  desktopStartOwnership(emptyState, "/game", "fresh-token", {
    project: "/game",
    token: "pending-owner-token",
  }, "alpha"),
  { token: "pending-owner-token", reusedClaim: false, reusedPending: true },
  "a same-project pending capability survives transient verification failure",
);
assert.deepEqual(
  desktopStartOwnership(emptyState, "/game", "fresh-token", {
    project: "/other-game",
    token: "pending-owner-token",
  }, "alpha"),
  { token: "fresh-token", reusedClaim: false, reusedPending: false },
  "pending capabilities must never cross project boundaries",
);

const appSource = fs.readFileSync(new URL("../app.js", import.meta.url), "utf8");
const bridgeSource = fs.readFileSync(new URL("../bridge.js", import.meta.url), "utf8");
const prepareSource = fs.readFileSync(new URL("../desktop/scripts/prepare.mjs", import.meta.url), "utf8");
const tauriLibSource = fs.readFileSync(new URL("../desktop/src-tauri/src/lib.rs", import.meta.url), "utf8");
const tauriDaemonSource = fs.readFileSync(new URL("../desktop/src-tauri/src/daemon.rs", import.meta.url), "utf8");

assert.match(
  appSource,
  /servedProjectIds:\s*\[\][\s\S]*?daemonSessions:\s*\{\}/,
  "Desktop state must represent desired projects and daemon sessions independently",
);
assert.match(
  appSource,
  /const pendingDesktopOwnershipByProject = new Map\(\)/,
  "in-flight ownership capabilities must be isolated by project ID",
);
assert.equal(
  appSource.includes('"--owner-token", ensureOwnerToken()'),
  false,
  "Terminal 64 launch must not put an owner token in argv",
);

const bridgeStopSource = bridgeSource.slice(
  bridgeSource.indexOf("async daemonStop("),
  bridgeSource.indexOf("async projectBrokerStatus("),
);
assert.match(
  bridgeStopSource,
  /invokeDesktop\("daemon_stop"[\s\S]*?project: spec\.project[\s\S]*?bootId: spec\.bootId[\s\S]*?ownerToken: spec\.ownerToken/,
  "the native stop bridge must send the complete project, boot, and owner claim",
);
assert.equal(bridgeStopSource.includes("pid:"), false, "native stop must never accept PID authority");
assert.equal(bridgeStopSource.includes("port:"), false, "native stop must never accept port authority");
assert.match(tauriLibSource, /daemon::daemon_list[\s\S]*?daemon::daemon_status[\s\S]*?daemon::daemon_stop/);

assert.match(
  tauriDaemonSource,
  /struct ExactManagedDaemonClaim \{[\s\S]*?project: String,[\s\S]*?port: u16,[\s\S]*?pid: u32,[\s\S]*?boot_id: String,[\s\S]*?owner_token: String,/,
  "native ownership must retain the complete exact daemon identity and capability",
);
assert.match(
  tauriDaemonSource,
  /struct ManagedDaemonClaimsState \{[\s\S]*?exiting: bool,[\s\S]*?by_project: HashMap<String, ExactManagedDaemonClaim>/,
  "native exit ownership must be plural and keyed by canonical project",
);
const exactClaimSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("fn exact_managed_daemon_claim"),
  tauriDaemonSource.indexOf("fn close_managed_daemons"),
);
for (const required of [
  'get("running")',
  'get("managed")',
  'get("managedBy")',
  'Some("desktop")',
  'get("externallyManaged")',
  "validate_owner_token(owner_token)",
  'get("bootId")',
  'get("canonicalProject")',
  'get("pid")',
  'get("port")',
]) {
  assert.equal(exactClaimSource.includes(required), true, `exact native claim must check ${required}`);
}
assert.match(
  tauriDaemonSource,
  /fn close_managed_daemons[\s\S]*?thread::scope[\s\S]*?for claim in &claims[\s\S]*?close_exact_managed_daemon\(claim\)/,
  "all project claims must close independently and in parallel during app exit",
);

const closeExactSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("fn close_exact_managed_daemon"),
  tauriDaemonSource.indexOf("fn local_json_request"),
);
assert.match(
  closeExactSource,
  /local_json_request\(claim\.port, "GET", "\/hello", &\[\]\)[\s\S]*?managedBy[\s\S]*?claim\.project[\s\S]*?claim\.boot_id[\s\S]*?claim\.pid[\s\S]*?claim\.port[\s\S]*?"token": claim\.owner_token[\s\S]*?local_json_request\(claim\.port, "POST", "\/manager-close", &body\)/,
  "native cleanup must revalidate project, boot, PID, and port before sending the owner token",
);

const tauriEnsureSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("pub(crate) async fn daemon_ensure"),
  tauriDaemonSource.indexOf("fn preferred_port_attempts"),
);
assert.match(
  tauriEnsureSource,
  /for port in attempts[\s\S]*?run_lifecycle[\s\S]*?Ok\(value\) => \{[\s\S]*?managed_daemons\.remember\(&value, &owner_token\);[\s\S]*?return Ok\(value\);/,
  "every successful project start must register its exact native ownership claim",
);
assert.match(
  tauriDaemonSource,
  /fn preferred_port_attempts[\s\S]*?Some\(port\) => vec!\[Some\(port\), None\]/,
  "Desktop must preserve a preferred listener first and fall back without evicting its owner",
);
assert.equal(
  tauriEnsureSource.includes('"--parent-stdin-lease"'),
  true,
  "Desktop daemon start must hold a parent-stdin lease",
);

const tauriStopSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("pub(crate) async fn daemon_stop"),
  tauriDaemonSource.indexOf("pub(crate) fn daemon_list"),
);
assert.match(
  tauriStopSource,
  /validate_project\(&spec\.project\)[\s\S]*?ensure_authorized_path[\s\S]*?expected_boot_id[\s\S]*?validate_lifecycle_identity[\s\S]*?owner_token[\s\S]*?validate_owner_token/,
  "native stop must validate the authorized project, boot ID, and owner capability",
);
assert.match(
  tauriStopSource,
  /daemon_status_for_project[\s\S]*?exact_managed_daemon_claim\(&value, owner_token\)[\s\S]*?claim\.project != canonical_project[\s\S]*?claim\.boot_id != expected_boot_id[\s\S]*?close_exact_managed_daemon\(&claim\)/,
  "native stop must status-check and close only the exact requested project boot",
);
for (const forbidden of ["CommandChild", ".kill()", '"daemon".to_string(),\n        "stop".to_string()']) {
  assert.equal(tauriStopSource.includes(forbidden), false, "native stop must not use process/PID termination");
}
assert.equal(
  tauriDaemonSource.includes("native_exit_close_never_claims_external_daemons"),
  true,
  "the native suite must retain an explicit external-daemon non-ownership regression test",
);

const desktopKillSource = appSource.slice(
  appSource.indexOf("async function killDaemon"),
  appSource.indexOf("// ---------- Health loop ----------"),
);
const desktopKillBranch = desktopKillSource.slice(0, desktopKillSource.indexOf("  if (!pid) {"));
assert.match(
  desktopKillBranch,
  /daemonSessions\?\.\[projectId\][\s\S]*?desktopStopPlan\(sessionPolicyState\(session\)\)[\s\S]*?stopOwnedDesktopDaemon/,
  "Desktop stop must select exactly one project session and require its ownership plan",
);
assert.equal(
  desktopKillBranch.includes("killPidCmd"),
  false,
  "Desktop-managed daemons must never be stopped by PID",
);
const ownedStopSource = appSource.slice(
  appSource.indexOf("async function stopOwnedDesktopDaemon"),
  appSource.indexOf("function clearDesktopDaemonTracking"),
);
assert.match(
  ownedStopSource,
  /inspectOwnedDesktopDaemon\(spec\)[\s\S]*?host\.daemonStop\(\{[\s\S]*?project: spec\.project[\s\S]*?bootId: owned\.bootId[\s\S]*?ownerToken: owned\.token/,
  "renderer stop must authenticate status/hello before invoking exact native stop",
);

const desktopEnsureDispatchSource = appSource.slice(
  appSource.indexOf("async function ensureDaemon(projectId = null)"),
  appSource.indexOf("function lifecycleValue"),
);
assert.match(
  desktopEnsureDispatchSource,
  /const promise = Promise\.resolve\(\)[\s\S]*?\.then\(\(\) => ensureDesktopDaemon\(id, project\.path\)\)[\s\S]*?desktopEnsurePromises\.get\(id\) === promise[\s\S]*?desktopEnsurePromises\.set\(id, promise\)/,
  "Desktop must reserve one project startup before its synchronous state updates can re-enter ensureDaemon",
);
const servedStateObserverSource = appSource.slice(
  appSource.indexOf('on("state", () => {'),
  appSource.indexOf("// Toast helper"),
);
assert.match(
  servedStateObserverSource,
  /const newlyServed =[\s\S]*?lastServedProjects = next;[\s\S]*?for \(const projectId of newlyServed\) void ensureDaemon\(projectId\)/,
  "served-project observation must commit before startup publishes nested state",
);

assert.match(
  tauriLibSource,
  /managed_daemons\.mark_exiting\(\);[\s\S]*?lifecycle_children\.terminate_all\(\);[\s\S]*?managed_daemons\.terminate\(\);/,
  "Desktop exit must reject late claims, terminate lifecycle children, then close all exact daemon claims",
);
const nativeListSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("fn list(&self)"),
  tauriDaemonSource.indexOf("pub(crate) fn mark_exiting"),
);
assert.equal(nativeListSource.includes("owner_token"), false, "daemon_list must never expose owner tokens");

const healthSource = appSource.slice(
  appSource.indexOf("async function healthTickDesktop"),
  appSource.indexOf("async function healthTick()"),
);
assert.match(
  healthSource,
  /host\.daemonList\(\)[\s\S]*?const nativeClaim = nativeByProject\.get[\s\S]*?if \(!nativeClaim\) \{[\s\S]*?await ensureDaemon\(projectId\)/,
  "a renderer reload must reattach every persisted project claim to native exit cleanup",
);
const closeSource = appSource.slice(
  appSource.indexOf("function notifyWidgetClosing"),
  appSource.indexOf("function activeProject"),
);
assert.match(
  closeSource,
  /Object\.values\(app\.state\.daemonSessions \|\| \{\}\)[\s\S]*?pendingDesktopOwnershipByProject\.values\(\)[\s\S]*?for \(const \[base, token\] of closeTargets\)/,
  "renderer close beacons must cover all committed and pending project sessions",
);

const tauriStatusSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("pub(crate) async fn daemon_status"),
  tauriDaemonSource.indexOf("async fn run_lifecycle"),
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

const settingsSource = fs.readFileSync(new URL("../views/settings.js", import.meta.url), "utf8");
assert.match(
  settingsSource,
  /data-daemon-action="restart"[\s\S]*?data-daemon-action="stop"[\s\S]*?api\.stopProject\(projectId\)[\s\S]*?api\.restartProject\(projectId\)/,
  "Settings controls must address one served project rather than a singleton daemon",
);
assert.match(
  settingsSource,
  /\$start\.addEventListener\("click"[\s\S]*?api\.serveProject\(project\.id\)/,
  "Settings Start must add the selected project to the served set",
);

console.log("lifecycle policy checks passed");
