import { createHash } from "node:crypto";
import { chmod, copyFile, mkdir, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktopDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const rootDir = path.resolve(desktopDir, "..");
const distDir = path.join(desktopDir, "dist");
const tauriDir = path.join(desktopDir, "src-tauri");
const resourcesDir = path.join(tauriDir, "resources");
const binariesDir = path.join(tauriDir, "binaries");

const frontendEntries = [
  "index.html",
  "app.js",
  "bridge.js",
  "lifecycle-policy.js",
  "project-init.js",
  "platform.js",
  "style.css",
  "views",
  "assets",
  "docs/client-commands.generated.json",
];

const resourceEntries = [
  "plugin/Plugin.rbxm",
  "plugin/Plugin.luau",
  "docs",
  "tools",
];

const targetSources = new Map([
  ["aarch64-apple-darwin", "daemon/rosync-darwin-arm64"],
  ["x86_64-apple-darwin", "daemon/rosync-darwin-x86_64"],
  ["x86_64-pc-windows-msvc", "daemon/rosync-windows-x86_64.exe"],
  ["x86_64-unknown-linux-gnu", "daemon/rosync-linux-x86_64"],
]);

await Promise.all([
  rm(distDir, { recursive: true, force: true }),
  rm(resourcesDir, { recursive: true, force: true }),
  rm(binariesDir, { recursive: true, force: true }),
]);
await Promise.all([
  mkdir(distDir, { recursive: true }),
  mkdir(resourcesDir, { recursive: true }),
  mkdir(binariesDir, { recursive: true }),
]);

for (const entry of frontendEntries) {
  await copyEntry(path.join(rootDir, entry), path.join(distDir, entry));
}
await copyFile(path.join(desktopDir, "host.css"), path.join(distDir, "host.css"));
const indexPath = path.join(distDir, "index.html");
const indexHtml = await readFile(indexPath, "utf8");
if (!indexHtml.includes("host.css")) {
  await writeFile(
    indexPath,
    indexHtml.replace("</head>", '    <link rel="stylesheet" href="host.css" />\n  </head>'),
    "utf8",
  );
}
for (const entry of resourceEntries) {
  await copyEntry(path.join(rootDir, entry), path.join(resourcesDir, entry));
}
if (!process.platform.startsWith("win")) {
  for (const executable of [
    "tools/luau-lsp/darwin-arm64/luau-lsp",
    "tools/luau/darwin-arm64/luau-compile",
    "tools/luau-lsp/darwin-x86_64/luau-lsp",
    "tools/luau/darwin-x86_64/luau-compile",
    "tools/luau-lsp/linux-x86_64/luau-lsp",
    "tools/luau/linux-x86_64/luau-compile",
  ]) {
    const file = path.join(resourcesDir, executable);
    if (await exists(file)) await chmod(file, 0o755);
  }
}

const target = detectTargetTriple();
const sourceRelative = targetSources.get(target);
if (!sourceRelative) {
  throw new Error(`No Ro Sync sidecar mapping exists for Tauri target ${target}`);
}
const sourceBinary = path.join(rootDir, sourceRelative);
if (!(await exists(sourceBinary))) {
  throw new Error(
    `The ${target} sidecar is missing (${sourceRelative}). Build that daemon artifact before packaging the desktop app.`,
  );
}
verifySidecarContract(sourceBinary);
const executableSuffix = target.includes("windows") ? ".exe" : "";
const bundledBinary = path.join(binariesDir, `rosync-${target}${executableSuffix}`);
await copyFile(sourceBinary, bundledBinary);
if (!target.includes("windows")) await chmod(bundledBinary, 0o755);

const manifest = {
  schemaVersion: 1,
  target,
  frontend: await hashTree(distDir),
  resources: await hashTree(resourcesDir),
  sidecar: {
    path: path.relative(desktopDir, bundledBinary).replaceAll(path.sep, "/"),
    sha256: await hashFile(bundledBinary),
  },
};
await writeFile(
  path.join(distDir, "prepare-manifest.json"),
  `${JSON.stringify(manifest, null, 2)}\n`,
  "utf8",
);

process.stdout.write(
  `Prepared Ro Sync desktop assets for ${target} (${manifest.frontend.length} UI files, ${manifest.resources.length} resource files).\n`,
);

async function copyEntry(source, destination) {
  const info = await stat(source);
  if (info.isDirectory()) {
    await mkdir(destination, { recursive: true });
    const children = (await readdir(source)).sort((a, b) => a.localeCompare(b));
    for (const child of children) {
      await copyEntry(path.join(source, child), path.join(destination, child));
    }
    return;
  }
  if (!info.isFile()) throw new Error(`Refusing to bundle non-file resource ${source}`);
  await mkdir(path.dirname(destination), { recursive: true });
  await copyFile(source, destination);
}

async function hashTree(directory) {
  const files = [];
  await walk(directory, "", files);
  return files;
}

async function walk(directory, relative, output) {
  const entries = (await readdir(directory, { withFileTypes: true })).sort((a, b) =>
    a.name.localeCompare(b.name),
  );
  for (const entry of entries) {
    const childRelative = relative ? `${relative}/${entry.name}` : entry.name;
    const child = path.join(directory, entry.name);
    if (entry.isDirectory()) await walk(child, childRelative, output);
    else if (entry.isFile()) output.push({ path: childRelative, sha256: await hashFile(child) });
    else throw new Error(`Refusing to hash non-file resource ${child}`);
  }
}

async function hashFile(file) {
  return createHash("sha256").update(await readFile(file)).digest("hex");
}

async function exists(file) {
  try {
    await stat(file);
    return true;
  } catch (error) {
    if (error && error.code === "ENOENT") return false;
    throw error;
  }
}

function detectTargetTriple() {
  const explicit = process.env.TAURI_ENV_TARGET_TRIPLE || process.env.CARGO_BUILD_TARGET;
  if (explicit) return explicit;
  const rustc = spawnSync("rustc", ["-vV"], { encoding: "utf8" });
  if (rustc.status === 0) {
    const host = rustc.stdout.match(/^host:\s*(\S+)$/m)?.[1];
    if (host) return host;
  }
  const fallback = `${process.arch}:${process.platform}`;
  const triples = {
    "arm64:darwin": "aarch64-apple-darwin",
    "x64:darwin": "x86_64-apple-darwin",
    "x64:win32": "x86_64-pc-windows-msvc",
    "x64:linux": "x86_64-unknown-linux-gnu",
  };
  if (triples[fallback]) return triples[fallback];
  throw new Error(`Could not infer a supported Rust target from ${fallback}`);
}

function verifySidecarContract(binary) {
  const checks = [
    {
      args: ["daemon", "--help"],
      flags: ["start", "status", "stop"],
    },
    {
      args: ["daemon", "start", "--help"],
      flags: [
        "--managed-by",
        "--owner-token-env",
        "--data-dir",
        "--game-id",
        "--group-id",
        "--place-id",
        "--projects-root",
      ],
    },
    {
      args: ["auth", "set", "--help"],
      flags: ["--from-env", "--data-dir"],
    },
  ];
  for (const check of checks) {
    const probe = spawnSync(binary, check.args, {
      encoding: "utf8",
      timeout: 10_000,
      env: { ...process.env },
    });
    const output = `${probe.stdout || ""}\n${probe.stderr || ""}`;
    const missing = check.flags.filter((flag) => !output.includes(flag));
    if (probe.error || probe.status !== 0 || missing.length) {
      const reason = probe.error
        ? probe.error.message
        : probe.status !== 0
          ? `exit ${probe.status}`
          : `missing ${missing.join(", ")}`;
      throw new Error(
        `The staged Ro Sync sidecar is incompatible (${check.args.join(" ")}: ${reason}). Rebuild the daemon release artifact before packaging.`,
      );
    }
  }
}
