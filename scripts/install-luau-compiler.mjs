#!/usr/bin/env node

import { createHash, randomBytes } from "node:crypto";
import { promises as fs } from "node:fs";
import https from "node:https";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { inflateRawSync } from "node:zlib";

export const LUAU_VERSION = "0.729";
export const LUAU_LICENSE_SHA256 = "1597b423ca8f9c76225b498071f03b2b3609c4960b8dc30e63b230490fc82efd";

// These are the three daemon targets shipped by .github/workflows/release.yml.
// Both the official release archive and the extracted executable are pinned so
// a changed archive, wrong entry, or corrupt extraction is rejected before the
// compiler reaches tools/luau/.
export const LUAU_COMPILER_TARGETS = Object.freeze({
  "darwin-arm64": Object.freeze({
    asset: "luau-macos.zip",
    archiveSha256: "1027273dd636b4a8ad1a4167f7a43d153fef8d0c13e8a8502ed488ce95d8e2d9",
    executable: "luau-compile",
    executableSha256: "a27e6ac06e24745c9c38478df8d0abdfc71ec535af925d7ff00cd3c5e9551a0d",
  }),
  "linux-x86_64": Object.freeze({
    asset: "luau-ubuntu.zip",
    archiveSha256: "cadc6e5737e6186c3b6a17047ffb25ff9ccd3728f8951ba39df7d39121a0f0f6",
    executable: "luau-compile",
    executableSha256: "463646ea8cb3f964297fde72e7624d169158ff4f1af7baee70be942f0b8f114a",
  }),
  "windows-x86_64": Object.freeze({
    asset: "luau-windows.zip",
    archiveSha256: "16c079e4eebe9ba5aabfd86357ae1e48ae0bfb04b7ac4be133d403da389f2e84",
    executable: "luau-compile.exe",
    executableSha256: "4cee61d651c8ac34f412bc408c65959a4f8dcdbe3ef06dfe8dd09c8dbb70d7be",
  }),
});

const SCRIPT_PATH = fileURLToPath(import.meta.url);
const REPOSITORY_ROOT = path.resolve(path.dirname(SCRIPT_PATH), "..");
const MAX_ARCHIVE_BYTES = 64 * 1024 * 1024;
const MAX_EXECUTABLE_BYTES = 32 * 1024 * 1024;
const DOWNLOAD_ATTEMPTS = 3;
const DOWNLOAD_TIMEOUT_MS = 30_000;
const MAX_REDIRECTS = 8;

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function releaseUrl(asset) {
  return `https://github.com/luau-lang/luau/releases/download/${LUAU_VERSION}/${asset}`;
}

export function validateManifest() {
  const expectedTargets = ["darwin-arm64", "linux-x86_64", "windows-x86_64"];
  const actualTargets = Object.keys(LUAU_COMPILER_TARGETS).sort();
  if (actualTargets.join("\n") !== expectedTargets.join("\n")) {
    throw new Error(`compiler manifest targets changed: ${actualTargets.join(", ")}`);
  }

  const seenAssets = new Set();
  for (const target of expectedTargets) {
    const entry = LUAU_COMPILER_TARGETS[target];
    if (!entry || !/^[a-f0-9]{64}$/.test(entry.archiveSha256)) {
      throw new Error(`${target}: invalid archive SHA-256`);
    }
    if (!/^[a-f0-9]{64}$/.test(entry.executableSha256)) {
      throw new Error(`${target}: invalid executable SHA-256`);
    }
    if (seenAssets.has(entry.asset)) {
      throw new Error(`${target}: duplicate release asset ${entry.asset}`);
    }
    seenAssets.add(entry.asset);

    const url = new URL(releaseUrl(entry.asset));
    if (url.protocol !== "https:" || url.hostname !== "github.com") {
      throw new Error(`${target}: release URL must use github.com over HTTPS`);
    }
    const expectedPath = `/luau-lang/luau/releases/download/${LUAU_VERSION}/${entry.asset}`;
    if (url.pathname !== expectedPath) {
      throw new Error(`${target}: release URL is not pinned to Luau ${LUAU_VERSION}`);
    }
    const expectedExecutable = target === "windows-x86_64"
      ? "luau-compile.exe"
      : "luau-compile";
    if (entry.executable !== expectedExecutable) {
      throw new Error(`${target}: unexpected executable name ${entry.executable}`);
    }
  }

  return { version: LUAU_VERSION, targets: expectedTargets };
}

