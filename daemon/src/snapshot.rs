#![allow(dead_code)] // public API consumed by http routes (wired by sibling modules).

//! Snapshot emitter for the narrowed daemon scope.
//!
//! Only `Folder`, `Script`, `LocalScript`, and `ModuleScript` are surfaced.
//! Everything else on disk is ignored here — non-script instances are the
//! plugin's responsibility and should be inspected through live CLI reads.

use crate::fs_map::{
    parse_init_file, parse_plain_init_file, path_is_parent_init_source, path_to_instance_meta,
    PathInstance, ScriptClass, META_FILE,
};
use crate::fs_safety::{
    file_generation_no_follow, metadata_no_follow, read_to_string_no_follow,
    resolve_rojo_path_no_follow, validate_rojo_project_directory, validate_service_path,
    PortableDirectoryIndex, SafeEntryKind,
};
use crate::project_config;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const RO_SYNC_MD: &str = "ro-sync.md";
pub const CLAUDE_MD: &str = "CLAUDE.md";
pub const AGENTS_MD: &str = "AGENTS.md";
pub const TREE_JSON: &str = "tree.json";
pub const CODEX_DIR: &str = ".codex";
pub const CODEX_CONFIG_TOML: &str = "config.toml";
pub const STYLUA_TOML: &str = ".stylua.toml";
pub const AFTMAN_TOML: &str = "aftman.toml";
pub const LUAURC: &str = ".luaurc";
pub const ROBLOX_DEFINITIONS_PATH: &str = "tools/luau-lsp/roblox/globalTypes.d.luau";

/// Claude Code resolves `@path` references as inline imports. New projects
/// import AGENTS.md so Claude Code and Codex route through one canonical file.
pub const RO_SYNC_IMPORT_LINE: &str = "@ro-sync.md";
pub const AGENTS_IMPORT_LINE: &str = "@AGENTS.md";
const RO_SYNC_CONTEXT_START: &str = "<!-- ro-sync:project-memory:start -->";
const RO_SYNC_CONTEXT_END: &str = "<!-- ro-sync:project-memory:end -->";
const CODEX_CONTEXT_START: &str = "<!-- ro-sync:codex-context:start -->";
const CODEX_CONTEXT_END: &str = "<!-- ro-sync:codex-context:end -->";
const ROJO_PROJECT_FILE: &str = "default.project.json";
const CODEX_PROJECT_DOC_FALLBACKS: &[&str] = &[
    "ro-sync.md",
    "ro-sync.MD",
    "rosync.md",
    "ROSYNC.md",
    "CLAUDE.md",
    "CLAUDE.MD",
    "Claude.MD",
];
const CLAUDE_DOC_VARIANTS: &[&str] = &["CLAUDE.md", "CLAUDE.MD", "Claude.MD"];
const RO_SYNC_DOC_VARIANTS: &[&str] = &["ro-sync.md", "ro-sync.MD", "rosync.md", "ROSYNC.md"];
const REQUIRED_RO_SYNC_MD_TOKENS: &[&str] = &[
    "LLM-first command budget",
    "rosync context --project .",
    "rosync commands --compact",
    "rosync commands <name>",
    "Cheap-first",
    "full command registry by default",
    "Before mutating Studio",
    "waypoint for batches",
    "rosync path",
    "rosync meta",
    "rosync lint",
    "get --prop",
    "rosync source",
    "rosync changes",
    "Playtesting is a separate environment",
    "rosync playtest run",
    "completely separate DataModel",
    "AvoidSync = true",
    "init (<Name>)",
    "conflicts",
    "writes.log",
];

const CLAUDE_MD_TEMPLATE: &str = r#"# Project memory for agents

This directory is a Roblox Studio project mirrored by Ro Sync. Claude Code
and Codex share the same project instructions through AGENTS.md.

@AGENTS.md
"#;

const STYLUA_TOOL_SPEC: &str = "JohnnyMorganz/StyLua@2.5.2";
const LUAU_LSP_TOOL_SPEC: &str = "JohnnyMorganz/luau-lsp@1.68.1";
#[cfg(test)]
const STYLUA_TOOL_LINE: &str = "stylua = \"JohnnyMorganz/StyLua@2.5.2\"";
#[cfg(test)]
const LUAU_LSP_TOOL_LINE: &str = "luau-lsp = \"JohnnyMorganz/luau-lsp@1.68.1\"";
const STYLUA_TOML_TEMPLATE: &str = r#"column_width = 120
line_endings = "Unix"
indent_type = "Tabs"
indent_width = 4
quote_style = "AutoPreferDouble"
call_parentheses = "Always"
collapse_simple_statement = "Never"
"#;

const AFTMAN_TOML_TEMPLATE: &str = concat!(
    "# This file lists tools managed by Aftman, a cross-platform toolchain manager.\n",
    "# For more information, see https://github.com/LPGhatguy/aftman\n\n",
    "[tools]\n",
    "stylua = \"JohnnyMorganz/StyLua@2.5.2\"\n",
    "luau-lsp = \"JohnnyMorganz/luau-lsp@1.68.1\"\n",
);
const ROBLOX_GLOBAL_TYPES: &str = include_str!("../../tools/luau-lsp/roblox/globalTypes.d.luau");
const DEFAULT_WALLY_FOLDER: &str = "ReplicatedStorage/Packages";

/// Top-level services mirrored under the project root. Order drives the
/// on-disk service sort for the snapshot response.
pub use crate::fs_safety::SYNCED_SERVICES;
const MAX_EMITTED_INSTANCE_DEPTH: usize = 48;
pub const MAX_FLAT_INSTANCE_DEPTH: usize = 256;

/// Source-free, parent-linked snapshot record used by protocol 6 streams.
///
/// IDs are dense preorder ordinals within one service. `child_index` retains
/// Studio/disk sibling order before the daemon's deterministic projection sort,
/// while `child_count` lets the receiver validate that no record was omitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FlatSnapshotRecord {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub child_index: u32,
    pub child_count: u32,
    #[serde(default)]
    pub has_children: bool,
    pub name: String,
    pub class: String,
    #[serde(default)]
    pub avoid_sync: bool,
    #[serde(default)]
    pub avoid_sync_carrier: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_fragment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_fragment_is_dir: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_included: Option<bool>,
}

#[derive(Debug)]
pub struct FlatDiskService {
    pub records: Vec<FlatSnapshotRecord>,
    pub source_paths: HashMap<u64, PathBuf>,
}

#[allow(clippy::useless_concat)]
const RO_SYNC_MD_TEMPLATE: &str = concat!(
    r#"# Ro Sync project memory

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

## 1c. Do not fight the daemon

The daemon and the Studio plugin form one live session. When the connection
misbehaves, diagnose with `rosync status --project .` and `rosync doctor` and
let the plugin's auto-reconnect work. Agents must never:

- Run `rosync daemon restart` (or stop/start) to "fix" a connection. Every
  restart drops Studio's live connection, changes the daemon's boot identity,
  and can move it to a different port. Repeated restarts present as "Ro Sync
  keeps disconnecting" in Studio and can cross-wire reconnects between
  projects.
- Move, rename, or delete the synced service directories (`ReplicatedStorage/`,
  `ServerStorage/`, ...) while a daemon is running. The watcher sees a mass
  deletion and intentionally halts sync to protect the Studio side. To
  restructure wholesale, stop sync first, or stage the work in a directory
  outside the project and apply it through normal edits.
- Restart the daemon to force or escape an overwrite decision. Answer the
  pending decision instead (`rosync decision`).

A daemon started or restarted via the CLI also stops being managed by the
Ro Sync Desktop app; the app will report ownership errors for that project
until it can restart the daemon itself.

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

Protocol 6 / plugin 2.4.1 exposes optional Studio features explicitly. Check
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
"#
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoSyncDocRefresh {
    Created,
    Updated,
    Unchanged,
    SkippedCustom,
}

impl RoSyncDocRefresh {
    pub fn as_str(self) -> &'static str {
        match self {
            RoSyncDocRefresh::Created => "created",
            RoSyncDocRefresh::Updated => "updated",
            RoSyncDocRefresh::Unchanged => "unchanged",
            RoSyncDocRefresh::SkippedCustom => "skipped-custom",
        }
    }

    pub fn changed(self) -> bool {
        matches!(self, RoSyncDocRefresh::Created | RoSyncDocRefresh::Updated)
    }
}

fn validated_project_tool_path(
    root: &Path,
    path: &Path,
    allow_missing: bool,
) -> io::Result<(PathBuf, PathBuf)> {
    let canonical_root = crate::fs_safety::stable_canonical_directory(root)?;
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .or_else(|_| path.strip_prefix(&canonical_root))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "project tooling path {} is outside {}",
                        path.display(),
                        canonical_root.display()
                    ),
                )
            })?
    } else {
        path
    };
    let validated =
        crate::fs_safety::validate_descendant_no_follow(&canonical_root, relative, allow_missing)?;
    Ok((canonical_root, validated))
}

pub(crate) fn project_tool_file_exists(root: &Path, path: &Path) -> io::Result<bool> {
    let (canonical_root, validated) = validated_project_tool_path(root, path, true)?;
    let guard = crate::fs_safety::guard_descendant_parent_chain(&canonical_root, &validated, true)?;
    guard.verify()?;
    let metadata = crate::fs_safety::metadata_no_follow(&validated)?;
    guard.verify()?;
    match metadata {
        None => Ok(false),
        Some(metadata) if metadata.is_file() => Ok(true),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "project tooling path is not a regular file: {}",
                validated.display()
            ),
        )),
    }
}

