# TERMINAL 64 WIDGET

# Ro Sync

Ro Sync is a Terminal 64 widget for Roblox Studio projects. It keeps a narrow,
safe filesystem mirror of your Studio DataModel in sync with your editor, and
ships an agent-friendly `rosync` CLI for inspecting and controlling a live
Studio session.

## What It Does

- Syncs `Folder`, `Script`, `LocalScript`, and `ModuleScript` instances between Roblox Studio and disk.
- Represents non-script containers that hold scripts as pass-through folders so script paths round-trip cleanly.
- Keeps non-file-backed Roblox instances Studio-authoritative and exposes their shape through `tree.json`.
- Runs a local Rust daemon that bridges the Terminal 64 widget, Roblox Studio plugin, filesystem watcher, and CLI.
- Installs a Rojo-built Roblox Studio plugin package, `plugin/Plugin.rbxm`, from the widget settings page.
- Generates `ro-sync.md`, `AGENTS.md`, `CLAUDE.md`, and `.codex/config.toml` so Codex and Claude Code start with the same Ro Sync CLI instructions.

## CLI Tool

The `rosync` CLI can work in two modes:

- Offline project inspection through files such as `tree.json`.
- Live Studio control through the daemon and plugin WebSocket bridge.

Useful read-only commands:

```sh
rosync query --project /path/to/project '**/RemoteEvent' --format paths
rosync path --project /path/to/project Workspace/Camera
rosync path --project /path/to/project --from fs ReplicatedStorage/Config.luau
rosync get --project /path/to/project --path Workspace/Camera --prop FieldOfView
rosync ls --project /path/to/project --path ReplicatedStorage
rosync tree --project /path/to/project --path Workspace --depth 3
rosync snapshot --project /path/to/project
rosync diff --project /path/to/project --port 7878
rosync find --project /path/to/project --class RemoteEvent
rosync classinfo --project /path/to/project --class BasePart
rosync enums --project /path/to/project
rosync logs --project /path/to/project --since 1m --level warn
rosync status --project /path/to/project --port 7878
rosync doctor --project /path/to/project --port 7878
```

Useful write commands:

```sh
rosync set --project /path/to/project --path Workspace/Part --prop Transparency --value 0.5
rosync new --project /path/to/project --path Workspace --class Folder --name Enemies
rosync rm --project /path/to/project --path Workspace/Enemies
rosync mv --project /path/to/project --from Workspace/Part --to ServerStorage --force
rosync attr set --project /path/to/project --path Workspace/Part --name Health --value 100
rosync tag add --project /path/to/project --path Workspace/Part --tag Enemy
rosync waypoint --project /path/to/project --name "before refactor"
rosync undo --project /path/to/project
rosync redo --project /path/to/project
rosync eval --project /path/to/project --source 'return #workspace:GetDescendants()'
```

Luau linting:

```sh
rosync lint --project /path/to/project
rosync lint --project /path/to/project --path ServerScriptService/Foo.server.luau
rosync lint --project /path/to/project --no-sourcemap
rosync lint --project /path/to/project --luau-lsp /path/to/luau-lsp
```

`rosync lint` delegates to `luau-lsp`, generates a temporary Ro-Sync sourcemap
for require resolution, and automatically uses bundled Roblox definitions when
`tools/luau-lsp/roblox/globalTypes.d.luau` exists.

Asset uploads:

```sh
rosync img ./icon.png --creator user:123456
rosync img ./icon.png --creator group:123456 --name "Inventory Icon" --asset-type decal
rosync img ./icon.png --creator user:123456 --auth bearer --api-key-env ROBLOX_OAUTH_TOKEN
rosync img ./icon.png --creator user:123456 --no-wait --raw
rosync imgs ./icons ./banner.png --project . --manifest uploaded-assets.json
```

`rosync img` uploads `.png`, `.jpg`, `.jpeg`, `.bmp`, and `.tga` files through
Roblox Open Cloud Assets. It reads the API key from `ROBLOX_API_KEY`, or from
the widget Settings > Secrets field when the env var is not set. Pass
`--creator user:<id>` or `--creator group:<id>` to choose the asset owner;
`ROBLOX_CREATOR` can provide the same value for scripts. If no creator is
provided, Ro Sync uses the project Group ID from `ro-sync.json` or the active
widget project. The default auth mode uses Roblox API keys (`x-api-key`); pass
`--auth bearer` for OAuth access tokens.

`rosync imgs` bulk uploads image files and directories using the same credential
and creator resolution as `rosync img`. Directories recurse by default, skip
non-image files, continue after per-file failures, and support `--manifest` for
a JSON file-to-asset-id result map. Default concurrency is `2`; tune with
`--concurrency`.

`rosync status` is a concise health summary for scripts and agents: daemon
reachability, plugin/version handshake, project config, `tree.json`,
sourcemap generation, and the `writes.log` location. Pass `--raw` for JSON.

`rosync diff` compares the local Ro-Sync script/folder representation with
live Studio state. It reports items added locally, removed locally, and scripts
whose `Source` differs. Pass `--raw` for JSON.

`rosync snapshot` exports the live Studio tree plus inspectable properties,
attributes, and tags into a deterministic JSON document. By default it writes a
timestamped `rosync-snapshot-<unix-seconds>.json` file under `--project` (or
the current directory); pass `--output` to choose a file or existing directory.
Use snapshots for debugging, backups, and comparing Studio state outside the
filesystem sync surface.