export async function verifyRepositoryMetadata() {
  const manifest = validateManifest();
  const [documentation, releaseWorkflow, license] = await Promise.all([
    fs.readFile(path.join(REPOSITORY_ROOT, "tools", "luau", "README.md"), "utf8"),
    fs.readFile(path.join(REPOSITORY_ROOT, ".github", "workflows", "release.yml"), "utf8"),
    fs.readFile(path.join(REPOSITORY_ROOT, "tools", "luau", "LICENSE.txt")),
  ]);

  const documentedTargets = [...documentation.matchAll(
    /^tools\/luau\/([^/]+)\/luau-compile(?:\.exe)?$/gm,
  )].map((match) => match[1]).sort();
  if (documentedTargets.join("\n") !== manifest.targets.join("\n")) {
    throw new Error(
      `tools/luau/README.md layout targets differ from the manifest: ${documentedTargets.join(", ")}`,
    );
  }

  for (const target of manifest.targets) {
    const entry = LUAU_COMPILER_TARGETS[target];
    const expectedRow = `| \`${target}\` | \`${entry.asset}\` | \`${entry.archiveSha256}\` | \`${entry.executableSha256}\` |`;
    if (!documentation.includes(expectedRow)) {
      throw new Error(`tools/luau/README.md is missing the pinned metadata row for ${target}`);
    }
  }
  if (!documentation.includes(`/luau-lang/luau/releases/tag/${LUAU_VERSION}`)) {
    throw new Error(`tools/luau/README.md is not linked to the Luau ${LUAU_VERSION} release`);
  }

  const workflowTargets = [...releaseWorkflow.matchAll(
    /^\s*tool_target:\s*([a-z0-9_-]+)\s*$/gm,
  )].map((match) => match[1]).sort();
  if (workflowTargets.join("\n") !== manifest.targets.join("\n")) {
    throw new Error(
      `.github/workflows/release.yml targets differ from the compiler manifest: ${workflowTargets.join(", ")}`,
    );
  }

  const licenseSha256 = sha256(license);
  if (licenseSha256 !== LUAU_LICENSE_SHA256) {
    throw new Error(
      `tools/luau/LICENSE.txt SHA-256 mismatch; expected ${LUAU_LICENSE_SHA256}, got ${licenseSha256}`,
    );
  }

  return { ...manifest, licenseSha256 };
}

export function hostTarget(platform = process.platform, arch = process.arch) {
  if (platform === "darwin" && arch === "arm64") return "darwin-arm64";
  if (platform === "linux" && arch === "x64") return "linux-x86_64";
  if (platform === "win32" && arch === "x64") return "windows-x86_64";
  throw new Error(
    `unsupported host ${platform}/${arch}; supported targets: ${Object.keys(LUAU_COMPILER_TARGETS).join(", ")}`,
  );
}

function assertBounds(buffer, offset, length, context) {
  if (!Number.isSafeInteger(offset) || !Number.isSafeInteger(length) ||
      offset < 0 || length < 0 || offset + length > buffer.length) {
    throw new Error(`invalid ZIP bounds for ${context}`);
  }
}

function findEndOfCentralDirectory(buffer) {
  const minimumOffset = Math.max(0, buffer.length - 22 - 0xffff);
  for (let offset = buffer.length - 22; offset >= minimumOffset; offset -= 1) {
    if (buffer.readUInt32LE(offset) === 0x06054b50) return offset;
  }
  throw new Error("ZIP end-of-central-directory record not found");
}

