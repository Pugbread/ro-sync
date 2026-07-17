# Ro Sync project memory

<!-- ro-sync:project-memory:start -->
Ro Sync mirrors a narrow slice of a Roblox Studio DataModel into this directory.
Read this file before editing — the scope is deliberately small.

## 0. Agent bootstrap

You are in a Ro Sync project. Do not look for `rbxcloud`, Rojo upload scripts,
or ad-hoc Roblox tooling before trying the built-in CLI.

Use `rosync` directly, but validate it has the modern subcommands first:

```
rosync upload --help
```

If that command is missing, do not investigate unrelated upload tools; use the
widget daemon binary directly:

- macOS arm64: `~/.terminal64/widgets/ro-sync/daemon/rosync-darwin-arm64`
- Windows x86_64: `%USERPROFILE%\.terminal64\widgets\ro-sync\daemon\rosync-windows-x86_64.exe`
- Linux x86_64: `~/.terminal64/widgets/ro-sync/daemon/rosync-linux-x86_64`

From the project root, start with:

```
rosync context --project .
rosync status --project . --raw
rosync path --project . Workspace/Camera
```

Do not run `diff`, `changes`, `conflicts`, or live `source` as a startup ritual.
Use them only when the task specifically needs that information.

For agents, the live Studio explorer is the source of truth for Explorer shape.
Use `rosync tree`, `rosync ls`, `rosync meta`, or `rosync get --prop` when you
need to inspect Studio-owned objects. For code you are about to edit, read the
local synced file directly; use live `rosync source` only when checking a
suspected Studio/editor divergence. Disk only mirrors Ro Sync's script/folder
projection; Studio-only explorer folders and non-script instances may
intentionally have no matching files.

For asset uploads, use Ro Sync, not external asset tools:

```
rosync upload ./image.png --project .
rosync upload ./audio.mp3 ./models --project . --manifest uploaded-assets.json
rosync upload ./clip.rbxm --project . --asset-type animation
```

`rosync upload` reads the Roblox Open Cloud credential from the key saved in
Ro Sync Settings first. `--api-key-env` is an explicit override; if omitted,
the CLI falls back to `ROBLOX_API_KEY`, `CLOUD_API_KEY`, and
`ROBLOX_OPEN_CLOUD_API_KEY`. It uses the project `groupId` as `group:<id>` when
`--creator` is omitted. It supports Roblox Open Cloud asset types including
image, decal, audio, model, mesh, animation, and video. Use `--asset-type` for
decals and ambiguous `.rbxm`/`.rbxmx` files.

## 0b. Refreshing agent docs

After upgrading Ro Sync, run:

```
rosync refresh --project .
```

This refreshes `ro-sync.md`, `AGENTS.md`, `CLAUDE.md`, and
`.codex/config.toml` without discarding project notes. Keep custom Codex notes
in `AGENTS.md` outside the Ro Sync marker block; keep Claude-specific notes in
`CLAUDE.md` around the `@AGENTS.md` import. `ro-sync.md` is the generated Ro
Sync tool reference.

## 1. What syncs, what doesn't

Two-way sync covers ONLY these four Roblox classes:

- `Folder`
- `Script`
- `LocalScript`
- `ModuleScript`

Edits to the matching files/directories flow into Studio and back. Every other
Roblox class is Studio-authoritative: inspect it with live CLI reads such as
`rosync tree`, `rosync ls`, `rosync meta`, `rosync get`, or `rosync props`.
The daemon never writes those instances to disk and never pushes property
changes to Studio.

Script source has one extra Studio caveat: `script.Source` is not a reliable
truth source while Drafts or an open Script Editor buffer is involved. Studio
does not always push draft/editor text into the `Source` property until the
script is committed. Ro Sync uses `ScriptEditorService:GetEditorSource()` /
`UpdateSourceAsync()` and ScriptDocument change events so editor text can
round-trip. For normal code work, read the local synced file directly; use live
`rosync source` only as a loose diagnostic when you suspect Studio/editor text
has diverged from disk.

## 1b. Playtesting is a separate environment

Roblox Studio playtesting creates a completely separate DataModel clone. The
Play/Solo/Run world and the edit-mode Studio workspace do not transfer instance
or script changes between each other. Script edits made while playtesting run
inside that temporary playtest DataModel and DO NOT mirror back into the edit
DataModel. Ro Sync is connected to the edit DataModel and this directory, not
the playtest clone.

If you change code while a playtest is running, make the durable edit in this
directory or in the non-playtest Studio edit view. Do not assume a script change
made during Play/Solo/Run has synced just because it worked in the playtest.

## 2. Filesystem conventions

| On disk                              | Roblox instance                                |
| ------------------------------------ | ---------------------------------------------- |
| `Foo.luau`                           | `ModuleScript` named `Foo`                     |
| `Foo.server.luau`                    | `Script` named `Foo`                           |
| `Foo.client.luau`                    | `LocalScript` named `Foo`                      |
| `Foo/`                               | `Folder` named `Foo`                           |
| `Foo/init (Foo).luau`                | `ModuleScript` named `Foo` with children       |
| `Foo/init (Foo).server.luau`         | `Script` named `Foo` with children             |
| `Foo/init (Foo).client.luau`         | `LocalScript` named `Foo` with children        |
| `Foo [1].luau`, `Foo [2].luau` …     | Siblings that share the name `Foo` (1-based)   |