pub(crate) fn read_project_tool_text(root: &Path, path: &Path) -> io::Result<Option<String>> {
    let (canonical_root, validated) = validated_project_tool_path(root, path, true)?;
    let guard = crate::fs_safety::guard_descendant_parent_chain(&canonical_root, &validated, true)?;
    guard.verify()?;
    let Some(metadata) = crate::fs_safety::metadata_no_follow(&validated)? else {
        guard.verify()?;
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "project tooling path is not a regular file: {}",
                validated.display()
            ),
        ));
    }
    guard.verify()?;
    let text = crate::fs_safety::read_to_string_no_follow(&validated)?;
    guard.verify()?;
    Ok(Some(text))
}

fn write_project_tool_text(root: &Path, path: &Path, text: &str) -> io::Result<()> {
    let (canonical_root, initial_target) = validated_project_tool_path(root, path, true)?;
    let parent = initial_target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("project tooling path has no parent: {}", path.display()),
        )
    })?;
    crate::fs_safety::ensure_descendant_directory_chain(&canonical_root, parent)?;
    let (_, target) = validated_project_tool_path(&canonical_root, &initial_target, true)?;
    let guard = crate::fs_safety::guard_descendant_parent_chain(&canonical_root, &target, true)?;
    let existing = crate::fs_safety::metadata_no_follow(&target)?;
    if existing
        .as_ref()
        .is_some_and(|metadata| !metadata.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "project tooling path is not a regular file: {}",
                target.display()
            ),
        ));
    }

    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid tooling filename"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary =
        target.with_file_name(format!(".{file_name}.{}-{nonce}.tmp", std::process::id()));
    let _ = validated_project_tool_path(&canonical_root, &temporary, true)?;

    let result = (|| {
        guard.verify()?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        if let Some(metadata) = existing {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }

        guard.verify()?;
        let _ = crate::fs_safety::metadata_no_follow(&target)?;
        crate::lifecycle::replace_file_atomic(&temporary, &target)?;
        guard.verify()?;
        let metadata = crate::fs_safety::require_metadata_no_follow(&target)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "project tooling replacement is not a regular file: {}",
                    target.display()
                ),
            ));
        }
        Ok(())
    })();

    if result.is_err()
        && guard.verify().is_ok()
        && matches!(
            crate::fs_safety::metadata_no_follow(&temporary),
            Ok(Some(metadata)) if metadata.is_file()
        )
    {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Write `ro-sync.md` at the root if it doesn't already exist. Existing
/// unmarked user files are left alone; generated-looking legacy files can be
/// upgraded to the current marked template.
pub fn write_ro_sync_md_if_missing(root: &Path) -> io::Result<bool> {
    Ok(refresh_ro_sync_md_impl(root, false)?.changed())
}

/// Refresh the generated Ro Sync project-memory reference. This updates marked
/// generated content and legacy generated-looking `ro-sync.md` files, but it
/// does not overwrite an unmarked custom file.
pub fn refresh_ro_sync_md(root: &Path) -> io::Result<RoSyncDocRefresh> {
    refresh_ro_sync_md_impl(root, true)
}

fn refresh_ro_sync_md_impl(root: &Path, explicit_refresh: bool) -> io::Result<RoSyncDocRefresh> {
    let p = root.join(RO_SYNC_MD);
    if let Some(existing) = read_project_tool_text(root, &p)? {
        if let Some(merged) = merge_ro_sync_generated_block(&existing) {
            if merged == existing {
                return Ok(RoSyncDocRefresh::Unchanged);
            }
            write_project_tool_text(root, &p, &merged)?;
            return Ok(RoSyncDocRefresh::Updated);
        }
        if looks_like_legacy_generated_ro_sync_md(&existing)
            && (explicit_refresh || ro_sync_md_missing_required_tokens(&existing))
        {
            if existing == RO_SYNC_MD_TEMPLATE {
                return Ok(RoSyncDocRefresh::Unchanged);
            }
            write_project_tool_text(root, &p, RO_SYNC_MD_TEMPLATE)?;
            return Ok(RoSyncDocRefresh::Updated);
        }
        if explicit_refresh && !looks_like_legacy_generated_ro_sync_md(&existing) {
            return Ok(RoSyncDocRefresh::SkippedCustom);
        }
        return Ok(RoSyncDocRefresh::Unchanged);
    }
    write_project_tool_text(root, &p, RO_SYNC_MD_TEMPLATE)?;
    Ok(RoSyncDocRefresh::Created)
}

fn ro_sync_md_missing_required_tokens(contents: &str) -> bool {
    REQUIRED_RO_SYNC_MD_TOKENS
        .iter()
        .any(|token| !contents.contains(token))
}

fn looks_like_legacy_generated_ro_sync_md(contents: &str) -> bool {
    contents.contains("# Ro Sync project memory")
        && (contents.contains("Ro Sync mirrors a narrow slice")
            || contents.contains("## 0. Agent bootstrap")
            || contents.contains("## 4. Generated files")
            || contents.contains("rosync status --project .")
            || contents.contains("do not investigate unrelated upload tools"))
}

fn merge_ro_sync_generated_block(existing: &str) -> Option<String> {
    let start = existing.find(RO_SYNC_CONTEXT_START)?;
    let end_rel = existing[start..].find(RO_SYNC_CONTEXT_END)?;
    let end = start + end_rel + RO_SYNC_CONTEXT_END.len();
    let block = ro_sync_generated_block();

    let mut merged = String::new();
    merged.push_str(&existing[..start]);
    merged.push_str(block);
    if existing[end..].starts_with('\n') {
        merged.push_str(&existing[end + 1..]);
    } else {
        merged.push_str(&existing[end..]);
    }
    Some(merged)
}

fn ro_sync_generated_block() -> &'static str {
    let start = RO_SYNC_MD_TEMPLATE
        .find(RO_SYNC_CONTEXT_START)
        .expect("ro-sync template missing start marker");
    let end_rel = RO_SYNC_MD_TEMPLATE[start..]
        .find(RO_SYNC_CONTEXT_END)
        .expect("ro-sync template missing end marker");
    let mut end = start + end_rel + RO_SYNC_CONTEXT_END.len();
    if RO_SYNC_MD_TEMPLATE[end..].starts_with('\n') {
        end += 1;
    }
    &RO_SYNC_MD_TEMPLATE[start..end]
}

/// Ensure `CLAUDE.md` at the project root imports `AGENTS.md` so Claude Code
/// and Codex use the same canonical project instructions. Behavior:
///
/// * No `CLAUDE.md`: write one with a short preamble and the `@AGENTS.md`
///   import line.
/// * `CLAUDE.md` exists without the import line: append a blank line followed
///   by the import line (user content is preserved verbatim).
/// * `CLAUDE.md` already imports `AGENTS.md`: no-op.
///
/// Returns `true` when the file was created or modified.
pub fn write_claude_md_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let p = root.join(CLAUDE_MD);
    let Some(existing) = read_project_tool_text(root, &p)? else {
        write_project_tool_text(root, &p, CLAUDE_MD_TEMPLATE)?;
        return Ok(true);
    };
    let migrated = replace_bare_ro_sync_imports_with_agents(&existing);
    if migrated != existing {
        write_project_tool_text(root, &p, &migrated)?;
        return Ok(true);
    }
    if claude_md_imports_agents(&existing) {
        return Ok(false);
    }
    let mut merged = existing;
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    if !merged.is_empty() && !merged.ends_with("\n\n") {
        merged.push('\n');
    }
    merged.push_str(AGENTS_IMPORT_LINE);
    merged.push('\n');
    write_project_tool_text(root, &p, &merged)?;
    Ok(true)
}

/// Ensure Codex and Claude Code receive the same project memory.
///
/// Codex reads `AGENTS.md` as its native project context. Claude Code reads
/// `CLAUDE.md`, which Ro Sync points at `AGENTS.md`. This keeps one canonical
/// agent file while preserving tool-specific entrypoints.
///
/// Returns `true` when any Codex-facing file was created or modified.
pub fn write_codex_context_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let mut changed = false;
    changed |= write_codex_config_if_missing_or_merge(root)?;
    changed |= write_agents_md_if_missing_or_merge(root)?;
    Ok(changed)
}

/// Ensure project-local formatter/toolchain defaults exist.
///
/// These files live at the Ro Sync project root and are intentionally not part
/// of the Roblox DataModel mirror. Existing project choices are preserved:
/// `.stylua.toml` is only created when missing, and `aftman.toml` is merged
/// only when the `[tools]` table does not already define `stylua`.
pub fn write_project_tooling_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let mut changed = false;
    changed |= write_stylua_toml_if_missing(root)?;
    changed |= write_aftman_stylua_if_missing_or_merge(root)?;
    changed |= write_roblox_definitions_if_missing_or_update(root)?;
    changed |= write_luaurc_if_missing_or_cleanup(root)?;
    Ok(changed)
}

pub fn write_stylua_toml_if_missing(root: &Path) -> io::Result<bool> {
    let p = root.join(STYLUA_TOML);
    if project_tool_file_exists(root, &p)? {
        return Ok(false);
    }
    write_project_tool_text(root, &p, STYLUA_TOML_TEMPLATE)?;
    Ok(true)
}

pub fn write_aftman_stylua_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let p = root.join(AFTMAN_TOML);
    let Some(existing) = read_project_tool_text(root, &p)? else {
        write_project_tool_text(root, &p, AFTMAN_TOML_TEMPLATE)?;
        return Ok(true);
    };
    let merged = merge_aftman_stylua_tool(&existing)?;
    if merged == existing {
        return Ok(false);
    }
    write_project_tool_text(root, &p, &merged)?;
    Ok(true)
}

