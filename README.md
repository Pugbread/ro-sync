# TERMINAL 64 WIDGET

# Ro Sync

Ro Sync is a Terminal 64 widget for Roblox Studio projects. It keeps a narrow,
safe filesystem mirror of your Studio DataModel in sync with your editor, and
ships an agent-friendly `rosync` CLI for inspecting and controlling a live
Studio session.

## What It Does

- Syncs `Folder`, `Script`, `LocalScript`, and `ModuleScript` instances between Roblox Studio and disk.
- Represents non-script containers that hold scripts as pass-through folders so script paths round-trip cleanly.
- Keeps non-file-backed Roblox instances Studio-authoritative and exposes their shape through live CLI reads.
- Runs a local Rust daemon that bridges the Roblox Studio plugin, filesystem watcher, CLI, and optional Terminal 64 widget.
- Provides a sidebar widget UI with searchable projects, serving controls, per-project status, recent activity, and one-click Terminal 64 session spawning.
- Provides a Docs tab generated from the same command catalogue used by `rosync commands`.
- Installs a Rojo-built Roblox Studio plugin package, `plugin/Plugin.rbxm`, from the widget settings page.
- Generates `ro-sync.md`, `AGENTS.md`, `CLAUDE.md`, and `.codex/config.toml` so Codex and Claude Code start with the same Ro Sync CLI instructions.

## CLI Tool

The `rosync` CLI can work in two modes:

- Live Studio inspection/control through the daemon and plugin WebSocket bridge.
- Offline project maintenance for tasks such as generated agent docs, linting, sourcemaps, and Open Cloud uploads.

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
rosync capabilities --project . --raw
rosync status --project . --raw
rosync path --project . Workspace/Camera
rosync upload ./icon.png --project .
```

Use `rosync commands <name>` for exact command JSON, or open the widget Docs tab
for the same searchable command reference.

## LLM Automation Surfaces

Protocol 2 and Studio plugin 2.0.0 add four composable surfaces intended for
LLMs: capability negotiation, binary artifacts and screenshots, isolated
playtest agents, and validated workflows. They use the same focused CLI model
as the existing Explorer commands, so an agent can discover support cheaply and
then chain only the operations it needs.

Start by negotiating the actual connected Studio environment:

```sh
rosync capabilities --project . --raw
```

The document includes plugin/protocol versions, Studio and host DataModel
details, and feature flags for capture, artifact transport,
PluginConnection playtest routing, runtime execution, UI inspection, and
virtual input. This avoids assuming an optional Studio API exists.

The localhost transport is capability- and role-scoped. Studio echoes a fresh
daemon capability from `/hello`; browser-backed widget requests carry the
widget owner token; CLI, watch, and plugin sockets receive only their intended
traffic. All peers negotiate protocol 2 and may send exactly one hello.

### Screenshots and binary artifacts

Ro Sync has two capture paths. `capture screen` uses Studio's permission-gated
screenshot provider, while `capture photo` uses a self-contained, locally
packaged Photo engine that needs no screenshot authorization or place-provided
capture dependency:

```sh
rosync capture status --project . --raw
rosync capture authorize --project .
rosync capture screen --project . \
  --region 200,120,1280,720 --output-size 1024x576 --ui all \
  --output ./captures/studio.png --raw
rosync capture photo --project . \
  --focus Workspace/Map/Boss --view isometric --size 1024x1024 \
  --padding 1.25 --fov 32 --background transparent --alpha-bleed \
  --delay 0.1 --output ./captures/boss.png --timeout 120 --raw
rosync capture photo --project . \
  --focus Workspace/Map/Boss --view isometric --size 1024x1024 \
  --no-tight-crop --output ./captures/boss-framed.png --raw
rosync capture photo --project . \
  --focus Workspace/Map/Boss \
  --camera-cframe '0,10,20,1,0,0,0,1,0,0,0,1' --fov 40 \
  --size 1600x900 --background transparent \
  --output ./captures/boss-exact-camera.png --raw
rosync capture photo --project . \
  --region 120,80,1280,720 --background scene --ui overlay \
  --output ./captures/viewport.png --raw
