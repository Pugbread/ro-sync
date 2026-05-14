#![allow(dead_code)] // public API consumed by http routes (wired by sibling modules).

//! Snapshot emitter for the narrowed daemon scope.
//!
//! Only `Folder`, `Script`, `LocalScript`, and `ModuleScript` are surfaced.
//! Everything else on disk is ignored here — non-script instances are the
//! plugin's responsibility and reach the project via the read-only
//! `tree.json` skeleton (class+name+children, no property values).

use crate::fs_map::{
    is_init_file, parse_init_file, parse_plain_init_file, path_to_instance_meta, ScriptClass,
    META_FILE,
};
use crate::project_config;
use serde_json::{json, Map, Value};
use std::fs;
use std::io;
use std::path::Path;

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
    "Never run mutating commands",
    "rosync plan",
    "rosync path",
    "rosync meta",
    "get --prop",
    "rosync source",
    "rosync changes",
    "Playtesting is a separate environment",
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

const STYLUA_TOOL_LINE: &str = "stylua = \"JohnnyMorganz/StyLua@2.4.1\"";
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
    "stylua = \"JohnnyMorganz/StyLua@2.4.1\"\n",
);
const ROBLOX_GLOBAL_TYPES: &str = include_str!("../../tools/luau-lsp/roblox/globalTypes.d.luau");
const DEFAULT_WALLY_FOLDER: &str = "ReplicatedStorage/Packages";

/// Top-level services mirrored under the project root. Order drives the
/// on-disk service sort for the snapshot response.
pub const SYNCED_SERVICES: &[&str] = &[
    "ReplicatedStorage",
    "ServerScriptService",
    "StarterPlayer",
    "StarterGui",
    "Workspace",
    "ReplicatedFirst",
    "ServerStorage",
    "Lighting",
];

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
rosync refresh --project .
rosync status --project .
rosync diff --project .
rosync path --project . Workspace/Camera
```

For asset uploads, use Ro Sync, not external asset tools:

```
rosync upload ./image.png --project .
rosync upload ./audio.mp3 ./models --project . --manifest uploaded-assets.json
rosync upload ./clip.rbxm --project . --asset-type animation
```

`rosync upload` reads the Roblox Open Cloud credential from the Ro Sync widget
Secrets store (or `ROBLOX_API_KEY`) and uses the project `groupId` as
`group:<id>` when `--creator` is omitted. It supports Roblox Open Cloud asset
types including image, decal, audio, model, mesh, animation, and video. Use
`--asset-type` for decals and ambiguous `.rbxm`/`.rbxmx` files.

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
Roblox class is Studio-authoritative: the plugin emits a read-only `tree.json`
skeleton at the project root so tooling can see the rest of the DataModel, but
the daemon never writes those instances to disk and never pushes property
changes to Studio.

## 1b. Playtesting is a separate environment

Roblox Studio playtesting clones the current edit environment. Script edits made
while playtesting run inside that cloned playtest environment and DO NOT mirror
back into the current edit DataModel. Ro Sync is connected to the current edit
DataModel and this directory, not the temporary playtest clone.

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
  exclude that subtree from filesystem sync. The subtree is still visible in
  `tree.json` as an `avoidSync` boundary, but child scripts under it are ignored.

Names containing characters POSIX paths can't express (`/`, control characters,
leading `.`) are percent-encoded.

**Out of scope:** `.meta.json` files, attribute/tag serialization, non-`Folder`
non-script Roblox classes (e.g. `Part`, `TextLabel`, `RemoteEvent`, `Sound`).
None of these round-trip through the filesystem — inspect them via `tree.json`.

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

- `tree.json` — read-only DataModel skeleton (class + name + children). The
  plugin regenerates it from Studio on every sync. Use it to discover instances
  that don't live on disk.
- `ro-sync.md` — this file. Ro Sync refreshes its generated tool reference.

## 5. Querying the tree

The `rosync query` subcommand reads `tree.json` directly (no daemon HTTP) and
matches a `/`-separated selector against the DataModel. Use `*` for a single
segment (any name) and `**` for zero or more segments.

```
rosync query --project . 'Workspace/**/Camera'
rosync query --project . 'ReplicatedStorage/Shared/*' --format paths
rosync query --project . '**/RemoteEvent' --format classes
rosync path --project . Workspace/Camera
rosync path --project . --from fs ReplicatedStorage/Config.luau
```

Non-script, non-folder instances are visible only via `tree.json` — query it
when you need to know the shape of the rest of the DataModel.
Use `rosync path` when you need to jump between Studio instance paths and the
syncable files on disk. It refuses Studio-authoritative classes and paths not
present in the latest `tree.json`.

## 5b. Linting Luau

`rosync lint` delegates to an installed `luau-lsp` executable and runs its
standalone analyzer with a temporary Ro-Sync sourcemap for Roblox-style require
resolution. It does not require the daemon or Studio to be connected. If
`luau-lsp` is not on `PATH`, set `ROSYNC_LUAU_LSP` or pass `--luau-lsp`.
When present, `tools/luau-lsp/roblox/globalTypes.d.luau` is passed as the
Roblox definitions file automatically.

```
rosync lint --project .
rosync lint --project . --path ServerScriptService/Foo.server.luau
rosync lint --project . --path ServerScriptService --path ReplicatedStorage/Shared --owned-only --summary
rosync lint --project . --no-sourcemap
rosync lint --project . -- --no-flags-enabled
rosync lint --project . --luau-lsp /path/to/luau-lsp
```

## 5c. Asset uploads

`rosync upload` uploads assets through Roblox Open Cloud Assets. It does not
require the daemon or Studio to be connected. The API key is read from
`ROBLOX_API_KEY`, or from the Ro Sync widget Settings > Secrets value when the
env var is not set. If `--creator` is omitted, Ro Sync uses the project
`groupId` from `ro-sync.json` or the active widget project.

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
`writes.log` at the project root for audit.

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

Live-DataModel ops beyond `set`/`eval`. Each write is appended to `writes.log`.
`mv` requires `--force` to cross a top-level service boundary.

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

## 6d. Introspection — class info, enums, attribute-scoped search

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

## 6e. LLM-first command budget

Do not paste or request the full command registry by default. It is large and
usually worse for agent reasoning. Use this flow instead:

1. Run `rosync context --project .` once at task start.
2. Run `rosync commands --compact` only when choosing between command families.
3. Run `rosync commands <name>` for the exact command you are about to use.
4. Prefer cheap offline commands before live Studio reads.
5. Never run mutating commands from an LLM workflow without a read-only
   `rosync plan` when plan coverage exists.

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
rosync get --project . --path Workspace/Part --prop Anchored
rosync props --project . --path Workspace/Part
rosync source --project . --path ReplicatedStorage/Client/App --disk
rosync source --project . --path ReplicatedStorage/Client/App
```

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

