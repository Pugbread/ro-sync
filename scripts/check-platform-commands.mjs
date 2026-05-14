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

const installPs = decodePowerShell(win.pluginInstallCmd({
  srcFile: win.joinShell(win.WIDGET_DIR_SHELL, "plugin/Plugin.rbxm"),
  destDir: win.PLUGIN_DIR_SHELL,
  destName: "RoSync.rbxm",
  staleNames: ["RoSync.lua", "RoSync.luau"],
}));
assert(installPs.includes("[Environment]::ExpandEnvironmentVariables"), "install must expand env vars");
assert(installPs.includes("Copy-Item -LiteralPath"), "install must copy literal paths");
assert(installPs.includes("Remove-Item -LiteralPath"), "install must remove literal paths");
assert(installPs.includes("RoSync.rbxm"), "install must target rbxm");

const launchPs = decodePowerShell(win.launchDaemonCmd({
  binaryPath: win.joinShell(win.WIDGET_DIR_SHELL, win.BINARY_REL),
  args: ["--project", "C:\\Users\\Test User\\Game [Dev]", "--port", "7878"],
  logPath: win.tmpLogPath("rosync-7878.log"),
  port: 7878,
}));
assert(launchPs.includes("Test-Path -LiteralPath $bin"), "launch must probe literal binary path");
assert(launchPs.includes("-RedirectStandardError $err"), "launch must capture stderr");
assert(launchPs.includes("'\"C:\\Users\\Test User\\Game [Dev]\"'"), "launch must preserve spaced/bracketed project path");

const tailPs = decodePowerShell(win.tailLogCmd("%TEMP%\\rosync-7878.log"));
assert(tailPs.includes("[Environment]::ExpandEnvironmentVariables"), "tail must expand env vars");
assert(tailPs.includes("Get-Content -LiteralPath"), "tail must read literal path");

const buildPs = decodePowerShell(win.buildDaemonCmd());
assert(buildPs.includes(".\\build.ps1"), "build command must run build.ps1");
assert(buildPs.includes("___EXIT:"), "build command must emit exit sentinel");

const winWritePs = decodePowerShell(win.writeFileFromB64Cmd("%TEMP%\\config.json", "e30="));
assert(winWritePs.includes("[IO.File]::WriteAllBytes($tmp"), "Windows write must target temp file first");
assert(winWritePs.includes("Remove-Item -LiteralPath $p"), "Windows write must remove existing file before replace");
assert(winWritePs.includes("Move-Item -LiteralPath $tmp"), "Windows write must move temp file into place");

const mac = await loadPlatform("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)", "mac");
assert(mac.PLATFORM === "darwin", "Mac UA must select darwin platform");
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

console.log("platform command checks passed");