export function extractZipEntry(archive, wantedName) {
  if (!Buffer.isBuffer(archive)) archive = Buffer.from(archive);
  if (archive.length < 22 || archive.length > MAX_ARCHIVE_BYTES) {
    throw new Error(`ZIP archive size ${archive.length} is outside the allowed range`);
  }

  const eocd = findEndOfCentralDirectory(archive);
  assertBounds(archive, eocd, 22, "end-of-central-directory");
  const diskNumber = archive.readUInt16LE(eocd + 4);
  const centralDisk = archive.readUInt16LE(eocd + 6);
  const entriesOnDisk = archive.readUInt16LE(eocd + 8);
  const entryCount = archive.readUInt16LE(eocd + 10);
  const centralSize = archive.readUInt32LE(eocd + 12);
  const centralOffset = archive.readUInt32LE(eocd + 16);
  const commentLength = archive.readUInt16LE(eocd + 20);
  assertBounds(archive, eocd + 22, commentLength, "ZIP comment");
  if (diskNumber !== 0 || centralDisk !== 0 || entriesOnDisk !== entryCount) {
    throw new Error("multi-disk ZIP archives are not supported");
  }
  if (entryCount === 0xffff || centralSize === 0xffffffff || centralOffset === 0xffffffff) {
    throw new Error("ZIP64 archives are not supported");
  }
  assertBounds(archive, centralOffset, centralSize, "central directory");

  let cursor = centralOffset;
  let selected = null;
  for (let index = 0; index < entryCount; index += 1) {
    assertBounds(archive, cursor, 46, `central entry ${index}`);
    if (archive.readUInt32LE(cursor) !== 0x02014b50) {
      throw new Error(`invalid ZIP central entry ${index}`);
    }
    const flags = archive.readUInt16LE(cursor + 8);
    const method = archive.readUInt16LE(cursor + 10);
    const compressedSize = archive.readUInt32LE(cursor + 20);
    const uncompressedSize = archive.readUInt32LE(cursor + 24);
    const nameLength = archive.readUInt16LE(cursor + 28);
    const extraLength = archive.readUInt16LE(cursor + 30);
    const entryCommentLength = archive.readUInt16LE(cursor + 32);
    const localOffset = archive.readUInt32LE(cursor + 42);
    const recordLength = 46 + nameLength + extraLength + entryCommentLength;
    assertBounds(archive, cursor, recordLength, `central entry ${index}`);
    const name = archive.subarray(cursor + 46, cursor + 46 + nameLength).toString("utf8");
    if (name === wantedName) {
      if (selected) throw new Error(`ZIP contains duplicate ${wantedName} entries`);
      selected = { flags, method, compressedSize, uncompressedSize, localOffset };
    }
    cursor += recordLength;
  }
  if (cursor !== centralOffset + centralSize) {
    throw new Error("ZIP central-directory length mismatch");
  }
  if (!selected) throw new Error(`ZIP does not contain ${wantedName}`);
  if ((selected.flags & 0x1) !== 0) throw new Error(`${wantedName} is encrypted`);
  if (![0, 8].includes(selected.method)) {
    throw new Error(`${wantedName} uses unsupported ZIP method ${selected.method}`);
  }
  if (selected.compressedSize > MAX_ARCHIVE_BYTES ||
      selected.uncompressedSize > MAX_EXECUTABLE_BYTES) {
    throw new Error(`${wantedName} exceeds the extraction size limit`);
  }

  assertBounds(archive, selected.localOffset, 30, `${wantedName} local header`);
  if (archive.readUInt32LE(selected.localOffset) !== 0x04034b50) {
    throw new Error(`invalid local header for ${wantedName}`);
  }
  const localNameLength = archive.readUInt16LE(selected.localOffset + 26);
  const localExtraLength = archive.readUInt16LE(selected.localOffset + 28);
  const dataOffset = selected.localOffset + 30 + localNameLength + localExtraLength;
  assertBounds(archive, dataOffset, selected.compressedSize, `${wantedName} payload`);
  const compressed = archive.subarray(dataOffset, dataOffset + selected.compressedSize);
  const executable = selected.method === 0
    ? Buffer.from(compressed)
    : inflateRawSync(compressed, { maxOutputLength: MAX_EXECUTABLE_BYTES });
  if (executable.length !== selected.uncompressedSize) {
    throw new Error(`${wantedName} extracted length mismatch`);
  }
  return executable;
}

