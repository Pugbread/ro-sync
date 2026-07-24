import assert from "node:assert/strict";
import fs from "node:fs";
import {
  canStopDesktopDaemon,
  desktopStartOwnership,
  desktopStopPlan,
  desktopTrackedOwnership,
  exactLifecycleCloseIdentity,
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
assert.deepEqual(exactLifecycleCloseIdentity({
  bootId: "boot-exact",
  pid: 4242,
  port: 7878,
  canonicalProject: "\\\\?\\C:\\Game",
}), {
  expectedBootId: "boot-exact",
  expectedPid: 4242,
  expectedPort: 7878,
  expectedCanonicalProject: "\\\\?\\C:\\Game",
});
assert.deepEqual(exactLifecycleCloseIdentity({
  daemonBootId: "widget-boot",
  daemonPid: 4343,
  daemonPort: 7879,
  daemonCanonicalProject: "\\\\?\\C:\\WidgetGame",
}), {
  expectedBootId: "widget-boot",
  expectedPid: 4343,
  expectedPort: 7879,
  expectedCanonicalProject: "\\\\?\\C:\\WidgetGame",
});
for (const field of ["bootId", "pid", "port", "canonicalProject"]) {
  assert.equal(
    exactLifecycleCloseIdentity({
      bootId: "boot-exact",
      pid: 4242,
      port: 7878,
      canonicalProject: "\\\\?\\C:\\Game",
      [field]: null,
    }),
    null,
    `destructive lifecycle identity requires ${field}`,
  );
}
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
const tauriConfig = JSON.parse(
  fs.readFileSync(new URL("../desktop/src-tauri/tauri.conf.json", import.meta.url), "utf8"),
);

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
  /const EXIT_CLOSE_TOTAL_TIMEOUT:[\s\S]*?const MAX_EXIT_CLOSE_WORKERS: usize = 4;/,
  "native exit cleanup must have one shared deadline and a fixed worker limit",
);
const closeAllSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("fn close_managed_daemons"),
  tauriDaemonSource.indexOf("fn close_exact_managed_daemon"),
);
assert.match(
  closeAllSource,
  /run_bounded_exit_workers\(claims\.len\(\), deadline,[\s\S]*?request_exact_managed_daemon_close[\s\S]*?run_bounded_exit_workers\(claims\.len\(\), deadline,[\s\S]*?wait_for_exact_managed_daemon_release/,
  "all project claims must use the bounded request and release-confirmation phases",
);

const closeRequestSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("fn request_exact_managed_daemon_close"),
  tauriDaemonSource.indexOf("fn wait_for_exact_managed_daemon_release"),
);
assert.match(
  closeRequestSource,
  /local_json_request_until\(claim\.port, "GET", "\/hello", &\[\], deadline\)[\s\S]*?managedBy[\s\S]*?claim\.project[\s\S]*?claim\.boot_id[\s\S]*?claim\.pid[\s\S]*?claim\.port[\s\S]*?"token": claim\.owner_token[\s\S]*?"expectedBootId": claim\.boot_id[\s\S]*?"expectedPid": claim\.pid[\s\S]*?"expectedPort": claim\.port[\s\S]*?"expectedCanonicalProject": claim\.project[\s\S]*?local_json_request_until\(claim\.port, "POST", "\/manager-close", &body, deadline\)/,
  "native cleanup must pin project, boot, PID, and port in the destructive request",
);

const boundedWorkersSource = tauriDaemonSource.slice(
  tauriDaemonSource.indexOf("fn run_bounded_exit_workers"),
  tauriDaemonSource.indexOf("#[derive(Default)]\npub(crate) struct LifecycleChildren"),
);
for (const required of [
  "AtomicUsize::new(0)",
  "item_count.min(MAX_EXIT_CLOSE_WORKERS)",
  "spawn_scoped",
  "Instant::now() >= deadline",
]) {
  assert.equal(
    boundedWorkersSource.includes(required),
    true,
    `native exit worker pool must include ${required}`,
  );
}

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
assert.match(
  tauriEnsureSource,
  /preferred_port_collision\(&error, port\)[\s\S]*?continue;[\s\S]*?return if let Some\(preferred_error\)/,
  "Desktop may retry without the preferred port only for an identified port collision",
);
assert.match(
  tauriDaemonSource,
  /fn preferred_port_collision[\s\S]*?already serving[\s\S]*?requested port[\s\S]*?serve: bind 127\.0\.0\.1/,
  "preferred-port fallback must be restricted to explicit listener collision diagnostics",
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
assert.match(
  tauriDaemonSource,
  /fn ensure_supervisor[\s\S]*?MANAGED_DAEMON_HEARTBEAT_INTERVAL[\s\S]*?heartbeat_exact_managed_daemon/,
  "native Desktop ownership must supervise heartbeats independently of the renderer",
);
assert.match(
  tauriDaemonSource,
  /fn heartbeat_exact_managed_daemon[\s\S]*?GET", "\/hello"[\s\S]*?managedBy[\s\S]*?claim\.boot_id[\s\S]*?"\/manager-heartbeat"/,
  "native heartbeats must revalidate the exact Desktop daemon before using its capability",
);

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
  /if \(IS_DESKTOP_HOST\) return;[\s\S]*?exactLifecycleCloseIdentity\(app\.state\)[\s\S]*?"\/widget-close"[\s\S]*?identity/,
  "Terminal64 teardown must pin widget-close while Desktop defers cleanup to native RunEvent::Exit",
);
assert.equal(closeSource.includes("/manager-close"), false, "renderer teardown must never stop Desktop daemons");
assert.match(
  appSource,
  /if \(!IS_DESKTOP_HOST\) \{[\s\S]*?addEventListener\("pagehide"[\s\S]*?addEventListener\("beforeunload"/,
  "page lifecycle close handlers must remain Terminal 64-only",
);
const terminalStopSource = appSource.slice(
  appSource.indexOf("async function killDaemon"),
  appSource.indexOf("// ---------- Health loop ----------"),
);
assert.match(
  terminalStopSource,
  /trackedSessionStillOwnsProcess[\s\S]*?exactLifecycleCloseIdentity\(registeredStop\)[\s\S]*?"\/manager-close"[\s\S]*?identity[\s\S]*?waitForPortRelease/,
  "explicit Terminal64 stop must pin its authenticated identity in manager-close",
);
assert.equal(
  terminalStopSource.includes('"/widget-close"'),
  false,
  "explicit stop must not use the page-teardown endpoint that keeps connected Studio daemons alive",
);
const pidLifecycleSource = appSource.slice(
  appSource.indexOf("const PID_STATUS_ALIVE"),
  appSource.indexOf("async function upsertSession"),
);
assert.match(
  pidLifecycleSource,
  /PID_STATUS_ALIVE[\s\S]*?PID_STATUS_DEAD[\s\S]*?PID_STATUS_UNKNOWN/,
  "Terminal64 PID probing must preserve a distinct unknown/error result",
);
assert.match(
  pidLifecycleSource,
  /catch \(error\)[\s\S]*?return PID_STATUS_UNKNOWN/,
  "host or PowerShell PID-probe failures must remain unknown rather than being treated as dead",
);
assert.match(
  pidLifecycleSource,
  /status !== PID_STATUS_DEAD\) retained\.push\(s\)/,
  "session pruning must retain both live and ambiguously probed daemon sessions",
);
assert.match(
  pidLifecycleSource,
  /status === PID_STATUS_DEAD[\s\S]*?daemonPid: null,[\s\S]*?daemonCanonicalProject: null,[\s\S]*?daemonBootId: null,[\s\S]*?daemonOwnerToken: null/,
  "confirmed daemon death must rotate the old boot's lifecycle capability and identity",
);
const removeSessionSource = appSource.slice(
  appSource.indexOf("async function removeSession"),
  appSource.indexOf("async function inspectTrackedSessionOwnership"),
);
assert.match(
  removeSessionSource,
  /const matches = \([\s\S]*?!hasPid[\s\S]*?&& \(!hasPort[\s\S]*?&& \(!hasBootId[\s\S]*?return !matches/,
  "exact session cleanup must match every supplied PID, port, and boot field conjunctively",
);
assert.match(
  appSource,
  /removeSession\(\{ pid: registeredStop\.pid, port, bootId: registeredStop\.bootId \}\)/,
  "successful explicit shutdown must remove only the exact stopped boot",
);
assert.match(
  pidLifecycleSource,
  /if \(status === PID_STATUS_DEAD\) \{[\s\S]*?setState\(\{[\s\S]*?daemonPid: null/,
  "persisted daemon state may be cleared only after a confirmed-dead PID result",
);
const trackedStopSource = appSource.slice(
  appSource.indexOf("async function stopTrackedSession"),
  appSource.indexOf("async function stopDuplicateTrackedSessions"),
);
assert(
  trackedStopSource.indexOf("inspectTrackedSessionOwnership(session)")
    < trackedStopSource.indexOf("pidStatus(session.pid)"),
  "tracked cleanup must authenticate exact daemon identity before consulting PID liveness",
);
assert.match(
  trackedStopSource,
  /status === PID_STATUS_UNKNOWN[\s\S]*?preserving session for a later retry[\s\S]*?return false/,
  "a PID probe failure must preserve the tracked session for later recovery",
);
assert.match(
  trackedStopSource,
  /ownership\.status === "identity-mismatch"[\s\S]*?removeSession[\s\S]*?tracked daemon ownership could not be re-authenticated; preserving session/,
  "a live mismatched PID may be discarded, but transient hello/authentication failures must remain tracked",
);
const duplicateStopSource = appSource.slice(
  appSource.indexOf("async function stopDuplicateTrackedSessions"),
  appSource.indexOf("async function probePort"),
);
assert.equal(
  duplicateStopSource.includes("saveSessions("),
  false,
  "duplicate cleanup must not bulk-discard sessions whose authenticated stop or PID probe was inconclusive",
);
const sessionRegistrySource = appSource.slice(
  appSource.indexOf("async function loadSessions"),
  appSource.indexOf("async function trackedSessionStillOwnsProcess"),
);
assert.match(
  sessionRegistrySource,
  /parseInt\(session\.port[\s\S]*?parseInt\(session\.pid/,
  "persisted legacy string PID/port values must be normalized when sessions load",
);
assert.match(
  sessionRegistrySource,
  /parseInt\(s\.port, 10\) === entryPort[\s\S]*?parseInt\(s\.pid, 10\) === entryPid/,
  "session upsert must deduplicate numeric and legacy string identities",
);
const portReleaseSource = appSource.slice(
  appSource.indexOf("async function waitForPortRelease"),
  appSource.indexOf("function makeOwnerToken"),
);
assert.match(
  portReleaseSource,
  /await probePort\(port\)[\s\S]*?consecutiveHelloMisses >= 2[\s\S]*?getPortOwner\(port\)/,
  "port-release polling must use cheap daemon probes before an OS owner check",
);
assert.equal(
  (portReleaseSource.match(/getPortOwner\(port\)/g) || []).length,
  2,
  "port-release polling must not spawn an OS process on every retry",
);
assert.match(
  portReleaseSource,
  /expectedPid[\s\S]*?pidStatus\(expectedPid\) === PID_STATUS_DEAD/,
  "an empty/failed OS owner probe must not discard tracking until the exact prior PID is confirmed dead",
);
const terminalLaunchSource = appSource.slice(
  appSource.indexOf("async function launchAndWait"),
  appSource.indexOf("let ensureDaemonPromise"),
);
assert.match(
  terminalLaunchSource,
  /helloPid[\s\S]*?=== launchedPid/,
  "a daemon launch must reject a different listener that wins the preferred-port race",
);
const terminalEnsureSource = appSource.slice(
  appSource.indexOf("async function ensureDaemonInner"),
  appSource.indexOf("async function killDaemon"),
);
assert.match(
  terminalEnsureSource,
  /await loadSessions\(\)[\s\S]*?isOwnDaemon\(hit\.info, project, trackedSessions\)/,
  "Terminal64 reload must use exact persisted canonical daemon identity when the UI path is an alias",
);
assert.match(
  terminalEnsureSource,
  /previous project still connected[\s\S]*?launchAndWait\(project, preferred\)[\s\S]*?getPortOwner\(preferred\)[\s\S]*?scanFallbackPorts\(project, preferred\)/,
  "a project switch must scan fallback ports when an unrelated listener still owns the preferred port",
);
const ownershipMatchSource = appSource.slice(
  appSource.indexOf("function isOwnDaemon"),
  appSource.indexOf("function isWidgetOwnedDaemon"),
);
assert.equal(
  ownershipMatchSource.includes("gameId"),
  false,
  "a Roblox game ID alone must never equate two different local sync roots",
);
const fallbackScanSource = appSource.slice(
  appSource.indexOf("async function scanFallbackPorts"),
  appSource.indexOf("// PowerShell serializes errors"),
);
assert.match(
  fallbackScanSource,
  /const trackedSessions = await loadSessions\(\)[\s\S]*?isOwnDaemon\(occ\.info, project, trackedSessions\)/,
  "fallback-port scans must retain exact canonical session identity for Windows aliases",
);
const ensureDesktopSource = appSource.slice(
  appSource.indexOf("async function ensureDesktopDaemon"),
  appSource.indexOf("async function ensureDaemonInner"),
);
assert.match(
  ensureDesktopSource,
  /await setDesktopDaemonSession\(projectId,[\s\S]*?emit\("daemon:up"/,
  "Desktop must durably persist an exact daemon session before announcing it as up",
);
assert.equal(
  tauriConfig.app.windows[0].backgroundThrottling,
  "disabled",
  "Desktop WebView background throttling must be disabled as a renderer liveness mitigation",
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
