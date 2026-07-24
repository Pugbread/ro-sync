import { readFile } from "node:fs/promises";

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

function decodePowerShell(command) {
  const marker = "-EncodedCommand ";
  const index = command.indexOf(marker);
  assert(index >= 0, `missing ${marker}`);
  const encoded = command.slice(index + marker.length).trim().split(/\s+/)[0];
  return Buffer.from(encoded, "base64").toString("utf16le");
}

async function loadPlatform(userAgent, tag) {
  Object.defineProperty(globalThis, "navigator", {
    value: { userAgent },
    configurable: true,
  });
  return import(`../platform.js?${tag}=${Date.now()}`);
}

const win = await loadPlatform("Mozilla/5.0 (Windows NT 10.0; Win64; x64)", "win");
assert(win.PLATFORM === "windows", "Windows UA must select windows platform");
assert(win.BINARY_REL === "daemon/rosync-windows-x86_64.exe", "Windows binary mismatch");
assert(
  win.joinShell(win.WIDGET_DIR_SHELL, "daemon/rosync-windows-x86_64.exe") ===
    "%USERPROFILE%\\.terminal64\\widgets\\ro-sync\\daemon\\rosync-windows-x86_64.exe",
  "Windows joinShell must preserve env vars and backslashes",
);
assert(
  win.projectPathsEqual(
    "C:\\Users\\Test User\\Game",
    "\\\\?\\c:\\Users\\Test User\\Game\\",
  ),
  "Windows project identity must equate Rust canonical and ordinary drive spellings",
);
assert(
  win.projectPathsEqual(
    "\\\\server\\share\\Game",
    "\\\\?\\UNC\\server\\share\\Game\\",
  ),
  "Windows project identity must equate extended and ordinary UNC paths",
);
assert(
  win.projectPathsEqual(
    "\\\\SERVER\\Share\\Game\\Scripts",
    "\\\\?\\UNC\\server\\share\\Game\\Scripts\\",
  ),
  "Windows project identity must fold UNC server and share case",
);
assert(
  win.projectPathsEqual("C:/", "\\\\?\\c:\\"),
  "Windows project identity must preserve and equate drive roots",
);
assert(
  win.projectPathsEqual("//server/share/Game//", "\\\\server\\share\\Game"),
  "Windows project identity must normalize mixed and repeated separators",
);
assert(
  !win.projectPathsEqual("C:\\Games\\Case", "C:\\Games\\case"),
  "Windows project identity must not conflate case-sensitive NTFS paths",
);
assert(
  !win.projectPathsEqual(
    "\\\\SERVER\\Share\\Game\\Scripts",
    "\\\\server\\share\\game\\Scripts",
  ),
  "Windows project identity must preserve descendant case below a UNC share",
);
assert(
  !win.projectPathsEqual("C:\\Games\\One", "C:\\Games\\Two"),
  "Windows project identity must keep different directories distinct",
);
const trackedDaemon = {
  pid: 4242,
  port: 7878,
  project: "c:\\Games\\Project",
  canonicalProject: "\\\\?\\C:\\Games\\Project",
  bootId: "boot-one",
};
const trackedHello = {
  pid: 4242,
  port: 7878,
  project: "C:\\Games\\Project",
  bootId: "boot-one",
};
assert(
  win.daemonIdentityMatchesTrackedSession(trackedDaemon, trackedHello),
  "Windows tracked daemon identity must include equivalent project, PID, port, and boot ID",
);
assert(
  !win.daemonIdentityMatchesTrackedSession(
    trackedDaemon,
    { ...trackedHello, bootId: "boot-two" },
  ),
  "A reused PID from another daemon boot must not match a tracked session",
);
assert(
  !win.daemonIdentityMatchesTrackedSession(
    trackedDaemon,
    { ...trackedHello, pid: 4243 },
  ),
  "A different listener PID must not match a tracked session",
);
assert(
  !win.daemonIdentityMatchesTrackedSession(
    trackedDaemon,
    { ...trackedHello, port: 7879 },
  ),
  "A different listener port must not match a tracked session",
);

