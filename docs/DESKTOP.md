# Ro Sync Desktop

Ro Sync Desktop is the standalone Tauri 2 host for the same frontend and Rust
engine used by the Terminal 64 widget and `rosync` CLI. It is a control surface,
not a second synchronization implementation.

## Installers

Tagged GitHub releases publish these desktop packages alongside the CLI bundles
and Studio plugin:

| Platform | Installer |
| --- | --- |
| macOS 11+, Apple silicon | `Ro-Sync-<version>-macos-arm64.dmg` |
| Windows 10/11, x64 | `Ro-Sync-<version>-windows-x64.msi` |
| Windows 10/11, x64 | `Ro-Sync-<version>-windows-x64-setup.exe` |

Every installer and platform manifest has an adjacent `.sha256` file. The
manifest records the exact Git commit plus hashes for the shared frontend,
Studio plugin, command reference, Luau tools, and bundled `rosync` sidecar.

Platform code signing and Tauri updater artifacts are intentionally disabled in
the current release workflow. Until release signing identities are configured,
macOS Gatekeeper and Windows SmartScreen may warn about downloaded installers.
Ro Sync does not ship an auto-updater that bypasses those platform checks.

On macOS, choose project folders with **Projects → Add Project → Browse**.
macOS may ask once for Files & Folders access when that project lives in
Documents, Desktop, or Downloads. Unsigned local rebuilds have a different
ad-hoc code identity, so macOS can ask again after replacing the app; a native
folder selection reauthorizes the existing project without moving or copying
it. Production builds should Developer ID-sign and notarize both the bundled
`rosync` sidecar and the outer app so that approval survives upgrades.

## Projects and Studio bootstrap

Set **Settings → Projects folder** once. The native picker authorizes that root
for the app and is also the only location where Studio-requested projects can
be created. When Studio cannot find a daemon for the open published universe,
the plugin discovers Ro Sync Desktop's loopback broker, shows **Create Project**,
and sends only Roblox metadata—never a filesystem path. Desktop creates or
reuses a direct child matched by `gameId`, imports it into Projects, starts its
managed daemon, and the plugin connects when that daemon becomes available.

Project switches are independent. Desktop may serve multiple projects at the
same time, each on its own loopback port. `activeProjectId` is only the focused
row for Activity, conflicts, and settings; it does not stop other served
projects. Settings lists every managed session with its own restart and stop
controls.

## Run from source

Prerequisites:

- Node.js 22
- the stable Rust toolchain
- Xcode command-line tools on macOS, or Visual Studio Build Tools on Windows

Build the sidecar first so the desktop packager can verify its CLI contract:

```sh
# macOS Apple silicon
./daemon/build.sh

cd desktop
npm ci
npm run sync
npm run test
npm run dev
```

On Windows, run `daemon\build.ps1` before the desktop commands. `npm run sync`
only stages and verifies files; its sidecar probes use `--help` and never start
a daemon or bind a port.

Create a local unsigned installer with:

```sh
cd desktop
npm run build -- --ci --no-sign
```

## What is bundled

Each installer contains:

- the shared Ro Sync frontend;
- the target-specific `rosync` executable as a Tauri sidecar;
- the same-commit Studio plugin;
- generated command documentation;
- checksum-pinned `luau-compile` and `luau-lsp` binaries for the target; and
- the Roblox definitions used by strict linting.

The release workflow builds the daemon and Studio plugin before the desktop
matrix starts. Desktop packaging downloads those artifacts from the current
workflow run, so it cannot silently package a binary from another checkout.

## Native security boundary

The webview does not receive a general shell or unrestricted filesystem API.
The Tauri host exposes allowlisted commands for project-scoped reads and writes,
folder selection, clipboard/open operations, plugin installation, credential
storage, and managed daemon lifecycle. Project paths and arguments are validated
again at the Rust boundary.

Credentials use the operating system credential vault where available. Managed
daemon ownership tokens travel through a private child-process environment,
are redacted from errors, and are never returned to the renderer.
Lifecycle sidecars are held by exact native child handles and terminated when
the app exits or a bounded command times out. On normal exit, the native host
sends authenticated closes in parallel using the exact project, boot, PID,
port, and in-memory capability of every Desktop-managed daemon it launched;
CLI-owned, manual, replaced, and mismatched daemons are left untouched. Each
daemon additionally shuts itself down after a prolonged loss of authenticated
manager heartbeats, so a crash or interrupted cleanup cannot leave a permanent
inaccessible background process.

For the component and trust-boundary map, see [ARCHITECTURE.md](ARCHITECTURE.md).