- Inspect one object: `meta` -> `get --prop` or `props` -> `source` only for scripts.
- Find source: `where` or `query` -> `source --disk`; use live `source` only when checking Studio divergence.
- Compare disk vs Studio: `changes`; avoid `diff --raw` unless machine parsing is needed.
- Resolve conflict: `conflicts` -> `changes` -> `plan resolve` -> explicit `resolve`.
- Write Studio: `plan set|new|rm|mv` -> user confirmation -> mutating command, preferably with a waypoint for batches.
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

`writes.log` auto-rotates once it passes 10 MiB: the current file is renamed
to `writes.log.1` (overwriting any prior generation) and a fresh `writes.log`
takes its place. Only one prior generation is preserved.

## 7. Safety note

The filesystem → Studio sync covers only `Folder`/`Script`/`LocalScript`/
`ModuleScript` source files. `set`, `eval`, `new`, `rm`, `mv`, `attr set|rm`,
`tag add|rm`, and `call` are **user-initiated escape hatches**, not automated
tools — never invoke them from a plugin or a script, and prefer asking the
user before running them even at the CLI. Every successful write is appended
to `writes.log` so the user can audit or replay anything an agent ran on
their behalf.

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
    fs::create_dir_all(root)?;
    let p = root.join(RO_SYNC_MD);
    if p.exists() {
        let existing = fs::read_to_string(&p)?;
        if let Some(merged) = merge_ro_sync_generated_block(&existing) {
            if merged == existing {
                return Ok(RoSyncDocRefresh::Unchanged);
            }
            fs::write(&p, merged)?;
            return Ok(RoSyncDocRefresh::Updated);
        }
        if looks_like_legacy_generated_ro_sync_md(&existing)
            && (explicit_refresh || ro_sync_md_missing_required_tokens(&existing))
        {
            if existing == RO_SYNC_MD_TEMPLATE {
                return Ok(RoSyncDocRefresh::Unchanged);
            }
            fs::write(&p, RO_SYNC_MD_TEMPLATE)?;
            return Ok(RoSyncDocRefresh::Updated);
        }
        if explicit_refresh && !looks_like_legacy_generated_ro_sync_md(&existing) {
            return Ok(RoSyncDocRefresh::SkippedCustom);
        }
        return Ok(RoSyncDocRefresh::Unchanged);
    }
    fs::write(&p, RO_SYNC_MD_TEMPLATE)?;
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
    fs::create_dir_all(root)?;
    let p = root.join(CLAUDE_MD);
    if !p.exists() {
        fs::write(&p, CLAUDE_MD_TEMPLATE)?;
        return Ok(true);
    }
    let existing = fs::read_to_string(&p)?;
    let migrated = replace_bare_ro_sync_imports_with_agents(&existing);
    if migrated != existing {
        fs::write(&p, migrated)?;
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
    fs::write(&p, merged)?;
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
    fs::create_dir_all(root)?;
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
    fs::create_dir_all(root)?;
    let mut changed = false;
    changed |= write_stylua_toml_if_missing(root)?;
    changed |= write_aftman_stylua_if_missing_or_merge(root)?;
    changed |= write_roblox_definitions_if_missing_or_update(root)?;
    changed |= write_luaurc_if_missing_or_cleanup(root)?;
    Ok(changed)
}