function downloadOnce(url, redirectsRemaining = MAX_REDIRECTS) {
  return new Promise((resolve, reject) => {
    const parsed = new URL(url);
    if (parsed.protocol !== "https:") {
      reject(new Error(`refusing non-HTTPS download URL: ${parsed.href}`));
      return;
    }
    const request = https.get(parsed, {
      headers: {
        Accept: "application/octet-stream",
        "Accept-Encoding": "identity",
        "User-Agent": `ro-sync-luau-installer/${LUAU_VERSION}`,
      },
    }, (response) => {
      const status = response.statusCode || 0;
      if (status >= 300 && status < 400 && response.headers.location) {
        response.resume();
        if (redirectsRemaining <= 0) {
          reject(new Error("too many redirects while downloading Luau"));
          return;
        }
        const redirect = new URL(response.headers.location, parsed);
        if (redirect.protocol !== "https:") {
          reject(new Error(`refusing non-HTTPS redirect: ${redirect.href}`));
          return;
        }
        downloadOnce(redirect, redirectsRemaining - 1).then(resolve, reject);
        return;
      }
      if (status !== 200) {
        response.resume();
        reject(new Error(`download returned HTTP ${status}`));
        return;
      }
      const declared = Number(response.headers["content-length"] || 0);
      if (declared > MAX_ARCHIVE_BYTES) {
        response.destroy();
        reject(new Error(`download is too large (${declared} bytes)`));
        return;
      }
      const chunks = [];
      let size = 0;
      response.on("data", (chunk) => {
        size += chunk.length;
        if (size > MAX_ARCHIVE_BYTES) {
          response.destroy(new Error(`download exceeded ${MAX_ARCHIVE_BYTES} bytes`));
          return;
        }
        chunks.push(chunk);
      });
      response.on("end", () => resolve(Buffer.concat(chunks, size)));
      response.on("error", reject);
    });
    request.setTimeout(DOWNLOAD_TIMEOUT_MS, () => {
      request.destroy(new Error(`download timed out after ${DOWNLOAD_TIMEOUT_MS} ms`));
    });
    request.on("error", reject);
  });
}

async function downloadWithRetries(url) {
  let lastError;
  for (let attempt = 1; attempt <= DOWNLOAD_ATTEMPTS; attempt += 1) {
    try {
      return await downloadOnce(url);
    } catch (error) {
      lastError = error;
      if (attempt < DOWNLOAD_ATTEMPTS) {
        await new Promise((resolve) => setTimeout(resolve, attempt * 500));
      }
    }
  }
  throw new Error(`failed to download ${url}: ${lastError?.message || lastError}`);
}

