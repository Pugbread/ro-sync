// platform.js — cross-platform shims for commands the widget issues through
// the Terminal 64 host's `t64:exec` RPC. Detects the current platform once at
// load time (via navigator.userAgent) and exposes command generators / path
// constants that DIFFER between POSIX and Windows.
//
// Callers build commands by composition (e.g. `${mkdirp(x)} && ${copy(a,b)}`)
// so both sides stay readable without per-call platform branches.

const UA = (typeof navigator !== "undefined" && navigator.userAgent) || "";
export const PLATFORM =
  /Win(dows|32|64)|WOW64|WinNT/i.test(UA) ? "windows" :
  /Mac|Darwin/i.test(UA)                  ? "darwin"  :
  /Linux|X11|Android|CrOS/i.test(UA)      ? "linux"   :
                                            "darwin"; // safest unknown default

export const IS_WINDOWS = PLATFORM === "windows";
export const IS_POSIX   = !IS_WINDOWS;

// ---------- Binary selection ----------
// Release binaries are checked into (or distributed alongside) `daemon/`.
// Users on unsupported platforms build from source and drop their binary in.
export const BINARY_REL =
  PLATFORM === "windows" ? "daemon/rosync-windows-x86_64.exe" :
  PLATFORM === "linux"   ? "daemon/rosync-linux-x86_64"       :
                           "daemon/rosync-darwin-arm64";

// The bare process name used for pattern-match kills (must match BINARY_REL's
// trailing file name — on Windows that's the .exe file, elsewhere the binary).
export const BINARY_NAME = BINARY_REL.split("/").pop();

// ---------- Base paths (shell-level, NOT resolved in JS) ----------
// These expand when the host runs the command, so we don't need to know the
// actual user directory from JS.
export const WIDGET_DIR_SHELL = IS_WINDOWS
  ? `%USERPROFILE%\\.terminal64\\widgets\\ro-sync`
  : `$HOME/.terminal64/widgets/ro-sync`;

export const PLUGIN_DIR_DISPLAY = IS_WINDOWS
  ? `%LOCALAPPDATA%\\Roblox\\Plugins`
  : `~/Documents/Roblox/Plugins`;

export const PLUGIN_DIR_SHELL = IS_WINDOWS
  ? `%LOCALAPPDATA%\\Roblox\\Plugins`
  : `$HOME/Documents/Roblox/Plugins`;

// Path separator for joining WIDGET_DIR_SHELL + relative binary/plugin paths.
export const PATH_SEP = IS_WINDOWS ? "\\" : "/";

