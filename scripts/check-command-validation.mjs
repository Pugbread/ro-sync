#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const commandDocs = JSON.parse(
  fs.readFileSync(path.join(repoRoot, "docs/client-commands.generated.json"), "utf8"),
);

const argv = process.argv.slice(2);
const option = (name, fallback) => {
  const index = argv.indexOf(name);
  return index === -1 ? fallback : argv[index + 1];
};
const runLive = argv.includes("--live");
const classifyOnly = argv.includes("--classify");
const jsonOutput = argv.includes("--json");
const project = option("--project", process.env.ROSYNC_AUDIT_PROJECT);
const reportOutput = option("--output", null);
const binary = path.resolve(
  option("--binary", path.join(repoRoot, "daemon/target/debug/rosync")),
);
const timeoutMs = Number(option("--timeout-ms", "30000"));

if (runLive === classifyOnly) {
  throw new Error("choose exactly one mode: --classify (no execution) or --live");
}
if (runLive && !project) {
  throw new Error("--live requires --project or ROSYNC_AUDIT_PROJECT");
}
if (runLive && !fs.existsSync(binary)) {
  throw new Error(`Ro Sync binary does not exist: ${binary}`);
}

function canonicalLeafPaths() {
  const paths = [];
  for (const command of commandDocs.commands) {
    const direct = command.subcommands ?? [];
    const descendants = command.subcommandPaths ?? [];
    if (direct.length === 0 && descendants.length === 0) {
      paths.push(command.name);
      continue;
    }
    if (descendants.length > 0) {
      const directGroups = new Set(
        descendants
          .filter((entry) => entry.includes(" "))
          .map((entry) => entry.split(" ")[0]),
      );
      for (const subcommand of direct) {
        if (!directGroups.has(subcommand)) {
          paths.push(`${command.name} ${subcommand}`);
        }
      }
      for (const descendant of descendants) {
        if (!directGroups.has(descendant)) {
          paths.push(`${command.name} ${descendant}`);
        }
      }
      continue;
    }
    for (const subcommand of direct) {
      paths.push(`${command.name} ${subcommand}`);
    }
  }
  // These are intentional hidden backwards-compatibility commands and are
  // therefore absent from the public generated command registry.
  paths.push("img", "imgs");
  return paths.sort();
}

const canonicalPaths = canonicalLeafPaths();
assert.equal(canonicalPaths.length, 106, "unexpected canonical executable command count");
assert.equal(new Set(canonicalPaths).size, canonicalPaths.length, "duplicate canonical command");

const COMMAND_ALIASES = new Map([["decide", "decision"]]);
for (const [alias, canonical] of [
  ["gamepasses", "gamepass"],
  ["gp", "gamepass"],
  ["pass", "gamepass"],
  ["products", "product"],
  ["dp", "product"],
  ["devproduct", "product"],
]) {
  for (const action of ["discover", "list", "create", "edit", "image", "images"]) {
    COMMAND_ALIASES.set(
      `monetization ${alias} ${action}`,
      `monetization ${canonical} ${action}`,
    );
  }
}
assert.equal(COMMAND_ALIASES.size, 37, "unexpected command alias count");

