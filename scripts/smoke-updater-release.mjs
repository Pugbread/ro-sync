#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  createUpdaterManifest,
  updaterReleasePolicy,
} from "./create-updater-manifest.mjs";
import {
  updaterPublicKeyFingerprint,
  verifyPinnedUpdaterKey,
  verifyTauriArtifactSignature,
} from "./updater-trust.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const desktop = path.join(root, "desktop");
const tauri = path.join(
  desktop,
  "node_modules",
  ".bin",
  process.platform === "win32" ? "tauri.cmd" : "tauri",
);
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "rosync-updater-smoke-"));
const password = "ephemeral-updater-smoke-password";
const privateKey = path.join(temporary, "ephemeral.key");
const keyCheckScript = path.join(root, "scripts", "check-updater-key-pin.mjs");
const manifestScript = path.join(root, "scripts", "create-updater-manifest.mjs");

function run(args) {
  const result = spawnSync(tauri, args, {
    cwd: desktop,
    encoding: "utf8",
    env: { ...process.env, CI: "true" },
  });
  if (result.status !== 0) {
    throw new Error(`tauri ${args.join(" ")} failed: ${(result.stderr || result.stdout).trim()}`);
  }
}

function assertDefaultPinResolves(cwd) {
  const env = { ...process.env };
  delete env.ROSYNC_UPDATER_KEY_PIN;
  delete env.ROSYNC_UPDATER_PUBLIC_KEY;
  const result = spawnSync(process.execPath, [keyCheckScript, "verify"], {
    cwd,
    encoding: "utf8",
    env,
  });
  assert.notEqual(result.status, 0, "the checked-in pin must require a public key or bootstrap");
  assert.doesNotMatch(result.stderr, /ENOENT|no such file/i);
  assert.match(result.stderr, /not bootstrapped|ROSYNC_UPDATER_PUBLIC_KEY is required/i);
}