`rosync path` is an offline resolver for jumping between Studio paths and disk.
Studio instance paths print the syncable filesystem path; `--from fs` maps a
Ro-Sync file such as `ReplicatedStorage/Config.luau` back to its Studio path.
It reads `tree.json` and errors clearly for Studio-authoritative classes,
unsynced services, generated files, and paths missing from the latest tree.

## Agent Context

Ro Sync keeps one canonical agent entrypoint:

- Codex reads `AGENTS.md`.
- Claude Code reads `CLAUDE.md`, which imports `@AGENTS.md`.
- `AGENTS.md` contains a regenerated Ro Sync block sourced from `ro-sync.md`.

The generated context tells agents to use `rosync` first, including
`rosync img`, before searching for unrelated Roblox upload tools.

## Requirements

- Terminal 64.
- Roblox Studio.
- Git.
- Rust toolchain, only if building the daemon locally.
- Rojo and Wally, only if rebuilding the Studio plugin package locally.
- Optional: `luau-lsp` for `rosync lint`.

## Install The Widget

1. Clone Ro Sync into the Terminal 64 widgets folder.

   macOS / Linux:

   ```sh
   mkdir -p ~/.terminal64/widgets
   git clone https://github.com/Pugbread/ro-sync.git ~/.terminal64/widgets/ro-sync
   ```

   Windows PowerShell:

   ```powershell
   New-Item -ItemType Directory -Force "$env:USERPROFILE\.terminal64\widgets" | Out-Null
   git clone https://github.com/Pugbread/ro-sync.git "$env:USERPROFILE\.terminal64\widgets\ro-sync"
   ```

2. Open Terminal 64.

3. Open the Ro Sync widget.

4. Add a Roblox project folder from the Projects view.

5. Optionally enter the project Game ID, Group ID, and Place IDs. The Group ID
   is used as the default owner for `rosync img` uploads.

6. Turn on the project switch to start serving that project.

## Install The Daemon

The widget looks for one of these files in `daemon/`:

- macOS arm64: `daemon/rosync-darwin-arm64`
- Windows x86_64: `daemon/rosync-windows-x86_64.exe`
- Linux x86_64: `daemon/rosync-linux-x86_64`

Option A: download a prebuilt daemon from GitHub Releases and place it in
`daemon/`.

Option B: build from source.

macOS / Linux:

```sh
cd ~/.terminal64/widgets/ro-sync/daemon
./build.sh
```

Windows PowerShell:

```powershell
cd "$env:USERPROFILE\.terminal64\widgets\ro-sync\daemon"
.\build.ps1
```

## Install The Roblox Studio Plugin

1. Open the Ro Sync widget.

2. Go to Settings.

3. Click **Install to Plugins folder**.

4. Restart Roblox Studio.

5. Open the Ro Sync plugin panel in Studio.

6. Click **Connect**.

## Store Secrets

Open the widget Settings tab and use the **Secrets** section to save the
Roblox OAuth / Open Cloud key used by upload/API workflows. Secrets are stored
in Terminal 64 widget state instead of `ro-sync.json`, so project files and
generated agent context do not receive credentials. The key field is designed
to grow into additional named secrets later.

Manual plugin install paths:

- macOS: `~/Documents/Roblox/Plugins/RoSync.rbxm`
- Windows: `%LOCALAPPDATA%\Roblox\Plugins\RoSync.rbxm`

## Build The Plugin Package

The shipped plugin package is `plugin/Plugin.rbxm`. To rebuild it:

```sh
aftman install
plugin/build-plugin.sh
```

The Rojo project lives in `plugin-src/` and bundles React Lua / ReactRoblox
through Wally. The React UI is in `plugin-src/src/App.luau`; the sync and daemon
protocol code remains in `plugin/Plugin.luau`.

## Run Ro Sync Manually

The widget usually launches the daemon for you. Manual launch:

```sh
rosync serve --project /path/to/project --port 7878
```

With game binding:

```sh
rosync serve --project /path/to/project --port 7878 --game-id 1234567890
```

Then open Roblox Studio, load the matching place, open the Ro Sync plugin, and
click **Connect**.

## Platform Support

| Platform | Daemon | Widget | Plugin install |
|---|---|---|---|
| macOS arm64 | Supported | Supported | Supported |
| Windows x86_64 | Supported and CI-gated | Supported and command-checked | Supported |
| Linux x86_64 | Supported | Supported | Roblox Studio is not native |

Windows support is checked by:

```sh
node scripts/check-platform-commands.mjs
cd daemon
cargo test
cargo check --target x86_64-pc-windows-msvc
```

The release workflow also builds and tests the daemon on `windows-2022`.

## Safety Rules

- Filesystem sync is intentionally limited to scripts and folders.
- Non-script Roblox classes do not round-trip through files.
- `set Parent = ...` is refused by default; use `rosync mv`.
- Cross-service moves require `--force`.
- Writes are audited to `~/.terminal64/widgets/ro-sync/writes.log`.

## Repository Layout

```text
daemon/        Rust daemon and CLI
plugin/        Roblox Studio plugin artifact and source bridge
plugin-src/    Rojo/Wally plugin package project
views/         Terminal 64 widget views
scripts/       Local verification helpers
tools/         Optional bundled tools such as luau-lsp
```

- Brought to you by Codex, Claude and Terminal 64.
