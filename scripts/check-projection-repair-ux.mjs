import assert from "node:assert/strict";
import fs from "node:fs";
import {
  buildProjectionLineDiff,
  formatFileBytes,
  markerStyleLabel,
  normalizeProjectionReport,
  shortHash,
} from "../views/projection-repair.js";

const report = normalizeProjectionReport({
  ok: true,
  project: "/tmp/Race Stars",
  conflicts: [{
    id: "projection_abc123",
    kind: "multiple-init-markers",
    directory: "ReplicatedStorage/Packages/_Index/example/pkg",
    files: [{
      name: "init (pkg).luau",
      style: "named",
      className: "ModuleScript",
      size: 21,
      sha256: "a".repeat(64),
      preview: "return <script>alert(1)</script>\n",
      previewTruncated: false,
      utf8: true,
    }, {
      name: "init.luau",
      style: "plain",
      className: "ModuleScript",
      size: 12,
      sha256: "b".repeat(64),
      preview: "return true\n",
      previewTruncated: false,
      utf8: true,
    }],
    identical: false,
  }],
  remaining: 1,
  totalConflicts: 1,
  countsKnown: true,
  truncated: false,
});

assert.equal(report.ok, true);
assert.equal(report.conflicts.length, 1);
assert.equal(report.conflicts[0].files[0].preview.includes("<script>"), true);
assert.equal(markerStyleLabel("named"), "Ro Sync named marker");
assert.equal(markerStyleLabel("plain"), "Package / Rojo marker");
assert.equal(formatFileBytes(1536), "1.5 KB");
assert.equal(shortHash("abcdef0123456789"), "abcdef0123");

const malformed = normalizeProjectionReport(null);
assert.equal(malformed.ok, false);
assert.equal(malformed.code, "MALFORMED_PROJECTION_REPORT");
assert.equal(normalizeProjectionReport({ conflicts: [] }).ok, false);
assert.equal(normalizeProjectionReport({
  ok: true,
  conflicts: [],
  remaining: 5,
  totalConflicts: 5,
  countsKnown: true,
  truncated: true,
}).ok, false);
assert.equal(normalizeProjectionReport({
  ok: true,
  conflicts: report.conflicts,
  remaining: 0,
  totalConflicts: 0,
  countsKnown: true,
  truncated: false,
}).ok, false);
assert.equal(normalizeProjectionReport({
  ok: true,
  conflicts: [],
  remaining: 0,
  totalConflicts: 0,
  countsKnown: false,
  truncated: false,
}).ok, false);

const tooManyFiles = normalizeProjectionReport({
  ok: true,
  conflicts: [{
    id: "many",
    kind: "multiple-init-markers",
    directory: "ReplicatedStorage/Many",
    files: Array.from({ length: 33 }, (_, index) => ({
      name: `init (${index}).luau`,
      sha256: String(index).padStart(64, "0"),
      preview: "return true",
    })),
  }],
  remaining: 1,
  totalConflicts: 1,
  countsKnown: true,
  truncated: false,
});
assert.equal(tooManyFiles.conflicts[0].files.length, 32);
assert.equal(tooManyFiles.conflicts[0].filesTruncated, true);

const recovery = normalizeProjectionReport({
  ok: false,
  code: "PROJECTION_RECOVERY_REQUIRED",
  error: "transaction proof failed",
  conflicts: [],
  resolution: {
    id: "recovery_abc123",
    receiptPath: ".rosync-backups/projection-repair/receipt.json",
    receiptAvailable: true,
    recoveryRequired: true,
    recoveryError: "kept the prepared receipt",
    recoveryActions: ["resume", "resume", "not-supported"],
  },
});
assert.equal(recovery.ok, false);
assert.equal(recovery.resolution.recoveryRequired, true);
assert.equal(recovery.resolution.receiptAvailable, true);
assert.deepEqual(recovery.resolution.recoveryActions, ["resume"]);
assert.equal(
  recovery.resolution.receiptPath,
  ".rosync-backups/projection-repair/receipt.json",
);

const diff = buildProjectionLineDiff(
  "local x = 1\nreturn x\n",
  "local x = 2\nreturn x\n",
);
assert.equal(diff.approximate, false);
assert(diff.rows.some((row) => row.left?.kind === "remove"));
assert(diff.rows.some((row) => row.right?.kind === "add"));
assert(diff.rows.some((row) =>
  row.left?.kind === "same" && row.left.text === "return x"
));