pub fn write_stylua_toml_if_missing(root: &Path) -> io::Result<bool> {
    fs::create_dir_all(root)?;
    let p = root.join(STYLUA_TOML);
    if p.exists() {
        return Ok(false);
    }
    fs::write(&p, STYLUA_TOML_TEMPLATE)?;
    Ok(true)
}

pub fn write_aftman_stylua_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    fs::create_dir_all(root)?;
    let p = root.join(AFTMAN_TOML);
    if !p.exists() {
        fs::write(&p, AFTMAN_TOML_TEMPLATE)?;
        return Ok(true);
    }

    let existing = fs::read_to_string(&p)?;
    let merged = merge_aftman_stylua_tool(&existing);
    if merged == existing {
        return Ok(false);
    }
    fs::write(&p, merged)?;
    Ok(true)
}

pub fn write_roblox_definitions_if_missing_or_update(root: &Path) -> io::Result<bool> {
    fs::create_dir_all(root)?;
    let p = root.join(ROBLOX_DEFINITIONS_PATH);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    if p.exists() && fs::read_to_string(&p)? == ROBLOX_GLOBAL_TYPES {
        return Ok(false);
    }
    fs::write(&p, ROBLOX_GLOBAL_TYPES)?;
    Ok(true)
}

pub fn write_luaurc_if_missing_or_cleanup(root: &Path) -> io::Result<bool> {
    fs::create_dir_all(root)?;
    let p = root.join(LUAURC);
    let existed = p.exists();
    let mut config = if existed {
        let existing = fs::read_to_string(&p)?;
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
    fs::write(&p, format!("{text}\n"))?;
    Ok(true)
}

fn merge_aftman_stylua_tool(existing: &str) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let mut tools_header_index = None;
    let mut in_tools = false;

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_tools = trimmed == "[tools]";
            if in_tools && tools_header_index.is_none() {
                tools_header_index = Some(index);
            }
            continue;
        }

        if in_tools && toml_key(trimmed) == Some("stylua") {
            return existing.to_string();
        }
    }

    let mut merged = String::new();
    if let Some(index) = tools_header_index {
        for (line_index, line) in lines.iter().enumerate() {
            merged.push_str(line);
            merged.push('\n');
            if line_index == index {
                merged.push_str(STYLUA_TOOL_LINE);
                merged.push('\n');
            }
        }
    } else {
        merged.push_str(existing);
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
        if !merged.ends_with("\n\n") {
            merged.push('\n');
        }
        merged.push_str("[tools]\n");
        merged.push_str(STYLUA_TOOL_LINE);
        merged.push('\n');
    }
    merged
}

fn toml_key(trimmed_line: &str) -> Option<&str> {
    let before_comment = trimmed_line.split('#').next()?.trim();
    let (key, _) = before_comment.split_once('=')?;
    Some(key.trim().trim_matches(|c| c == '"' || c == '\''))
}