const LIVE_CASES = new Map([
  ["daemon status", ({ project }) => ["daemon", "status", "--project", project, "--raw"]],
  [
    "daemon logs",
    ({ project }) => ["daemon", "logs", "--project", project, "--lines", "2", "--raw"],
  ],
  ["context", ({ project }) => ["context", "--project", project]],
  ["capabilities", ({ project }) => ["capabilities", "--project", project, "--raw"]],
  ["capture status", ({ project }) => ["capture", "status", "--project", project, "--raw"]],
  ["playtest status", ({ project }) => ["playtest", "status", "--project", project, "--raw"]],
  [
    "playtest contexts",
    ({ project }) => ["playtest", "contexts", "--project", project, "--raw"],
  ],
  [
    "run",
    ({ project, temp }) => {
      const workflow = path.join(temp, "read-only-workflow.json");
      fs.writeFileSync(
        workflow,
        `${JSON.stringify({
          version: 1,
          name: "command-audit-read-only",
          steps: [{ id: "camera", op: "get", path: "Workspace/Camera", property: "FieldOfView" }],
        })}\n`,
      );
      return ["run", "--file", workflow, "--project", project, "--raw"];
    },
  ],
  [
    "query",
    ({ project }) => [
      "query",
      "--project",
      project,
      "ReplicatedStorage/Shared/Market",
      "--format",
      "paths",
    ],
  ],
  [
    "path",
    ({ project }) => [
      "path",
      "--project",
      project,
      "--from",
      "studio",
      "ReplicatedStorage/Shared/Market",
      "--raw",
    ],
  ],
  [
    "lint",
    ({ project }) => [
      "lint",
      "--project",
      project,
      "--path",
      "ReplicatedStorage/Shared/Market.luau",
      "--scope-only",
      "--compile",
      "off",
      "--data-model",
      "loose",
      "--summary",
    ],
  ],
  [
    "monetization gamepass discover",
    ({ project }) => ["monetization", "gamepass", "discover", "--project", project, "--raw"],
  ],
  [
    "monetization product discover",
    ({ project }) => ["monetization", "product", "discover", "--project", project, "--raw"],
  ],
  [
    "get",
    ({ project }) => [
      "get",
      "--project",
      project,
      "--path",
      "Workspace/Camera",
      "--prop",
      "FieldOfView",
      "--raw",
    ],
  ],
  ["ls", ({ project }) => ["ls", "--project", project, "--path", "ReplicatedStorage", "--raw"]],
  [
    "tree",
    ({ project }) => [
      "tree",
      "--project",
      project,
      "--path",
      "ReplicatedStorage/Shared",
      "--depth",
      "1",
      "--raw",
    ],
  ],
  [
    "snapshot",
    ({ project, temp }) => [
      "snapshot",
      "--project",
      project,
      "--output",
      path.join(temp, "snapshot.json"),
      "--raw",
    ],
  ],
  ["diff", ({ project }) => ["diff", "--project", project, "--raw"]],
  ["changes", ({ project }) => ["changes", "--project", project, "--raw"]],
  ["services", ({ project }) => ["services", "--project", project, "--raw"]],
  [
    "meta",
    ({ project }) => [
      "meta",
      "--project",
      project,
      "--from",
      "studio",
      "ReplicatedStorage/Shared/Market",
      "--raw",
    ],
  ],
  [
    "props",
    ({ project }) => [
      "props",
      "--project",
      project,
      "--path",
      "Workspace/Camera",
      "--raw",
    ],
  ],
  [
    "source",
    ({ project }) => [
      "source",
      "--project",
      project,
      "--path",
      "ReplicatedStorage/Shared/Market",
      "--raw",
    ],
  ],
  [
    "where",
    ({ project }) => [
      "where",
      "--project",
      project,
      "Market",
      "--under",
      "ReplicatedStorage/Shared",
      "--raw",
    ],
  ],
  ["conflicts", ({ project }) => ["conflicts", "--project", project, "--raw"]],
  ["decision", ({ project }) => ["decision", "--project", project, "--raw"]],
  [
    "tail",
    ({ project }) => [
      "tail",
      "--project",
      project,
      "--since",
      "1s",
      "--limit",
      "2",
      "--raw",
    ],
  ],
  ["watch", ({ project }) => ["watch", "--project", project, "--compact"]],
  [
    "repair tree",
    ({ project }) => ["repair", "tree", "--project", project, "--depth", "128", "--raw"],
  ],
  [
    "repair sourcemap",
    ({ project, temp }) => [
      "repair",
      "sourcemap",
      "--project",
      project,
      "--output",
      path.join(temp, "sourcemap.json"),
      "--raw",
    ],
  ],
  [
    "find",
    ({ project }) => [
      "find",
      "--project",
      project,
      "--class",
      "ModuleScript",
      "--name",
      "Market",
      "--under",
      "ReplicatedStorage/Shared",
      "--raw",
    ],
  ],
  [
    "find-attr",
    ({ project }) => [
      "find-attr",
      "--project",
      project,
      "--name",
      "__RoSyncCommandAuditMissingAttribute",
      "--under",
      "Workspace",
      "--raw",
    ],
  ],
  [
    "classinfo",
    ({ project }) => ["classinfo", "--project", project, "--class", "BasePart", "--raw"],
  ],
  ["enums", ({ project }) => ["enums", "--project", project, "--raw"]],
  ["enum", ({ project }) => ["enum", "--project", project, "--name", "Material", "--raw"]],
  [
    "logs",
    ({ project }) => [
      "logs",
      "--project",
      project,
      "--since",
      "1s",
      "--limit",
      "2",
      "--raw",
    ],
  ],
  ["status", ({ project }) => ["status", "--project", project, "--raw"]],
  ["doctor", ({ project }) => ["doctor", "--project", project, "--raw"]],
  ["ping", ({ project }) => ["ping", "--project", project, "--raw"]],
  ["version", ({ project }) => ["version", "--project", project, "--raw"]],
  [
    "attr ls",
    ({ project }) => [
      "attr",
      "ls",
      "--project",
      project,
      "--path",
      "Workspace/Camera",
      "--raw",
    ],
  ],
  [
    "call",
    ({ project }) => [
      "call",
      "--project",
      project,
      "--path",
      "ReplicatedStorage",
      "--method",
      "FindFirstChild",
      "--args",
      '["Shared"]',
      "--raw",
    ],
  ],
  ["select get", ({ project }) => ["select", "get", "--project", project, "--raw"]],
]);