pub fn write_roblox_definitions_if_missing_or_update(root: &Path) -> io::Result<bool> {
    let p = root.join(ROBLOX_DEFINITIONS_PATH);
    if read_project_tool_text(root, &p)?.as_deref() == Some(ROBLOX_GLOBAL_TYPES) {
        return Ok(false);
    }
    write_project_tool_text(root, &p, ROBLOX_GLOBAL_TYPES)?;
    Ok(true)
}

pub fn write_luaurc_if_missing_or_cleanup(root: &Path) -> io::Result<bool> {
    let p = root.join(LUAURC);
    let existing = read_project_tool_text(root, &p)?;
    let existed = existing.is_some();
    let mut config = if let Some(existing) = existing {
        serde_json::from_str::<Value>(&existing).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse {}: {e}", p.display()),
            )
        })?
    } else {
        json!({
            "languageMode": "nonstrict",
        })
    };
    let original = config.clone();

    let object = config.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} must contain a JSON object", p.display()),
        )
    })?;

    let definition = Value::String(ROBLOX_DEFINITIONS_PATH.to_string());
    if let Some(definitions) = object.get_mut("definitions") {
        let definitions = definitions.as_array_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}.definitions must be an array", p.display()),
            )
        })?;
        definitions.retain(|value| value != &definition);
        if definitions.is_empty() {
            object.remove("definitions");
        }
    }

    if existed && config == original {
        return Ok(false);
    }

    let text = serde_json::to_string_pretty(&config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_project_tool_text(root, &p, &format!("{text}\n"))?;
    Ok(true)
}

fn merge_aftman_stylua_tool(existing: &str) -> io::Result<String> {
    use toml_edit::{value, DocumentMut, Item, Table, Value as TomlValue};

    let mut document = existing.parse::<DocumentMut>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parse aftman.toml: {error}"),
        )
    })?;
    if !document.contains_key("tools") {
        document.insert("tools", Item::Table(Table::new()));
    }
    let tools = document
        .get_mut("tools")
        .expect("tools was inserted immediately above");

    let (has_stylua, has_luau_lsp) = if let Some(table) = tools.as_table() {
        (table.contains_key("stylua"), table.contains_key("luau-lsp"))
    } else if let Some(table) = tools.as_inline_table() {
        (table.contains_key("stylua"), table.contains_key("luau-lsp"))
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "aftman.toml `tools` must be a table or inline table",
        ));
    };
    if has_stylua && has_luau_lsp {
        return Ok(existing.to_string());
    }

    if let Some(table) = tools.as_table_mut() {
        if !has_stylua {
            table.insert("stylua", value(STYLUA_TOOL_SPEC));
        }
        if !has_luau_lsp {
            table.insert("luau-lsp", value(LUAU_LSP_TOOL_SPEC));
        }
    } else if let Some(table) = tools.as_inline_table_mut() {
        if !has_stylua {
            table.insert("stylua", TomlValue::from(STYLUA_TOOL_SPEC));
        }
        if !has_luau_lsp {
            table.insert("luau-lsp", TomlValue::from(LUAU_LSP_TOOL_SPEC));
        }
    }

    Ok(document.to_string())
}

pub fn write_codex_config_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let p = root.join(CODEX_DIR).join(CODEX_CONFIG_TOML);
    let desired_line = codex_project_doc_fallback_line();
    let Some(existing) = read_project_tool_text(root, &p)? else {
        write_project_tool_text(root, &p, &format!("{desired_line}\n"))?;
        return Ok(true);
    };
    let merged = merge_codex_project_doc_fallbacks(&existing);
    if merged == existing {
        return Ok(false);
    }
    write_project_tool_text(root, &p, &merged)?;
    Ok(true)
}

pub fn write_agents_md_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let p = root.join(AGENTS_MD);
    let block = codex_agents_block(root)?;
    let existing = read_project_tool_text(root, &p)?;
    let next = match existing.as_deref() {
        None => format!(
            "# Agent project memory\n\nThis file is maintained by Ro Sync. Codex reads AGENTS.md directly; Claude Code reads CLAUDE.md, which imports this file.\n\n{block}"
        ),
        Some(existing) => merge_generated_block(existing, &block),
    };

    if existing.as_deref() == Some(next.as_str()) {
        return Ok(false);
    }
    write_project_tool_text(root, &p, &next)?;
    Ok(true)
}

fn codex_agents_block(root: &Path) -> io::Result<String> {
    let mut ro_sync_sections = read_doc_variants(root, RO_SYNC_DOC_VARIANTS)?;
    if ro_sync_sections.is_empty() {
        ro_sync_sections.push((RO_SYNC_MD.to_string(), RO_SYNC_MD_TEMPLATE.into()));
    }
    let ro_sync = format_doc_sections(ro_sync_sections);
    let wally = wally_agents_section(root)?.unwrap_or_default();
    Ok(format!(
        "{CODEX_CONTEXT_START}\n\
         # Ro Sync Codex Context\n\n\
         The section between these markers is regenerated by Ro Sync. Put durable project-specific Codex notes outside the markers.\n\n\
         ## Ro Sync Project Memory\n\n\
         {ro_sync}\n\
         {wally}\
         {CODEX_CONTEXT_END}\n"
    ))
}

fn wally_agents_section(root: &Path) -> io::Result<Option<String>> {
    let cfg = project_config::read_from_disk(root)?;
    let mut parts = Vec::new();

    if let Some(cfg) = cfg.as_ref() {
        if cfg.wally_enabled
            || cfg
                .wally_file
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
        {
            let folder = cfg.wally_folder.as_deref().unwrap_or(DEFAULT_WALLY_FOLDER);
            let wally_path = wally_toml_path_for_folder(root, folder);
            let file_text = read_project_tool_text(root, &wally_path)?
                .or_else(|| cfg.wally_file.clone())
                .filter(|text| !text.trim().is_empty());

            parts.push(format!(
                "### ro-sync.json Wally settings\n\n```json\n{}\n```\n",
                serde_json::to_string_pretty(&json!({
                    "wallyEnabled": cfg.wally_enabled,
                    "wallyFolder": cfg.wally_folder.as_deref().unwrap_or(DEFAULT_WALLY_FOLDER),
                    "wallyTomlPath": relative_label(root, &wally_path),
                }))
                .unwrap_or_else(|_| "{}".to_string())
            ));

            if let Some(text) = file_text {
                parts.push(format_wally_file_section(root, &wally_path, &text));
            }
        }
    }

    if parts.is_empty() {
        for path in fallback_wally_toml_candidates(root) {
            let Some(text) = read_project_tool_text(root, &path)? else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            parts.push(format_wally_file_section(root, &path, &text));
            break;
        }
    }

    if parts.is_empty() {
        return Ok(None);
    }

    Ok(Some(format!(
        "\n## Wally Package Context\n\nRo Sync detected Wally package configuration for this project. Keep this in mind when resolving `Packages` requires or dependency-owned diagnostics.\n\n{}\n",
        parts.join("\n")
    )))
}

fn format_wally_file_section(root: &Path, path: &Path, text: &str) -> String {
    format!(
        "### {}\n\n````toml\n{}\n````\n",
        relative_label(root, path),
        text.trim_end()
    )
}

fn fallback_wally_toml_candidates(root: &Path) -> Vec<std::path::PathBuf> {
    let mut candidates = vec![root.join("wally.toml")];
    for service in SYNCED_SERVICES {
        candidates.push(root.join(service).join("wally.toml"));
    }
    candidates
}

fn wally_toml_path_for_folder(root: &Path, folder: &str) -> std::path::PathBuf {
    let normalized = folder.trim_matches('/').replace('\\', "/");
    let parent = normalized
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    if parent.is_empty() {
        root.join("wally.toml")
    } else {
        root.join(parent).join("wally.toml")
    }
}

fn relative_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn read_doc_variants(root: &Path, names: &[&str]) -> io::Result<Vec<(String, String)>> {
    let canonical_root = crate::fs_safety::stable_canonical_directory(root)?;
    let index = PortableDirectoryIndex::read_raw(&canonical_root)?;
    let mut docs = Vec::new();
    for name in names {
        if let Some(link) = index.exact_link(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "refusing linked/reparse project document: {}",
                    link.path.display()
                ),
            ));
        }
        let Some(entry) = index.exact(name) else {
            continue;
        };
        if entry.kind != SafeEntryKind::File {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "project document is not a regular file: {}",
                    entry.path.display()
                ),
            ));
        }
        let text = read_project_tool_text(&canonical_root, &entry.path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("project document disappeared: {}", entry.path.display()),
            )
        })?;
        if docs.iter().any(|(_, existing)| existing == &text) {
            continue;
        }
        docs.push(((*name).to_string(), text));
    }
    Ok(docs)
}

