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

The app bundles `Plugin.rbxm`, generated command documentation, and the Luau analysis/compiler toolchain. Studio plugin installation copies the bundle to Roblox's per-user plugin directory and reports that Studio must be restarted.