const ISOLATED_CASES = new Map([
  [
    "init",
    ({ temp }) => ["init", "--project", path.join(temp, "fixture-project"), "--name", "Audit", "--raw"],
  ],
  [
    "plugin install",
    ({ temp }) => [
      "plugin",
      "install",
      "--source",
      path.join(repoRoot, "plugin/Plugin.rbxm"),
      "--plugin-dir",
      path.join(temp, "plugins"),
      "--raw",
    ],
  ],
  [
    "plugin status",
    ({ temp }) => [
      "plugin",
      "status",
      "--source",
      path.join(repoRoot, "plugin/Plugin.rbxm"),
      "--plugin-dir",
      path.join(temp, "plugins"),
      "--raw",
    ],
  ],
  [
    "auth set",
    ({ temp }) => [
      "auth",
      "set",
      "--from-env",
      "ROSYNC_COMMAND_AUDIT_CREDENTIAL",
      "--data-dir",
      path.join(temp, "auth"),
      "--raw",
    ],
  ],
  [
    "auth status",
    ({ temp }) => ["auth", "status", "--data-dir", path.join(temp, "auth"), "--raw"],
  ],
  [
    "auth clear",
    ({ temp }) => ["auth", "clear", "--data-dir", path.join(temp, "auth"), "--raw"],
  ],
  ["commands", () => ["commands", "--compact"]],
  [
    "plan set",
    () => ["plan", "set", "--path", "Workspace/Camera", "--prop", "FieldOfView", "--value", "70"],
  ],
  [
    "plan new",
    () => ["plan", "new", "--path", "Workspace", "--class", "Folder", "--name", "Audit"],
  ],
  ["plan rm", () => ["plan", "rm", "--path", "Workspace/Audit"]],
  ["plan mv", () => ["plan", "mv", "--from", "Workspace/Audit", "--to", "Workspace"]],
  [
    "plan resolve",
    () => ["plan", "resolve", "--path", "ReplicatedStorage/Audit.luau", "--disk"],
  ],
  [
    "refresh",
    ({ temp }) => ["refresh", "--project", path.join(temp, "fixture-project"), "--raw"],
  ],
]);