fn format_doc_sections(sections: Vec<(String, String)>) -> String {
    sections
        .into_iter()
        .map(|(name, body)| format!("### {name}\n\n{body}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn merge_generated_block(existing: &str, block: &str) -> String {
    let Some(start) = existing.find(CODEX_CONTEXT_START) else {
        let mut merged = existing.to_string();
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
        if !merged.ends_with("\n\n") {
            merged.push('\n');
        }
        merged.push_str(block);
        return merged;
    };
    let Some(end_rel) = existing[start..].find(CODEX_CONTEXT_END) else {
        let mut merged = existing.to_string();
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
        if !merged.ends_with("\n\n") {
            merged.push('\n');
        }
        merged.push_str(block);
        return merged;
    };
    let end = start + end_rel + CODEX_CONTEXT_END.len();
    let mut merged = String::new();
    merged.push_str(&existing[..start]);
    merged.push_str(block);
    if existing[end..].starts_with('\n') {
        merged.push_str(&existing[end + 1..]);
    } else {
        merged.push_str(&existing[end..]);
    }
    merged
}

fn codex_project_doc_fallback_line() -> String {
    let quoted: Vec<String> = CODEX_PROJECT_DOC_FALLBACKS
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect();
    format!("project_doc_fallback_filenames = [{}]", quoted.join(", "))
}

fn merge_codex_project_doc_fallbacks(existing: &str) -> String {
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    let mut found = false;
    for line in &mut lines {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("project_doc_fallback_filenames") {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "project_doc_fallback_filenames" {
            continue;
        }
        let names = order_codex_project_doc_fallbacks(parse_toml_string_array(value));
        let prefix_len = line.len() - trimmed.len();
        let prefix = " ".repeat(prefix_len);
        let quoted: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
        *line = format!(
            "{prefix}project_doc_fallback_filenames = [{}]",
            quoted.join(", ")
        );
        found = true;
        break;
    }
    if !found {
        lines.push(codex_project_doc_fallback_line());
    }

    let mut merged = lines.join("\n");
    if existing.ends_with('\n') || !merged.is_empty() {
        merged.push('\n');
    }
    merged
}

fn order_codex_project_doc_fallbacks(existing: Vec<String>) -> Vec<String> {
    let mut ordered = Vec::new();
    for desired in CODEX_PROJECT_DOC_FALLBACKS {
        if !ordered.iter().any(|name| name == desired) {
            ordered.push((*desired).to_string());
        }
    }
    for name in existing {
        if !ordered.iter().any(|existing_name| existing_name == &name) {
            ordered.push(name);
        }
    }
    ordered
}

fn parse_toml_string_array(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut item = String::new();
        let mut escaped = false;
        for next in chars.by_ref() {
            if escaped {
                item.push(next);
                escaped = false;
                continue;
            }
            match next {
                '\\' => escaped = true,
                '"' => break,
                other => item.push(other),
            }
        }
        if !out.iter().any(|existing| existing == &item) {
            out.push(item);
        }
    }
    out
}

/// True when any line of `contents` (after trimming whitespace) is exactly an
/// import token, optionally prefixed with `./`. Keeps detection robust against
/// minor user edits while avoiding false positives from mentions inside prose.
fn claude_md_imports_agents(contents: &str) -> bool {
    for line in contents.lines() {
        let t = line.trim();
        if t == AGENTS_IMPORT_LINE || t == "@./AGENTS.md" {
            return true;
        }
    }
    false
}

fn replace_bare_ro_sync_imports_with_agents(contents: &str) -> String {
    let mut changed = false;
    let has_agents_import = claude_md_imports_agents(contents);
    let mut inserted_agents_import = false;
    let mut lines = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed == RO_SYNC_IMPORT_LINE || trimmed == "@./ro-sync.md" {
            if !has_agents_import && !inserted_agents_import {
                lines.push(AGENTS_IMPORT_LINE.to_string());
                inserted_agents_import = true;
            }
            changed = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !changed {
        return contents.to_string();
    }
    let mut out = lines.join("\n");
    if contents.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Walk each service directory and return a list of service nodes, each
/// `{class, name, properties: {}, children: [...]}`. Only `Folder`, `Script`,
/// `LocalScript`, and `ModuleScript` descendants are emitted; every other
/// class is filtered out. Script nodes carry their file contents under
/// `properties.Source`; non-script nodes have an empty `properties` map for
/// schema stability.
pub fn emit_services(root: &Path) -> io::Result<Vec<Value>> {
    let mut services = Vec::new();
    for svc in SYNCED_SERVICES {
        let service_path = validate_service_path(root, svc, true)?;
        if metadata_no_follow(&service_path)?.is_none() {
            continue;
        }
        services.push(emit_service(root, svc)?);
    }
    Ok(services)
}

/// Emit one complete service projection. Unlike [`emit_services`], this also
/// returns an empty service node when its directory is absent so a strict,
/// per-service disk pull can prune stale Studio projection state.
pub fn emit_service(root: &Path, service: &str) -> io::Result<Value> {
    if !SYNCED_SERVICES.contains(&service) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported synced service: {service}"),
        ));
    }
    let service_dir = validate_service_path(root, service, true)?;
    let children = if metadata_no_follow(&service_dir)?.is_some() {
        validate_rojo_project_directory(&service_dir)?;
        walk_children(&service_dir, false)?
    } else {
        Vec::new()
    };
    Ok(json!({
        "class": service,
        "name": service,
        "properties": {},
        "children": children,
    }))
}

/// Emit one service as bounded-stream metadata without reading any script
/// Source bytes. Source file paths are returned separately so callers can hash
/// or segment one script at a time.
pub fn emit_flat_service(root: &Path, service: &str) -> io::Result<FlatDiskService> {
    if !SYNCED_SERVICES.contains(&service) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported synced service: {service}"),
        ));
    }
    let mut state = FlatDiskService {
        records: vec![FlatSnapshotRecord {
            id: 0,
            parent_id: None,
            child_index: 0,
            child_count: 0,
            has_children: true,
            name: service.to_string(),
            class: service.to_string(),
            avoid_sync: false,
            avoid_sync_carrier: false,
            disk_fragment: None,
            disk_fragment_is_dir: None,
            source_included: None,
        }],
        source_paths: HashMap::new(),
    };
    let service_dir = validate_service_path(root, service, true)?;
    if metadata_no_follow(&service_dir)?.is_some() {
        validate_rojo_project_directory(&service_dir)?;
        let children = collect_flat_disk_children(&service_dir, false, 1)?;
        state.records[0].child_count = u32::try_from(children.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot service has more than u32::MAX direct children",
            )
        })?;
        for (child_index, child) in children.into_iter().enumerate() {
            flatten_disk_node(child, 0, child_index, &mut state)?;
        }
    }
    Ok(state)
}

struct FlatDiskCandidate {
    effective_path: PathBuf,
    instance: PathInstance,
    name: String,
    disk_fragment: String,
    disk_fragment_is_dir: bool,
}

struct PendingFlatDiskNode {
    candidate: FlatDiskCandidate,
    children: Vec<PendingFlatDiskNode>,
}

fn flat_disk_candidate(path: &Path) -> io::Result<Option<FlatDiskCandidate>> {
    let Some(source_fragment) = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
    else {
        return Ok(None);
    };
    if source_fragment == META_FILE {
        return Ok(None);
    }

    let Some(path_metadata) = metadata_no_follow(path)? else {
        return Ok(None);
    };
    let path_is_dir = path_metadata.is_dir();
    let mut effective_path = path.to_path_buf();
    let mut name_override = None;
    if path_is_dir {
        if let Some(target) = default_project_path(path)? {
            let target_is_own_init =
                target.parent() == Some(path) && path_is_parent_init_source(&target)?;
            if !target_is_own_init && metadata_no_follow(&target)?.is_some() {
                name_override = path_to_instance_meta(path)?
                    .map(|instance| instance.name)
                    .or_else(|| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_string)
                    });
                effective_path = target;
            }
        }
    }

    let Some(instance) = path_to_instance_meta(&effective_path)? else {
        return Ok(None);
    };
    if instance.class == "Folder" && crate::fs_map::is_empty_plain_folder(&effective_path)? {
        return Ok(None);
    }
    if instance.class != "Folder"
        && !matches!(
            instance.class.as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        )
    {
        return Ok(None);
    }

    Ok(Some(FlatDiskCandidate {
        effective_path,
        name: name_override.unwrap_or_else(|| instance.name.clone()),
        disk_fragment: source_fragment,
        disk_fragment_is_dir: path_is_dir,
        instance,
    }))
}

fn collect_flat_disk_children(
    dir: &Path,
    parent_is_script: bool,
    depth: usize,
) -> io::Result<Vec<PendingFlatDiskNode>> {
    if depth > MAX_FLAT_INSTANCE_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "flat snapshot tree depth exceeds the supported limit of {MAX_FLAT_INSTANCE_DEPTH} instances at {}",
                dir.display()
            ),
        ));
    }

    let mut candidates = Vec::new();
    let index = PortableDirectoryIndex::read(dir)?;
    let parent_source = if parent_is_script {
        index.unique_init_source().map(|entry| entry.path.as_path())
    } else {
        None
    };
    for entry in index.entries() {
        if parent_source == Some(entry.path.as_path()) {
            continue;
        }
        if let Some(candidate) = flat_disk_candidate(&entry.path)? {
            candidates.push(candidate);
        }
    }
    candidates.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.disk_fragment.cmp(&right.disk_fragment))
    });
    let mut nodes = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let children = if candidate.instance.is_dir {
            collect_flat_disk_children(
                &candidate.effective_path,
                candidate.instance.is_script_with_children,
                depth + 1,
            )?
        } else {
            Vec::new()
        };
        // Plain folders only project when at least one descendant projects.
        // Files such as notes.md and assets are intentionally invisible.
        if candidate.instance.class == "Folder" && children.is_empty() {
            continue;
        }
        nodes.push(PendingFlatDiskNode {
            candidate,
            children,
        });
    }
    Ok(nodes)
}

