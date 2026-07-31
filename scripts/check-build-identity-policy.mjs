import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmodSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const verifier = path.join(root, "scripts", "verify-build-identity.mjs");
const temporary = mkdtempSync(path.join(tmpdir(), "rosync-build-identity-"));

try {
  for (const relative of ["daemon/build.rs", "desktop/src-tauri/build.rs"]) {
    const buildScript = readFileSync(path.join(root, relative), "utf8");
    assert.doesNotMatch(
      buildScript,
      /refs\/heads\/main/,
      `${relative} must watch the resolved branch rather than hard-coding main`,
    );
    assert.doesNotMatch(
      buildScript,
      /ls-files/,
      `${relative} must not invalidate a Rust build for every unrelated tracked file`,
    );
    assert.match(
      buildScript,
      /unwrap_or_else\(\|\| "source"\.to_string\(\)\)/,
      `${relative} must use stable local build identity when CI did not provide one`,
    );
    assert.match(
      buildScript,
      /_ if short_commit == "source" => false/,
      `${relative} must keep local source builds stable instead of recompiling for a dirty bit`,
    );
  }
  const releaseWorkflow = readFileSync(
    path.join(root, ".github", "workflows", "release.yml"),
    "utf8",
  );
  assert.match(
    releaseWorkflow,
    /--version "\$ROSYNC_DESKTOP_VERSION"/,
    "release verification must receive the resolved package/tag version",
  );
  assert.match(
    releaseWorkflow,
    /Build[\s\S]*?cargo build --profile dist --locked --target \$\{\{ matrix\.target \}\}[\s\S]*?Verify standalone daemon build identity[\s\S]*?--daemon "daemon\/target\/\$\{\{ matrix\.target \}\}\/dist\/\$\{\{ matrix\.src_bin \}\}"[\s\S]*?--version "\$daemon_version"/,
    "every standalone daemon matrix artifact must be identity-checked before staging",
  );
  assert.match(
    releaseWorkflow,
    /Check cross-runtime protocol constants[\s\S]*?node scripts\/check-plugin-reconnect-policy\.mjs/,
    "release builds must reject cross-runtime protocol drift",
  );
  assert.match(
    releaseWorkflow,
    /\.prepared\.sidecar\.version == \.version/,
    "final release manifests must prove embedded and package versions match",
  );

  const commit = "123456789abc";
  const version = "0.3.0";
  const desktop = path.join(temporary, "desktop");
  const daemon = path.join(temporary, "daemon");
  const plugin = path.join(temporary, "Plugin.rbxm");
  const manifest = path.join(temporary, "Plugin.build.json");
  writeExecutable(
    desktop,
    JSON.stringify({ version, buildCommit: commit, buildDirty: false }),
  );
  writeExecutable(
    daemon,
    JSON.stringify({ daemon: version, buildCommit: commit, buildDirty: false }),
  );
  writeFileSync(plugin, "deterministic-plugin-fixture");
  writeFileSync(
    manifest,
    `${JSON.stringify({
      schemaVersion: 1,
      artifact: "Plugin.rbxm",
      sha256: createHash("sha256").update(readFileSync(plugin)).digest("hex"),
      pluginVersion: "2.4.1",
      protocolVersion: 6,
      buildCommit: commit,
      buildDirty: false,
    })}\n`,
  );

  const common = [
    verifier,
    "--desktop",
    desktop,
    "--sidecar",
    daemon,
    "--plugin",
    plugin,
    "--plugin-manifest",
    manifest,
    "--commit",
    commit,
  ];
  const accepted = spawnSync(process.execPath, [...common, "--version", version], {
    encoding: "utf8",
  });
  if (accepted.status !== 0) {
    throw new Error(`matching identity fixture was rejected: ${accepted.stderr}`);
  }

  const rejected = spawnSync(process.execPath, [...common, "--version", "0.3.1"], {
    encoding: "utf8",
  });
  if (rejected.status === 0 || !rejected.stderr.includes("version mismatch")) {
    throw new Error("a tag/package version mismatch was not rejected");
  }

  const standaloneAccepted = spawnSync(
    process.execPath,
    [
      verifier,
      "--daemon",
      daemon,
      "--commit",
      commit,
      "--version",
      version,
    ],
    { encoding: "utf8" },
  );
  if (standaloneAccepted.status !== 0) {
    throw new Error(
      `matching standalone daemon fixture was rejected: ${standaloneAccepted.stderr}`,
    );
  }

  writeExecutable(
    daemon,
    JSON.stringify({ daemon: version, buildCommit: commit, buildDirty: true }),
  );
  const standaloneRejected = spawnSync(
    process.execPath,
    [
      verifier,
      "--daemon",
      daemon,
      "--commit",
      commit,
      "--version",
      version,
    ],
    { encoding: "utf8" },
  );
  if (standaloneRejected.status === 0 || !standaloneRejected.stderr.includes("dirty build")) {
    throw new Error("a dirty standalone daemon artifact was not rejected");
  }
  process.stdout.write("Build identity/version policy checks passed.\n");
} finally {
  rmSync(temporary, { recursive: true, force: true });
}

function writeExecutable(file, payload) {
  writeFileSync(file, `#!/bin/sh\nprintf '%s\\n' '${payload}'\n`);
  chmodSync(file, 0o755);
}