const MOCK_INTEGRATION = new Map([
  [
    "playtest run",
    "daemon/tests/playtest_run_cli.rs exercises the real binary against a fake daemon (22 cases)",
  ],
  ["daemon start", "tier2_tests::daemon_start_* fake-runtime and capability tests"],
  ["capture authorize", "command_audit_cli routes the real binary to a fake permission provider"],
  ["playtest start", "command_audit_cli routes the real binary to a fake playtest controller"],
  ["playtest wait", "command_audit_cli routes the real binary to a fake playtest controller"],
  ["playtest exec", "command_audit_cli routes the real binary to a fake runtime context"],
  ["playtest logs", "command_audit_cli routes the real binary to a fake runtime context"],
  ["playtest ui", "command_audit_cli routes the real binary to a fake runtime context"],
  ["playtest input", "command_audit_cli routes the real binary to a fake runtime context"],
  ["playtest stop", "command_audit_cli routes the real binary to a fake playtest controller"],
  ["playtest request", "command_audit_cli routes the real binary to a fake runtime context"],
  ["set", "tier2_tests::set_parent_* plus fake-daemon command audit coverage"],
  ["new", "fake-daemon command audit coverage"],
  ["rm", "fake-daemon command audit coverage"],
  ["mv", "fake-daemon command audit coverage"],
  ["attr set", "fake-daemon command audit coverage"],
  ["attr rm", "fake-daemon command audit coverage"],
  ["tag add", "fake-daemon command audit coverage"],
  ["tag rm", "fake-daemon command audit coverage"],
  ["open", "fake-daemon command audit coverage"],
  ["resolve", "fake-daemon command audit coverage"],
  ["eval", "fake-daemon command audit coverage"],
  ["save", "fake-daemon command audit coverage"],
  ["waypoint", "fake-daemon command audit coverage"],
  ["undo", "fake-daemon command audit coverage"],
  ["redo", "fake-daemon command audit coverage"],
  ["select set", "fake-daemon command audit coverage"],
]);

const UNIT_ONLY = new Map([
  ["daemon stop", "direct lifecycle parser boundary test; no safe full stop execution in this audit"],
  ["daemon restart", "direct lifecycle parser boundary test; no safe full restart execution in this audit"],
  ["capture screen", "capture bounds/provider/artifact unit coverage; no live screen artifact requested"],
  ["capture photo", "photo framing/PNG/artifact tests; no live camera/UI manipulation requested"],
  ["capture scene", "scene framing validation tests; no live camera manipulation requested"],
  [
    "playtest capture",
    "real-binary fake-daemon request-builder and artifact-rejection test; no active live client",
  ],
  ["copy", "studio_clipboard content-addressed/private/limit tests; live clipboard untouched"],
  ["paste", "studio_clipboard content-addressed/private/limit tests; live clipboard untouched"],
  ["transmit", "transmit argument/name/artifact helpers only; no live EditableImage fixture"],
  ["serve", "serve/lifecycle/watch/http integration tests; no third daemon started"],
]);

const EXTERNAL_BLOCKED = new Map([
  ["upload", "creates an external Roblox asset and requires a credential"],
  ["img", "hidden legacy external Roblox asset upload"],
  ["imgs", "hidden legacy bulk external Roblox asset upload"],
  ["monetization gamepass list", "external Open Cloud read intentionally not sent"],
  ["monetization gamepass create", "creates an external game pass"],
  ["monetization gamepass edit", "mutates an external game pass"],
  ["monetization gamepass image", "uploads an external game-pass image"],
  ["monetization gamepass images", "bulk external game-pass image upload"],
  ["monetization product list", "external Open Cloud read intentionally not sent"],
  ["monetization product create", "creates an external developer product"],
  ["monetization product edit", "mutates an external developer product"],
  ["monetization product image", "uploads an external product image"],
  ["monetization product images", "bulk external product image upload"],
]);

const allCoverage = new Map();
for (const [kind, entries] of [
  ["live-read-only", LIVE_CASES],
  ["isolated-execution", ISOLATED_CASES],
  ["mock-integration", MOCK_INTEGRATION],
  ["unit-only", UNIT_ONLY],
  ["external-blocked", EXTERNAL_BLOCKED],
]) {
  for (const [command, detail] of entries) {
    assert(!allCoverage.has(command), `duplicate coverage for ${command}`);
    allCoverage.set(command, { kind, detail });
  }
}

