# Contributing to Ro Sync

Ro Sync is one engine presented through three surfaces: the Tauri desktop app,
the Terminal 64 widget, and the `rosync` CLI. Changes should preserve that shared
model rather than adding surface-specific implementations of sync behavior.

## Before you change code

1. Search existing issues and pull requests.
2. Keep Roblox Studio protocol changes backward-compatible whenever possible.
3. Never commit API keys, owner tokens, game source, runtime logs, screenshots,
   `.rosync-*` artifacts, or project-specific `state.json` files.
4. Use focused fixtures. Do not test destructive commands against a real place.

## Repository areas

| Path | Responsibility |
| --- | --- |
| `daemon/` | Rust CLI, daemon, filesystem sync, workflow execution, and local transport |
| `desktop/` | Tauri application shell and native host commands |
| `views/`, `app.js`, `style.css` | Shared frontend used by Desktop and Terminal 64 |
| `plugin/` | Built Studio plugin and protocol implementation |
| `plugin-src/` | Rojo/Wally source for the Studio plugin interface |
| `docs/commands/` | Source records for generated CLI documentation |
| `scripts/` | Deterministic build and verification helpers |

## Development checks

Run the narrowest checks for your change, then the full release gates before a
release-facing pull request:

```sh
cd daemon
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

From the repository root:

```sh
node --check app.js
node --check bridge.js
node scripts/check-platform-commands.mjs
node scripts/build-command-docs.mjs
node scripts/check-luau-bytecode.mjs plugin/Plugin.luau
node plugin/build-plugin.mjs
```

The desktop-specific commands live in `desktop/package.json`. Desktop builds
must use the same checked-in frontend sources and the same `rosync` binary that
the CLI release ships.

## Generated files

- Edit `docs/commands/*.json`, then run `node scripts/build-command-docs.mjs`.
- Edit plugin sources, then run `node plugin/build-plugin.mjs`.
- Do not hand-edit generated command bundles or `plugin/Plugin.rbxm`.

## Pull requests

Keep pull requests focused and explain the user-visible result. Include exact
commands used for verification. If a change mutates Studio state, describe its
guardrails, audit behavior, and rollback path.
