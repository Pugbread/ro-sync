#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import { verifyTauriArtifactSignature } from "./updater-trust.mjs";

const UPDATER_SEMVER = /^\d+\.\d+\.\d+(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?$/;

function parseArgs(values) {
  const result = {};
  for (let index = 0; index < values.length; index += 2) {
    const flag = values[index];
    const value = values[index + 1];
    if (!flag?.startsWith("--") || value == null) throw new Error(`missing value for ${flag || "argument"}`);
    result[flag.slice(2)] = value;
  }
  return result;
}

function safeAssetName(value, label) {
  const name = String(value || "");
  if (!name || path.basename(name) !== name || name === "." || name === "..") {
    throw new Error(`${label} must be one file name`);
  }
  return name;
}

export function updaterReleasePolicy(version) {
  const match = UPDATER_SEMVER.exec(String(version || ""));
  if (!match) throw new Error(`invalid updater version: ${version}`);
  const prerelease = match[1] != null;
  return { prerelease, makeLatest: !prerelease };
}

export function createUpdaterManifest({
  version,
  tag,
  assetDirectory,
  outputPath,
  macAsset,
  windowsMsiAsset,
  windowsNsisAsset,
  publicKey,
  publicationDate = new Date().toISOString(),
}) {
  updaterReleasePolicy(version);
  if (!String(tag || "").trim()) throw new Error("updater release tag is required");
  if (tag !== `v${version}`) throw new Error(`updater tag ${tag} does not match version ${version}`);
  if (!String(publicKey || "").trim()) throw new Error("pinned updater public key is required");

  const directory = path.resolve(assetDirectory);
  const assets = {
    "darwin-aarch64-app": safeAssetName(macAsset, "macOS app updater asset"),
    "windows-x86_64-msi": safeAssetName(windowsMsiAsset, "Windows MSI updater asset"),
    "windows-x86_64-nsis": safeAssetName(windowsNsisAsset, "Windows NSIS updater asset"),
  };
  const platforms = {};
  for (const [platform, asset] of Object.entries(assets)) {
    const artifactPath = path.join(directory, asset);
    const signaturePath = `${artifactPath}.sig`;
    if (!fs.existsSync(artifactPath) || !fs.statSync(artifactPath).isFile()) {
      throw new Error(`missing updater asset: ${artifactPath}`);
    }
    if (!fs.existsSync(signaturePath) || !fs.statSync(signaturePath).isFile()) {
      throw new Error(`missing updater signature: ${signaturePath}`);
    }
    const signature = verifyTauriArtifactSignature({
      artifactPath,
      signature: fs.readFileSync(signaturePath, "utf8"),
      publicKey,
    });
    platforms[platform] = {
      signature,
      url: `https://github.com/Pugbread/ro-sync/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(asset)}`,
    };
  }

  const manifest = {
    version,
    notes: `Ro Sync ${version}`,
    pub_date: publicationDate,
    platforms,
  };
  fs.writeFileSync(outputPath, `${JSON.stringify(manifest, null, 2)}\n`);
  return manifest;
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  try {
    const args = parseArgs(process.argv.slice(2));
    createUpdaterManifest({
      version: args.version,
      tag: args.tag,
      assetDirectory: args["asset-dir"],
      outputPath: args.output,
      macAsset: args["mac-asset"],
      windowsMsiAsset: args["windows-msi-asset"],
      windowsNsisAsset: args["windows-nsis-asset"],
      publicKey: process.env.ROSYNC_UPDATER_PUBLIC_KEY,
    });
  } catch (error) {
    console.error(`updater manifest creation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