Additional sync rules:

- The project root represents `game`. Only the listed synced service
  directories are valid roots; arbitrary folders under the project root do not
  become children of `game`.
- A script with children is represented by a directory plus one matching
  `init (<Name>)` file. Edit that init file for the script's `Source`; edit
  child files/directories for child instances.
- `init (Name).server.luau` maps to `Script`, `init (Name).client.luau` maps to
  `LocalScript`, and `init (Name).luau` maps to `ModuleScript`.
- Plain Wally/Rojo package roots such as `init.lua`, `init.server.lua`, and
  `init.client.lua` are recognized for package-style modules, but Ro Sync emits
  its own script-with-children files as `init (<Name>).*.luau`.
- Directories map to `Folder` unless they are script-with-children directories
  with an `init (<Name>)` file. Empty plain directories are ignored until they
  contain a syncable script or child directory, so placeholder folders cannot
  shadow same-named scripts.
- File and directory renames/moves sync as Roblox instance renames/reparents
  when they stay under a synced service.
- Set a boolean Studio attribute `AvoidSync = true` on a folder/instance to
  exclude that subtree from filesystem sync. Use live `rosync tree` or
  `rosync meta` to inspect AvoidSync boundaries.

Names containing non-ASCII characters or characters POSIX paths can't express
(`/`, control characters, leading `.`) are percent-encoded. Encoding non-ASCII
UTF-8 keeps distinct Studio names distinct on case-insensitive or
Unicode-normalizing filesystems.

**Out of scope:** `.meta.json` files, attribute/tag serialization, non-`Folder`
non-script Roblox classes (e.g. `Part`, `TextLabel`, `RemoteEvent`, `Sound`).
None of these round-trip through the filesystem — inspect them through live
CLI reads from Studio.

## 3. Top-level services

The project root mirrors the `game` DataModel. Each subdirectory below is a
service the plugin keeps in sync:

- `ReplicatedStorage/`
- `ServerScriptService/`
- `StarterPlayer/`
- `StarterGui/`
- `Workspace/`
- `ReplicatedFirst/`
- `ServerStorage/`
- `Lighting/`

## 4. Generated files (do not edit)

- `ro-sync.md` — this file. Ro Sync refreshes its generated tool reference.

## 5. Querying the live tree

The `rosync query` subcommand asks the running daemon/plugin for the live
Studio tree and matches a `/`-separated selector against the DataModel. Use
`*` for a single segment (any name) and `**` for zero or more segments.

```
rosync query --project . 'Workspace/**/Camera'
rosync query --project . 'ReplicatedStorage/Shared/*' --format paths
rosync query --project . '**/RemoteEvent' --format classes
rosync path --project . Workspace/Camera
rosync path --project . --from fs ReplicatedStorage/Config.luau
```

Non-script, non-folder instances are visible through live `rosync tree`,
`rosync ls`, `rosync query`, `rosync find`, `rosync meta`, and `rosync get`.
Use `rosync path` when you need to jump between Studio instance paths and the
syncable files on disk. It refuses Studio-authoritative classes and paths not
present in the live Studio tree.

## 5b. Linting Luau

`rosync lint` runs `luau-lsp analyze` with current Roblox definitions and a
temporary Ro-Sync sourcemap. Its default `--data-model auto` mode merges the
complete live Studio tree and enables strict DataModel diagnostics when the
matching daemon/plugin is connected; offline it reports a relaxed fallback.
Use `--data-model studio` to require live strict coverage, `filesystem` for a
strict disk-only audit (which can flag Studio-only children), or `loose` for
gradual disk types.

The default `--compile auto` pass also runs in-scope scripts through
`luau-compile` at `-O0`, `-O1`, and `-O2` when available, catching compiler-only
failures such as the 200-register limit. Use `--compile required` to require it
or `off` to skip it. `--raw` returns structured analyzer/compiler diagnostics
and coverage metadata. Ro Sync checks bundled tools and environment/PATH
overrides; `rosync doctor` reports the selected toolchain. Human `--summary`
includes both stages. Default and GNU analyzer output are supported;
`--formatter=plain` is rejected because it does not preserve failure status.

```
rosync lint --project .
rosync lint --project . --path ServerScriptService/Foo.server.luau
rosync lint --project . --path ServerScriptService --path ReplicatedStorage/Shared --owned-only --summary
rosync lint --project . --data-model studio --raw
rosync lint --project . --data-model filesystem
rosync lint --project . --compile required
rosync lint --project . -- --no-flags-enabled
rosync lint --project . --luau-lsp /path/to/luau-lsp
```

## 5c. Asset uploads

`rosync upload` uploads assets through Roblox Open Cloud Assets. It does not
require the daemon or Studio to be connected. The API key is read from Ro Sync
Settings first; `--api-key-env` is only an explicit override. If `--creator` is
omitted, Ro Sync uses the project `groupId` from `ro-sync.json` or the active
widget project.