async function readExisting(destination, expectedSha256) {
  try {
    const bytes = await fs.readFile(destination);
    return sha256(bytes) === expectedSha256;
  } catch (error) {
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

async function installAtomically(destination, executable) {
  await fs.mkdir(path.dirname(destination), { recursive: true });
  const temporary = path.join(
    path.dirname(destination),
    `.${path.basename(destination)}.${process.pid}.${randomBytes(6).toString("hex")}.tmp`,
  );
  try {
    await fs.writeFile(temporary, executable, { mode: 0o755, flag: "wx" });
    if (process.platform !== "win32") await fs.chmod(temporary, 0o755);
    await fs.rename(temporary, destination);
  } finally {
    await fs.rm(temporary, { force: true }).catch(() => {});
  }
}

export async function installCompiler({ target, destination, archivePath } = {}) {
  validateManifest();
  const selectedTarget = target || hostTarget();
  const entry = LUAU_COMPILER_TARGETS[selectedTarget];
  if (!entry) {
    throw new Error(
      `unknown target ${selectedTarget}; supported targets: ${Object.keys(LUAU_COMPILER_TARGETS).join(", ")}`,
    );
  }
  const output = path.resolve(
    destination || path.join(REPOSITORY_ROOT, "tools", "luau", selectedTarget, entry.executable),
  );
  if (!archivePath && await readExisting(output, entry.executableSha256)) {
    if (process.platform !== "win32") await fs.chmod(output, 0o755);
    return {
      cached: true,
      destination: output,
      executableSha256: entry.executableSha256,
      target: selectedTarget,
      version: LUAU_VERSION,
    };
  }

  const url = releaseUrl(entry.asset);
  const archive = archivePath
    ? await fs.readFile(path.resolve(archivePath))
    : await downloadWithRetries(url);
  if (archive.length > MAX_ARCHIVE_BYTES) {
    throw new Error(`Luau archive exceeds ${MAX_ARCHIVE_BYTES} bytes`);
  }
  const actualArchiveSha256 = sha256(archive);
  if (actualArchiveSha256 !== entry.archiveSha256) {
    throw new Error(
      `${selectedTarget}: archive SHA-256 mismatch; expected ${entry.archiveSha256}, got ${actualArchiveSha256}`,
    );
  }

  const executable = extractZipEntry(archive, entry.executable);
  const actualExecutableSha256 = sha256(executable);
  if (actualExecutableSha256 !== entry.executableSha256) {
    throw new Error(
      `${selectedTarget}: executable SHA-256 mismatch; expected ${entry.executableSha256}, got ${actualExecutableSha256}`,
    );
  }
  await installAtomically(output, executable);
  return {
    archiveSha256: actualArchiveSha256,
    cached: false,
    destination: output,
    executableSha256: actualExecutableSha256,
    source: url,
    target: selectedTarget,
    version: LUAU_VERSION,
  };
}

function parseArguments(argv) {
  const options = { json: false, verifyManifest: false };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    const takeValue = () => {
      const value = argv[++index];
      if (!value) throw new Error(`${argument} requires a value`);
      return value;
    };
    if (argument === "--target") options.target = takeValue();
    else if (argument === "--dest") options.destination = takeValue();
    else if (argument === "--archive") options.archivePath = takeValue();
    else if (argument === "--json") options.json = true;
    else if (argument === "--verify-manifest") options.verifyManifest = true;
    else if (argument === "--help" || argument === "-h") options.help = true;
    else throw new Error(`unknown argument: ${argument}`);
  }
  return options;
}

function printHelp() {
  console.log(`Usage: node scripts/install-luau-compiler.mjs [options]

Securely install the pinned Luau ${LUAU_VERSION} compiler used by rosync lint.

Options:
  --target <target>   darwin-arm64, linux-x86_64, or windows-x86_64
  --dest <path>       Override the tools/luau/<target>/ destination
  --archive <path>    Verify and extract an already-downloaded official archive
  --verify-manifest   Validate pinned checksums, release targets, docs, and license
  --json              Print the result as JSON
  -h, --help          Show this help
`);
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  if (options.help) {
    printHelp();
    return;
  }
  const result = options.verifyManifest
    ? await verifyRepositoryMetadata()
    : await installCompiler(options);
  if (options.json) console.log(JSON.stringify(result, null, 2));
  else if (options.verifyManifest) {
    console.log(`Luau compiler manifest OK: ${result.version} (${result.targets.join(", ")})`);
  } else {
    console.log(
      `${result.cached ? "using" : "installed"} Luau ${result.version} compiler for ${result.target}: ${result.destination}`,
    );
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === SCRIPT_PATH) {
  main().catch((error) => {
    console.error(`install-luau-compiler: ${error.message}`);
    process.exitCode = 1;
  });
}