fn flatten_disk_node(
    node: PendingFlatDiskNode,
    parent_id: u64,
    child_index: usize,
    state: &mut FlatDiskService,
) -> io::Result<u64> {
    let id = u64::try_from(state.records.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "snapshot contains more than u64::MAX instances",
        )
    })?;
    let child_count = u32::try_from(node.children.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "snapshot parent has more than u32::MAX children",
        )
    })?;
    let is_script = matches!(
        node.candidate.instance.class.as_str(),
        "Script" | "LocalScript" | "ModuleScript"
    );
    state.records.push(FlatSnapshotRecord {
        id,
        parent_id: Some(parent_id),
        child_index: u32::try_from(child_index).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot parent has more than u32::MAX children",
            )
        })?,
        child_count,
        has_children: node.candidate.instance.is_dir,
        name: node.candidate.name,
        class: node.candidate.instance.class.clone(),
        avoid_sync: false,
        avoid_sync_carrier: false,
        disk_fragment: Some(node.candidate.disk_fragment),
        disk_fragment_is_dir: Some(node.candidate.disk_fragment_is_dir),
        source_included: None,
    });
    if is_script {
        let source_path = if node.candidate.instance.is_script_with_children {
            find_init_source_path(
                &node.candidate.effective_path,
                node.candidate.instance.script_class,
            )?
        } else {
            node.candidate.effective_path.clone()
        };
        state.source_paths.insert(id, source_path);
    }
    for (index, child) in node.children.into_iter().enumerate() {
        flatten_disk_node(child, id, index, state)?;
    }
    Ok(id)
}

fn walk_children(dir: &Path, parent_is_script: bool) -> io::Result<Vec<Value>> {
    walk_children_at_depth(dir, parent_is_script, 1)
}

fn walk_children_at_depth(
    dir: &Path,
    parent_is_script: bool,
    depth: usize,
) -> io::Result<Vec<Value>> {
    if depth > MAX_EMITTED_INSTANCE_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "snapshot tree depth exceeds the supported limit of {MAX_EMITTED_INSTANCE_DEPTH} instances at {}",
                dir.display()
            ),
        ));
    }
    let mut out = Vec::new();
    let index = PortableDirectoryIndex::read(dir)?;
    let parent_source = if parent_is_script {
        index.unique_init_source().map(|entry| entry.path.as_path())
    } else {
        None
    };
    for entry in index.entries() {
        if entry.fragment == META_FILE {
            continue;
        }
        // The script-with-children init file describes the parent, not a child.
        if parent_source == Some(entry.path.as_path()) {
            continue;
        }
        if let Some(node) = build_whitelisted_node(&entry.path, depth)? {
            out.push(node);
        }
    }
    out.sort_by(|a, b| {
        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let af = a.get("diskFragment").and_then(Value::as_str).unwrap_or("");
        let bf = b.get("diskFragment").and_then(Value::as_str).unwrap_or("");
        an.cmp(bn).then_with(|| af.cmp(bf))
    });
    Ok(out)
}

fn build_whitelisted_node(path: &Path, depth: usize) -> io::Result<Option<Value>> {
    let source_fragment = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);
    let Some(path_metadata) = metadata_no_follow(path)? else {
        return Ok(None);
    };
    let path_is_dir = path_metadata.is_dir();
    if path_is_dir {
        if let Some(target) = default_project_path(path)? {
            let target_is_own_init =
                target.parent() == Some(path) && path_is_parent_init_source(&target)?;
            if !target_is_own_init && metadata_no_follow(&target)?.is_some() {
                let name = path_to_instance_meta(path)?
                    .map(|inst| inst.name)
                    .or_else(|| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.to_string())
                    });
                return build_whitelisted_node_at(
                    &target,
                    name,
                    source_fragment.map(|fragment| (fragment, true)),
                    depth,
                );
            }
        }
    }

    build_whitelisted_node_at(
        path,
        None,
        source_fragment.map(|fragment| (fragment, path_is_dir)),
        depth,
    )
}

fn build_whitelisted_node_at(
    path: &Path,
    name_override: Option<String>,
    disk_fragment_override: Option<(String, bool)>,
    depth: usize,
) -> io::Result<Option<Value>> {
    let Some(inst) = path_to_instance_meta(path)? else {
        return Ok(None);
    };
    if inst.class == "Folder" && crate::fs_map::is_empty_plain_folder(path)? {
        return Ok(None);
    }
    let is_script = matches!(
        inst.class.as_str(),
        "Script" | "LocalScript" | "ModuleScript"
    );
    let is_folder = inst.class == "Folder";
    if !is_script && !is_folder {
        return Ok(None);
    }

    let mut props: Map<String, Value> = Map::new();
    if is_script {
        let source = if inst.is_script_with_children {
            read_init_source(path, inst.script_class)?
        } else {
            file_generation_no_follow(path).map_err(io::Error::other)?;
            read_to_string_no_follow(path)?
        };
        props.insert("Source".to_string(), Value::String(source));
    }

    let children = if inst.is_dir {
        walk_children_at_depth(path, inst.is_script_with_children, depth + 1)?
    } else {
        Vec::new()
    };
    if is_folder && children.is_empty() {
        return Ok(None);
    }
    let (disk_fragment, disk_fragment_is_dir) = disk_fragment_override.unwrap_or_else(|| {
        (
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
            inst.is_dir,
        )
    });

    Ok(Some(json!({
        "class": inst.class,
        "name": name_override.unwrap_or(inst.name),
        "diskFragment": disk_fragment,
        "diskFragmentIsDir": disk_fragment_is_dir,
        "properties": Value::Object(props),
        "children": children,
    })))
}

fn default_project_path(dir: &Path) -> io::Result<Option<std::path::PathBuf>> {
    let index = PortableDirectoryIndex::read(dir)?;
    let Some(project_entry) = index.exact(ROJO_PROJECT_FILE) else {
        return Ok(None);
    };
    if project_entry.kind != SafeEntryKind::File {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Rojo project marker is not a regular file: {}",
                project_entry.path.display()
            ),
        ));
    }

    file_generation_no_follow(&project_entry.path).map_err(io::Error::other)?;
    let text = read_to_string_no_follow(&project_entry.path)?;
    let value: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let Some(path) = value
        .get("tree")
        .and_then(|tree| tree.get("$path"))
        .and_then(|path| path.as_str())
    else {
        return Ok(None);
    };

    Ok(Some(resolve_rojo_path_no_follow(dir, path, true)?))
}

/// Read the `init (...).luau` file inside a script-with-children directory.
/// Returns an error when the directory or source cannot be read. Treating an
/// unreadable script as empty can later push destructive empty Source text.
fn read_init_source(dir: &Path, sc: Option<ScriptClass>) -> io::Result<String> {
    let path = find_init_source_path(dir, sc)?;
    file_generation_no_follow(&path).map_err(io::Error::other)?;
    read_to_string_no_follow(&path)
}