pub fn write_codex_config_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let dir = root.join(CODEX_DIR);
    fs::create_dir_all(&dir)?;
    let p = dir.join(CODEX_CONFIG_TOML);
    let desired_line = codex_project_doc_fallback_line();
    if !p.exists() {
        fs::write(&p, format!("{desired_line}\n"))?;
        return Ok(true);
    }

    let existing = fs::read_to_string(&p)?;
    let merged = merge_codex_project_doc_fallbacks(&existing);
    if merged == existing {
        return Ok(false);
    }
    fs::write(&p, merged)?;
    Ok(true)
}

pub fn write_agents_md_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let p = root.join(AGENTS_MD);
    let block = codex_agents_block(root);
    let next = if !p.exists() {
        format!(
            "# Agent project memory\n\nThis file is maintained by Ro Sync. Codex reads AGENTS.md directly; Claude Code reads CLAUDE.md, which imports this file.\n\n{block}"
        )
    } else {
        let existing = fs::read_to_string(&p)?;
        merge_generated_block(&existing, &block)
    };

    if p.exists() && fs::read_to_string(&p)? == next {
        return Ok(false);
    }
    fs::write(&p, next)?;
    Ok(true)
}

fn codex_agents_block(root: &Path) -> String {
    let mut ro_sync_sections = read_doc_variants(root, RO_SYNC_DOC_VARIANTS);
    if ro_sync_sections.is_empty() {
        ro_sync_sections.push((RO_SYNC_MD.to_string(), RO_SYNC_MD_TEMPLATE.into()));
    }
    let ro_sync = format_doc_sections(ro_sync_sections);
    let wally = wally_agents_section(root).unwrap_or_default();
    format!(
        "{CODEX_CONTEXT_START}\n\
         # Ro Sync Codex Context\n\n\
         The section between these markers is regenerated by Ro Sync. Put durable project-specific Codex notes outside the markers.\n\n\
         ## Ro Sync Project Memory\n\n\
         {ro_sync}\n\
         {wally}\
         {CODEX_CONTEXT_END}\n"
    )
}

fn wally_agents_section(root: &Path) -> Option<String> {
    let cfg = project_config::read_from_disk(root).ok().flatten();
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
            let file_text = fs::read_to_string(&wally_path)
                .ok()
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
            let Ok(text) = fs::read_to_string(&path) else {
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
        return None;
    }

    Some(format!(
        "\n## Wally Package Context\n\nRo Sync detected Wally package configuration for this project. Keep this in mind when resolving `Packages` requires or dependency-owned diagnostics.\n\n{}\n",
        parts.join("\n")
    ))
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

fn read_doc_variants(root: &Path, names: &[&str]) -> Vec<(String, String)> {
    let mut docs = Vec::new();
    for name in names {
        let Ok(text) = fs::read_to_string(root.join(name)) else {
            continue;
        };
        if docs.iter().any(|(_, existing)| existing == &text) {
            continue;
        }
        docs.push(((*name).to_string(), text));
    }
    docs
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
        let svc_dir = root.join(svc);
        if !svc_dir.is_dir() {
            continue;
        }
        let children = walk_children(&svc_dir, false)?;
        services.push(json!({
            "class": svc,
            "name": svc,
            "properties": {},
            "children": children,
        }));
    }
    Ok(services)
}

fn walk_children(dir: &Path, parent_is_script: bool) -> io::Result<Vec<Value>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let e = entry?;
        let fname = e.file_name();
        let Some(name_str) = fname.to_str() else {
            continue;
        };
        if name_str == META_FILE {
            continue;
        }
        // The script-with-children init file describes the parent, not a child.
        if parent_is_script && is_init_file(name_str) {
            continue;
        }
        let p = e.path();
        if let Some(node) = build_whitelisted_node(&p)? {
            out.push(node);
        }
    }
    out.sort_by(|a, b| {
        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        an.cmp(bn)
    });
    Ok(out)
}

fn build_whitelisted_node(path: &Path) -> io::Result<Option<Value>> {
    if path.is_dir() {
        if let Some(target) = default_project_path(path)? {
            if target.exists() {
                let name = path_to_instance_meta(path)?
                    .map(|inst| inst.name)
                    .or_else(|| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.to_string())
                    });
                return build_whitelisted_node_at(&target, name);
            }
        }
    }

    build_whitelisted_node_at(path, None)
}

