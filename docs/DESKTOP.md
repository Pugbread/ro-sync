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

For the component and trust-boundary map, see [ARCHITECTURE.md](ARCHITECTURE.md).