// Convert a forward-slash relative path (e.g. "daemon/rosync-..." ) to the
// native separator for composition with WIDGET_DIR_SHELL.
export function nativeRel(rel) {
  return IS_WINDOWS ? String(rel).replace(/\//g, "\\") : String(rel);
}

// ---------- Quoting ----------
// POSIX: single-quote wrap, escape ' as '\''
// Windows cmd: double-quote wrap, escape " as ""  (paths rarely contain ",
// but this keeps it safe). For PowerShell blocks we use the single-quoted form
// inside the `-Command "..."` string directly.
function posixQuote(s) { return "'" + String(s).replace(/'/g, "'\\''") + "'"; }
function winQuote(s)   { return '"' + String(s).replace(/"/g, '""') + '"'; }
export function shQuote(s) { return IS_WINDOWS ? winQuote(s) : posixQuote(s); }

// PowerShell single-quote escape (for use inside a PS script).
// PowerShell: single-quoted strings escape ' as '' and do not expand variables.
export function psQuote(s) { return "'" + String(s).replace(/'/g, "''") + "'"; }

// Encode a PowerShell script as UTF-16LE base64 so it can be passed via
// `powershell -EncodedCommand <base64>`. This ELIMINATES every quoting concern
// because the argument is pure [A-Za-z0-9+/=] — no shell can mangle it.
//
// Works in both browser (btoa) and Node (Buffer) contexts.
function toUtf16LEBase64(s) {
  const u8 = new Uint8Array(s.length * 2);
  for (let i = 0; i < s.length; i++) {
    const cp = s.charCodeAt(i);
    u8[i * 2]     = cp & 0xFF;
    u8[i * 2 + 1] = (cp >>> 8) & 0xFF;
  }
  if (typeof btoa === "function") {
    let bin = "";
    // Build binary string in chunks to avoid stack overflow on huge inputs.
    for (let i = 0; i < u8.length; i += 0x8000) {
      bin += String.fromCharCode.apply(null, u8.subarray(i, i + 0x8000));
    }
    return btoa(bin);
  }
  // Node fallback for tests.
  return Buffer.from(u8).toString("base64");
}

// Build a `powershell -EncodedCommand <base64>` invocation. Arg tokens are all
// bare ASCII, so no parent-shell can break them. Use for anything that would
// otherwise need embedded " inside -Command.
export function psEncodedCmd(psScript) {
  const b64 = toUtf16LEBase64(psScript);
  return `powershell -NoProfile -NonInteractive -EncodedCommand ${b64}`;
}

// ---------- Temp dir / log path ----------
export function tmpLogPath(name) {
  // Host expands the shell var when the command runs.
  return IS_WINDOWS ? `%TEMP%\\${name}` : `/tmp/${name}`;
}

// ---------- Command generators ----------

// Check whether a PID is alive. Stdout will contain "alive" or "dead".
export function pidAliveCmd(pid) {
  const n = parseInt(pid, 10);
  if (!Number.isFinite(n) || n <= 0) return `echo dead`;
  if (IS_WINDOWS) {
    return psEncodedCmd(
      `if (Get-Process -Id ${n} -ErrorAction SilentlyContinue) { 'alive' } else { 'dead' }`
    );
  }
  return `kill -0 ${n} 2>/dev/null && echo alive || echo dead`;
}

export function parsePidAlive(stdout) {
  return /alive/i.test(String(stdout || ""));
}

// Kill a single PID (SIGTERM equivalent).
export function killPidCmd(pid) {
  const n = parseInt(pid, 10);
  if (!Number.isFinite(n) || n <= 0) return `echo nope`;
  return IS_WINDOWS
    ? `taskkill /PID ${n} /F`
    : `/bin/kill ${n}`;
}

// Kill any process whose command line matches the binary name + --port <port>
// (fuzzy but adequate — nothing else should match both strings together).
// Waits briefly so the listener socket is actually free before returning.
export function killDaemonOnPortCmd(port) {
  const p = parseInt(port, 10);
  if (!Number.isFinite(p)) return `echo skip`;
  if (IS_WINDOWS) {
    // Match on process name + command-line substring via CIM; Stop-Process -Force.
    // Sleep 600ms after the kill so the port is fully released.
    const ps =
      `$procs = Get-CimInstance Win32_Process -Filter "Name='${BINARY_NAME}'" | ` +
      `Where-Object { $_.CommandLine -like '*--port ${p}*' }; ` +
      `if ($procs) { $procs | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue } }; ` +
      `Start-Sleep -Milliseconds 600`;
    return psEncodedCmd(ps);
  }
  const pat = `${BINARY_NAME}.*--port ${p}`;
  return (
    `pkill -f ${posixQuote(pat)} 2>/dev/null ; ` +
    `sleep 0.4 ; ` +
    `pkill -9 -f ${posixQuote(pat)} 2>/dev/null ; ` +
    `sleep 0.6 ; ` +
    `true`
  );
}

// Tail the last ~40 lines of a log file (best-effort; empty if missing).
export function tailLogCmd(path) {
  if (IS_WINDOWS) {
    return psEncodedCmd(
      `if (Test-Path ${psQuote(path)}) { Get-Content -Tail 40 ${psQuote(path)} }`
    );
  }
  return `tail -40 ${posixQuote(path)} 2>/dev/null || true`;
}

// Find which process owns a TCP listen socket, as "name(pid)" or empty.
export function portOwnerCmd(port) {
  const p = parseInt(port, 10);
  if (!Number.isFinite(p)) return `echo`;
  if (IS_WINDOWS) {
    const ps =
      `$c = Get-NetTCPConnection -LocalPort ${p} -State Listen -ErrorAction SilentlyContinue | Select -First 1; ` +
      `if ($c) { $proc = Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue; ` +
      `if ($proc) { $proc.ProcessName + '(' + $proc.Id + ')' } }`;
    return psEncodedCmd(ps);
  }
  return `lsof -nP -iTCP:${p} -sTCP:LISTEN 2>/dev/null | awk 'NR>1 {print $1 "(" $2 ")"}' | head -1`;
}

// Launch the daemon detached, redirect output to `logPath`, and print the PID
// on a line by itself after a "---" separator.
//
// Args are passed in UNQUOTED (raw strings). We apply platform-native quoting
// inside this function so callers don't need to know the convention.
//
// `binaryPath` may contain unexpanded env references ($HOME on POSIX,
// %USERPROFILE% on Windows) — the outer shell expands them before the command
// runs. On Windows, cmd.exe performs %VAR% expansion inside the double-quoted
// PowerShell -Command argument before PowerShell parses it.
export function launchDaemonCmd({ binaryPath, args, logPath, port }) {
  if (IS_WINDOWS) {
    // Expand %VAR% references inside PS via ExpandEnvironmentVariables — no
    // reliance on cmd-level pre-expansion. Start-Process -PassThru returns
    // the Process object synchronously, so we don't need to poll.
    const psArgs = args.map(psQuote).join(",");
    const ps =
      `$ErrorActionPreference = 'Stop'; ` +
      `$xp = { param($s) [Environment]::ExpandEnvironmentVariables($s) }; ` +
      `$bin = & $xp ${psQuote(binaryPath)}; ` +
      `$log = & $xp ${psQuote(logPath)}; ` +
      `$err = & $xp ${psQuote(logPath + ".err")}; ` +
      `$proc = Start-Process -FilePath $bin ` +
      `-ArgumentList @(${psArgs}) ` +
      `-PassThru -WindowStyle Hidden ` +
      `-RedirectStandardOutput $log ` +
      `-RedirectStandardError $err; ` +
      `Write-Output '---'; Write-Output $proc.Id`;
    return psEncodedCmd(ps);
  }
  // POSIX: double-quote binaryPath so $HOME expands; single-quote each arg so
  // spaces and metachars are preserved. Background the daemon under nohup and
  // poll pgrep up to ~3s for a slow start.
  const quotedArgs = args.map(posixQuote).join(" ");
  const grepPat = `${BINARY_NAME}.*--port ${port}`;
  return (
    `( nohup "${binaryPath}" ${quotedArgs} ` +
    `</dev/null >${posixQuote(logPath)} 2>&1 & ) ; ` +
    `for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 ; do ` +
    `  PID=$(pgrep -f ${posixQuote(grepPat)} | tail -1) ; ` +
    `  if [ -n "$PID" ] ; then echo "---" ; echo "$PID" ; exit 0 ; fi ; ` +
    `  sleep 0.2 ; ` +
    `done ; ` +
    `echo "---" ; echo ""`
  );
}

// ---------- Plugin install helpers ----------

// Atomic "install" command: ensure target dir exists, copy file in, then
// remove any stale alternate-extension file. Returns a SINGLE command string
// that works regardless of what shell t64:exec invokes, because Windows uses
// a self-contained PowerShell block and POSIX uses an sh-compatible chain.
//
// Arguments are RAW absolute paths (no shell quoting). This function does
// platform-appropriate quoting internally.
export function pluginInstallCmd({ srcFile, destDir, destName, staleName }) {
  if (IS_WINDOWS) {
    // Inside PS we expand env vars explicitly via [Environment]::ExpandEnvironmentVariables
    // instead of relying on cmd-level %VAR% expansion. Makes the command
    // shell-independent.
    const destFile  = destDir + "\\" + destName;
    const staleFile = destDir + "\\" + staleName;
    const ps =
      `$ErrorActionPreference = 'Stop'; ` +
      `$xp = { param($s) [Environment]::ExpandEnvironmentVariables($s) }; ` +
      `$src   = & $xp ${psQuote(srcFile)}; ` +
      `$dir   = & $xp ${psQuote(destDir)}; ` +
      `$dest  = & $xp ${psQuote(destFile)}; ` +
      `$stale = & $xp ${psQuote(staleFile)}; ` +
      `if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }; ` +
      `Copy-Item -Path $src -Destination $dest -Force; ` +
      `if (Test-Path $stale) { Remove-Item -Path $stale -Force }`;
    return psEncodedCmd(ps);
  }
  // POSIX: sh sequence — mkdir -p is idempotent, cp -f overwrites, rm -f
  // silently skips missing. All in one line so it works under any invoking
  // shell (even ones that pass args as a single string to /bin/sh).
  const dq = (s) => posixQuote(s);
  const dest = destDir + "/" + destName;
  const stale = destDir + "/" + staleName;
  return (
    `mkdir -p ${dq(destDir)} && ` +
    `cp -f ${dq(srcFile)} ${dq(dest)} && ` +
    `rm -f ${dq(stale)}`
  );
}

// Ensure a folder exists, then open it in the system file explorer.
export function openFolderEnsuredCmd(dir) {
  if (IS_WINDOWS) {
    const ps =
      `$dest = [Environment]::ExpandEnvironmentVariables(${psQuote(dir)}); ` +
      `if (-not (Test-Path $dest)) { New-Item -ItemType Directory -Path $dest -Force | Out-Null }; ` +
      `Start-Process explorer.exe -ArgumentList $dest`;
    return psEncodedCmd(ps);
  }
  const dq = posixQuote(dir);
  const opener = PLATFORM === "linux" ? "xdg-open" : "/usr/bin/open";
  return `mkdir -p ${dq} && ${opener} ${dq}`;
}

// Show a "pick folder" dialog and print the selected absolute path to stdout
// (empty string if the user cancels).
export function pickFolderCmd(prompt) {
  if (IS_WINDOWS) {
    // FolderBrowserDialog requires STA. Use encoded cmd so embedded quotes
    // can't be mangled by the host shell, and emit ONLY the path (no banner)
    // to stdout on success.
    const ps =
      `Add-Type -AssemblyName System.Windows.Forms | Out-Null; ` +
      `$dlg = New-Object System.Windows.Forms.FolderBrowserDialog; ` +
      `$dlg.Description = ${psQuote(prompt)}; ` +
      `$dlg.ShowNewFolderButton = $true; ` +
      `if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { ` +
      `  [Console]::Out.WriteLine($dlg.SelectedPath) }`;
    // -STA is required for Windows Forms dialogs.
    const b64 = toUtf16LEBase64(ps);
    return `powershell -NoProfile -NonInteractive -STA -EncodedCommand ${b64}`;
  }
  if (PLATFORM === "linux") {
    // zenity is on nearly every GNOME/GTK desktop; kdialog on KDE. Try both.
    const safe = String(prompt).replace(/"/g, '\\"');
    return (
      `zenity --file-selection --directory --title="${safe}" 2>/dev/null ` +
      `|| kdialog --getexistingdirectory "$HOME" --title "${safe}" 2>/dev/null ` +
      `|| true`
    );
  }
  const script =
    `tell application "System Events" to activate\n` +
    `POSIX path of (choose folder with prompt "${prompt.replace(/"/g, '\\"')}")`;
  const b64 = typeof btoa === "function"
    ? btoa(unescape(encodeURIComponent(script)))
    : Buffer.from(script, "utf8").toString("base64");
  return `echo ${b64} | base64 -d | osascript`;
}

// Write a base64-encoded UTF-8 string to `absPath`, creating/truncating.
export function writeFileFromB64Cmd(absPath, b64) {
  if (IS_WINDOWS) {
    const ps =
      `$p = [Environment]::ExpandEnvironmentVariables(${psQuote(absPath)}); ` +
      `[IO.File]::WriteAllBytes($p, [Convert]::FromBase64String(${psQuote(b64)}))`;
    return psEncodedCmd(ps);
  }
  return `echo ${posixQuote(b64)} | base64 -d > ${posixQuote(absPath)}`;
}

// Read a text file's full contents to stdout (empty if missing).
export function readFileCmd(absPath) {
  if (IS_WINDOWS) {
    return psEncodedCmd(
      `$p = [Environment]::ExpandEnvironmentVariables(${psQuote(absPath)}); ` +
      `if (Test-Path $p) { Get-Content -Raw $p }`
    );
  }
  return `cat ${posixQuote(absPath)} 2>/dev/null || true`;
}

// ---------- Path joining for shell-level concatenation ----------
// Join a $HOME-ish shell prefix with a relative path using the native sep.
// Example:  joinShell(WIDGET_DIR_SHELL, "daemon/rosync-...")
export function joinShell(...parts) {
  return parts
    .map((p, i) => (i === 0 ? String(p) : nativeRel(p)))
    .join(PATH_SEP)
    .replace(new RegExp(`${PATH_SEP === "\\" ? "\\\\" : "/"}{2,}`, "g"), PATH_SEP);
}
