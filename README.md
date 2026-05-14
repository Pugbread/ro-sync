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
- Provides a sidebar widget UI with searchable projects, serving controls, per-project status, recent activity, and one-click Terminal 64 session spawning.
- Provides a Docs tab generated from the same command catalogue used by `rosync commands`.
- Installs a Rojo-built Roblox Studio plugin package, `plugin/Plugin.rbxm`, from the widget settings page.
- Generates `ro-sync.md`, `AGENTS.md`, `CLAUDE.md`, and `.codex/config.toml` so Codex and Claude Code start with the same Ro Sync CLI instructions.

## CLI Tool

The `rosync` CLI can work in two modes:

- Offline project inspection through files such as `tree.json`.
- Live Studio control through the daemon and plugin WebSocket bridge.

The command catalogue is sourced from one JSON file per command under
`docs/commands/`. Run the builder after editing command docs:

```sh
node scripts/build-command-docs.mjs
```

The builder writes:

- `docs/client-commands.generated.json` for the widget Docs tab.
- `docs/client-commands.md`, a full Markdown reference kept out of default agent startup context.

Common starting points:

```sh
rosync refresh --project .
rosync context --project .
rosync commands --compact
rosync status --project . --raw
rosync path --project . Workspace/Camera
rosync upload ./icon.png --project .
```

Open the widget Docs tab for the full searchable command reference.

## Agent Context

Ro Sync keeps one canonical agent entrypoint:

- Codex reads `AGENTS.md`.
- Claude Code reads `CLAUDE.md`, which imports `@AGENTS.md`.
- `AGENTS.md` contains a regenerated Ro Sync block sourced from `ro-sync.md`.
- `ro-sync.md` contains compact LLM-first command guidance and points agents
  to `rosync commands --compact` plus `rosync commands <name>` for on-demand
  usage JSON.
- When Wally is enabled, `AGENTS.md` also embeds the resolved Wally package
  configuration from `ro-sync.json` / `wally.toml` so agents can reason about
  `Packages` requires without opening the project settings first.

The generated context tells agents to use `rosync` first, including
`rosync upload`, before searching for unrelated Roblox upload tools.

Run `rosync refresh --project /path/to/project` after updating Ro Sync to pull
the latest generated agent docs into an existing project. It refreshes the Ro
Sync generated block in `AGENTS.md`, ensures `CLAUDE.md` imports `@AGENTS.md`,
and updates `ro-sync.md` when it is a generated Ro Sync file. Custom content in
`AGENTS.md` outside the marker block and custom content in `CLAUDE.md` are left
in place.

## Project Tooling

`rosync serve` and `rosync refresh` also ensure each served project has a small
local toolchain baseline:

- `.stylua.toml` is created when missing, using Ro Sync's Luau formatting defaults.
- `aftman.toml` is created or merged so `[tools]` includes
  `stylua = "JohnnyMorganz/StyLua@2.4.1"`.
- `tools/luau-lsp/roblox/globalTypes.d.luau` is restored from Ro Sync's
  bundled Roblox definitions, and `.luaurc` is merged so editor-side
  `luau-lsp` can load those definitions too.
- Existing project choices are preserved. Ro Sync does not overwrite an
  existing `.stylua.toml`, does not replace existing Aftman tools such as
  Wally, and only appends the Ro Sync definitions path to `.luaurc`.

These tooling files are ignored by the filesystem watcher so they do not sync
into Studio.

`rosync lint` wraps `luau-lsp analyze` with Ro Sync defaults:

```sh
rosync lint --project .
rosync lint --project . --path ServerScriptService --path ReplicatedStorage/Shared
rosync lint --project . --path ServerScriptService --owned-only --summary
```

The command loads bundled Roblox definitions when available, passes a generated
Ro Sync sourcemap by default, supports repeated `--path`, and hides diagnostics
from common dependency/tooling folders such as `Packages`, `_Index`,
`Madwork*`, `PlayerModule`, `node_modules`, `tools`, `.codex`, and `.vscode`.
Use `--ignore <glob>` for project-specific generated/vendor paths, or
`--no-vendor-ignores` when you intentionally want dependency diagnostics.

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
   is used as the default owner for `rosync upload`.

6. Turn on the project switch to start serving that project.

## Use The Widget

The widget uses a left sidebar for Projects, Activity, Conflicts, Docs, and Settings.

Projects is the main workspace:

- Use **Add Project** or the add tile to register a local Roblox project folder.
- Use the search box and filters to narrow larger project lists.
- Toggle a project on to serve it. Ro Sync serves one project at a time, so turning one on replaces the previous active project.
- Select a project card to open its detail pane. The detail pane shows recent daemon/plugin activity for the active project and exposes edit, folder, status refresh, diff, delete, and **Spawn Session** actions.
- Duplicate Studio sibling names are surfaced on project cards as duplicate-name chips when the daemon snapshot contains `[N]` disambiguated paths.

Activity shows the live daemon stream with ops, last sync timing, plugin state,
and the active project. The log can be paused with **Stop live log** and cleared
without stopping sync. High-volume op bursts are collapsed before full JSON
parsing so large initial syncs do not flood the Terminal 64 host.

The app-level daemon stream remains connected for control prompts such as
initial sync decisions and batch previews. Raw op frames are handled on the
string hot path and only control events are fully parsed globally.

Docs shows the generated command catalogue with search, category filters, usage
examples, notes, and copy buttons.

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
node plugin/build-plugin.mjs
```

On macOS / Linux, `plugin/build-plugin.sh` is also available and delegates to
the same Node builder.

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
- Empty plain directories are ignored until they contain syncable content, so
  placeholder folders cannot shadow same-named scripts in Studio.
- Renaming between `.luau`, `.server.luau`, and `.client.luau` converts the
  Studio script class instead of leaving a stale `Script`/`LocalScript`/`ModuleScript`.
- Non-script Roblox classes do not round-trip through files.
- `set Parent = ...` is refused by default; use `rosync mv`.
- Cross-service moves require `--force`.
- Writes are audited to `~/.terminal64/widgets/ro-sync/writes.log`.

## Repository Layout

```text
daemon/        Rust daemon and CLI
docs/          Command JSON source and generated command docs
plugin/        Roblox Studio plugin artifact and source bridge
plugin-src/    Rojo/Wally plugin package project
views/         Terminal 64 widget views
scripts/       Local verification helpers
tools/         Optional bundled tools such as luau-lsp
```

- Brought to you by Codex, Claude and Terminal 64.
