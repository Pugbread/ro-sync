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

Ro Sync checks the latest GitHub release when Desktop opens. If a newer signed
release is available, an **Update** button appears in the titlebar. The update
is downloaded, signature-verified, installed, and relaunched by Tauri. The
button is hidden in source builds and release builds that do not embed an
updater public key. If any projects are currently being served, Ro Sync asks
for confirmation before updating because the restart disconnects those Studio
sessions; when no projects are running, the update starts immediately.

The updater signature is separate from platform code signing. Until Apple and
Windows signing identities are configured, macOS Gatekeeper and Windows
SmartScreen may still warn about a first install. Updater signatures do not
replace the operating system's platform-signing checks.

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
- [Aftman](https://github.com/LPGhatguy/aftman), only if you also intend to
  rebuild the Studio plugin — `aftman install` fetches the versions pinned in
  `aftman.toml` (Rojo, Wally, StyLua, luau-lsp). The desktop app bundles the
  checked-in `plugin/Plugin.rbxm`, so packaging Desktop alone does not need it.

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

The equivalent from a clean checkout on Windows:

```powershell
.\daemon\build.ps1                  # writes daemon\rosync-windows-x86_64.exe

cd desktop
npm ci
npm run sync                        # stages the sidecar and resources
npm run build                       # produces the MSI and NSIS installers
```

`npm run sync` fails if `daemon\rosync-windows-x86_64.exe` is absent, so build
the sidecar first. Note that `build.ps1` sets `$ErrorActionPreference = 'Stop'`;
invoking it from a shell that redirects native stderr (for example a wrapper
using `2>&1`) can turn Cargo's ordinary progress output into a terminating
error under Windows PowerShell 5.1. Run it directly, or use `cargo build
--release --locked --target x86_64-pc-windows-msvc` and stage the binary
yourself.

Rebuilding the Studio plugin is a separate step and is only needed when
`plugin/` changes:

```sh
aftman install                      # once, to get the pinned Rojo and Wally
node plugin/build-plugin.mjs        # wally install, then rojo build
```

That regenerates `plugin/Plugin.rbxm` reproducibly: a clean build reproduces
the checked-in artifact byte for byte, and `plugin/Plugin.build.json` records
the SHA-256 to check against.

`npm run sync` stages and verifies files; it never starts a daemon. It does
reach the loopback interface, though: alongside the `--help` contract probes it
reads the staged sidecar's build identity with `version --port 1 --raw`, and
`version` without `--project` falls back to scanning 7878-7890 for a matching
daemon. That scan is bounded by a 10s budget, so a host where closed ports are
slow to refuse can fail the step outright rather than merely running slowly.

Create a local unsigned installer with:

```sh
cd desktop
npm run build -- --ci --no-sign
```

## Release updater signing

Before publishing the first updater-enabled tag, generate and securely archive
one Tauri updater keypair:

```sh
cd desktop
npx tauri signer generate -w ~/.tauri/ro-sync.key
cd ..
```

Ro Sync deliberately ships with `desktop/updater-key.pin.json` in a
`bootstrap-required` state. After generating the production key, calculate the
fingerprint of its public half:

```sh
node scripts/check-updater-key-pin.mjs fingerprint ~/.tauri/ro-sync.key.pub
```

Review that fingerprint out of band, then change the pin file to `configured`
and commit the printed SHA-256 as `publicKeySha256`. This is a one-time trust
bootstrap, not a release-time substitution. Until it is complete, tagged
releases fail closed with bootstrap instructions.

Configure the private key contents as the `TAURI_SIGNING_PRIVATE_KEY` GitHub
Actions secret, its password (when present) as
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and the public key contents as the
`ROSYNC_UPDATER_PUBLIC_KEY` repository variable. Never commit or share the
private key. Losing it prevents existing installations from accepting future
updates.

Tagged releases fail closed when the public variable or private secret is
missing, when the public key differs from the reviewed fingerprint, or when an
artifact signature cannot be verified before `latest.json` is created. Key
rotation therefore requires an explicit pin change plus a migration plan for
already-installed clients; changing only the GitHub variable is rejected as a
silent rotation.

Pull-request CI exercises the signing, pin-validation, tamper-detection, and
manifest-generation path with an ephemeral key. Run the same smoke test locally
after `npm ci` in `desktop`:

```sh
cd desktop
npm ci
cd ..
node scripts/smoke-updater-release.mjs
```

The ephemeral private key exists only in a temporary directory and is deleted
when the test exits. Production private keys are never committed. Successful
tagged releases publish a signed macOS app archive, signed Windows MSI and NSIS
installers, and `latest.json` alongside the normal installers. Stable tags are
explicitly marked as GitHub's latest release. Semver prerelease tags are marked
as prereleases and never replace the stable `/releases/latest/` updater feed.
Workflow-dispatch builds do not require signing credentials and do not create
updater artifacts.

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