fn build_whitelisted_node_at(
    path: &Path,
    name_override: Option<String>,
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
            read_init_source(path, inst.script_class)
        } else {
            fs::read_to_string(path).unwrap_or_default()
        };
        props.insert("Source".to_string(), Value::String(source));
    }

    let children = if inst.is_dir {
        walk_children(path, inst.is_script_with_children)?
    } else {
        Vec::new()
    };
    if is_folder && children.is_empty() {
        return Ok(None);
    }

    Ok(Some(json!({
        "class": inst.class,
        "name": name_override.unwrap_or(inst.name),
        "properties": Value::Object(props),
        "children": children,
    })))
}

fn default_project_path(dir: &Path) -> io::Result<Option<std::path::PathBuf>> {
    let project_file = dir.join(ROJO_PROJECT_FILE);
    if !project_file.is_file() {
        return Ok(None);
    }

    let text = fs::read_to_string(project_file)?;
    let value: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let Some(path) = value
        .get("tree")
        .and_then(|tree| tree.get("$path"))
        .and_then(|path| path.as_str())
    else {
        return Ok(None);
    };

    let Some(relative_path) = safe_rojo_relative_path(path) else {
        return Ok(None);
    };

    Ok(Some(dir.join(relative_path)))
}

fn safe_rojo_relative_path(path: &str) -> Option<std::path::PathBuf> {
    if path.is_empty() || Path::new(path).is_absolute() || looks_like_windows_rooted_path(path) {
        return None;
    }

    let mut out = std::path::PathBuf::new();
    for segment in path.split(['/', '\\']) {
        if segment.is_empty() || segment == ".." {
            return None;
        }
        if segment != "." {
            out.push(segment);
        }
    }
    Some(out)
}

fn looks_like_windows_rooted_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || path.starts_with('\\')
        || path.starts_with("//")
}

/// Read the `init (...).luau` file inside a script-with-children directory.
/// Returns "" when the directory is malformed (the walker already decided the
/// parent is a script instance, so an empty Source is the safest fallback).
fn read_init_source(dir: &Path, sc: Option<ScriptClass>) -> String {
    let Ok(iter) = fs::read_dir(dir) else {
        return String::new();
    };
    for entry in iter.flatten() {
        let fname = entry.file_name();
        let Some(name_str) = fname.to_str() else {
            continue;
        };
        let class = parse_init_file(name_str)
            .map(|(class, _)| class)
            .or_else(|| parse_plain_init_file(name_str));
        let Some(class) = class else { continue };
        if sc.map(|want| want == class).unwrap_or(true) {
            return fs::read_to_string(entry.path()).unwrap_or_default();
        }
    }
    String::new()
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

        let luaurc = fs::read_to_string(d.path().join(LUAURC)).unwrap();
        assert!(luaurc.contains("\"languageMode\""));
        assert!(!luaurc.contains("\"definitions\""));
        assert!(d.path().join(ROBLOX_DEFINITIONS_PATH).is_file());

        assert!(!write_project_tooling_if_missing_or_merge(d.path()).unwrap());
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
        assert!(
            merged.find(STYLUA_TOOL_LINE).unwrap() < merged.find("wally =").unwrap(),
            "stylua should be inserted inside [tools]; got:\n{merged}"
        );
    }

    #[test]
    fn aftman_merge_preserves_existing_stylua() {
        let d = TempDir::new("aftman-existing");
        let p = d.path().join(AFTMAN_TOML);
        let existing = "[tools]\nstylua = \"JohnnyMorganz/StyLua@2.4.1\"\nwally = \"UpliftGames/wally@0.3.2\"\n";
        fs::write(&p, existing).unwrap();

        assert!(!write_aftman_stylua_if_missing_or_merge(d.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), existing);
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
    fn default_project_path_rejects_windows_parent_traversal() {
        let d = TempDir::new("wally-default-project-traversal");
        let pkg = d.path().join("ReplicatedStorage").join("Packages");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("default.project.json"),
            r#"{"tree":{"$path":"..\\Outside"}}"#,
        )
        .unwrap();

        assert!(default_project_path(&pkg).unwrap().is_none());
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