const installPs = decodePowerShell(win.pluginInstallCmd({
  srcFile: win.joinShell(win.WIDGET_DIR_SHELL, "plugin/Plugin.rbxm"),
  destDir: win.PLUGIN_DIR_SHELL,
  destName: "RoSync.rbxm",
  staleNames: ["RoSync.lua", "RoSync.luau"],
}));
assert(installPs.includes("[Environment]::ExpandEnvironmentVariables"), "install must expand env vars");
assert(installPs.includes("Copy-Item -LiteralPath"), "install must copy literal paths");
assert(installPs.includes("Remove-Item -LiteralPath"), "install must remove literal paths");
assert(installPs.includes("[IO.File]::Replace($tmp, $dest"), "install must atomically replace the existing plugin");
assert(installPs.includes("[IO.File]::Move($tmp, $dest)"), "install must atomically install a new plugin");
assert(
  !installPs.includes("Copy-Item -LiteralPath $src -Destination $dest"),
  "install must stage bytes before replacing the live plugin",
);
assert(installPs.includes("RoSync.rbxm"), "install must target rbxm");

const forbiddenOwnerToken = "owner-token-must-never-enter-command-or-argv";
const launchPs = decodePowerShell(win.launchDaemonCmd({
  binaryPath: win.joinShell(win.WIDGET_DIR_SHELL, win.BINARY_REL),
  args: ["--project", "C:\\Users\\Test User\\Game [Dev]", "--port", "7878"],
  logPath: win.tmpLogPath("rosync-7878.log"),
  port: 7878,
  ownerTokenStatePath: win.joinShell(win.WIDGET_DIR_SHELL, "state.json"),
  ownerToken: forbiddenOwnerToken,
}));
assert(launchPs.includes("Test-Path -LiteralPath $bin"), "launch must probe literal binary path");
assert(launchPs.includes("-RedirectStandardError $err"), "launch must capture stderr");
assert(launchPs.includes("'\"C:\\Users\\Test User\\Game [Dev]\"'"), "launch must preserve spaced/bracketed project path");
assert(launchPs.includes("--owner-token-state-file"), "Windows launch must use the private widget state source");
assert(!launchPs.includes(forbiddenOwnerToken), "Windows launch must never embed the owner token");

const tailPs = decodePowerShell(win.tailLogCmd("%TEMP%\\rosync-7878.log"));
assert(tailPs.includes("[Environment]::ExpandEnvironmentVariables"), "tail must expand env vars");
assert(tailPs.includes("Get-Content -LiteralPath"), "tail must read literal path");
const portOwnerPs = decodePowerShell(win.portOwnerCmd(7878));
assert(
  portOwnerPs.includes("$_.LocalAddress -eq '127.0.0.1'") &&
    portOwnerPs.includes("$_.LocalAddress -eq '0.0.0.0'"),
  "Windows owner lookup must ignore coexisting IPv6-only listeners",
);

const buildPs = decodePowerShell(win.buildDaemonCmd());
assert(buildPs.includes(".\\build.ps1"), "build command must run build.ps1");
assert(buildPs.includes("___EXIT:"), "build command must emit exit sentinel");
const windowsBuildScript = await readFile(new URL("../daemon/build.ps1", import.meta.url), "utf8");
assert(
  windowsBuildScript.includes("--target $target"),
  "Windows source build must compile the advertised target explicitly",
);
assert(
  windowsBuildScript.includes("target\\$target\\release\\rosync.exe"),
  "Windows source build must copy from the target-specific Cargo directory",
);
assert(
  windowsBuildScript.includes("[IO.File]::Replace($staged, $destination"),
  "Windows source build must atomically replace an existing daemon binary",
);
assert(
  windowsBuildScript.includes("[IO.File]::Move($staged, $destination)"),
  "Windows source build must atomically install a new daemon binary",
);
assert(
  !windowsBuildScript.includes("Copy-Item -LiteralPath $built -Destination 'rosync-windows-x86_64.exe'"),
  "Windows source build must stage output instead of copying over the live binary",
);