try {
  if (!fs.existsSync(tauri)) throw new Error("desktop dependencies are missing; run `npm ci` in desktop");
  assertDefaultPinResolves(root);
  assertDefaultPinResolves(desktop);
  run(["signer", "generate", "--ci", "--password", password, "--write-keys", privateKey]);
  const publicKey = fs.readFileSync(`${privateKey}.pub`, "utf8").trim();
  const fingerprint = updaterPublicKeyFingerprint(publicKey);
  const pin = {
    schemaVersion: 1,
    state: "configured",
    algorithm: "sha256-ed25519-public-key",
    publicKeySha256: fingerprint,
  };
  assert.equal(verifyPinnedUpdaterKey(pin, publicKey), fingerprint);
  assert.throws(
    () => verifyPinnedUpdaterKey({ ...pin, publicKeySha256: "0".repeat(64) }, publicKey),
    /refusing silent key rotation/,
  );

  assert.deepEqual(updaterReleasePolicy("1.2.3"), { prerelease: false, makeLatest: true });
  assert.deepEqual(updaterReleasePolicy("1.2.3+build.4"), { prerelease: false, makeLatest: true });
  assert.deepEqual(updaterReleasePolicy("1.2.3-rc.1"), { prerelease: true, makeLatest: false });
  assert.deepEqual(updaterReleasePolicy("1.2.3-rc.1+build.4"), { prerelease: true, makeLatest: false });
  assert.throws(() => updaterReleasePolicy("1.2"), /invalid updater version/);

  const releaseWorkflow = fs.readFileSync(path.join(root, ".github", "workflows", "release.yml"), "utf8");
  assert.ok(
    releaseWorkflow.includes("prerelease: ${{ steps.release-policy.outputs.prerelease }}"),
    "the release action must use the tested prerelease policy",
  );
  assert.ok(
    releaseWorkflow.includes("make_latest: ${{ steps.release-policy.outputs.make_latest }}"),
    "the release action must explicitly keep prereleases out of GitHub's latest release",
  );

  const version = "9.8.7-smoke.1";
  const macAsset = `Ro-Sync-${version}-macos-arm64.app.tar.gz`;
  const windowsMsiAsset = `Ro-Sync-${version}-windows-x64.msi`;
  const windowsNsisAsset = `Ro-Sync-${version}-windows-x64-setup.exe`;
  for (const [asset, contents] of [
    [macAsset, "ephemeral macOS updater artifact\n"],
    [windowsMsiAsset, "ephemeral Windows MSI updater artifact\n"],
    [windowsNsisAsset, "ephemeral Windows NSIS updater artifact\n"],
  ]) {
    const artifactPath = path.join(temporary, asset);
    fs.writeFileSync(artifactPath, contents);
    run(["signer", "sign", "--private-key-path", privateKey, "--password", password, artifactPath]);
    verifyTauriArtifactSignature({
      artifactPath,
      signature: fs.readFileSync(`${artifactPath}.sig`, "utf8"),
      publicKey,
    });
  }

  const outputPath = path.join(temporary, "latest.json");
  const manifest = createUpdaterManifest({
    version,
    tag: `v${version}`,
    assetDirectory: temporary,
    outputPath,
    macAsset,
    windowsMsiAsset,
    windowsNsisAsset,
    publicKey,
    publicationDate: "2026-01-01T00:00:00.000Z",
  });
  assert.deepEqual(Object.keys(manifest.platforms).sort(), [
    "darwin-aarch64-app",
    "windows-x86_64-msi",
    "windows-x86_64-nsis",
  ]);
  assert.match(manifest.platforms["darwin-aarch64-app"].url, /macos-arm64\.app\.tar\.gz$/);
  assert.match(manifest.platforms["windows-x86_64-msi"].url, /windows-x64\.msi$/);
  assert.match(manifest.platforms["windows-x86_64-nsis"].url, /windows-x64-setup\.exe$/);
  assert.equal(manifest.platforms["windows-x86_64"], undefined);
  assert.equal(JSON.parse(fs.readFileSync(outputPath, "utf8")).version, version);

  const cliOutputPath = path.join(temporary, "latest-cli.json");
  const cliResult = spawnSync(process.execPath, [
    manifestScript,
    "--version", version,
    "--tag", `v${version}`,
    "--asset-dir", temporary,
    "--mac-asset", macAsset,
    "--windows-msi-asset", windowsMsiAsset,
    "--windows-nsis-asset", windowsNsisAsset,
    "--output", cliOutputPath,
  ], {
    cwd: root,
    encoding: "utf8",
    env: { ...process.env, ROSYNC_UPDATER_PUBLIC_KEY: publicKey },
  });
  assert.equal(
    cliResult.status,
    0,
    `manifest CLI failed: ${(cliResult.stderr || cliResult.stdout).trim()}`,
  );
  assert.deepEqual(Object.keys(JSON.parse(fs.readFileSync(cliOutputPath, "utf8")).platforms).sort(), [
    "darwin-aarch64-app",
    "windows-x86_64-msi",
    "windows-x86_64-nsis",
  ]);

  fs.appendFileSync(path.join(temporary, windowsMsiAsset), "tampered\n");
  assert.throws(
    () => createUpdaterManifest({
      version,
      tag: `v${version}`,
      assetDirectory: temporary,
      outputPath,
      macAsset,
      windowsMsiAsset,
      windowsNsisAsset,
      publicKey,
      publicationDate: "2026-01-01T00:00:00.000Z",
    }),
    /invalid updater signature/,
  );
  fs.writeFileSync(path.join(temporary, windowsMsiAsset), "ephemeral Windows MSI updater artifact\n");

  fs.appendFileSync(path.join(temporary, macAsset), "tampered\n");
  assert.throws(
    () => verifyTauriArtifactSignature({
      artifactPath: path.join(temporary, macAsset),
      signature: fs.readFileSync(path.join(temporary, `${macAsset}.sig`), "utf8"),
      publicKey,
    }),
    /invalid updater signature/,
  );
  console.log("updater signing, pinning, installer routing, release policy, and manifest smoke test passed");
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}