rosync capture photo --project . \
  --ui only --region 120,80,1280,720 --size 1920x1080 --alpha-bleed \
  --output ./captures/hud.png --raw
rosync capture photo --project . \
  --ui-target StarterGui/HUD/InventoryPanel --size 1200x800 --alpha-bleed \
  --output ./captures/inventory-panel.png --raw
```

Photo `--focus` is optional. With a focus, Ro Sync normally makes a script-free
temporary clone, isolates it from the world, and frames it with `--view` or an
arbitrary `--direction x,y,z`. Isolated transparent focus captures tight-crop
the rendered subject's alpha bounds by default. An exact `--size WIDTHxHEIGHT`
is preserved by aspect-containing the cropped subject in a transparent canvas;
raw metadata reports its `region`, `fullSize`, and `regionSource` as
`subject-alpha`, with `tightCrop: true`. Pass `--no-tight-crop` to keep the
full camera-framed render.
`--padding` and `--fov` tune framing. `--include-world` instead frames the
original target in place. `capture photo --background scene` and
`--include-world` captures remain camera-framed rather than alpha-cropped;
the isolated, transparent `capture scene` compatibility alias inherits the
default tight crop and accepts the same opt-out. For an authored angle,
`--camera-cframe` accepts the 12 finite values returned by
`CFrame:GetComponents()` and preserves that exact subject-relative camera
position, orientation, and roll for isolated clones; the default tight crop
still applies. With `--include-world`, the same CFrame is used directly in
world space. It is compatible with `--fov`, and replaces `--view`,
`--direction`, and `--padding`.

`--ui overlay` keeps in-game ScreenGui layers over the scene, while `--ui only`
extracts the edit-mode ScreenGui layer as a transparent RGBA PNG with no 3D
world or Studio chrome. `--ui-target <Studio/path>` implies `--ui only`, accepts
a `ScreenGui` or one `GuiObject`, clones only that element and its descendants,
and hides every unrelated UI layer for the capture. Without `--region`, Ro Sync
tight-crops the rendered target; `--size` then aspect-contains that crop in the
exact requested transparent canvas. An explicit `--region` overrides the
automatic crop and continues to fill the requested output exactly. UI-only
capture cannot be combined with `--focus` and requires `--background
transparent`; the legacy `--include-ui` flag remains an alias for `--ui
overlay`. When a full untargeted UI-only capture uses `--size`, Ro Sync
preserves the native viewport aspect ratio, centers it in the exact requested
canvas, and leaves any extra area transparent instead of stretching the
interface.
Without a focus, Photo captures the current viewport; `--region` is then a
native viewport-pixel rectangle measured from its top-left. Combine `--region`
with `--size` to crop an arbitrary viewport or UI rectangle and resample it to
exact output dimensions. `--background` is `transparent` or `scene`;
`--alpha-bleed` keeps useful RGB in transparent edge pixels, and `--delay`
allows streaming/rendering to settle. Photo captures are limited to 4096
pixels per axis and 16,777,216 pixels total. `capture scene` is a compatibility
alias for the same locally packaged Photo engine.

Photo transports raw RGBA in bounded chunks, validates its exact length, and
encodes the PNG locally. Camera, UI, and lighting state are restored and any
temporary clone is destroyed on both success and failure. `--output`,
`--timeout`, and `--raw` are available for both viewport and subject captures.

For `capture screen`, `--region` uses global logical-screen coordinates as
`x,y,width,height`; `--output-size` uses `WIDTHxHEIGHT`, and `--ui` can include
all Studio UI or only the 3D viewport. Checking screen status never prompts;
authorization is explicit. If Studio's permission API returns its exact
`Feature not supported yet` result, the explicit `capture authorize` command
records that state and requests macOS Screen & System Audio Recording
permission. Subsequent `capture screen --ui all` calls can then use a
window-only macOS capture. CoreGraphics discovery is restricted to a visible
Roblox Studio window, and regions outside that window are rejected. A merely
unauthorized Studio provider never triggers fallback; `--ui none` does not use
it. Photo and scene capture are independent of this provider. PNG bytes from
Studio move through a short-lived tokenized artifact lease in bounded chunks,
never as a giant base64 stdout value. The native fallback writes the same
bounded, decoded, SHA-verified PNG directly. Raw screen output identifies the
provider and includes the absolute path, MIME type, byte length, dimensions,
position, and SHA-256.
After verification the CLI consumes its transport copy; abandoned artifacts
are bounded by a 15-minute TTL, LRU eviction, and a total-byte budget.

### Playtest runtime agents

Play, Run, and local multiplayer tests are asynchronous jobs. The edit-mode
plugin remains the only localhost connection; plugin copies inside PlayServer
and PlayClient DataModels connect back through `PluginConnectionService` and
appear as stable `server` / `client:N` contexts.
Each CLI-started job has an authenticated generation token, so stale contexts
cannot satisfy or receive requests from a later test.

```sh
rosync playtest start --project . --mode multiplayer --players 2 --wait --raw
rosync playtest contexts --project . --raw
rosync playtest exec --project . --context server \
  --source 'return #game.Players:GetPlayers()' --identity game --raw
