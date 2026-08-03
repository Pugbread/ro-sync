import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Prefer the compiler `scripts/install-luau-compiler.mjs` pins, the same one
// `rosync lint` uses. Without this the check silently no-ops locally for anyone
// who has not put luau-compile on PATH — and this check is the only thing that
// catches the per-function register limit, whose failure mode in Studio is a
// plugin that simply never appears.
function pinnedCompiler() {
  const platform = process.platform === "win32"
    ? "windows"
    : process.platform === "darwin"
      ? "darwin"
      : "linux";
  const arch = process.arch === "arm64" ? "arm64" : "x64";
  const binary = platform === "windows" ? "luau-compile.exe" : "luau-compile";
  const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  const candidate = path.join(repoRoot, "tools", "luau", `${platform}-${arch}`, binary);
  return fs.existsSync(candidate) ? candidate : null;
}

const compiler = process.env.LUAU_COMPILE || pinnedCompiler() || "luau-compile";
const source = path.resolve(process.argv[2] || "plugin/Plugin.luau");

for (const optimization of ["0", "1", "2"]) {
  const result = spawnSync(
    compiler,
    ["--null", `-O${optimization}`, source],
    { encoding: "utf8", stdio: "pipe" },
  );

  if (result.error?.code === "ENOENT") {
    console.error(
      `Luau compiler not found at ${compiler}. Install luau-compile or set LUAU_COMPILE.`,
    );
    process.exit(127);
  }
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    process.stderr.write(result.stderr || result.stdout);
    console.error(`Luau bytecode compilation failed at -O${optimization}.`);
    process.exit(result.status ?? 1);
  }
}

console.log(`Luau bytecode compilation passed at -O0, -O1, and -O2: ${source}`);