const projectsSource = fs.readFileSync(new URL("../views/projects.js", import.meta.url), "utf8");
const appSource = fs.readFileSync(new URL("../app.js", import.meta.url), "utf8");
const bridgeSource = fs.readFileSync(new URL("../bridge.js", import.meta.url), "utf8");
const repairSource = fs.readFileSync(new URL("../views/projection-repair.js", import.meta.url), "utf8");
const styleSource = fs.readFileSync(new URL("../style.css", import.meta.url), "utf8");
assert.match(
  projectsSource,
  /escapeHTML\(line\.text \|\| " "\)/,
  "source previews must be HTML escaped",
);
assert.match(
  projectsSource,
  /projectionInspectSequence[\s\S]*?projectionInspect[\s\S]*?if \(projectionInspectSequence\.get\(project\.id\) !== sequence\) return/,
  "superseded inspections must not replace current repair state",
);
assert.match(
  projectsSource,
  /joinProjectFile\(project\.path, "\.rosync-backups"\)/,
  "Open Backup must use the fixed project-local recovery root",
);
assert.doesNotMatch(
  projectsSource,
  /openFolder\([^)]*backupPaths/,
  "returned backup paths must not become native open targets",
);
assert.match(
  projectsSource,
  /projectionResolve[\s\S]*?report\.countsKnown[\s\S]*?!report\.truncated[\s\S]*?report\.conflicts\.length === 0[\s\S]*?report\.remaining === 0[\s\S]*?report\.totalConflicts === 0[\s\S]*?serve\(project\.id, \{ restart: true \}\)/,
  "the daemon should retry only after the offline blocker list is empty",
);
assert.match(
  projectsSource,
  /report\.resolution\?\.recoveryRequired[\s\S]*?status: "recovery"/,
  "uncertain filesystem mutations and inspections must become an explicit recovery state",
);
assert.match(
  projectsSource,
  /Ro Sync will not retry the daemon from this state/,
  "recovery UI must make the no-auto-retry behavior explicit",
);
assert.match(
  projectsSource,
  /Resume replays the already-recorded decision/,
  "recovery UI must explain verified replay semantics",
);
assert.match(
  projectsSource,
  /data-repair-act="resume-recovery"/,
  "the UI must expose replay when advertised by the backend",
);
assert.match(
  projectsSource,
  /data-repair-act="quarantine-recovery"[\s\S]*?confirmQuarantine/,
  "quarantine must require an explicit confirmation state",
);
assert.match(
  projectsSource,
  /data-repair-act="confirm-quarantine"/,
  "the UI must expose replay and two-step quarantine actions advertised by the backend",
);
assert.match(
  projectsSource,
  /resolveProjectionConflict\(project, failure, \{ id: recoveryId \}, "quarantine"\)/,
  "quarantine must use the opaque recovery id through the narrow resolver seam",
);
assert.match(
  projectsSource,
  /receiptAvailable[\s\S]*?Unavailable — inspect the backup root and conflicting folder manually/,
  "recovery UI must not claim an unproven receipt path is available",
);
assert.match(
  projectsSource,
  /failureKey: projectionFailureKey\(failure\)|const failureKey = projectionFailureKey\(failure\)[\s\S]*?failureKey,/,
  "inspection state must stay bound to the startup failure it was opened for",
);
assert.match(
  projectsSource,
  /projectionRepairLocksDaemon\(id\)[\s\S]*?before starting the daemon/,
  "daemon startup must be guarded while a repair transaction or recovery receipt is active",
);
assert.match(
  projectsSource,
  /const projectionRepairByProject = new Map\(\)[\s\S]*?export function mountProjects/,
  "repair and recovery state must survive Projects view navigation",
);
assert.match(
  projectsSource,
  /publishProjectionRepairState[\s\S]*?projection:repair-state[\s\S]*?offProjectionRepair/,
  "a replacement Projects mount must be notified when an older mount's repair command finishes",
);
assert.match(
  projectsSource,
  /suspendProjectForRepair\(project\.id\)[\s\S]*?projectionInspect\(project\.path\)/,
  "automatic daemon retries must be persistently suspended before offline inspection",
);
assert.match(
  appSource,
  /function suspendProjectForRepair[\s\S]*?servedProjectIds[\s\S]*?filter\(\(id\) => id !== projectId\)[\s\S]*?suspendProjectForRepair,/,
  "the app-level supervisor API must remove a repairing project from desired serving state",
);
assert.match(
  appSource,
  /async function ensureDaemonInner[\s\S]*?activeId && isProjectServed\(activeId\)[\s\S]*?\? activeProjectPath\(\)[\s\S]*?: null/,
  "Terminal 64 bootstrap must honor desired serving state after repair suspension",
);
assert.match(
  projectsSource,
  /projectionRecoveryRequiredProjects\.add[\s\S]*?PROJECTION_RESULT_UNVERIFIED/,
  "a lost host result must fail closed as an unverified recovery state",
);
assert.match(
  projectsSource,
  /currentState\.status === "recovery"[\s\S]*?status: preserveRecovery \? "recovery" : "ready"/,
  "a failed recovery action must preserve the actionable recovery panel",
);
assert.match(
  repairSource,
  /report\.ok === true[\s\S]*?completeShape/,
  "successful reports must be explicit and have internally consistent completeness metadata",
);
assert.match(
  bridgeSource,
  /projectionInspect[\s\S]*?projectionResolve/,
  "both hosts must expose the narrow offline repair seam",
);
assert.doesNotMatch(
  repairSource,
  /(?:from|import)\s*\(?\s*["']https?:\/\/|cdn\./,
  "offline comparison must not depend on a network diff library",
);
assert.match(
  styleSource,
  /\.projection-stepper[\s\S]*?grid-template-columns[\s\S]*?max-height:[\s\S]*?\.projection-stepper button[\s\S]*?min-height:\s*40px/,
  "large blocker sets must scroll instead of clipping and keep 40px targets",
);

console.log("projection repair UX checks passed");
