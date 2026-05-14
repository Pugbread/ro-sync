import { copyFile, mkdir } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const pluginDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(pluginDir, "..");
const pluginSrcDir = path.join(repoRoot, "plugin-src");

function run(command, args) {
  const result = spawnSync(command, args, {
    cwd: pluginSrcDir,
    stdio: "inherit",
    shell: process.platform === "win32",
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

await mkdir(path.join(pluginSrcDir, "src"), { recursive: true });
await copyFile(
  path.join(repoRoot, "plugin", "Plugin.luau"),
  path.join(pluginSrcDir, "src", "RoSync.server.luau"),
);

run("wally", ["install"]);
run("rojo", ["build", "plugin.project.json", "--output", "../plugin/Plugin.rbxm"]);
