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

// Rust's std::fs::canonicalize() returns extended-length paths on Windows
// (`\\?\C:\...` / `\\?\UNC\...`). Folder pickers and drag/drop return ordinary
// Win32 paths, so raw string equality would make the widget reject the daemon
// it just launched after a renderer reload. Keep path component case exact:
// Windows supports case-sensitive directories, and tracked sessions bridge an
// ordinary UI spelling to the exact canonical spelling returned by /hello.
export function normalizeProjectPathForComparison(value) {
  let path = String(value ?? "");
  if (!IS_WINDOWS) return path;

  path = path.replace(/\//g, "\\");
  if (/^\\\\\?\\UNC\\/i.test(path)) {
    path = "\\\\" + path.slice("\\\\?\\UNC\\".length);
  } else if (/^\\\\\?\\/i.test(path)) {
    path = path.slice("\\\\?\\".length);
  }
  // Drive designators are case-insensitive even inside a case-sensitive NTFS
  // directory. Normalize only that designator; preserve every component.
  path = path.replace(/^([a-z]):/, (_, drive) => `${drive.toUpperCase()}:`);

  // Preserve the two-character UNC introducer while collapsing redundant
  // separators elsewhere. UNC server and share identifiers are
  // case-insensitive on Windows, even when a directory below the share opts
  // into case sensitivity. Normalize exactly those two components and preserve
  // every descendant spelling. A drive root keeps its final separator because
  // `C:\` and drive-relative `C:` are not the same location.
  if (path.startsWith("\\\\")) {
    path = "\\\\" + path.slice(2).replace(/\\+/g, "\\");
    const components = path.slice(2).split("\\");
    if (components.length >= 2 && components[0] && components[1]) {
      components[0] = components[0].toUpperCase();
      components[1] = components[1].toUpperCase();
      path = "\\\\" + components.join("\\");
    }
  } else {
    path = path.replace(/\\+/g, "\\");
  }
  if (!/^[A-Za-z]:\\$/.test(path)) path = path.replace(/\\+$/, "");
  return path;
}

export function projectPathsEqual(left, right) {
  if (left == null || right == null) return false;
  return normalizeProjectPathForComparison(left) === normalizeProjectPathForComparison(right);
}

export function daemonIdentityMatchesTrackedSession(session, info) {
  if (!session || !info || typeof session !== "object" || typeof info !== "object") return false;
  const expectedPid = Number.parseInt(session.pid, 10);
  const actualPid = Number.parseInt(info.pid, 10);
  const expectedPort = Number.parseInt(session.port, 10);
  const actualPort = Number.parseInt(info.port, 10);
  if (!Number.isFinite(expectedPid) || expectedPid <= 0 || actualPid !== expectedPid) return false;
  if (!Number.isFinite(expectedPort) || expectedPort <= 0 || actualPort !== expectedPort) return false;
  if (!session.bootId || info.bootId !== session.bootId) return false;
  return projectPathsEqual(session.canonicalProject || session.project, info.project);
}

// ---------- Quoting ----------
// POSIX: single-quote wrap, escape ' as '\''
// Windows cmd: double-quote wrap, escape " as ""  (paths rarely contain ",
// but this keeps it safe). For PowerShell blocks we use the single-quoted form
// inside the `-Command "..."` string directly.
function posixQuote(s) { return "'" + String(s).replace(/'/g, "'\\''") + "'"; }
function posixExpandQuote(s) {
  return '"' + String(s).replace(/(["\\`])/g, "\\$1") + '"';
}
function winQuote(s)   { return '"' + String(s).replace(/"/g, '""') + '"'; }
export function shQuote(s) { return IS_WINDOWS ? winQuote(s) : posixQuote(s); }

// PowerShell single-quote escape (for use inside a PS script).
// PowerShell: single-quoted strings escape ' as '' and do not expand variables.
export function psQuote(s) { return "'" + String(s).replace(/'/g, "''") + "'"; }

// Start-Process joins -ArgumentList arrays with spaces before creating the
// child process. Include literal double quotes inside each PowerShell string so
// args containing spaces survive that flattening as one argv token.
export function psArgQuote(s) {
  const arg = String(s)
    .replace(/(\\*)"/g, '$1$1\\"')
    .replace(/(\\+)$/g, "$1$1");
  return psQuote(`"${arg}"`);
}

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

function posixBase64DecodeStdinCmd() {
  return `{ if base64 --help 2>&1 | grep -q -- '--decode'; then base64 --decode; else base64 -D; fi; }`;
}

// Build a `powershell -EncodedCommand <base64>` invocation. Arg tokens are all
// bare ASCII, so no parent-shell can break them. Use for anything that would
// otherwise need embedded " inside -Command.
//
// The UTF-8 prelude forces PowerShell to emit stdout as UTF-8 instead of the
// console code page (often cp437/1252), so non-ASCII characters (em-dashes,
// emoji, localized paths) round-trip through the host shell intact.
export function psEncodedCmd(psScript) {
  const prelude =
    `[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; ` +
    `$OutputEncoding = [System.Text.Encoding]::UTF8; `;
  const b64 = toUtf16LEBase64(prelude + psScript);
  return `powershell -NoProfile -NonInteractive -EncodedCommand ${b64}`;
}

// ---------- Temp dir / log path ----------
export function tmpLogPath(name) {
  // Host expands the shell var when the command runs.
  return IS_WINDOWS ? `%TEMP%\\${name}` : `/tmp/${name}`;
}

// Terminal 64 keeps state and secrets together in this ignored JSON file.
// Unix umasks are not guaranteed to be private, so harden it after every host
// state write and again immediately before a daemon reads its owner token.
export function secureWidgetStateCmd(
  path = joinShell(WIDGET_DIR_SHELL, "state.json"),
) {
  if (IS_WINDOWS) return "echo secured";
  const expanded = posixExpandQuote(path);
  return `if [ -f ${expanded} ]; then chmod 600 ${expanded}; else exit 1; fi`;
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

// Tail the last ~40 lines of a log file (best-effort; empty if missing).
export function tailLogCmd(path) {
  if (IS_WINDOWS) {
    return psEncodedCmd(
      `$p = [Environment]::ExpandEnvironmentVariables(${psQuote(path)}); ` +
      `if (Test-Path -LiteralPath $p) { Get-Content -LiteralPath $p -Tail 40 -Encoding UTF8 }`
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
      `$c = Get-NetTCPConnection -LocalPort ${p} -State Listen -ErrorAction SilentlyContinue | ` +
      `Where-Object { $_.LocalAddress -eq '127.0.0.1' -or $_.LocalAddress -eq '0.0.0.0' } | ` +
      `Select-Object -First 1; ` +
      `if ($c) { $proc = Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue; ` +
      `if ($proc) { $proc.ProcessName + '(' + $proc.Id + ')' } }`;
    return psEncodedCmd(ps);
  }
  return `lsof -nP -i4TCP:${p} -sTCP:LISTEN 2>/dev/null | awk 'NR>1 {print $1 "(" $2 ")"}' | head -1`;
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
export function launchDaemonCmd({ binaryPath, args, logPath, port, ownerTokenStatePath = null }) {
  if (IS_WINDOWS) {
    // Wrap Start-Process in try/catch so we ALWAYS emit a structured response
    // on stdout: `---\n<pid>` on success, `---\nERROR: <message>` on failure.
    // This stops PowerShell's default CLIXML error-serialization from leaking
    // into the hint the widget displays.
    const psArgs = args.map(psArgQuote).join(",");
    const ownerStateSetup = ownerTokenStatePath
      ? `$ownerState = & $xp ${psQuote(ownerTokenStatePath)}; ` +
        `if (-not (Test-Path -LiteralPath $ownerState)) { throw ('widget state file not found: ' + $ownerState) }; ` +
        `$launchArgs = @(${psArgs}); ` +
        `$launchArgs += @(${psArgQuote("--owner-token-state-file")}, ('"' + $ownerState + '"')); `
      : `$launchArgs = @(${psArgs}); `;
    const ps =
      `$ErrorActionPreference = 'Stop'; ` +
      `$ProgressPreference = 'SilentlyContinue'; ` +
      `$xp = { param($s) [Environment]::ExpandEnvironmentVariables($s) }; ` +
      `try { ` +
      `  $bin = & $xp ${psQuote(binaryPath)}; ` +
      `  if (-not (Test-Path -LiteralPath $bin)) { throw ('binary not found — open Settings -> Build daemon to build it, or download from GitHub Releases. Missing: ' + $bin) }; ` +
      `  $log = & $xp ${psQuote(logPath)}; ` +
      `  $err = & $xp ${psQuote(logPath + ".err")}; ` +
      ownerStateSetup +
      `  $proc = Start-Process -FilePath $bin ` +
      `    -ArgumentList $launchArgs ` +
      `    -PassThru -WindowStyle Hidden ` +
      `    -RedirectStandardOutput $log ` +
      `    -RedirectStandardError $err; ` +
      `  Write-Output '---'; Write-Output $proc.Id ` +
      `} catch { ` +
      `  Write-Output '---'; Write-Output ('ERROR: ' + $_.Exception.Message) ` +
      `}`;
    return psEncodedCmd(ps);
  }
  // POSIX: double-quote binaryPath so $HOME expands; single-quote each arg so
  // spaces and metachars are preserved. Pre-flight: if the binary doesn't
  // exist, short-circuit with an ERROR sentinel the widget parses.
  //
  // Print `$!` from the launch shell instead of using pgrep. The old pgrep
  // approach matched by `--port`, which could report a stale daemon from a
  // different project as the process we just launched.
  const quotedArgs = args.map(posixQuote).join(" ");
  const ownerStateArg = ownerTokenStatePath
    ? ` --owner-token-state-file ${posixExpandQuote(ownerTokenStatePath)}`
    : "";
  const ownerStatePreflight = ownerTokenStatePath
    ? `if [ ! -f ${posixExpandQuote(ownerTokenStatePath)} ] || ! chmod 600 ${posixExpandQuote(ownerTokenStatePath)} ; then ` +
      `  echo "---" ; echo "ERROR: widget state file is missing or could not be secured" ; exit 0 ; fi ; `
    : "";
  return (
    `if [ ! -x "${binaryPath}" ] ; then ` +
    `  echo "---" ; ` +
    `  echo "ERROR: binary not found — open Settings -> Build daemon to build it, or download from GitHub Releases. Missing: ${binaryPath}" ; ` +
    `  exit 0 ; ` +
    `fi ; ` +
    ownerStatePreflight +
    `nohup "${binaryPath}" ${quotedArgs}${ownerStateArg} ` +
    `</dev/null >${posixQuote(logPath)} 2>&1 & ` +
    `PID=$! ; ` +
    `echo "---" ; echo "$PID"`
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
export function pluginInstallCmd({ srcFile, destDir, destName, staleName, staleNames }) {
  const staleList = Array.isArray(staleNames)
    ? staleNames
    : (staleName ? [staleName] : []);
  if (IS_WINDOWS) {
    // Inside PS we expand env vars explicitly via [Environment]::ExpandEnvironmentVariables
    // instead of relying on cmd-level %VAR% expansion. Makes the command
    // shell-independent.
    const destFile  = destDir + "\\" + destName;
    const staleDeletes = staleList.map((name) => {
      const staleFile = destDir + "\\" + name;
      return `$stale = & $xp ${psQuote(staleFile)}; if (Test-Path -LiteralPath $stale) { Remove-Item -LiteralPath $stale -Force }; `;
    }).join("");
    const ps =
      `$ErrorActionPreference = 'Stop'; ` +
      `$xp = { param($s) [Environment]::ExpandEnvironmentVariables($s) }; ` +
      `$src   = & $xp ${psQuote(srcFile)}; ` +
      `$dir   = & $xp ${psQuote(destDir)}; ` +
      `$dest  = & $xp ${psQuote(destFile)}; ` +
      `if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }; ` +
      `$tmp = Join-Path $dir ('.' + [IO.Path]::GetFileName($dest) + '.' + [Diagnostics.Process]::GetCurrentProcess().Id + '.tmp'); ` +
      `try { ` +
      `  Copy-Item -LiteralPath $src -Destination $tmp -Force; ` +
      `  if (Test-Path -LiteralPath $dest) { ` +
      `    [IO.File]::Replace($tmp, $dest, $null, $true) ` +
      `  } else { ` +
      `    [IO.File]::Move($tmp, $dest) ` +
      `  }; ` +
      staleDeletes +
      `} catch { ` +
      `  if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }; ` +
      `  throw ` +
      `}`;
    return psEncodedCmd(ps);
  }
  // POSIX: sh sequence — mkdir -p is idempotent, cp -f overwrites, rm -f
  // silently skips missing. All in one line so it works under any invoking
  // shell (even ones that pass args as a single string to /bin/sh).
  const dq = (s) => posixExpandQuote(s);
  const dest = destDir + "/" + destName;
  const stale = staleList.map((name) => destDir + "/" + name);
  return (
    `mkdir -p ${dq(destDir)} && ` +
    `cp -f ${dq(srcFile)} ${dq(dest)} && ` +
    `rm -f ${stale.map(dq).join(" ")}`
  );
}

// Ensure a folder exists, then open it in the system file explorer.
export function openFolderEnsuredCmd(dir) {
  if (IS_WINDOWS) {
    // Wrap $dest in embedded double-quotes so explorer.exe sees a single path
    // argument even when it contains spaces (e.g. `C:\Users\My Name\...`).
    const ps =
      `$dest = [Environment]::ExpandEnvironmentVariables(${psQuote(dir)}); ` +
      `if (-not (Test-Path -LiteralPath $dest)) { New-Item -ItemType Directory -Path $dest -Force | Out-Null }; ` +
      `Start-Process explorer.exe -ArgumentList ('"' + $dest + '"')`;
    return psEncodedCmd(ps);
  }
  const dq = posixExpandQuote(dir);
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
      `[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; ` +
      `$OutputEncoding = [System.Text.Encoding]::UTF8; ` +
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
  return `printf %s ${posixQuote(b64)} | ${posixBase64DecodeStdinCmd()} | osascript`;
}

// Write a base64-encoded UTF-8 string to `absPath`, creating/truncating.
export function writeFileFromB64Cmd(absPath, b64) {
  if (IS_WINDOWS) {
    const ps =
      `$ErrorActionPreference = 'Stop'; ` +
      `$p = [Environment]::ExpandEnvironmentVariables(${psQuote(absPath)}); ` +
      `$dir = Split-Path -Parent $p; ` +
      `if (-not $dir) { $dir = (Get-Location).Path }; ` +
      `[IO.Directory]::CreateDirectory($dir) | Out-Null; ` +
      `$tmp = Join-Path $dir ('.' + [IO.Path]::GetFileName($p) + '.' + [Diagnostics.Process]::GetCurrentProcess().Id + '.tmp'); ` +
      `try { ` +
      `  [IO.File]::WriteAllBytes($tmp, [Convert]::FromBase64String(${psQuote(b64)})); ` +
      `  if (Test-Path -LiteralPath $p) { ` +
      `    [IO.File]::Replace($tmp, $p, $null, $true) ` +
      `  } else { ` +
      `    [IO.File]::Move($tmp, $p) ` +
      `  } ` +
      `} catch { ` +
      `  if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }; ` +
      `  throw ` +
      `}`;
    return psEncodedCmd(ps);
  }
  const q = posixQuote(absPath);
  return (
    `dir=$(dirname -- ${q}) && ` +
    `base=$(basename -- ${q}) && ` +
    `mkdir -p "$dir" && ` +
    `tmp="$dir/.$base.$$.tmp" && ` +
    `trap 'rm -f "$tmp"' EXIT && ` +
    `printf %s ${posixQuote(b64)} | ${posixBase64DecodeStdinCmd()} > "$tmp" && ` +
    `mv -f "$tmp" ${q} && ` +
    `trap - EXIT`
  );
}

// Read a text file's full contents to stdout (empty if missing).
export function readFileCmd(absPath) {
  if (IS_WINDOWS) {
    // -Encoding UTF8 is essential: PS 5.1's default `Default` encoding is the
    // system ANSI code page, which mangles any JSON/config file written as UTF-8
    // (project configs, writes.log, anything we authored ourselves).
    return psEncodedCmd(
      `$p = [Environment]::ExpandEnvironmentVariables(${psQuote(absPath)}); ` +
      `if (Test-Path -LiteralPath $p) { Get-Content -LiteralPath $p -Raw -Encoding UTF8 }`
    );
  }
  return `cat ${posixQuote(absPath)} 2>/dev/null || true`;
}

// ---------- Binary presence + build helpers ----------

// Probe whether the daemon binary exists. Stdout is "yes" or "no".
export function checkBinaryCmd() {
  const rel = joinShell(WIDGET_DIR_SHELL, BINARY_REL);
  if (IS_WINDOWS) {
    return psEncodedCmd(
      `$p = [Environment]::ExpandEnvironmentVariables(${psQuote(rel)}); ` +
      `if (Test-Path -LiteralPath $p) { 'yes' } else { 'no' }`
    );
  }
  return `if [ -x ${posixExpandQuote(rel)} ]; then echo yes; else echo no; fi`;
}

// Probe whether a `cargo` toolchain is on PATH. Stdout is "yes" or "no".
export function checkCargoCmd() {
  if (IS_WINDOWS) {
    // rustup's Windows installer puts cargo under %USERPROFILE%\.cargo\bin and
    // updates the User PATH, but the host shell may have been launched before
    // PATH refreshed. Fall back to the canonical install path so a fresh
    // rustup install doesn't require a logout/login to be visible.
    return psEncodedCmd(
      `if (Get-Command cargo -ErrorAction SilentlyContinue) { 'yes' } ` +
      `elseif (Test-Path (Join-Path $env:USERPROFILE '.cargo\\bin\\cargo.exe')) { 'yes' } ` +
      `else { 'no' }`
    );
  }
  // POSIX login shells often don't inherit the cargo path, so also look in
  // the canonical $HOME/.cargo/bin as a fallback.
  return (
    `if command -v cargo >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo" ]; ` +
    `then echo yes; else echo no; fi`
  );
}

// Run the platform-appropriate build script. Emits combined stdout+stderr
// followed by a final line `___EXIT:<code>` so the caller can parse the
// outcome without relying on t64:exec's own exit-code surface.
export function buildDaemonCmd() {
  if (IS_WINDOWS) {
    const ps =
      `$ErrorActionPreference = 'Continue'; ` +
      `$ProgressPreference = 'SilentlyContinue'; ` +
      `$dir = [Environment]::ExpandEnvironmentVariables(${psQuote(joinShell(WIDGET_DIR_SHELL, "daemon"))}); ` +
      `Push-Location $dir; ` +
      `try { ` +
      `  & powershell -NoProfile -ExecutionPolicy Bypass -File .\\build.ps1 2>&1 | ForEach-Object { $_.ToString() }; ` +
      `  $code = $LASTEXITCODE; ` +
      `  if ($null -eq $code) { $code = 0 }; ` +
      `  Write-Output ('___EXIT:' + $code) ` +
      `} catch { ` +
      `  Write-Output $_.Exception.Message; ` +
      `  Write-Output '___EXIT:1' ` +
      `} finally { Pop-Location }`;
    return psEncodedCmd(ps);
  }
  // POSIX: cd into daemon, run build.sh, capture exit code. Prefer the user's
  // cargo if present on PATH; build.sh falls back to $HOME/.cargo/bin/cargo.
  return (
    `cd ${posixExpandQuote(joinShell(WIDGET_DIR_SHELL, "daemon"))} && ` +
    `bash ./build.sh 2>&1; ` +
    `echo "___EXIT:$?"`
  );
}

// Inspect or resolve startup-blocking filesystem projection conflicts without
// starting the daemon. The renderer can only select an opaque conflict ID and
// an exact candidate name; it never receives a generic shell or rename
// primitive.
export function projectionRepairCmd({ project, resolveId = null, keep = null }) {
  const binaryPath = joinShell(WIDGET_DIR_SHELL, BINARY_REL);
  const args = [
    "repair",
    "projection",
    "--project",
    String(project),
    "--raw",
  ];
  if (resolveId != null) {
    args.push("--resolve", String(resolveId));
    if (keep != null && String(keep)) args.push("--keep", String(keep));
  }

  if (IS_WINDOWS) {
    const psArgs = args.map(psQuote).join(", ");
    return psEncodedCmd(
      `$ErrorActionPreference = 'Stop'; ` +
      `$bin = [Environment]::ExpandEnvironmentVariables(${psQuote(binaryPath)}); ` +
      `if (-not (Test-Path -LiteralPath $bin -PathType Leaf)) { ` +
      `  throw ('Ro Sync CLI not found: ' + $bin) ` +
      `}; ` +
      `$argv = @(${psArgs}); ` +
      `& $bin @argv; ` +
      `exit $LASTEXITCODE`
    );
  }

  return `${posixExpandQuote(binaryPath)} ${args.map(posixQuote).join(" ")}`;
}

// Parse the `___EXIT:<code>` tail emitted by buildDaemonCmd. Returns
// `{ ok, code, log }` where `log` is the build output without the sentinel.
export function parseBuildOutput(stdout) {
  const s = typeof stdout === "string" ? stdout : "";
  const m = s.match(/___EXIT:(\d+)\s*$/);
  const code = m ? parseInt(m[1], 10) : NaN;
  const log = m ? s.slice(0, m.index).trimEnd() : s.trimEnd();
  return { ok: code === 0, code: Number.isFinite(code) ? code : null, log };
}

// Install the Wally package graph rooted at `cwd`. This remains a Terminal 64
// implementation detail; packaged desktop builds expose a narrow native
// `wally_install` command instead of forwarding a shell string from the UI.
export function wallyInstallCmd(cwd) {
  if (IS_WINDOWS) {
    return psEncodedCmd(
      `$aftmanBin = Join-Path $env:USERPROFILE '.aftman\\bin'; ` +
      `$env:PATH = "$aftmanBin;$env:LOCALAPPDATA\\aftman\\bin;$env:PATH"; ` +
      `Set-Location -LiteralPath ${psQuote(cwd)}; ` +
      `$wally = Get-Command wally -ErrorAction SilentlyContinue; ` +
      `if ($wally) { & $wally.Source install; exit $LASTEXITCODE }; ` +
      `$aftman = Get-Command aftman -ErrorAction SilentlyContinue; ` +
      `if ($aftman) { & $aftman.Source run wally install; exit $LASTEXITCODE }; ` +
      `Write-Error 'wally not found on PATH and aftman is not available'; exit 127`
    );
  }
  const script =
    `export PATH="$HOME/.aftman/bin:$HOME/.local/share/aftman/bin:$HOME/.wally/bin:$PATH"; ` +
    `cd ${shQuote(cwd)} && ` +
    `if command -v wally >/dev/null 2>&1; then wally install; ` +
    `elif command -v aftman >/dev/null 2>&1; then aftman run wally install; ` +
    `else echo "wally not found on PATH and aftman is not available" >&2; exit 127; fi`;
  return `if [ -x /bin/zsh ]; then /bin/zsh -lc ${shQuote(script)}; elif [ -x /bin/bash ]; then /bin/bash -lc ${shQuote(script)}; else /bin/sh -c ${shQuote(script)}; fi`;
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