assert.deepEqual(
  [...allCoverage.keys()].sort(),
  canonicalPaths,
  "command validation policy must classify every canonical executable command exactly once",
);

function execute(args, extraEnv = {}, commandTimeoutMs = timeoutMs) {
  const started = Date.now();
  const result = spawnSync(binary, args, {
    cwd: repoRoot,
    encoding: "utf8",
    timeout: commandTimeoutMs,
    maxBuffer: 16 * 1024 * 1024,
    env: { ...process.env, ...extraEnv },
  });
  return {
    args,
    durationMs: Date.now() - started,
    exitCode: result.status,
    signal: result.signal,
    error: result.error?.message ?? null,
    stdoutBytes: Buffer.byteLength(result.stdout ?? ""),
    stderrBytes: Buffer.byteLength(result.stderr ?? ""),
    stderrTail: (result.stderr ?? "").trim().split("\n").slice(-3).join("\n"),
    passed: result.status === 0,
  };
}

async function executeStreamProbe(args, options = {}) {
  const started = Date.now();
  return await new Promise((resolve) => {
    const child = spawn(binary, args, {
      cwd: repoRoot,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    let killRequested = false;
    let trigger = null;
    let forceKill = null;
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("exit", (code, signal) => {
      if (trigger) clearTimeout(trigger);
      if (forceKill) clearTimeout(forceKill);
      const concreteEvidence = options.requireOutput ? stdout.trim().length > 0 : true;
      resolve({
        args,
        durationMs: Date.now() - started,
        exitCode: code,
        signal,
        error: null,
        stdoutBytes: Buffer.byteLength(stdout),
        stderrBytes: Buffer.byteLength(stderr),
        stderrTail: stderr.trim().split("\n").slice(-3).join("\n"),
        passed:
          killRequested &&
          concreteEvidence &&
          stderr.trim().length === 0 &&
          (signal === "SIGTERM" || signal === "SIGKILL" || code === 0),
        detail: killRequested
          ? options.requireOutput
            ? `bounded stream probe observed ${Buffer.byteLength(stdout)} output byte(s) and awaited termination`
            : "bounded stream probe stayed connected and the harness awaited termination"
          : "stream exited before the bounded observer probe completed",
      });
    });
    if (options.triggerArgs) {
      trigger = setTimeout(() => {
        execute(options.triggerArgs);
      }, 350);
    }
    setTimeout(() => {
      if (child.exitCode !== null || child.signalCode !== null) return;
      killRequested = true;
      child.kill("SIGTERM");
      forceKill = setTimeout(() => child.kill("SIGKILL"), 2000);
    }, 1500);
  });
}

const temp = fs.mkdtempSync(path.join(os.tmpdir(), "rosync-command-audit-"));
const executionResults = new Map();
try {
  if (runLive) {
    const context = { project: path.resolve(project), temp };
    for (const [command, makeArgs] of ISOLATED_CASES) {
      executionResults.set(
        command,
        execute(makeArgs(context), {
          ROSYNC_COMMAND_AUDIT_CREDENTIAL: "audit-fixture-credential-not-for-network-use",
        }),
      );
    }
    for (const [command, makeArgs] of LIVE_CASES) {
      const args = makeArgs(context);
      executionResults.set(
        command,
        command === "watch"
          ? await executeStreamProbe(args, {
              requireOutput: true,
            })
          : command === "tail"
            ? await executeStreamProbe(args)
          : execute(args, {}, command === "snapshot" ? Math.max(timeoutMs, 120_000) : timeoutMs),
      );
    }
    // Exercise the only top-level alias and every nested group alias using
    // the non-networked discover action. Action aliases do not exist.
    executionResults.set(
      "alias:decide",
      execute(["decide", "--project", context.project, "--raw"]),
    );
    for (const alias of ["gamepasses", "gp", "pass", "products", "dp", "devproduct"]) {
      executionResults.set(
        `alias:monetization ${alias}`,
        execute(["monetization", alias, "discover", "--project", context.project, "--raw"]),
      );
    }
  }
} finally {
  fs.rmSync(temp, { recursive: true, force: true });
}

const rows = canonicalPaths.map((command) => {
  const coverage = allCoverage.get(command);
  const result = executionResults.get(command);
  return {
    command,
    aliases: [...COMMAND_ALIASES.entries()]
      .filter(([, canonical]) => canonical === command)
      .map(([alias]) => alias),
    strategy: coverage.kind,
    outcome: result ? (result.passed ? "passed" : "failed") : "not-run",
    evidence:
      typeof coverage.detail === "string"
        ? coverage.detail
        : result
          ? `actual CLI execution: rosync ${result.args.join(" ")}`
          : "run this harness with --live",
    result: result ?? undefined,
  };
});

const aliasRows = [...COMMAND_ALIASES.entries()].map(([alias, canonical]) => {
  const groupKey = alias === "decide" ? "alias:decide" : `alias:${alias.split(" ").slice(0, 2).join(" ")}`;
  const result = executionResults.get(groupKey);
  return {
    alias,
    canonical,
    outcome: result ? (result.passed ? "passed" : "failed") : "not-run",
    evidence: result
      ? alias === "decide"
        ? "actual read-only alias execution"
        : "the alias is attached to the shared monetization group; its discover action was executed"
      : "classification only; run the harness with --live to execute this alias",
    result: result ?? undefined,
  };
});

const report = {
  schema: "ro-sync.command-validation.v1",
  mode: runLive ? "live-and-isolated-execution" : "classification-only",
  generatedAt: new Date().toISOString(),
  project: runLive ? path.resolve(project) : null,
  binary,
  summary: {
    canonicalExecutableCommands: rows.length,
    commandAliases: aliasRows.length,
    passed: rows.filter((row) => row.outcome === "passed").length,
    failed: rows.filter((row) => row.outcome === "failed").length,
    notRun: rows.filter((row) => row.outcome === "not-run").length,
    liveReadOnly: rows.filter((row) => row.strategy === "live-read-only").length,
    isolatedExecution: rows.filter((row) => row.strategy === "isolated-execution").length,
    mockIntegration: rows.filter((row) => row.strategy === "mock-integration").length,
    unitOnly: rows.filter((row) => row.strategy === "unit-only").length,
    externalBlocked: rows.filter((row) => row.strategy === "external-blocked").length,
    aliasPassed: aliasRows.filter((row) => row.outcome === "passed").length,
    aliasFailed: aliasRows.filter((row) => row.outcome === "failed").length,
  },
  commands: rows,
  aliases: aliasRows,
};

if (runLive) {
  const missingExpectedExecution = rows.filter(
    (row) =>
      (row.strategy === "live-read-only" || row.strategy === "isolated-execution") &&
      row.outcome === "not-run",
  );
  assert.deepEqual(
    missingExpectedExecution,
    [],
    "--live must execute every expected live and isolated command case",
  );
}

if (reportOutput) {
  fs.mkdirSync(path.dirname(path.resolve(reportOutput)), { recursive: true });
  fs.writeFileSync(path.resolve(reportOutput), `${JSON.stringify(report, null, 2)}\n`);
}

if (jsonOutput) {
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
} else {
  console.log(
    `command validation (${report.mode}): ${report.summary.canonicalExecutableCommands} canonical paths, ` +
      `${report.summary.commandAliases} aliases`,
  );
  if (classifyOnly) {
    console.log("classification only: no commands were executed");
  }
  for (const row of rows) {
    console.log(
      `${row.outcome.padEnd(7)} ${row.strategy.padEnd(20)} ${row.command}` +
        (row.result?.stderrTail ? ` — ${row.result.stderrTail}` : ""),
    );
  }
  console.log(
    `summary: passed=${report.summary.passed} failed=${report.summary.failed} ` +
      `not-run=${report.summary.notRun} alias-passed=${report.summary.aliasPassed} ` +
      `alias-failed=${report.summary.aliasFailed}`,
  );
}

if (report.summary.failed > 0 || report.summary.aliasFailed > 0) {
  process.exitCode = 1;
}
