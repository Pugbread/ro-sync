import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";

const options = new Map();
for (let index = 2; index < process.argv.length; index += 2) {
  const key = process.argv[index];
  const value = process.argv[index + 1];
  if (!key?.startsWith("--") || value === undefined) {
    throw new Error(`invalid argument near ${key || "<end>"}`);
  }
  options.set(key.slice(2), value);
}

const expectedCommit = required("commit").slice(0, 12);
const expectedVersion = required("version");

const standaloneDaemon = options.get("daemon");
if (standaloneDaemon) {
  const daemonIdentity = runJson(
    standaloneDaemon,
    ["version", "--port", "1", "--raw"],
    "standalone daemon",
  );
  assertIdentity(daemonIdentity, expectedCommit, "standalone daemon");
  if (daemonIdentity.daemon !== expectedVersion) {
    throw new Error(
      `daemon embedded version mismatch: ${daemonIdentity.daemon} != ${expectedVersion}`,
    );
  }
  process.stdout.write(
    `verified standalone daemon ${expectedCommit}: ${daemonIdentity.daemon}\n`,
  );
} else {
  const desktop = required("desktop");
  const sidecar = required("sidecar");
  const plugin = required("plugin");
  const pluginManifestPath = required("plugin-manifest");
  const desktopIdentity = runJson(desktop, ["--build-info"], "desktop host");
  const sidecarIdentity = runJson(
    sidecar,
    ["version", "--port", "1", "--raw"],
    "daemon sidecar",
  );
  const pluginManifest = JSON.parse(readFileSync(pluginManifestPath, "utf8"));
  const pluginSha256 = createHash("sha256")
    .update(readFileSync(plugin))
    .digest("hex");

  assertIdentity(desktopIdentity, expectedCommit, "desktop host");
  assertIdentity(sidecarIdentity, expectedCommit, "daemon sidecar");
  assertIdentity(pluginManifest, expectedCommit, "Studio plugin");

  if (desktopIdentity.version !== sidecarIdentity.daemon) {
    throw new Error(
      `desktop/daemon version mismatch: ${desktopIdentity.version} != ${sidecarIdentity.daemon}`,
    );
  }
  if (desktopIdentity.version !== expectedVersion) {
    throw new Error(
      `desktop embedded version mismatch: ${desktopIdentity.version} != ${expectedVersion}`,
    );
  }
  if (sidecarIdentity.daemon !== expectedVersion) {
    throw new Error(
      `daemon embedded version mismatch: ${sidecarIdentity.daemon} != ${expectedVersion}`,
    );
  }
  if (pluginManifest.schemaVersion !== 1) {
    throw new Error(`unsupported plugin build manifest schema: ${pluginManifest.schemaVersion}`);
  }
  if (pluginManifest.artifact !== "Plugin.rbxm") {
    throw new Error(`unexpected plugin artifact name: ${pluginManifest.artifact}`);
  }
  if (!Number.isInteger(pluginManifest.protocolVersion) || pluginManifest.protocolVersion <= 0) {
    throw new Error("plugin build manifest has an invalid protocolVersion");
  }
  if (pluginManifest.sha256 !== pluginSha256) {
    throw new Error(
      `plugin artifact hash mismatch: ${pluginManifest.sha256} != ${pluginSha256}`,
    );
  }

  process.stdout.write(
    `verified build ${expectedCommit}: desktop/daemon ${desktopIdentity.version}, plugin ${pluginManifest.pluginVersion} protocol ${pluginManifest.protocolVersion}\n`,
  );
}

function required(name) {
  const value = options.get(name);
  if (!value) throw new Error(`missing --${name}`);
  return value;
}

function runJson(command, args, label) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    timeout: 15_000,
    windowsHide: true,
  });
  if (result.error || result.status !== 0) {
    throw new Error(
      `${label} identity probe failed: ${result.error?.message || `exit ${result.status}`}`,
    );
  }
  try {
    return JSON.parse(String(result.stdout || "").trim());
  } catch (error) {
    throw new Error(`${label} returned invalid build identity JSON: ${error.message}`);
  }
}

function assertIdentity(identity, commit, label) {
  if (!identity || typeof identity !== "object" || Array.isArray(identity)) {
    throw new Error(`${label} build identity is not an object`);
  }
  if (identity.buildCommit !== commit) {
    throw new Error(`${label} commit mismatch: ${identity.buildCommit} != ${commit}`);
  }
  if (identity.buildDirty !== false) {
    throw new Error(`${label} is marked as a dirty build`);
  }
}