```
rosync upload ./icon.png --creator user:123456
rosync upload ./icon.png --creator group:123456 --name "Inventory Icon" --asset-type decal
rosync upload ./sound.mp3 ./models --project . --manifest uploaded-assets.json
rosync upload ./clip.rbxm --project . --asset-type animation
rosync upload ./icon.png --creator user:123456 --auth bearer --api-key-env ROBLOX_OAUTH_TOKEN
rosync upload ./icon.png --creator user:123456 --no-wait --raw
```

`rosync upload` accepts files and directories, recurses by default, skips
unsupported files found inside directories, continues after per-file failures,
and can write a JSON manifest with `--manifest`. It infers image, audio, model,
mesh, and video types from extensions; pass `--asset-type` for decals and
ambiguous `.rbxm`/`.rbxmx` model or animation files.

## 6. Agent usage — live Studio control

When the daemon is running (the user has Ro Sync connected to Studio), these
subcommands speak to the plugin over WebSocket and inspect or mutate live
instances. They work across the entire DataModel — not just the four
filesystem-synced classes. Every call that mutates state is appended to
`writes.log` in the platform-native Ro Sync state directory
(`ROSYNC_STATE_DIR` overrides it).

Treat these live explorer commands as authoritative when deciding what exists in
Studio. The filesystem view is intentionally narrower and can omit empty
Studio-only folders, Models, Parts, UI objects, Remotes, and other
Studio-owned instances.