for (const view of ["active.js", "projects.js"]) {
  const source = await readFile(new URL(`../views/${view}`, import.meta.url), "utf8");
  assert(
    !source.includes('daemonJson(base, "/snapshot")'),
    `${view} status refresh must not materialize the whole project snapshot`,
  );
  assert(
    source.includes('daemonJson(base, "/hello")'),
    `${view} status refresh must use the constant-size hello endpoint`,
  );
}

const winWritePs = decodePowerShell(win.writeFileFromB64Cmd("%TEMP%\\config.json", "e30="));
assert(winWritePs.includes("[IO.File]::WriteAllBytes($tmp"), "Windows write must target temp file first");
assert(winWritePs.includes("[IO.File]::Replace($tmp, $p"), "Windows write must atomically replace an existing file");
assert(winWritePs.includes("[IO.File]::Move($tmp, $p)"), "Windows write must move a new file into place");
assert(
  !winWritePs.includes("Remove-Item -LiteralPath $p"),
  "Windows write must never delete the destination before replacement succeeds",
);

const mac = await loadPlatform("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)", "mac");
assert(mac.PLATFORM === "darwin", "Mac UA must select darwin platform");
assert(
  !mac.projectPathsEqual("/tmp/Game", "/tmp/game"),
  "POSIX project identity must remain case-sensitive",
);
assert(
  mac.portOwnerCmd(7878).includes("-i4TCP:7878"),
  "POSIX owner lookup must inspect the IPv4 listener used by the daemon",
);
const macInstall = mac.pluginInstallCmd({
  srcFile: mac.joinShell(mac.WIDGET_DIR_SHELL, "plugin/Plugin.rbxm"),
  destDir: mac.PLUGIN_DIR_SHELL,
  destName: "RoSync.rbxm",
  staleNames: ["RoSync.lua", "RoSync.luau"],
});
assert(macInstall.includes('"$HOME/Documents/Roblox/Plugins"'), "POSIX install must expand HOME");
assert(!macInstall.includes("'$HOME/Documents/Roblox/Plugins'"), "POSIX install must not single-quote HOME");

const macWrite = mac.writeFileFromB64Cmd("$HOME/project/config.json", "e30=");
assert(macWrite.includes("base64 --decode"), "POSIX base64 decode must support GNU base64");
assert(macWrite.includes("base64 -D"), "POSIX base64 decode must support macOS BSD base64");
assert(macWrite.includes("> \"$tmp\""), "POSIX write must target temp file first");
assert(macWrite.includes("mv -f \"$tmp\""), "POSIX write must replace from temp file");

const macPick = mac.pickFolderCmd("Pick Folder");
assert(macPick.includes("base64 --decode"), "macOS folder picker must support GNU base64");
assert(macPick.includes("base64 -D"), "macOS folder picker must support BSD base64");

const macBuild = mac.buildDaemonCmd();
assert(macBuild.includes("bash ./build.sh"), "POSIX build must run build.sh");
assert(!macBuild.includes('CARGO="$HOME/.cargo/bin/cargo"'), "POSIX build must not force home cargo");

const macLaunch = mac.launchDaemonCmd({
  binaryPath: mac.joinShell(mac.WIDGET_DIR_SHELL, mac.BINARY_REL),
  args: ["serve", "--project", "/tmp/Game", "--port", "7878", "--widget-owned"],
  logPath: mac.tmpLogPath("rosync-7878.log"),
  port: 7878,
  ownerTokenStatePath: mac.joinShell(mac.WIDGET_DIR_SHELL, "state.json"),
  ownerToken: forbiddenOwnerToken,
});
assert(macLaunch.includes("--owner-token-state-file"), "POSIX launch must use the private widget state source");
assert(macLaunch.includes('"$HOME/.terminal64/widgets/ro-sync/state.json"'), "POSIX state path must expand HOME");
assert(macLaunch.includes("chmod 600"), "POSIX launch must harden widget state before reading its token");
assert(!macLaunch.includes(forbiddenOwnerToken), "POSIX launch must never embed the owner token");

const secureState = mac.secureWidgetStateCmd();
assert(secureState.includes("chmod 600"), "POSIX state writes must restore mode 0600");
assert(secureState.includes('"$HOME/.terminal64/widgets/ro-sync/state.json"'), "state hardening must expand HOME");

console.log("platform command checks passed");
