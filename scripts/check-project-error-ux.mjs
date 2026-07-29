import assert from "node:assert/strict";
import fs from "node:fs";
import {
  collapseDaemonDiagnostic,
  conflictDeletePaths,
  formatProjectFailure,
} from "../views/project-error.js";

const repeatedInitConflict = [
  "Error: daemon start: child 1 exited before handshake",
  "rosync listening on http://127.0.0.1:7878 (project: /tmp/Race Stars)",
  "rosync listening on http://127.0.0.1:7878 (project: /tmp/Race Stars)",
  'Error: serve: validate watched filesystem: scan /tmp/Race Stars/ReplicatedStorage/Packages/_Index/alicesaidhi_vide@0.4.0/vide: multiple init source markers in /tmp/Race Stars/ReplicatedStorage/Packages/_Index/alicesaidhi_vide@0.4.0/vide: "init (vide).luau" and "init.luau"',
  'Error: serve: validate watched filesystem: scan /tmp/Race Stars/ReplicatedStorage/Packages/_Index/alicesaidhi_vide@0.4.0/vide: multiple init source markers in /tmp/Race Stars/ReplicatedStorage/Packages/_Index/alicesaidhi_vide@0.4.0/vide: "init (vide).luau" and "init.luau"',
].join(" ");

const collapsed = collapseDaemonDiagnostic(repeatedInitConflict);
assert.equal(collapsed.match(/rosync listening/g)?.length, 1);
assert.equal(collapsed.match(/Error: serve/g)?.length, 1);

const conflict = formatProjectFailure(repeatedInitConflict, "/tmp/Race Stars");
assert.equal(conflict.code, "multiple-init-markers");
assert.equal(conflict.statusLabel, "File conflict");
assert.deepEqual(conflict.files, ["init (vide).luau", "init.luau"]);
assert.equal(conflict.path.endsWith("/vide"), true);
assert.deepEqual(conflict.deletePaths, [
  "/tmp/Race Stars/ReplicatedStorage/Packages/_Index/alicesaidhi_vide@0.4.0/vide/init (vide).luau",
  "/tmp/Race Stars/ReplicatedStorage/Packages/_Index/alicesaidhi_vide@0.4.0/vide/init.luau",
]);
assert.match(conflict.guidance, /Delete permanently removes both files/);

const namedClassConflict = formatProjectFailure(
  'multiple init source markers in /tmp/Controller: "init (Controller).server.luau" and "init (Controller).client.luau"',
);
assert.equal(namedClassConflict.deletePaths.length, 0);
assert.match(namedClassConflict.guidance, /Delete is unavailable/);

assert.deepEqual(
  conflictDeletePaths("/tmp/Game", "/tmp/Game/ReplicatedStorage/Pkg", [
    "init (Pkg).luau",
    "init.luau",
  ]),
  [
    "/tmp/Game/ReplicatedStorage/Pkg/init (Pkg).luau",
    "/tmp/Game/ReplicatedStorage/Pkg/init.luau",
  ],
);
assert.deepEqual(
  conflictDeletePaths("/tmp/Game", "/tmp/Other", ["init.luau"]),
  [],
  "diagnostics outside the selected project must never become delete targets",
);
assert.deepEqual(
  conflictDeletePaths("/tmp/Game", "/tmp/Game", ["../../state.json"]),
  [],
  "non-marker filenames must never become delete targets",
);

const legacyLeaf = formatProjectFailure(
  "Error: legacy leaf script ReplicatedStorage/Client/UIController/Misc/init (Notifications).luau uses the reserved init-marker filename grammar; rename it to ReplicatedStorage/Client/UIController/Misc/%69nit (Notifications).luau before syncing",
  "/tmp/Race Stars",
);
assert.equal(legacyLeaf.code, "legacy-init-leaf");
assert.equal(legacyLeaf.statusLabel, "Rename required");
assert.deepEqual(legacyLeaf.files, [
  "init (Notifications).luau",
  "%69nit (Notifications).luau",
]);
assert.equal(
  legacyLeaf.path,
  "/tmp/Race Stars/ReplicatedStorage/Client/UIController/Misc",
);
assert.match(legacyLeaf.guidance, /script name in Studio will stay the same/);