Every subcommand accepts `--project <path>` (defaults aren't inferred). All
instance paths use `/`-separated Studio names rooted at `DataModel` — e.g.
`Workspace/Camera`, `ReplicatedStorage/Shared/Module`.

Read-only (safe to use unattended):

```
# Inspect one property on one instance (omit --prop for a full view).
rosync get --project . --path Workspace/Camera --prop FieldOfView

# List the direct children of an instance. --path "" lists DataModel services.
rosync ls --project . --path ReplicatedStorage

# Print the class+name tree under an instance (depth default 3).
rosync tree --project . --path Workspace --depth 3

# Export the live tree plus inspectable properties, attributes, and tags.
# Defaults to ./rosync-snapshot-<unix-seconds>.json; pass --output to choose
# a file or existing directory. Use snapshots for debugging and backups.
rosync snapshot --project .

# Compare the local script/folder representation with live Studio state.
rosync diff --project .

# Find instances by ClassName and/or name substring (live, whole DataModel).
rosync find --project . --class RemoteEvent
rosync find --project . --name Camera
```

Mutating (ask the user first — see the safety note below):

```
# Set a property on one instance. Value is a JSON literal.
rosync set --project . --path Workspace/Camera --prop FieldOfView --value 90

# Tagged values use their __type tag:
rosync set --project . --path Workspace/Part --prop Position \
  --value '{"__type":"Vector3","x":1,"y":2,"z":3}'

# Batch writes from a JSON file: [{"path":"…","prop":"…","value":…}, …]
rosync set --project . --batch writes.json

# Wrap a write (or a batch) in a named change-history waypoint so one
# ctrl-Z in Studio reverses the entire operation.
rosync set --project . --batch writes.json --waypoint "refactor camera"

# Execute arbitrary Luau inside the plugin sandbox. Escape hatch only.
rosync eval --project . --source 'return #game.Workspace:GetChildren()'
```

All of the above time out after 5 seconds if the plugin doesn't respond; a
non-zero exit code means the request never completed.

## 6b. Change-history, save, logs, and handshake

These subcommands bracket batches, roll state back, capture output, and
verify the plugin is reachable.

```
# Health / handshake. `status --raw` prints concise JSON for automation.
rosync status --project .
rosync doctor --project .
rosync ping --project .
rosync version --project .

# Tail Studio output (info/warn/error). `--tail` streams until ctrl-C.
rosync logs --project . --since 1m --level warn
rosync logs --project . --tail

# Save the place file (asynchronous; the CLI returns when Studio accepts it).
rosync save --project .

# Change history. One waypoint flanking a batch means one ctrl-Z reverses
# the whole batch; `undo` / `redo` also work from the CLI.
rosync waypoint --project . --name "before refactor"
rosync undo --project .
rosync redo --project .
```

## 6c. Structured writes — construct, destroy, reparent, attrs, tags, call, select

Live-DataModel ops beyond `set`/`eval`. Each write is appended to `writes.log`
in the platform-native Ro Sync state directory. `mv` requires
`--force` to cross a top-level service boundary.

```
# Create a new instance. --path is the parent; --props is an optional JSON
# object of initial properties (same codec as `rosync set --value`).
rosync new --project . --path Workspace --class Part --name Box \
  --props '{"Anchored":true,"Position":{"__type":"Vector3","x":0,"y":5,"z":0}}'

# Destroy an instance (:Destroy()).
rosync rm --project . --path Workspace/Box

# Reparent. Cross-service moves refuse without --force to catch mistakes like
# punting something from Workspace into ServerStorage.
rosync mv --project . --from Workspace/Box --to Workspace/Folder
rosync mv --project . --from Workspace/Box --to ServerStorage --force

# Attributes.
rosync attr set --project . --path Workspace/Box --name Speed --value 12.5
rosync attr rm  --project . --path Workspace/Box --name Speed
rosync attr ls  --project . --path Workspace/Box

# CollectionService tags.
rosync tag add --project . --path Workspace/Box --tag Enemy
rosync tag rm  --project . --path Workspace/Box --tag Enemy

# Invoke a method on an instance. --args is a JSON array encoded with the
# same codec as --value; the return value is printed as pretty JSON.
rosync call --project . --path Workspace/Folder --method FindFirstChild \
  --args '["Box"]'

# Studio Selection.
rosync select get --project .
rosync select set --project . --paths '["Workspace/Box","Workspace/SpawnLocation"]'
```

## 6d. Cross-project Studio clipboard

`rosync copy` and `rosync paste` move arbitrary native Roblox instance trees
between simultaneously connected projects. The clipboard lives in Ro Sync's
private platform state directory, so the normal agent flow is simply copy in
one project directory, `cd` to another, and paste there:

```
# Source Studio: no paths means the current Explorer Selection.
cd /path/to/source-project
rosync copy --project .

# Multiple explicit roots are serialized together, preserving references
# between them.
rosync copy --project . Workspace/Map/Boss ReplicatedStorage/BossConfig

# Destination Studio: original parent routes are restored when they exist.
cd /path/to/destination-project
rosync paste --project .

# Override the parent for every copied root.
rosync paste --project . --to Workspace/Imported
```

The payload is native `.rbxm` produced by Roblox `SerializationService`, not a
lossy JSON property projection. It preserves engine-supported classes,
properties, descendants, attributes, tags, scripts, and references among all
roots copied in the same command. Open Script Editor text is refreshed before
serialization. Services themselves cannot be copied, and references to
instances outside the copied roots do not cross places, matching native Studio
copy/paste behavior. Paste selects the new roots by default and is recorded as
one Studio Undo action; use `--no-select` to keep the current selection. Copy is
reusable and atomically replaces the prior private clipboard only after size
and SHA-256 verification.

## 6e. Introspection — class info, enums, attribute-scoped search

Read-only helpers for mapping an agent's mental model of the DataModel onto
Studio's real type system. Cheap, safe to call freely.

```
# List properties (grouped by category) and methods for a class. Uses Studio's
# reflection APIs when available; otherwise falls back to a baked table
# covering the 20 most-inspected classes.
rosync classinfo --project . --class BasePart

# List every Enum type name Studio exposes.
rosync enums --project .

# List the items (name + underlying int value) for one Enum.
rosync enum --project . --name Material

# Scope `find` to a subtree instead of the whole DataModel.
rosync find --project . --class Part --under Workspace/Map

# Find every instance that has an attribute set. Optionally filter by value —
# `--value` takes the same JSON-literal / tagged-value codec as `set --value`.
rosync find-attr --project . --name Health --under Workspace
rosync find-attr --project . --name Color --value \
  '{"__type":"Color3","r":1,"g":0,"b":0}'
```

## 6f. Capability discovery and screenshots

Protocol 2 / plugin 2.1.0 exposes optional Studio features explicitly. Check
them before choosing capture or playtest commands:

```
rosync capabilities --project . --raw
rosync capture status --project . --raw
```

Capture status is read-only, reports both screen and packaged-Photo
availability, and never prompts. Screen authorization is a separate, explicit
action that may show a Studio permission dialog. The locally packaged Photo
engine is self-contained and needs no screenshot authorization:

```
rosync capture authorize --project .
rosync capture screen --project . --region 200,120,1280,720 \
  --output-size 1024x576 --ui all --output ./captures/studio.png --raw
rosync capture photo --project . --focus Workspace/Map/Boss \
  --view isometric --size 1024x1024 --padding 1.25 --fov 32 \
  --background transparent --alpha-bleed --output ./captures/boss.png --raw
rosync capture photo --project . --focus Workspace/Map/Boss \
  --view isometric --size 1024x1024 --no-tight-crop \
  --output ./captures/boss-framed.png --raw
rosync capture photo --project . --focus Workspace/Map/Boss \
  --camera-cframe '0,10,20,1,0,0,0,1,0,0,0,1' --fov 40 \
  --size 1600x900 --output ./captures/boss-exact-camera.png --raw
rosync capture photo --project . --region 120,80,1280,720 \
  --background scene --ui overlay --output ./captures/viewport.png --raw
rosync capture photo --project . --ui only --region 120,80,1280,720 --size 1920x1080 \
  --alpha-bleed --output ./captures/hud.png --raw
rosync capture photo --project . --ui-target StarterGui/HUD/InventoryPanel \
  --size 1200x800 --alpha-bleed --output ./captures/inventory-panel.png --raw
rosync capture scene --project . --focus Workspace/Map/Boss \
  --view isometric --size 1024x1024 --output ./captures/boss.png --raw
```

Photo `--focus` is optional. With it, Ro Sync normally makes a script-free
isolated clone and frames it with `--view` or `--direction x,y,z`; use
`--include-world` to frame the original in place. Isolated transparent focus
captures tight-crop the rendered subject alpha by default. `--size WIDTHxHEIGHT`
remains exact and aspect-contains the crop in a transparent canvas; raw
metadata reports `tightCrop: true`, `region`, `fullSize`, and
`regionSource: "subject-alpha"`. Pass `--no-tight-crop` to retain the full
camera-framed render. `capture photo --background scene` and include-world
captures remain framed instead of
alpha-cropped; the isolated transparent `capture scene` alias inherits the
default crop and accepts the same opt-out. `--padding` and `--fov` tune framing.
`--camera-cframe` takes the 12
`CFrame:GetComponents()` values for an exact subject-relative camera pose and
still uses the default tight crop (or supplies an exact world pose with
`--include-world`) and works with `--fov`. Without a focus, `--region` is
`x,y,width,height` in native viewport pixels. Combine `--region` with `--size`
to crop an arbitrary viewport or UI rectangle and resample it to exact output
dimensions. `--background` is `transparent` or `scene`; `--alpha-bleed`
preserves transparent-edge RGB and `--delay` allows rendering to settle. `--ui`
is `none` (default), `overlay`, or `only`.
`overlay` keeps ScreenGui layers over the scene; `only` produces transparent
edit-mode ScreenGui RGBA without the 3D world or Studio chrome. UI-only capture
requires a transparent background and cannot use `--focus`. A full UI-only
capture with `--size` preserves the native viewport aspect ratio and centers it
in the exact output canvas with transparent padding. An explicit `--region`
continues to fill the requested output exactly. `--ui-target` implies UI-only,
isolates one ScreenGui or GuiObject subtree, and tight-crops it automatically;
`--size` aspect-contains the target while an explicit region overrides the
automatic crop. Legacy `--include-ui` remains
an alias for `--ui overlay`. All Photo paths accept
`--output`, `--timeout`, and `--raw`, with a 4096-pixel axis and 16777216-pixel
total limit. `capture scene` is an alias for this Photo engine and also requires
no authorization or place-provided capture dependency.

Photo RGBA moves in bounded chunks and is length-checked before local PNG
encoding. Camera, UI, and lighting state are restored and temporary clones are
destroyed after success or failure. For `capture screen`, `--region` is the
global logical-screen rectangle and `--output-size` is the output size; use
`--ui none` for only the 3D viewport. On macOS, explicit `capture authorize`
records Studio's exact `Feature not supported yet` result and requests system
Screen & System Audio Recording permission. Only then can a window-only native
capture serve `capture screen --ui all`. CoreGraphics selection is restricted
to a visible Roblox Studio window, and the requested region must fit completely
inside it. A merely unauthorized Studio provider does not trigger fallback;
`--ui none` does not use it. Studio PNG bytes use a short-lived, tokenized,
bounded-chunk artifact lease; native PNGs receive the same bounds, decode, and
SHA checks and are written directly. The CLI verifies and consumes Studio
transport artifacts after writing the requested output; orphaned finalized
artifacts are bounded by TTL, LRU, and a total-byte budget.

## 6g. Playtest agents

Playtests run as asynchronous jobs. Runtime plugin copies communicate with the
edit plugin through PluginConnectionService and appear as `server` and
`client:N`; they do not open their own localhost connections. Every CLI-started
job uses a private generation token, and stale contexts cannot satisfy a later
job's wait or receive its runtime requests.

### Playscript-owned runs

`rosync playtest run` is the one-command path for agent automation. It starts a
playtest, injects a main Luau playscript when its runtime context is ready,
streams progress while that script runs, prints its return value, and stops the
playtest before the command exits. No external `start` / `wait` / `exec` / poll /
`stop` choreography is required.

```
rosync playtest run --project . --script ./bench.server.luau

rosync playtest run --project . \
  --script ./bench.server.luau \
  --client-script ./join.client.luau \
  --mode multiplayer --players 2 \
  --args '{"map":"Lighthouse","laps":3}' \
  --timeout 600 --raw
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--script <file>` | required | Main playscript. Its first successful completion ends the run. |
| `--context server\|client:N` | `server` | Runtime context in which to run the main playscript. |
| `--client-script <file>` | none | Companion script injected once into every client as it becomes ready. Its return is side data and its errors do not end the run. |
| `--mode play\|run\|multiplayer` | `play` | Studio test mode. `--mode run` is Studio's server-only Run mode and is unrelated to the subcommand name. |
| `--players N` | `1` | Client count for multiplayer mode; valid range is 1-8. |
| `--args <json>` | `{}` | JSON decoded and exposed as `playtest.args` in every playscript. Invalid JSON is rejected before a playtest starts. |
| `--timeout <seconds>` | `600` | Hard wall-clock budget for boot, execution, and teardown; maximum 3600. |
| `--identity game\|plugin` | `game` | Execution identity. Plugin identity is an opt-in escape hatch. |
| `--logs off\|info\|warn\|error` | `off` | Interleave Studio output from all runtime contexts at or above the selected level. |
| `--keep-open` | off | Print the result and job ID without stopping the playtest, for subsequent `exec`, `logs`, or `capture` autopsy. |
| `--quiet` | off | Suppress progress/event lines and print only the terminal result. |
| `--raw` | off | Emit newline-delimited JSON (NDJSON), one complete object per physical line. |

Playscripts are ordinary Luau sources executed through the same temporary
Script/LocalScript mechanism and value codec as `playtest exec`. The following
job-scoped namespace is injected:

| API | Meaning |
| --- | --- |
| `playtest.args` | Value decoded from `--args`. |
| `playtest.mode` | `"play"`, `"run"`, or `"multiplayer"`. |
| `playtest.context` | Current `server` or `client:N` context name. |
| `playtest.jobId` | Current playtest job ID. |
| `playtest.emit(data)` | Stream one JSON-encodable progress value. |
| `playtest.log(msg)` | Sugar for `playtest.emit({ log = tostring(msg) })`. |
| `playtest.done(value)` | Successfully complete the run from any task or callback. |
| `playtest.fail(msg)` | Fail the run with exit code 2. |
| `playtest.signal(name, payload)` | Broadcast a generation-scoped signal to all live runtime contexts, including the sender. |
| `playtest.await(name, timeoutSec)` | Yield for a matching signal, returning `nil` on timeout. |
| `playtest.awaitClients(n, timeoutSec)` | Server-only wait for `n` ready client contexts. |

The first completion wins; later returns or calls are ignored:

1. The main script returns: its value is the result and the command exits 0.
2. Any task calls `playtest.done(value)`: that value is the result and the command exits 0.
3. The main script throws or any task calls `playtest.fail(msg)`: the error and Luau traceback are reported and the command exits 2.
4. The wall-clock deadline expires: partial events remain visible, the playtest is stopped, and the command exits 3.
5. Studio stops the job, closes, or disconnects: an `aborted` record includes the fetched final `jobStatus` and the command exits 4.
6. The playtest or required contexts never become ready: the command exits 5.

| Exit | Meaning |
| --- | --- |
| `0` | Main return or `playtest.done`; result printed. |
| `2` | Main-script error or `playtest.fail`; traceback printed. |
| `3` | Hard session deadline expired; partial events retained. |
| `4` | Job ended externally; final observed `jobStatus` reported. |
| `5` | Playtest or required runtime contexts failed to boot. |

Completion paths 1-4 automatically stop the playtest unless `--keep-open` was
passed. With `--keep-open`, the result and job ID are printed while the job
remains available to the existing `playtest exec`, `logs`, `capture`, and `stop`
commands. A playscript waiting on callbacks can end with
`return playtest.await("finished")`; `done` or `fail` may still finish it from a
spawned task.

In `--raw` mode stdout is a live NDJSON stream. Each line parses independently;
the CLI does not deduplicate, sample, or suppress `event` records. Representative
records are:

```json
{"type":"started","jobId":"...","mode":"multiplayer","timeout":600}
{"type":"ready","context":"server","t":2.1}
{"type":"ready","context":"client:1","t":4.2}
{"type":"event","t":12.4,"context":"server","data":{"phase":"Racing"}}
{"type":"log","t":13.0,"context":"client:1","level":"warn","message":"..."}
{"type":"clientResult","context":"client:1","ok":true,"value":"ok"}
{"type":"dropped","context":"server","count":17}
{"type":"aborted","reason":"job ended externally","jobStatus":"completed"}
{"type":"result","ok":true,"elapsed":214.6,"value":{"laps":[71.2,68.9,70.3]}}
```

Runtime agents send internal heartbeats about every two seconds, so a quiet
script is distinguishable from a vanished job. Missing heartbeats trigger a job
status check before an abort is reported. Every failure record includes the
final observed job status rather than presenting an empty exec-style value.
`emit` is source-rate-limited to roughly 20 records/second with a small burst
allowance and a 64 KiB encoded payload cap. Over-budget events produce explicit
`dropped` records whose counts account for the loss; they are never silently
discarded.

Main and client result values use the `playtest exec` codec and bounded-chunk
transport. An encoded result larger than 1 MiB is replaced in its result
envelope by `{"truncated":true,"bytes":N}`, where `N` is the original encoded
byte count. Stream bulk telemetry with `playtest.emit` instead.

`--identity game` is the safe default and can require modules from the temporary
playtest DataModel. `--identity plugin` runs in the plugin sandbox and cannot `require` game modules;
use it only as an explicit escape hatch.
Every coordination message is authenticated with the job's private generation
token. Stale contexts cannot emit into a later run, complete it, or satisfy its
waits.

Like every Studio playtest, a playscript runs only inside the disposable runtime
DataModel clone. Its instance, property, and source changes never sync to disk or persist back into edit mode.
`playtest run` is user-intent-gated. Its start audit entry records the script
paths and SHA-256 hashes; its completion entry records the outcome and exit
code in `writes.log`.

### Canonical playscript example

The client joins a queue and votes like a player; the server waits for that
signal, streams lap progress, and returns the final bot report:

```lua
-- join.client.luau
local Net = require(game:GetService("ReplicatedStorage"):WaitForChild("Packages"):WaitForChild("net"))
local queue = workspace:WaitForChild("Ques"):WaitForChild("1")
Net:RemoteFunction("JoinQueue"):InvokeServer(queue)
Net:RemoteFunction("VoteMap"):InvokeServer(playtest.args.map)
playtest.signal("voted")
return "ok"
```

```lua
-- bench.server.luau
local MatchService = require(game:GetService("ServerScriptService").Server.MatchService)
local Players = game:GetService("Players")

playtest.awaitClients(1, 60)
playtest.await("voted", 30)
local match = MatchService:GetPlayerMatch(Players:GetPlayers()[1])

repeat task.wait(0.25) until match.State == "Racing" or match.IsDestroyed
playtest.emit({ phase = match.State })

local bots, startedAt = {}, os.clock()
while match.State == "Racing" and not match.IsDestroyed do
	for name, ai in pairs(match.AICars) do
		local bot = bots[name] or { lap = 0, lapT = {} }
		bots[name] = bot
		local lap = ai.car:GetAttribute("Lap") or 0
		if lap > bot.lap then
			bot.lap = lap
			table.insert(bot.lapT, os.clock() - startedAt)
			playtest.emit({ lap = lap, bot = name, t = os.clock() - startedAt })
		end
	end
	local allDone = next(bots) ~= nil
	for _, bot in pairs(bots) do
		if bot.lap < playtest.args.laps then allDone = false end
	end
	if allDone then break end
	task.wait(0.3)
end
return bots
```

```
rosync playtest run --project . \
  --script ./bench.server.luau --client-script ./join.client.luau \
  --mode multiplayer --players 1 \
  --args '{"map":"Lighthouse","laps":3}' --timeout 600 --raw
```

The low-level playtest commands remain available and unchanged:

```
rosync playtest start --project . --mode play --wait --raw
rosync playtest start --project . --mode multiplayer --players 2 --wait --raw
rosync playtest status --project . --raw
rosync playtest contexts --project . --raw
rosync playtest wait --project . --minimum 2 --timeout 60 --raw

rosync playtest exec --project . --context server \
  --source 'return #game.Players:GetPlayers()' --identity game --raw
rosync playtest logs --project . --context client:1 --since-seq 0 --raw
rosync playtest ui --project . --context client:1 --class TextButton --raw
rosync playtest input --project . --context client:1 \
  --actions '[{"type":"click","x":640,"y":420}]' --raw
rosync playtest capture --project . --context client:1 \
  --output ./captures/client-1.png --raw
rosync playtest stop --project . --raw
```

`exec` accepts `--source` or `--source-file`; game identity uses a temporary
Script/LocalScript, while plugin identity must be requested explicitly. UI
inspection returns resolved visibility, text, position, and size. Input action
types are `key`, `key_press`, `mouse_move`, `mouse_delta`, `mouse_button`,
`click`, `text`, and `wait`; use `--file` for longer sequences. Use
`playtest request --context ... --op ... --args '{}'` only for advanced runtime
operations. Start/stop, exec, and input require explicit user intent. Runtime
changes are temporary and never sync back into edit mode. Input sequences are
bounded to 200 actions and 30 seconds; runtime screenshots use the same axis,
pixel, byte, session-count, and TTL limits as edit-mode capture. Plugin-identity
timeouts are cooperative, so code that spawns its own tasks remains responsible
for stopping them.

## 6h. Versioned workflows

`rosync run` validates schema version 1, then executes all steps over one
persistent remote session:

```
rosync run --file ./workflow.json --project . --dry-run
rosync run --file ./workflow.json --project . --raw
```

Minimal workflow shape:

```json
{
  "version": 1,
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

Supported steps are `get`, `set`, `new`, `rm`, `mv`, `attr-set`, `attr-rm`,
`attr-ls`, `tag-add`, `tag-rm`, `assert`, `wait`, `eval`, `capture`, `call`,
`playtest`, and `upload`. An exact string such as `$read.value` inserts an
earlier result with its JSON type intact; `$$literal` escapes `$`. References
cannot point forward or to the current step.

Use `expectedMode` / `expectedPlaceId` on the workflow and `expectedClass` /
`etag` on target steps to reject stale state. `verify: true` reads supported
writes back. Atomic transaction members must be contiguous; failure cancels the
Studio change-history recording. `eval`, `call`, `wait`, `capture`, `playtest`,
and `upload` are forbidden inside atomic groups. Assertions support equality,
existence, truthiness, containment, and numeric comparisons; `wait` polls until
its assertion passes or `timeoutMs` expires. A successful `idempotencyKey` is
stored under `.rosync-workflows/` and replays without rerunning side effects.

Workflows do not grant write authority: inspect live targets and confirm user
intent before running a workflow that mutates Studio, starts a test, sends
input, or uploads assets.

## 6i. LLM-first command budget

Do not paste or request the full command registry by default. It is large and
usually worse for agent reasoning. Use this flow instead:

1. Run `rosync context --project .` once only when you need Ro Sync project
   context that is not already in AGENTS.md.
2. Prefer local file reads and cheap offline commands for normal code work.
3. For Explorer shape or Studio-owned objects, use focused live reads:
   `rosync tree`, `rosync ls`, `rosync meta`, or `rosync get --prop`.
4. Use `rosync commands --compact` only when choosing between command families.
5. Run `rosync commands <name>` for exact flags only for the command you are
   about to use.
6. Prefer cheap offline commands for path lookup, but do not let disk-only
   inference override live Studio reads.
7. Before mutating Studio from an LLM workflow, inspect the exact live target
   with focused read-only commands and confirm explicit user intent. Use
   `rosync plan` only when a dry-run explanation is useful; do not treat it as
   a mandatory ritual.

Special-case commands:

- `rosync source` is a loose diagnostic for suspected Studio/editor divergence.
  For ordinary code inspection and verification, read the local file directly
  and lint it instead.
- `rosync conflicts` is for resolving an observed conflict. Do not poll it as a
  general health check, and do not block normal edits on it.
- `rosync changes` / `diff` can be noisy on large or already-drifty projects.
  Prefer targeted linting after focused code edits.

Cheap-first discovery:

```
rosync context --project .
rosync status --project . --raw
rosync query --project . 'ReplicatedStorage/**/Thing' --format paths
rosync path --project . ReplicatedStorage/Thing
rosync meta --project . ReplicatedStorage/Thing
rosync services --project . --raw
```

Targeted reads:

```
rosync tree --project . --path ReplicatedStorage/Client --depth 4
rosync ls --project . --path ReplicatedStorage/Client
rosync meta --project . ReplicatedStorage/Client/App
rosync get --project . --path Workspace/Part --prop Anchored
rosync props --project . --path Workspace/Part
```

`rosync source` without `--disk` asks the live plugin for Studio/editor text and
uses ScriptEditorService for script source. Treat it as an optional divergence
debug tool, not a default verification step. Prefer direct local file reads for
the file that lint and Git see.

For post-edit verification, do not treat unrelated global `rosync changes`
output as proof that your touched file failed to sync. Ro Sync projects can
have pre-existing Studio-only scripts, duplicate-name instances, or ignored
tooling under other paths. For normal script edits, the preferred verification
is the narrowest relevant `rosync lint --project . --path <path>` plus local
file inspection. Use live `rosync source`, `rosync conflicts`, `rosync changes`,
or `diff` only when the task specifically points at divergence, a reported
conflict, or sync drift.

Higher-token reads; use only when the task needs them:

```
rosync changes --project .
rosync tree --project . --path Workspace --depth 3
rosync find --project . --name Camera --under Workspace
rosync logs --project . --limit 50
```

Backup/debug only:

```
rosync snapshot --project .
```

Use plain `rosync commands` only when the user explicitly needs the full
machine-readable registry.

Preferred workflow snippets:

- Inspect one object: `meta` -> `get --prop` or `props`; use local files for script source.
- Find code: `rg`/local file reads first; use `where`/`query` when mapping Studio names.
- Verify touched scripts: local read + focused `rosync lint --path`.
- Resolve conflict: only after a conflict is reported, inspect `conflicts` -> explicit `resolve`.
- Write Studio: inspect target with `meta`/`get`/`tree` -> user confirmation -> mutating command, preferably with a waypoint for batches.
- Upload/Open Cloud: enumerate files or `monetization discover/list` first; avoid recursive/bulk writes until the target set is clear.

Two write-path flags every agent should know:

- **`--waypoint <name>`** on `set` (single or `--batch`) records a named
  Studio change-history waypoint before and after the operation, so one
  ctrl-Z in the editor reverts the whole thing. Use this for any multi-step
  write: `rosync set --batch edits.json --waypoint "re-skin box"`.
- **`set Parent` is guardrailed.** `rosync set --prop Parent …` refuses with
  a loud error by default — raw Parent assignment is the single most common
  way to corrupt a DataModel. Use `rosync mv --from X --to Y` for
  reparenting. If you genuinely need the raw write, pass `--force-parent`
  explicitly.

The audit log auto-rotates once it passes 10 MiB: `writes.log` is renamed to
`writes.log.1` in the same state directory (overwriting any prior
generation), and a fresh `writes.log` takes its place. Only one prior
generation is preserved.

Any explicit force-overwrite/strict-prune path copies the removed script tree
to `.rosync-backups/<timestamp>/` before deletion. The backup directory is
ignored by sync and Git; remove old backups after confirming the retained place.

## 7. Safety note

The filesystem → Studio sync covers only `Folder`/`Script`/`LocalScript`/
`ModuleScript` source files. `set`, `eval`, `new`, `rm`, `mv`, `attr set|rm`,
`tag add|rm`, and `call` are **user-initiated escape hatches**, not automated
tools — never invoke them from a plugin or a script, and prefer asking the
user before running them even at the CLI. Every successful write is appended
to `writes.log` in the platform-native Ro Sync state directory so the user can
audit or replay anything an agent ran on their behalf.

This build deliberately skips Roblox property sync through the filesystem;
attempts to push property changes by editing files are silently ignored. Use
`rosync set` (with the user's consent) if a property really needs to change.
<!-- ro-sync:project-memory:end -->
