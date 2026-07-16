import { spawnSync } from "node:child_process";
import path from "node:path";

const compiler = process.env.LUAU_COMPILE || "luau-compile";
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