assert.equal(
  formatProjectFailure("Error: address already in use on port 7878").code,
  "port-unavailable",
);
assert.equal(
  formatProjectFailure("TypeError: Failed to fetch").code,
  "daemon-unreachable",
);
assert.equal(
  formatProjectFailure("Error: permission denied reading /tmp/project").code,
  "permission-denied",
);
assert.match(
  formatProjectFailure("Error: bind socket: operation not permitted").title,
  /operating system denied/,
);
assert.equal(
  formatProjectFailure("unexpected child exit").code,
  "daemon-start-failed",
);

const appSource = fs.readFileSync(new URL("../app.js", import.meta.url), "utf8");
const projectsSource = fs.readFileSync(new URL("../views/projects.js", import.meta.url), "utf8");
assert.match(
  appSource,
  /daemonFailureByProject[\s\S]*?getDaemonFailure[\s\S]*?event\.error/,
  "daemon failures must survive disposable view mounts",
);
assert.match(
  appSource,
  /attemptedFreePortLaunch[\s\S]*?fallbackLaunchError[\s\S]*?All ports/,
  "fallback scanning must preserve launch failures instead of misreporting every port as busy",
);
assert.match(
  projectsSource,
  /api\.getDaemonFailure[\s\S]*?api\.reportDaemonFailure/,
  "Projects must retain and recover the full startup diagnostic after navigation",
);
assert.match(
  projectsSource,
  /announcedFailureByProject[\s\S]*?openFailureDetailsByProject[\s\S]*?data-error-act="details"/,
  "error details must preserve interaction state without repeating assertive announcements",
);
assert.match(
  projectsSource,
  /async function refreshStatuses\(\) \{\s*if \(disposed\) return;[\s\S]*?function ensureActivityStream\(\) \{\s*if \(disposed\) \{\s*closeActivityStream\(\);/,
  "disposed project views must not reopen activity sockets after an in-flight refresh",
);
assert.match(
  projectsSource,
  /if \(t === "shutdown"\) \{[\s\S]*?pushActivityFrame\(data\);[\s\S]*?handleRuntimeShutdown\(data, activityProjectId\);[\s\S]*?return;/,
  "terminal runtime shutdowns must remain observable in the activity stream",
);
assert.match(
  projectsSource,
  /function handleRuntimeShutdown[\s\S]*?frame\.retryable === false[\s\S]*?terminalFailureProjects\.add[\s\S]*?api\.reportDaemonFailure/,
  "terminal runtime shutdowns must persist their exact diagnostic",
);
assert.match(
  projectsSource,
  /open: \(\) => \{[\s\S]*?void refreshStatuses\(\)[\s\S]*?frame\.retryable === true[\s\S]*?Reconnecting/,
  "retryable runtime shutdowns must retain the reconnecting stream and refresh authoritative status",
);
assert.match(
  projectsSource,
  /open: \(\) => \{[\s\S]*?clearActivityStreamErrors\(\)[\s\S]*?function clearActivityStreamErrors\(\)[\s\S]*?entry\?\.frame\?\.type === "stream-error"/,
  "a recovered observer stream must remove its stale interruption cards",
);
assert.match(
  appSource,
  /t === "shutdown"[\s\S]*?data\.retryable === false[\s\S]*?reportDaemonFailure[\s\S]*?emit\("plugin:shutdown"/,
  "app-level streams must retain terminal plugin failures while Projects is unmounted",
);
const appShutdownBranch = appSource.slice(
  appSource.indexOf('if (t === "shutdown")'),
  appSource.indexOf("// Transport-only frames"),
);
assert.doesNotMatch(
  appShutdownBranch,
  /stream\.close|appStreams\.delete/,
  "terminal plugin shutdowns must not blind the UI watcher to later recovery",
);
assert.match(projectsSource, /data-error-act="fix"[\s\S]*?data-error-act="delete"/);
assert.match(
  projectsSource,
  /const stopped = await cancelServing\(p\.id\)[\s\S]*?api\.host\.deleteProjectFiles[\s\S]*?await serve\(p\.id\)/,
  "Delete must stop serving, remove only the verified conflict files, then retry",
);
assert.match(
  projectsSource,
  /const fixConflict = async[\s\S]*?cancelServing\(p\.id\)[\s\S]*?openFolder/,
  "Fix must stop serving, leave files untouched, and open their folder",
);

console.log("project error UX checks passed");
