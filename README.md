# Ro Sync

Two-way filesystem sync between Roblox Studio and your editor, plus an agent-friendly CLI that can read/write the live DataModel while Studio is open.

- **Safe scope** — Scripts and Folders sync bidirectionally. Every other class is Studio-authoritative and surfaced via a read-only `tree.json` skeleton. No reflection-based property round-tripping, no `.meta.json` files.
- **Agent surface** — A `rosync` CLI speaks to the daemon to inspect, mutate, and introspect Studio over WebSocket: `get`, `set`, `new`, `rm`, `mv`, `call`, `attr`, `tag`, `select`, `find`, `classinfo`, `enums`, `logs`, `waypoint`, `undo`, `redo`, `eval`, and more.
- **Terminal 64 widget** — Ships with a web-based widget UI (`index.html`, `app.js`, `views/`) that runs inside the [Terminal 64](https://terminal64.com) host, manages projects, supervises the daemon, and resolves conflicts.

## Layout

```
daemon/        Rust daemon — HTTP + WebSocket server, filesystem watcher, conflict detector
plugin/        Roblox Studio plugin (Plugin.luau) — WS client, DataModel mirror, remote handlers
views/         Terminal 64 widget views (projects, active, conflicts, settings, modals)
app.js         Widget router + state store + daemon supervisor
bridge.js      postMessage + SSE helpers for the T64 host
index.html     Widget entry
style.css      Widget styles
```

## Building

```sh
cd daemon
cargo build --release
# binary at daemon/target/release/rosync — copy into $PATH
```

Plugin: open `plugin/Plugin.luau` in Roblox Studio's plugin folder (or use the widget's Settings → Install).

## Daemon

```sh
rosync serve --project /path/to/studio/project --port 7878
```

On first run the daemon writes `ro-sync.md` (agent docs) and `CLAUDE.md` (import line) into the project root.

## CLI

Full subcommand list:

```
rosync help
rosync <subcommand> --help
```

Highlights:

```sh
rosync query --project /path '**/RemoteEvent' --format paths
rosync get  --path Workspace/Baseplate
rosync set  --path Workspace/Part --prop Transparency --value 0.5 --yes
rosync new  --path Workspace --class Folder --name Enemies --yes
rosync find --class RemoteEvent
rosync logs --since 5m --level warn
rosync waypoint --name "big-change" && rosync undo --yes
```

`eval` is available as an escape hatch:

```sh
rosync eval --source 'return #workspace:GetDescendants()' --yes
```

## Safety

- All mutating CLI ops require `--yes`.
- `set Parent = ...` refuses by default (use `rosync mv` instead); override with `--force-parent`.
- Cross-service `mv` requires `--force`.
- Writes are audited to `~/.terminal64/widgets/ro-sync/writes.log` (rotated at 10 MiB).

## License

No license file yet — treat as "all rights reserved" until one is added.