fn find_init_source_path(dir: &Path, sc: Option<ScriptClass>) -> io::Result<PathBuf> {
    let index = PortableDirectoryIndex::read(dir)?;
    if let Some(entry) = index.unique_init_source() {
        let class = parse_init_file(&entry.fragment)
            .map(|(class, _)| class)
            .or_else(|| parse_plain_init_file(&entry.fragment));
        if class.is_some_and(|class| sc.map(|want| want == class).unwrap_or(true)) {
            return Ok(entry.path.clone());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no init source found in {}", dir.display()),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(tempfile::TempDir);
    impl TempDir {
        fn new(tag: &str) -> Self {
            TempDir(
                tempfile::Builder::new()
                    .prefix(&format!("rosync-snap-{tag}-"))
                    .tempdir()
                    .unwrap(),
            )
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    fn find_service<'a>(services: &'a [Value], name: &str) -> Option<&'a Value> {
        services
            .iter()
            .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
    }

    fn find_child<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
        node.get("children")?
            .as_array()?
            .iter()
            .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
    }

    #[test]
    fn writes_ro_sync_md_once() {
        let d = TempDir::new("md");
        assert!(write_ro_sync_md_if_missing(d.path()).unwrap());
        assert!(d.path().join(RO_SYNC_MD).exists());
        assert!(!write_ro_sync_md_if_missing(d.path()).unwrap());
    }

    #[test]
    fn refreshes_stale_generated_ro_sync_md() {
        let d = TempDir::new("md-stale");
        let p = d.path().join(RO_SYNC_MD);
        fs::write(
            &p,
            "# Ro Sync project memory\n\nRo Sync mirrors a narrow slice of a Roblox Studio DataModel into this directory.\n\nOld generated content without the asset upload command.\n",
        )
        .unwrap();
        assert!(write_ro_sync_md_if_missing(d.path()).unwrap());
        let body = fs::read_to_string(&p).unwrap();
        assert!(body.contains("rosync upload"));
    }

    #[test]
    fn refresh_skips_unmarked_custom_ro_sync_md() {
        let d = TempDir::new("md-custom");
        let p = d.path().join(RO_SYNC_MD);
        let custom = "# My own project notes\n\nKeep this file mine.\n";
        fs::write(&p, custom).unwrap();

        assert_eq!(
            refresh_ro_sync_md(d.path()).unwrap(),
            RoSyncDocRefresh::SkippedCustom
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), custom);
    }

    #[test]
    fn refresh_preserves_content_around_marked_ro_sync_block() {
        let d = TempDir::new("md-marked");
        let p = d.path().join(RO_SYNC_MD);
        fs::write(
            &p,
            format!(
                "# Ro Sync project memory\n\nUser preface.\n\n{RO_SYNC_CONTEXT_START}\nold\n{RO_SYNC_CONTEXT_END}\n\nUser footer.\n"
            ),
        )
        .unwrap();

        assert_eq!(
            refresh_ro_sync_md(d.path()).unwrap(),
            RoSyncDocRefresh::Updated
        );
        let body = fs::read_to_string(&p).unwrap();
        assert!(body.contains("User preface."));
        assert!(body.contains("User footer."));
        assert!(body.contains("rosync refresh --project ."));
        assert!(!body.contains("\nold\n"));
    }

    #[test]
    fn ro_sync_md_template_lists_new_cli_subcommands() {
        // The template is the contract agents read to learn which commands
        // exist. Lock it against regressions so future edits don't silently
        // drop a subcommand section.
        for token in REQUIRED_RO_SYNC_MD_TOKENS {
            assert!(
                RO_SYNC_MD_TEMPLATE.contains(token),
                "ro-sync.md template missing {token:?}"
            );
        }
    }

    #[test]
    fn ro_sync_md_template_documents_playscript_owned_runs() {
        for token in [
            "rosync playtest run",
            "--client-script",
            "playtest.emit",
            "playtest.done",
            "playtest.fail",
            "playtest.signal",
            "playtest.awaitClients",
            "NDJSON",
            "clientResult",
            "jobStatus",
            "64 KiB",
            "1 MiB",
            "{\"truncated\":true,\"bytes\":N}",
            "--keep-open",
            "cannot `require` game modules",
            "never sync to disk or persist back into edit mode",
            "SHA-256 hashes",
            "join.client.luau",
            "bench.server.luau",
        ] {
            assert!(
                RO_SYNC_MD_TEMPLATE.contains(token),
                "ro-sync.md playscript documentation missing {token:?}"
            );
        }
    }

    #[test]
    fn checked_in_ro_sync_md_matches_the_refresh_template() {
        assert_eq!(
            include_str!("../../ro-sync.md"),
            RO_SYNC_MD_TEMPLATE,
            "edit daemon/src/snapshot.rs whenever generated ro-sync.md changes"
        );
    }

    #[test]
    fn claude_md_created_when_missing() {
        let d = TempDir::new("claude-missing");
        assert!(write_claude_md_if_missing_or_merge(d.path()).unwrap());
        let p = d.path().join(CLAUDE_MD);
        let body = fs::read_to_string(&p).unwrap();
        assert!(
            body.lines().any(|l| l.trim() == AGENTS_IMPORT_LINE),
            "new CLAUDE.md must import AGENTS.md; got:\n{body}"
        );
        // Idempotent: a second call must not rewrite the file.
        assert!(!write_claude_md_if_missing_or_merge(d.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), body);
    }

    #[test]
    fn claude_md_merged_when_existing_without_import() {
        let d = TempDir::new("claude-merge");
        let p = d.path().join(CLAUDE_MD);
        let user_content = "# My project\n\nSome notes the user wrote.\n";
        fs::write(&p, user_content).unwrap();

        assert!(write_claude_md_if_missing_or_merge(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(
            merged.starts_with(user_content),
            "user content must be preserved verbatim at the top"
        );
        assert!(
            merged.lines().any(|l| l.trim() == AGENTS_IMPORT_LINE),
            "merged CLAUDE.md must contain the import line; got:\n{merged}"
        );

        // Second call is a no-op now that the import is present.
        assert!(!write_claude_md_if_missing_or_merge(d.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), merged);
    }

    #[test]
    fn claude_md_preserved_when_import_present() {
        let d = TempDir::new("claude-present");
        let p = d.path().join(CLAUDE_MD);
        let existing = "# Existing\n\n@AGENTS.md\n\nMore user notes.\n";
        fs::write(&p, existing).unwrap();
        assert!(!write_claude_md_if_missing_or_merge(d.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), existing);
    }

    #[test]
    fn claude_md_migrates_old_ro_sync_import_to_agents() {
        let d = TempDir::new("claude-old-import");
        let p = d.path().join(CLAUDE_MD);
        fs::write(&p, "# Existing\n\n@ro-sync.md\n").unwrap();
        assert!(write_claude_md_if_missing_or_merge(d.path()).unwrap());
        let migrated = fs::read_to_string(&p).unwrap();
        assert!(migrated.contains("@AGENTS.md"));
        assert!(!migrated.lines().any(|line| line.trim() == "@ro-sync.md"));
    }

    #[test]
    fn claude_md_detects_relative_import_form() {
        // `@./AGENTS.md` resolves to the same file in Claude Code, so it
        // must count as already-imported and not trigger an append.
        let d = TempDir::new("claude-relative");
        let p = d.path().join(CLAUDE_MD);
        let existing = "# doc\n\n@./AGENTS.md\n";
        fs::write(&p, existing).unwrap();
        assert!(!write_claude_md_if_missing_or_merge(d.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), existing);
    }

    #[test]
    fn claude_md_does_not_match_mention_inside_prose() {
        // A line like "see @AGENTS.md for details" should NOT count as an
        // import — Claude Code only treats bare `@path` lines as imports.
        let d = TempDir::new("claude-prose");
        let p = d.path().join(CLAUDE_MD);
        let existing = "# doc\n\nsee @AGENTS.md for details\n";
        fs::write(&p, existing).unwrap();
        assert!(write_claude_md_if_missing_or_merge(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(merged.starts_with(existing), "user content preserved");
        assert!(
            merged.lines().any(|l| l.trim() == AGENTS_IMPORT_LINE),
            "bare import line should have been appended; got:\n{merged}"
        );
    }

    #[test]
    fn claude_md_handles_file_without_trailing_newline() {
        let d = TempDir::new("claude-nonewline");
        let p = d.path().join(CLAUDE_MD);
        fs::write(&p, b"# tight").unwrap(); // no trailing newline
        assert!(write_claude_md_if_missing_or_merge(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(merged.starts_with("# tight"));
        assert!(merged.lines().any(|l| l.trim() == AGENTS_IMPORT_LINE));
    }

    #[test]
    fn codex_context_inlines_ro_sync_docs() {
        let d = TempDir::new("codex-context");
        fs::write(d.path().join(CLAUDE_MD), "# Claude notes\n").unwrap();
        fs::write(d.path().join(RO_SYNC_MD), "# Ro Sync notes\n").unwrap();

        assert!(write_codex_context_if_missing_or_merge(d.path()).unwrap());
        let agents = fs::read_to_string(d.path().join(AGENTS_MD)).unwrap();
        assert!(agents.contains(CODEX_CONTEXT_START));
        assert!(agents.contains("# Ro Sync notes"));
        assert!(!agents.contains("# Claude notes"));

        let config = fs::read_to_string(d.path().join(CODEX_DIR).join(CODEX_CONFIG_TOML)).unwrap();
        assert!(config.contains("\"CLAUDE.md\""));
        assert!(config.contains("\"ro-sync.md\""));
        assert!(
            config.find("\"ro-sync.md\"").unwrap() < config.find("\"CLAUDE.md\"").unwrap(),
            "ro-sync.md must be the first matching Codex fallback; got:\n{config}"
        );

        assert!(!write_codex_context_if_missing_or_merge(d.path()).unwrap());
    }

    #[test]
    fn codex_context_preserves_existing_agents_notes() {
        let d = TempDir::new("codex-agents-merge");
        fs::write(d.path().join(CLAUDE_MD), "# Claude v1\n").unwrap();
        fs::write(d.path().join(RO_SYNC_MD), "# Ro Sync v1\n").unwrap();
        let p = d.path().join(AGENTS_MD);
        fs::write(&p, "# User Codex notes\n\nKeep this.\n").unwrap();

        assert!(write_codex_context_if_missing_or_merge(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(merged.starts_with("# User Codex notes\n\nKeep this.\n"));
        assert!(merged.contains("# Ro Sync v1"));

        fs::write(d.path().join(RO_SYNC_MD), "# Ro Sync v2\n").unwrap();
        assert!(write_codex_context_if_missing_or_merge(d.path()).unwrap());
        let updated = fs::read_to_string(&p).unwrap();
        assert!(updated.contains("# Ro Sync v2"));
        assert!(!updated.contains("# Ro Sync v1"));
        assert_eq!(updated.matches(CODEX_CONTEXT_START).count(), 1);
    }

    #[test]
    fn codex_context_does_not_inline_claude_to_avoid_import_cycles() {
        let d = TempDir::new("codex-no-claude-cycle");
        fs::write(d.path().join(CLAUDE_MD), "@AGENTS.md\n").unwrap();
        fs::write(d.path().join(RO_SYNC_MD), "# Ro Sync notes\n").unwrap();

        assert!(write_codex_context_if_missing_or_merge(d.path()).unwrap());
        let agents = fs::read_to_string(d.path().join(AGENTS_MD)).unwrap();
        assert!(!agents.contains("@AGENTS.md"));
        assert!(!agents.contains("### CLAUDE.md"));
        assert!(agents.contains("# Ro Sync notes"));
    }

    #[test]
    fn codex_context_embeds_wally_config_from_project_config() {
        let d = TempDir::new("codex-wally");
        fs::write(d.path().join(RO_SYNC_MD), "# Ro Sync notes\n").unwrap();
        fs::write(
            d.path().join("ro-sync.json"),
            r#"{
  "name": "WallyProject",
  "gameId": null,
  "groupId": null,
  "placeIds": [],
  "wallyEnabled": true,
  "wallyFolder": "ReplicatedStorage/Packages",
  "wallyFile": "[dependencies]\nNet = \"sleitnick/net@0.2.0\"\n",
  "version": 1
}"#,
        )
        .unwrap();

        assert!(write_codex_context_if_missing_or_merge(d.path()).unwrap());
        let agents = fs::read_to_string(d.path().join(AGENTS_MD)).unwrap();
        assert!(agents.contains("## Wally Package Context"));
        assert!(agents.contains("### ro-sync.json Wally settings"));
        assert!(agents.contains("\"wallyFolder\": \"ReplicatedStorage/Packages\""));
        assert!(agents.contains("\"wallyTomlPath\": \"ReplicatedStorage/wally.toml\""));
        assert!(agents.contains("### ReplicatedStorage/wally.toml"));
        assert!(agents.contains("Net = \"sleitnick/net@0.2.0\""));
    }

    #[test]
    fn codex_config_merges_existing_fallbacks() {
        let existing =
            "mcp_servers = {}\nproject_doc_fallback_filenames = [\"CUSTOM.md\", \"CLAUDE.md\"]\n";
        let merged = merge_codex_project_doc_fallbacks(existing);
        assert!(merged.contains("mcp_servers = {}"));
        assert!(merged.contains("\"CUSTOM.md\""));
        assert!(merged.contains("\"CLAUDE.md\""));
        assert!(merged.contains("\"ro-sync.md\""));
        assert_eq!(merged.matches("\"CLAUDE.md\"").count(), 1);
        assert!(
            merged.find("\"ro-sync.md\"").unwrap() < merged.find("\"CLAUDE.md\"").unwrap(),
            "ro-sync.md must be moved ahead of CLAUDE.md; got:\n{merged}"
        );
    }

    #[test]
    fn project_tooling_defaults_are_created() {
        let d = TempDir::new("tooling-defaults");
        assert!(write_project_tooling_if_missing_or_merge(d.path()).unwrap());

        let stylua = fs::read_to_string(d.path().join(STYLUA_TOML)).unwrap();
        assert!(stylua.contains("indent_type = \"Tabs\""));
        assert!(stylua.contains("collapse_simple_statement = \"Never\""));

        let aftman = fs::read_to_string(d.path().join(AFTMAN_TOML)).unwrap();
        assert!(aftman.contains("[tools]"));
        assert!(aftman.contains(STYLUA_TOOL_LINE));
        assert!(aftman.contains(LUAU_LSP_TOOL_LINE));

        let luaurc = fs::read_to_string(d.path().join(LUAURC)).unwrap();
        assert!(luaurc.contains("\"languageMode\""));
        assert!(!luaurc.contains("\"definitions\""));
        assert!(d.path().join(ROBLOX_DEFINITIONS_PATH).is_file());

        assert!(!write_project_tooling_if_missing_or_merge(d.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_refuses_a_linked_generated_document() {
        use std::os::unix::fs::symlink;

        let project = TempDir::new("linked-refresh-doc");
        let external = TempDir::new("linked-refresh-external");
        let sentinel = external.path().join("sentinel.md");
        fs::write(&sentinel, "# external sentinel\n").unwrap();
        symlink(&sentinel, project.path().join(RO_SYNC_MD)).unwrap();

        let error = refresh_ro_sync_md(project.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(&sentinel).unwrap(),
            "# external sentinel\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tooling_creation_refuses_a_linked_parent_directory() {
        use std::os::unix::fs::symlink;

        let project = TempDir::new("linked-tooling-parent");
        let external = TempDir::new("linked-tooling-external");
        fs::write(external.path().join("sentinel"), "keep").unwrap();
        symlink(external.path(), project.path().join("tools")).unwrap();

        let error = write_roblox_definitions_if_missing_or_update(project.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!external.path().join("luau-lsp").exists());
        assert_eq!(
            fs::read_to_string(external.path().join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn luaurc_merge_preserves_existing_config_without_definitions_key() {
        let d = TempDir::new("luaurc-merge");
        let p = d.path().join(LUAURC);
        fs::write(
            &p,
            "{\n  \"languageMode\": \"strict\",\n  \"diagnostics\": {\"unused-local\": \"ignore\"}\n}\n",
        )
        .unwrap();

        assert!(!write_luaurc_if_missing_or_cleanup(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(merged.contains("\"languageMode\": \"strict\""));
        assert!(merged.contains("\"diagnostics\""));
        assert!(!merged.contains("\"definitions\""));

        assert!(!write_luaurc_if_missing_or_cleanup(d.path()).unwrap());
    }

    #[test]
    fn luaurc_merge_removes_generated_definitions_key() {
        let d = TempDir::new("luaurc-generated-definitions");
        let p = d.path().join(LUAURC);
        fs::write(
            &p,
            format!(
                "{{\n  \"definitions\": [\"{ROBLOX_DEFINITIONS_PATH}\"],\n  \"languageMode\": \"nonstrict\"\n}}\n"
            ),
        )
        .unwrap();

        assert!(write_luaurc_if_missing_or_cleanup(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(merged.contains("\"languageMode\": \"nonstrict\""));
        assert!(!merged.contains("\"definitions\""));
        assert!(!merged.contains(ROBLOX_DEFINITIONS_PATH));
    }

    #[test]
    fn aftman_merge_adds_stylua_to_existing_tools() {
        let d = TempDir::new("aftman-merge");
        let p = d.path().join(AFTMAN_TOML);
        fs::write(
            &p,
            "# existing\n\n[tools]\nwally = \"UpliftGames/wally@0.3.2\"\n",
        )
        .unwrap();

        assert!(write_aftman_stylua_if_missing_or_merge(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(merged.contains("wally = \"UpliftGames/wally@0.3.2\""));
        assert!(merged.contains(STYLUA_TOOL_LINE));
        assert!(merged.contains(LUAU_LSP_TOOL_LINE));
        let parsed = merged.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(parsed["tools"]["stylua"].as_str(), Some(STYLUA_TOOL_SPEC));
        assert_eq!(
            parsed["tools"]["luau-lsp"].as_str(),
            Some(LUAU_LSP_TOOL_SPEC)
        );
    }

    #[test]
    fn aftman_merge_preserves_existing_stylua() {
        let d = TempDir::new("aftman-existing");
        let p = d.path().join(AFTMAN_TOML);
        let existing = "[tools]\nstylua = \"JohnnyMorganz/StyLua@2.4.1\"\nwally = \"UpliftGames/wally@0.3.2\"\n";
        fs::write(&p, existing).unwrap();

        assert!(write_aftman_stylua_if_missing_or_merge(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(merged.contains("stylua = \"JohnnyMorganz/StyLua@2.4.1\""));
        assert!(merged.contains(LUAU_LSP_TOOL_LINE));
        assert!(merged.contains("wally = \"UpliftGames/wally@0.3.2\""));

        let custom = "[tools]\nstylua = \"JohnnyMorganz/StyLua@2.4.1\"\nluau-lsp = \"JohnnyMorganz/luau-lsp@1.67.0\"\n";
        fs::write(&p, custom).unwrap();
        assert!(!write_aftman_stylua_if_missing_or_merge(d.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), custom);
    }

    #[test]
    fn aftman_merge_handles_valid_table_spellings_without_duplicate_tools() {
        let cases = [
            "[tools] # managed\nwally = \"UpliftGames/wally@0.3.2\"\n",
            "['tools']\nwally = \"UpliftGames/wally@0.3.2\"\n",
            "tools.wally = \"UpliftGames/wally@0.3.2\"\n",
            "tools = { wally = \"UpliftGames/wally@0.3.2\" }\n",
        ];

        for (index, existing) in cases.into_iter().enumerate() {
            let d = TempDir::new(&format!("aftman-valid-{index}"));
            let path = d.path().join(AFTMAN_TOML);
            fs::write(&path, existing).unwrap();

            assert!(write_aftman_stylua_if_missing_or_merge(d.path()).unwrap());
            let merged = fs::read_to_string(&path).unwrap();
            let parsed = merged.parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(
                parsed["tools"]["wally"].as_str(),
                Some("UpliftGames/wally@0.3.2")
            );
            assert_eq!(parsed["tools"]["stylua"].as_str(), Some(STYLUA_TOOL_SPEC));
            assert_eq!(
                parsed["tools"]["luau-lsp"].as_str(),
                Some(LUAU_LSP_TOOL_SPEC)
            );
        }
    }

    #[test]
    fn aftman_merge_rejects_non_table_tools_without_modifying_file() {
        let d = TempDir::new("aftman-invalid-tools");
        let path = d.path().join(AFTMAN_TOML);
        let existing = "tools = \"not a table\"\n";
        fs::write(&path, existing).unwrap();

        let error = write_aftman_stylua_if_missing_or_merge(d.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(path).unwrap(), existing);
    }

    #[test]
    fn empty_project_emits_no_services() {
        let d = TempDir::new("empty");
        let services = emit_services(d.path()).unwrap();
        assert!(services.is_empty());
    }

    #[test]
    fn emits_services_and_scripts() {
        let d = TempDir::new("full");
        let rs = d.path().join("ReplicatedStorage");
        fs::create_dir_all(&rs).unwrap();
        fs::write(rs.join("Config.luau"), b"return {}").unwrap();
        fs::write(rs.join("Main.server.luau"), b"-- svr").unwrap();

        let shared = rs.join("Shared");
        fs::create_dir(&shared).unwrap();
        fs::write(shared.join("Util.luau"), b"return 42").unwrap();

        let services = emit_services(d.path()).unwrap();
        let rs_node = find_service(&services, "ReplicatedStorage").expect("service present");
        assert_eq!(rs_node["class"], "ReplicatedStorage");

        let config = find_child(rs_node, "Config").unwrap();
        assert_eq!(config["class"], "ModuleScript");
        assert_eq!(config["diskFragment"], "Config.luau");
        assert_eq!(config["diskFragmentIsDir"], false);
        assert_eq!(config["properties"]["Source"], "return {}");
        assert_eq!(config["children"].as_array().unwrap().len(), 0);

        let main = find_child(rs_node, "Main").unwrap();
        assert_eq!(main["class"], "Script");
        assert_eq!(main["properties"]["Source"], "-- svr");

        let shared_node = find_child(rs_node, "Shared").unwrap();
        assert_eq!(shared_node["class"], "Folder");
        let util = find_child(shared_node, "Util").unwrap();
        assert_eq!(util["class"], "ModuleScript");
        assert_eq!(util["properties"]["Source"], "return 42");
    }

    #[test]
    fn duplicate_snapshot_nodes_retain_exact_disk_fragment_identity() {
        let d = TempDir::new("duplicate-fragment-identity");
        let rs = d.path().join("ReplicatedStorage");
        fs::create_dir_all(&rs).unwrap();
        fs::write(rs.join("Same.luau"), b"return 'first'").unwrap();
        fs::write(rs.join("Same [1].luau"), b"return 'second'").unwrap();

        let services = emit_services(d.path()).unwrap();
        let rs_node = find_service(&services, "ReplicatedStorage").unwrap();
        let children = rs_node["children"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0]["name"], "Same");
        assert_eq!(children[0]["diskFragment"], "Same [1].luau");
        assert_eq!(children[0]["properties"]["Source"], "return 'second'");
        assert_eq!(children[1]["name"], "Same");
        assert_eq!(children[1]["diskFragment"], "Same.luau");
        assert_eq!(children[1]["properties"]["Source"], "return 'first'");
    }

    #[test]
    fn over_deep_disk_tree_fails_cleanly_before_recursive_overflow() {
        let d = TempDir::new("over-deep-disk-tree");
        let mut cursor = d.path().join("ReplicatedStorage");
        fs::create_dir_all(&cursor).unwrap();
        for index in 0..=MAX_EMITTED_INSTANCE_DEPTH {
            cursor = cursor.join(format!("D{index}"));
            fs::create_dir(&cursor).unwrap();
        }
        fs::write(cursor.join("Leaf.luau"), b"return true").unwrap();

        let error = emit_services(d.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("snapshot tree depth exceeds"));
    }

    #[test]
    fn omits_folder_chains_with_no_syncable_descendants() {
        let d = TempDir::new("empty-folder-chain");
        let leaf = d
            .path()
            .join("ReplicatedStorage")
            .join("Assets")
            .join("EventVFX")
            .join("Galaxy");
        fs::create_dir_all(&leaf).unwrap();

        let services = emit_services(d.path()).unwrap();

        let rs_node = find_service(&services, "ReplicatedStorage").expect("service present");
        assert_eq!(rs_node["children"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn emits_script_with_children() {
        let d = TempDir::new("swc");
        let sss = d.path().join("ServerScriptService");
        fs::create_dir_all(&sss).unwrap();
        let net = sss.join("Net");
        fs::create_dir(&net).unwrap();
        fs::write(net.join("init (Net).server.luau"), b"-- root").unwrap();
        fs::write(net.join("Helper.luau"), b"return {}").unwrap();

        let services = emit_services(d.path()).unwrap();
        let sss_node = find_service(&services, "ServerScriptService").unwrap();
        let net_node = find_child(sss_node, "Net").unwrap();
        assert_eq!(net_node["class"], "Script");
        assert_eq!(net_node["properties"]["Source"], "-- root");
        assert_eq!(net_node["children"].as_array().unwrap().len(), 1);
        let helper = find_child(net_node, "Helper").unwrap();
        assert_eq!(helper["class"], "ModuleScript");
    }

    #[test]
    fn script_with_children_emits_mismatched_named_init_leaf() {
        let d = TempDir::new("swc-mismatched-init-leaf");
        let misc = d.path().join("ReplicatedStorage").join("Misc");
        fs::create_dir_all(&misc).unwrap();
        fs::write(misc.join("init (Misc).luau"), b"return 'parent'").unwrap();
        fs::write(
            misc.join("init (Notifications).luau"),
            b"return 'literal child'",
        )
        .unwrap();

        let services = emit_services(d.path()).unwrap();
        let rs = find_service(&services, "ReplicatedStorage").unwrap();
        let misc_node = find_child(rs, "Misc").unwrap();
        assert_eq!(misc_node["properties"]["Source"], "return 'parent'");
        let literal = find_child(misc_node, "init (Notifications)").unwrap();
        assert_eq!(literal["class"], "ModuleScript");
        assert_eq!(literal["properties"]["Source"], "return 'literal child'");

        let flat = emit_flat_service(d.path(), "ReplicatedStorage").unwrap();
        let misc_flat = flat
            .records
            .iter()
            .find(|record| record.name == "Misc")
            .unwrap();
        let literal_flat = flat
            .records
            .iter()
            .find(|record| {
                record.parent_id == Some(misc_flat.id) && record.name == "init (Notifications)"
            })
            .unwrap();
        assert_eq!(literal_flat.class, "ModuleScript");
    }

    #[test]
    fn emits_wally_plain_init_folder_as_module_script() {
        let d = TempDir::new("wally-init");
        let pkg = d
            .path()
            .join("ReplicatedStorage")
            .join("Packages")
            .join("_Index")
            .join("sleitnick_net@0.2.0")
            .join("net");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("init.lua"), b"return { create = function() end }").unwrap();
        fs::write(pkg.join("Client.lua"), b"return {}").unwrap();

        let services = emit_services(d.path()).unwrap();
        let rs_node = find_service(&services, "ReplicatedStorage").unwrap();
        let packages = find_child(rs_node, "Packages").unwrap();
        let index = find_child(packages, "_Index").unwrap();
        let version = find_child(index, "sleitnick_net@0.2.0").unwrap();
        let net = find_child(version, "net").unwrap();

        assert_eq!(net["class"], "ModuleScript");
        assert_eq!(
            net["properties"]["Source"],
            "return { create = function() end }"
        );
        assert!(find_child(net, "init").is_none());
        assert_eq!(find_child(net, "Client").unwrap()["class"], "ModuleScript");
    }

    #[test]
    fn emits_wally_default_project_path_as_package_root_module() {
        let d = TempDir::new("wally-default-project");
        let pkg = d
            .path()
            .join("ReplicatedStorage")
            .join("Packages")
            .join("_Index")
            .join("evaera_promise@4.0.0")
            .join("promise");
        let lib = pkg.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            pkg.join("default.project.json"),
            br#"{"name":"promise","tree":{"$path":"lib"}}"#,
        )
        .unwrap();
        fs::write(lib.join("init.lua"), b"return { status = 'ok' }").unwrap();
        fs::write(lib.join("Error.lua"), b"return {}").unwrap();

        let services = emit_services(d.path()).unwrap();
        let rs_node = find_service(&services, "ReplicatedStorage").unwrap();
        let packages = find_child(rs_node, "Packages").unwrap();
        let index = find_child(packages, "_Index").unwrap();
        let version = find_child(index, "evaera_promise@4.0.0").unwrap();
        let promise = find_child(version, "promise").unwrap();

        assert_eq!(promise["class"], "ModuleScript");
        assert_eq!(promise["name"], "promise");
        assert_eq!(promise["properties"]["Source"], "return { status = 'ok' }");
        assert!(find_child(promise, "init").is_none());
        assert_eq!(
            find_child(promise, "Error").unwrap()["class"],
            "ModuleScript"
        );
    }

    #[test]
    fn direct_init_rojo_path_preserves_wrapper_directory_shape_in_flat_and_legacy() {
        let d = TempDir::new("wally-direct-init-wrapper");
        let package = d.path().join("ReplicatedStorage").join("Promise");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("default.project.json"),
            r#"{"tree":{"$path":"init.luau"}}"#,
        )
        .unwrap();
        fs::write(package.join("init.luau"), "return {}").unwrap();
        fs::write(package.join("Error.luau"), "return {}").unwrap();

        let legacy = emit_service(d.path(), "ReplicatedStorage").unwrap();
        let promise = find_child(&legacy, "Promise").unwrap();
        assert_eq!(promise["class"], "ModuleScript");
        assert_eq!(promise["diskFragmentIsDir"], true);
        assert_eq!(promise["properties"]["Source"], "return {}");
        assert_eq!(
            find_child(promise, "Error").unwrap()["class"],
            "ModuleScript"
        );

        let flat = emit_flat_service(d.path(), "ReplicatedStorage").unwrap();
        let promise = flat
            .records
            .iter()
            .find(|record| record.name == "Promise")
            .unwrap();
        assert_eq!(promise.class, "ModuleScript");
        assert_eq!(promise.disk_fragment_is_dir, Some(true));
        assert!(promise.has_children);
        assert_eq!(
            flat.source_paths
                .get(&promise.id)
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str()),
            Some("init.luau")
        );
    }

    #[test]
    fn default_project_path_rejects_windows_parent_traversal() {
        let d = TempDir::new("wally-default-project-traversal");
        let pkg = d.path().join("ReplicatedStorage").join("Packages");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("default.project.json"),
            r#"{"tree":{"$path":"..\\Outside"}}"#,
        )
        .unwrap();

        let error = default_project_path(&pkg).unwrap_err();
        assert!(error.to_string().contains("unsafe Rojo $path"));
    }

    #[test]
    fn stray_meta_json_is_ignored() {
        // `.meta.json` is out of scope in the narrowed daemon — it must not
        // surface as its own node and must not affect its parent's emission.
        let d = TempDir::new("stray-meta");
        let rs = d.path().join("ReplicatedStorage");
        fs::create_dir_all(&rs).unwrap();
        fs::write(rs.join(".meta.json"), br#"{"className":"Anything"}"#).unwrap();
        fs::write(rs.join("Config.luau"), b"return {}").unwrap();

        let services = emit_services(d.path()).unwrap();
        let rs_node = find_service(&services, "ReplicatedStorage").unwrap();
        let names: Vec<&str> = rs_node["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Config"], "only the script should surface");
    }

    #[test]
    fn missing_service_dirs_are_skipped() {
        let d = TempDir::new("partial");
        fs::create_dir_all(d.path().join("Workspace")).unwrap();
        let services = emit_services(d.path()).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0]["name"], "Workspace");
    }
}
