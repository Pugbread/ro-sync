# Ro Sync desktop host

This directory packages the same dependency-free renderer used by the Terminal 64 widget inside a Tauri 2 application. The renderer cannot invoke a general-purpose shell. It receives only allowlisted native commands for project configuration, secrets, folder selection, clipboard/open operations, plugin installation, Wally installation, bundled-resource discovery, and managed daemon lifecycle.

## Development

```sh
cd desktop
npm install
npm run sync
npm run check
npm run test
npm run dev
```

`npm run sync` deterministically rebuilds `dist/`, copies packaged resources, names the Ro Sync sidecar for the active Rust target, and runs non-launching `--help` probes for the managed-lifecycle and credential contracts. It never starts the daemon or binds a port. A target build fails early when its release daemon binary has not been produced yet or is too old for the desktop host; rebuild the daemon release artifact first.

Application state lives below the OS application-data directory. Credentials use the native credential vault when available; a deliberately isolated fallback uses a mode-`0600` JSON file inside that private application-data directory. The Roblox Open Cloud credential is additionally synchronized, through a private child environment variable, into the CLI's canonical mode-`0600` credential store so `rosync upload` and monetization commands can use the Settings value without receiving it in argv or output. Daemon ownership tokens use the same no-argv environment boundary, are redacted from errors, and are never returned to the renderer by lifecycle results.

The desktop host serves multiple projects concurrently. Its persisted state
separates UI focus from the set of served projects, while native ownership is
keyed by canonical project. On normal app exit, the native host uses every
exact in-memory ownership capability to close only the daemons it launched, in
parallel, and waits a bounded three seconds for their listeners to disappear.
A CLI-owned, manually started, replaced, or identity-mismatched daemon cannot
authenticate with those capabilities and is left untouched. The daemon
heartbeat watchdog remains the fallback for crashes, interrupted startup, or a
bounded close failure.

The app also owns a small loopback-only project broker on ports 7867–7870.
After the user selects a Projects folder in Settings, the Studio plugin can
request creation of a project for its current published universe. The request
contains Roblox metadata rather than a local path; the broker creates or reuses
one direct child under the authorized root and queues it for the renderer to
import and serve.

The app bundles `Plugin.rbxm`, generated command documentation, and the Luau analysis/compiler toolchain. Studio plugin installation copies the bundle to Roblox's per-user plugin directory and reports that Studio must be restarted.

On macOS, project folders must be selected through the native **Browse** picker.
That selection is both Ro Sync's project-root authorization and the operating
system's Files & Folders consent for protected locations such as Documents.
Local unsigned rebuilds change their ad-hoc code identity and can require the
folder to be selected again. Distributable builds need a stable Developer ID
signature on the nested sidecar and the final app bundle.
