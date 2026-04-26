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
- Generates `ro-sync.md`, `CLAUDE.md`, `AGENTS.md`, and `.codex/config.toml` so coding agents understand the project rules.

## CLI Tool

The `rosync` CLI can work in two modes:

- Offline project inspection through files such as `tree.json`.
- Live Studio control through the daemon and plugin WebSocket bridge.

Useful read-only commands:

```sh
rosync query --project /path/to/project '**/RemoteEvent' --format paths
rosync get --project /path/to/project --path Workspace/Camera --prop FieldOfView
rosync ls --project /path/to/project --path ReplicatedStorage
rosync tree --project /path/to/project --path Workspace --depth 3
rosync find --project /path/to/project --class RemoteEvent
rosync classinfo --project /path/to/project --class BasePart
rosync enums --project /path/to/project
rosync logs --project /path/to/project --since 1m --level warn
rosync doctor --project /path/to/project --port 7878
```

Useful write commands, all guarded by `--yes`:

```sh
rosync set --project /path/to/project --path Workspace/Part --prop Transparency --value 0.5 --yes
rosync new --project /path/to/project --path Workspace --class Folder --name Enemies --yes
rosync rm --project /path/to/project --path Workspace/Enemies --yes
rosync mv --project /path/to/project --from Workspace/Part --to ServerStorage --force --yes
rosync attr set --project /path/to/project --path Workspace/Part --name Health --value 100 --yes
rosync tag add --project /path/to/project --path Workspace/Part --tag Enemy --yes
rosync waypoint --project /path/to/project --name "before refactor"
rosync undo --project /path/to/project --yes
rosync redo --project /path/to/project --yes
rosync eval --project /path/to/project --source 'return #workspace:GetDescendants()' --yes
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

5. Turn on the project switch to start serving that project.

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
- Mutating CLI operations require `--yes`.
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