rosync playtest ui --project . --context client:1 --class TextButton --raw
rosync playtest input --project . --context client:1 \
  --actions '[{"type":"click","x":640,"y":420}]' --raw
rosync playtest logs --project . --context client:1 --since-seq 0 --raw
rosync playtest capture --project . --context client:1 \
  --output ./captures/client-1.png --raw
rosync playtest stop --project . --raw
```

Runtime `exec` defaults to game identity through a temporary Script or
LocalScript, with plugin identity available explicitly. UI inspection reports
resolved visibility, text, and absolute geometry. Virtual input supports
`key`, `key_press`, `mouse_move`, `mouse_delta`, `mouse_button`, `click`,
`text`, and `wait` action objects. Playtest DataModels are temporary: changes
made inside them never sync back to the edit DataModel. Inputs are capped at
200 actions/30 seconds, and runtime captures use bounded dimensions, bytes,
session count, and TTL. Plugin-identity timeouts are cooperative; code that
spawns child tasks must stop those tasks itself.

### Versioned workflows

`rosync run` validates schema version 1 before opening one persistent remote
session. A workflow can compose focused reads/writes, assertions, waits,
captures, playtest actions, and uploads while preserving JSON types across
step references:

```json
{
  "version": 1,
  "name": "verify transparency",
  "idempotencyKey": "demo-transparency-v1",
  "expectedMode": "edit",
  "transactions": [{ "id": "edit", "atomic": true }],
  "steps": [
    {
      "id": "write",
      "op": "set",
      "path": "Workspace/Box",
      "property": "Transparency",
      "value": 0.5,
      "expectedClass": "Part",
      "transaction": "edit",
      "verify": true
    },
    {
      "id": "read",
      "op": "get",
      "path": "Workspace/Box",
      "property": "Transparency"
    },
    {
      "id": "check",
      "op": "assert",
      "actual": "$read.value",
      "check": { "op": "equals", "expected": 0.5 }
    }
  ]
}
```

```sh
rosync run --file ./workflow.json --project . --dry-run
rosync run --file ./workflow.json --project . --raw
```

An exact string such as `$read.value` inserts an earlier result without
stringifying it; `$$literal` escapes an initial dollar sign. Workflows can
guard the host mode/place and individual target class/etag, poll with `wait`,
and opt into read-back verification on supported writes. Contiguous atomic
groups map to Studio change-history recordings and are cancelled on failure;
unbounded operations such as eval, call, wait, capture, playtest, and upload
are rejected inside atomic groups. A successful `idempotencyKey` is recorded
under `.rosync-workflows/`, and a repeat returns the stored result instead of
performing the steps twice.

### Conflict-safe disk deletes and renames

Filesystem deletes and renames are held for 500 ms before they are forwarded
to Studio. This is longer than the plugin's editor-source debounce, so a Studio
edit already in flight can be compared with the last agreed source first. If
Studio diverged, the destructive op is parked as a conflict and written to the
widget audit log instead of being applied silently.

The conflict keeps the disk action as structured state. `rosync resolve --disk`
deletes or renames the Studio instance and, after a rename, reapplies the
retained destination source tree. `rosync resolve --studio` recreates a deleted
leaf script or reverses the disk rename before writing Studio's source. A
directory rename can restore its complete subtree because the renamed
destination is retained on disk. A directory deleted from disk cannot be
reconstructed from the one divergent source stored in a conflict, so Keep
Studio fails closed without writing a partial tree, leaves the conflict parked,
and asks for a full Studio subtree restore. Clean deletes and renames continue
to propagate normally.

The grace window deliberately narrows rather than claims to eliminate every
possible race: a Studio edit that is not delivered until after the bounded
window can still arrive too late for this preflight. The plugin-side safety
checks, next initial comparison, and audit record remain the recovery boundary
for that case.

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
  `stylua = "JohnnyMorganz/StyLua@2.5.2"` and
  `luau-lsp = "JohnnyMorganz/luau-lsp@1.68.1"`. Existing user pins win.
- `tools/luau-lsp/roblox/globalTypes.d.luau` is restored from Ro Sync's
  bundled Roblox definitions. `rosync lint` injects it as the named `@roblox`
  definition set on the analyzer command line.
- Existing project choices are preserved. Ro Sync does not overwrite an
  existing `.stylua.toml`, does not replace existing Aftman tools such as
  Wally, or replace valid `.luaurc` settings. Refresh removes the obsolete
  Ro-Sync-generated `.luaurc.definitions` entry because it is not a supported
  Luau configuration key; definitions are supplied by the lint command instead.

These tooling files are ignored by the filesystem watcher so they do not sync
into Studio.

`rosync lint` wraps `luau-lsp analyze` with Ro Sync defaults:

```sh
rosync lint --project .
rosync lint --project . --data-model studio --port 7878
rosync lint --project . --path ServerScriptService --path ReplicatedStorage/Shared
rosync lint --project . --path ServerScriptService --owned-only --summary
rosync lint --project . --data-model filesystem --raw
rosync lint --project . --compile required --luau-compile /path/to/luau-compile
```

The default `--data-model auto` mode enriches the filesystem sourcemap with the
complete live Studio tree when a matching daemon and plugin are connected. It
then enables strict DataModel diagnostics, so engine service types and
Studio-owned Models, Parts, Remotes, and UI instances are checked using their
real classes while disk script nodes retain their file mappings. If Studio is
unavailable, auto mode falls back to relaxed filesystem types rather than
claiming that an incomplete tree is authoritative.

Use `--data-model studio` to require the live full-DataModel check and fail when
Studio cannot be reached. `--data-model filesystem` enables strict checking
against only the disk projection; it is useful for offline audits but can
report false unknown-child errors for Studio-only objects. `--data-model loose`
always uses the filesystem map with gradual DataModel types. `--port` selects
the daemon used by auto/studio modes, and `--raw` returns structured diagnostics
plus coverage source, strictness, live-node count, analyzer exit code, and the
number of diagnostics suppressed by an owned scope.

Without an explicit `--path`, common dependency/tooling folders such as
`Packages`, `_Index`, `Madwork*`, `PlayerModule`, `node_modules`, `tools`,
`.git`, `.codex`, `.vscode`, and Ro Sync's `.rosync-*` runtime/backup folders
are hidden by default. An explicit `--path` is treated as an ownership boundary
and is never swallowed by those defaults. Use `--ignore <glob>` for project-specific generated paths or
`--no-vendor-ignores` to inspect every dependency when linting the project root.

The bundled definitions are passed as `--definitions:@roblox=...`. Additional
named definition sets can coexist after `--`, for example
`--definitions:@testez=types/testez.d.luau`; an explicit
`--definitions:@roblox=...` replaces the bundled Roblox set. Ro Sync recommends
`luau-lsp` 1.68.1 or newer and warns when the selected analyzer is older. Pass
`--luau-lsp`, set `ROSYNC_LUAU_LSP`, or make the executable available on
`PATH`.

The default `--compile auto` pass also bytecode-compiles every in-scope script
at `-O0`, `-O1`, and `-O2` whenever `luau-compile` is available. This catches
compiler-only failures such as the 200-local-register limit that static type
analysis cannot see. Ro Sync checks `--luau-compile`, `ROSYNC_LUAU_COMPILE`,
`LUAU_COMPILE`, its bundled platform tool, Aftman, and `PATH`, in that order.
Use `--compile required` to make a missing compiler an error or `--compile off`
to skip the pass. Raw output labels analyzer versus compiler diagnostics and
includes compiler coverage, failures, and unparsed analyzer messages such as
configuration errors. Human `--summary` totals both analyzer and compiler
diagnostics. The default and GNU analyzer formatters support structured output;
Ro Sync rejects `--formatter=plain` because that upstream mode can report a
TypeError while returning a successful process status.

## Requirements

- Terminal 64.
- Roblox Studio.
- Git.
- Rust toolchain, only if building the daemon locally.
- Rojo and Wally, only if rebuilding the Studio plugin package locally.
- Optional: `luau-lsp` for `rosync lint`.
- Optional for source installs: Node.js 18+ to acquire the checksum-pinned
  `luau-compile`; release bundles already include it.

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

Option A: download the bundle for your platform from GitHub Releases and
extract it at the Ro Sync widget root. Each bundle preserves the expected
layout and includes both the daemon and the checksum-pinned Luau compiler:

- `rosync-darwin-arm64-bundle.zip`
- `rosync-linux-x86_64-bundle.zip`
- `rosync-windows-x86_64-bundle.zip`

Every bundle has a sibling `.sha256` file in the release. Standalone daemon
binaries remain available for compatibility, but do not carry the optional
compiler pass by themselves.

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

The build helpers attempt to install the matching pinned compiler. You can
also acquire or repair it directly from the widget root:

```sh
node scripts/install-luau-compiler.mjs
```

The installer accepts only the three supported release targets, downloads the
official Luau 0.729 asset over HTTPS, verifies both archive and executable
SHA-256 digests, extracts only `luau-compile`, and replaces it atomically. See
[`tools/luau/README.md`](tools/luau/README.md) for the pinned digest table and
offline verification flags.

## Install The Roblox Studio Plugin

1. Open the Ro Sync widget.

2. Go to Settings.

3. Click **Install to Plugins folder**.

4. Restart Roblox Studio.

5. Open the Ro Sync plugin panel in Studio.

6. Click **Connect**.

## Credentials

Ro Sync reads the Open Cloud key saved in widget Settings **Secrets** first.
Pass `--api-key-env` to explicitly select an environment variable instead; if
no saved key or explicit override exists, the CLI falls back to
`ROBLOX_API_KEY`, `CLOUD_API_KEY`, then `ROBLOX_OPEN_CLOUD_API_KEY`.

Manual plugin install paths:

- macOS: `~/Documents/Roblox/Plugins/RoSync.rbxm`
- Windows: `%LOCALAPPDATA%\Roblox\Plugins\RoSync.rbxm`

## Build The Plugin Package

The shipped plugin package is `plugin/Plugin.rbxm`. To rebuild it:

```sh
aftman install
node scripts/check-luau-bytecode.mjs # requires luau-compile on PATH
node plugin/build-plugin.mjs
```

The bytecode check compiles at `-O0`, `-O1`, and `-O2`; this catches Luau's
per-function 200-local-register limit, which source analysis and Rojo packaging
do not detect. Set `LUAU_COMPILE=/path/to/luau-compile` when it is not on
`PATH`. CI downloads a checksum-pinned official compiler release.

On macOS / Linux, `plugin/build-plugin.sh` is also available and delegates to
the same Node builder.

The Rojo project lives in `plugin-src/` and bundles React Lua / ReactRoblox
through Wally. The React UI is in `plugin-src/src/App.luau`; the sync and daemon
protocol code remains in `plugin/Plugin.luau`.

## Run Ro Sync From The CLI

Start the daemon directly:

```sh
rosync serve --project /path/to/project --port 7878
```

With game binding:

```sh
rosync serve --project /path/to/project --port 7878 --game-id 1234567890
```

Then open Roblox Studio, load the matching place, open the Ro Sync plugin, and
click **Connect**.

Initial sync choices can be handled without the widget:

```sh
rosync decision --project .
rosync decision --project . --studio
rosync decision --project . --disk
```

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
