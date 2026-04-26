#![allow(dead_code)] // public API consumed by http routes (wired by sibling modules).

//! Snapshot emitter for the narrowed daemon scope.
//!
//! Only `Folder`, `Script`, `LocalScript`, and `ModuleScript` are surfaced.
//! Everything else on disk is ignored here — non-script instances are the
//! plugin's responsibility and reach the project via the read-only
//! `tree.json` skeleton (class+name+children, no property values).

use crate::fs_map::{parse_init_file, path_to_instance_meta, ScriptClass, META_FILE};
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

/// The single line that signals `ro-sync.md` is linked from `CLAUDE.md`.
/// Claude Code (and compatible agent harnesses) resolve `@path` references as
/// inline imports, so a bare line containing this token pulls `ro-sync.md`
/// into every session that loads `CLAUDE.md`.
pub const RO_SYNC_IMPORT_LINE: &str = "@ro-sync.md";
const CODEX_CONTEXT_START: &str = "<!-- ro-sync:codex-context:start -->";
const CODEX_CONTEXT_END: &str = "<!-- ro-sync:codex-context:end -->";
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

const CLAUDE_MD_TEMPLATE: &str = r#"# Project memory for agents

This directory is a Roblox Studio project mirrored by Ro Sync. Before editing
any file or issuing CLI commands, read the imported context below — it
describes what syncs, what doesn't, and which `rosync` subcommands are safe to
run unattended.

@ro-sync.md
"#;

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

const RO_SYNC_MD_TEMPLATE: &str = r#"# Ro Sync project memory

Ro Sync mirrors a narrow slice of a Roblox Studio DataModel into this directory.
Read this file before editing — the scope is deliberately small.

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
- `ro-sync.md` — this file. Regenerated only if it goes missing.

## 5. Querying the tree

The `rosync query` subcommand reads `tree.json` directly (no daemon HTTP) and
matches a `/`-separated selector against the DataModel. Use `*` for a single
segment (any name) and `**` for zero or more segments.

```
rosync query --project . 'Workspace/**/Camera'
rosync query --project . 'ReplicatedStorage/Shared/*' --format paths
rosync query --project . '**/RemoteEvent' --format classes
```

Non-script, non-folder instances are visible only via `tree.json` — query it
when you need to know the shape of the rest of the DataModel.

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
rosync lint --project . --no-sourcemap
rosync lint --project . -- --no-flags-enabled
rosync lint --project . --luau-lsp /path/to/luau-lsp
```

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

# Find instances by ClassName and/or name substring (live, whole DataModel).
rosync find --project . --class RemoteEvent
rosync find --project . --name Camera
```

Mutating (ask the user first — see the safety note below):

```
# Set a property on one instance. Value is a JSON literal. --yes required.
rosync set --project . --path Workspace/Camera --prop FieldOfView --value 90 --yes

# Tagged values use their __type tag:
rosync set --project . --path Workspace/Part --prop Position \
  --value '{"__type":"Vector3","x":1,"y":2,"z":3}' --yes

# Batch writes from a JSON file: [{"path":"…","prop":"…","value":…}, …]
# Batch mode implies user intent, so --yes is not required.
rosync set --project . --batch writes.json

# Wrap a write (or a batch) in a named change-history waypoint so one
# ctrl-Z in Studio reverses the entire operation.
rosync set --project . --batch writes.json --waypoint "refactor camera"

# Execute arbitrary Luau inside the plugin sandbox. Escape hatch only.
rosync eval --project . --source 'return #game.Workspace:GetChildren()' --yes
```

All of the above time out after 5 seconds if the plugin doesn't respond; a
non-zero exit code means the request never completed.

## 6b. Change-history, save, logs, and handshake

These subcommands bracket batches, roll state back, capture output, and
verify the plugin is reachable.

```
# Health / handshake — prints plugin round-trip latency and version.
rosync doctor --project .
rosync ping --project .
rosync version --project .

# Tail Studio output (info/warn/error). `--tail` streams until ctrl-C.
rosync logs --project . --since 1m --level warn
rosync logs --project . --tail

# Save the place file (asynchronous; the CLI returns when Studio accepts it).
rosync save --project . --yes

# Change history. One waypoint flanking a batch means one ctrl-Z reverses
# the whole batch; `undo` / `redo` also work from the CLI.
rosync waypoint --project . --name "before refactor"
rosync undo --project . --yes
rosync redo --project . --yes
```

## 6c. Structured writes — construct, destroy, reparent, attrs, tags, call, select

Live-DataModel ops beyond `set`/`eval`. Each write is appended to `writes.log`.
Every destructive op requires `--yes`; `mv` additionally requires `--force` to
cross a top-level service boundary.

```
# Create a new instance. --path is the parent; --props is an optional JSON
# object of initial properties (same codec as `rosync set --value`).
rosync new --project . --path Workspace --class Part --name Box \
  --props '{"Anchored":true,"Position":{"__type":"Vector3","x":0,"y":5,"z":0}}' --yes

# Destroy an instance (:Destroy()).
rosync rm --project . --path Workspace/Box --yes

# Reparent. Cross-service moves refuse without --force to catch mistakes like
# punting something from Workspace into ServerStorage.
rosync mv --project . --from Workspace/Box --to Workspace/Folder --yes
rosync mv --project . --from Workspace/Box --to ServerStorage --force --yes

# Attributes.
rosync attr set --project . --path Workspace/Box --name Speed --value 12.5 --yes
rosync attr rm  --project . --path Workspace/Box --name Speed --yes
rosync attr ls  --project . --path Workspace/Box

# CollectionService tags.
rosync tag add --project . --path Workspace/Box --tag Enemy --yes
rosync tag rm  --project . --path Workspace/Box --tag Enemy --yes

# Invoke a method on an instance. --args is a JSON array encoded with the
# same codec as --value; the return value is printed as pretty JSON.
rosync call --project . --path Workspace/Folder --method FindFirstChild \
  --args '["Box"]' --yes

# Studio Selection.
rosync select get --project .
rosync select set --project . --paths '["Workspace/Box","Workspace/SpawnLocation"]' --yes
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

## 6e. Agent usage — extended (what Tier 1/2/3 adds)

Every `rosync` subcommand above is part of a larger catalogue. Quick reference:

- **Read-only inspection**: `get`, `ls`, `tree`, `find`, `find-attr`,
  `classinfo`, `enum`, `enums`, `query`, `attr ls`, `select get`, `logs`,
  `version`, `ping`, `lint`.
- **Structured writes (require `--yes`)**: `set`, `new`, `rm`, `mv`,
  `attr set|rm`, `tag add|rm`, `call`, `select set`, `save`, `undo`, `redo`,
  `waypoint`, `eval`.

Two write-path flags every agent should know:

- **`--waypoint <name>`** on `set` (single or `--batch`) records a named
  Studio change-history waypoint before and after the operation, so one
  ctrl-Z in the editor reverts the whole thing. Use this for any multi-step
  write: `rosync set --batch edits.json --waypoint "re-skin box"`.
- **`set Parent` is guardrailed.** `rosync set --prop Parent …` refuses with
  a loud error by default — raw Parent assignment is the single most common
  way to corrupt a DataModel. Use `rosync mv --path X --to Y --yes` for
  reparenting. If you genuinely need the raw write, pass `--force-parent`
  along with `--yes`.

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
"#;

/// Write `ro-sync.md` at the root if it doesn't already exist. Returns `true`
/// when the file was freshly written.
pub fn write_ro_sync_md_if_missing(root: &Path) -> io::Result<bool> {
    fs::create_dir_all(root)?;
    let p = root.join(RO_SYNC_MD);
    if p.exists() {
        return Ok(false);
    }
    fs::write(&p, RO_SYNC_MD_TEMPLATE)?;
    Ok(true)
}

/// Ensure `CLAUDE.md` at the project root imports `ro-sync.md` so agent
/// sessions load it automatically. Behavior:
///
/// * No `CLAUDE.md`: write one with a short preamble and the `@ro-sync.md`
///   import line.
/// * `CLAUDE.md` exists without the import line: append a blank line followed
///   by the import line (user content is preserved verbatim).
/// * `CLAUDE.md` already imports `ro-sync.md`: no-op.
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
    if claude_md_imports_ro_sync(&existing) {
        return Ok(false);
    }
    let mut merged = existing;
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    if !merged.is_empty() && !merged.ends_with("\n\n") {
        merged.push('\n');
    }
    merged.push_str(RO_SYNC_IMPORT_LINE);
    merged.push('\n');
    fs::write(&p, merged)?;
    Ok(true)
}

/// Ensure Codex receives the same project memory Claude Code does.
///
/// Codex reads `AGENTS.md` as its native project context. It can also be
/// configured with fallback project-doc filenames, but current Codex builds use
/// the first matching fallback file rather than concatenating all matches. Ro
/// Sync therefore prioritizes `ro-sync.md` in fallback config and writes a
/// generated block into `AGENTS.md` with the full Ro Sync memory first.
///
/// Returns `true` when any Codex-facing file was created or modified.
pub fn write_codex_context_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    fs::create_dir_all(root)?;
    let mut changed = false;
    changed |= write_codex_config_if_missing_or_merge(root)?;
    changed |= write_agents_md_if_missing_or_merge(root)?;
    Ok(changed)
}

fn write_codex_config_if_missing_or_merge(root: &Path) -> io::Result<bool> {
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

fn write_agents_md_if_missing_or_merge(root: &Path) -> io::Result<bool> {
    let p = root.join(AGENTS_MD);
    let block = codex_agents_block(root);
    let next = if !p.exists() {
        format!(
            "# Codex project memory\n\nThis file is maintained by Ro Sync so Codex receives the same context as Claude Code.\n\n{block}"
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
    let mut claude_sections = read_doc_variants(root, CLAUDE_DOC_VARIANTS);
    if claude_sections.is_empty() {
        claude_sections.push((CLAUDE_MD.to_string(), String::new()));
    }
    let mut ro_sync_sections = read_doc_variants(root, RO_SYNC_DOC_VARIANTS);
    if ro_sync_sections.is_empty() {
        ro_sync_sections.push((RO_SYNC_MD.to_string(), RO_SYNC_MD_TEMPLATE.into()));
    }
    let ro_sync = format_doc_sections(ro_sync_sections);
    let claude = format_doc_sections(claude_sections);
    format!(
        "{CODEX_CONTEXT_START}\n\
         # Ro Sync Codex Context\n\n\
         The section between these markers is regenerated by Ro Sync. Put durable project-specific Codex notes outside the markers.\n\n\
         ## Ro Sync Project Memory\n\n\
         {ro_sync}\n\n\
         ## Claude Project Memory\n\n\
         {claude}\n\
         {CODEX_CONTEXT_END}\n"
    )
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

/// True when any line of `contents` (after trimming whitespace) is exactly the
/// `@ro-sync.md` import token, optionally prefixed with `./`. Keeps detection
/// robust against minor user edits (leading spaces, trailing whitespace) while
/// avoiding false positives from mentions inside prose.
fn claude_md_imports_ro_sync(contents: &str) -> bool {
    for line in contents.lines() {
        let t = line.trim();
        if t == RO_SYNC_IMPORT_LINE || t == "@./ro-sync.md" {
            return true;
        }
    }
    false
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
        if parent_is_script && parse_init_file(name_str).is_some() {
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
    let Some(inst) = path_to_instance_meta(path)? else {
        return Ok(None);
    };
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

    Ok(Some(json!({
        "class": inst.class,
        "name": inst.name,
        "properties": Value::Object(props),
        "children": children,
    })))
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
        let Some((class, _)) = parse_init_file(name_str) else {
            continue;
        };
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
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "rosync-snap-{}-{}-{}",
                tag,
                std::process::id(),
                rand_token()
            ));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn rand_token() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:x}", nanos)
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
    fn ro_sync_md_template_lists_new_cli_subcommands() {
        // The template is the contract agents read to learn which commands
        // exist. Lock it against regressions so future edits don't silently
        // drop a subcommand section.
        for token in [
            "rosync get",
            "rosync set",
            "rosync ls",
            "rosync tree",
            "rosync find",
            "rosync eval",
            "rosync doctor",
            "rosync lint",
            "rosync classinfo",
            "rosync enums",
            "rosync enum ",
            "rosync find-attr",
            "--under",
            "--force-parent",
            "--waypoint",
            "writes.log",
            "writes.log.1",
            "escape",
            "hatches",
        ] {
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
            body.lines().any(|l| l.trim() == RO_SYNC_IMPORT_LINE),
            "new CLAUDE.md must import ro-sync.md; got:\n{body}"
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
            merged.lines().any(|l| l.trim() == RO_SYNC_IMPORT_LINE),
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
        let existing = "# Existing\n\n@ro-sync.md\n\nMore user notes.\n";
        fs::write(&p, existing).unwrap();
        assert!(!write_claude_md_if_missing_or_merge(d.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), existing);
    }

    #[test]
    fn claude_md_detects_relative_import_form() {
        // `@./ro-sync.md` resolves to the same file in Claude Code, so it
        // must count as already-imported and not trigger an append.
        let d = TempDir::new("claude-relative");
        let p = d.path().join(CLAUDE_MD);
        let existing = "# doc\n\n@./ro-sync.md\n";
        fs::write(&p, existing).unwrap();
        assert!(!write_claude_md_if_missing_or_merge(d.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), existing);
    }

    #[test]
    fn claude_md_does_not_match_mention_inside_prose() {
        // A line like "see @ro-sync.md for details" should NOT count as an
        // import — Claude Code only treats bare `@path` lines as imports.
        let d = TempDir::new("claude-prose");
        let p = d.path().join(CLAUDE_MD);
        let existing = "# doc\n\nsee @ro-sync.md for details\n";
        fs::write(&p, existing).unwrap();
        assert!(write_claude_md_if_missing_or_merge(d.path()).unwrap());
        let merged = fs::read_to_string(&p).unwrap();
        assert!(merged.starts_with(existing), "user content preserved");
        assert!(
            merged.lines().any(|l| l.trim() == RO_SYNC_IMPORT_LINE),
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
        assert!(merged.lines().any(|l| l.trim() == RO_SYNC_IMPORT_LINE));
    }

    #[test]
    fn codex_context_inlines_claude_and_ro_sync_docs() {
        let d = TempDir::new("codex-context");
        fs::write(d.path().join(CLAUDE_MD), "# Claude notes\n").unwrap();
        fs::write(d.path().join(RO_SYNC_MD), "# Ro Sync notes\n").unwrap();

        assert!(write_codex_context_if_missing_or_merge(d.path()).unwrap());
        let agents = fs::read_to_string(d.path().join(AGENTS_MD)).unwrap();
        assert!(agents.contains(CODEX_CONTEXT_START));
        assert!(agents.contains("# Claude notes"));
        assert!(agents.contains("# Ro Sync notes"));
        assert!(
            agents.find("## Ro Sync Project Memory").unwrap()
                < agents.find("## Claude Project Memory").unwrap(),
            "AGENTS.md must put full Ro Sync memory before Claude import notes; got:\n{agents}"
        );

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
        assert!(merged.contains("# Claude v1"));

        fs::write(d.path().join(CLAUDE_MD), "# Claude v2\n").unwrap();
        assert!(write_codex_context_if_missing_or_merge(d.path()).unwrap());
        let updated = fs::read_to_string(&p).unwrap();
        assert!(updated.contains("# Claude v2"));
        assert!(!updated.contains("# Claude v1"));
        assert_eq!(updated.matches(CODEX_CONTEXT_START).count(), 1);
    }

    #[test]
    fn codex_context_includes_distinct_claude_md_variant() {
        let d = TempDir::new("codex-claude-case");
        fs::write(d.path().join(CLAUDE_MD), "# Claude lower\n").unwrap();
        fs::write(d.path().join("CLAUDE.MD"), "# Claude upper\n").unwrap();
        fs::write(d.path().join(RO_SYNC_MD), "# Ro Sync notes\n").unwrap();

        assert!(write_codex_context_if_missing_or_merge(d.path()).unwrap());
        let agents = fs::read_to_string(d.path().join(AGENTS_MD)).unwrap();
        assert!(agents.contains("### CLAUDE.md"));
        assert!(agents.contains("# Claude upper"));
        let same_file = fs::canonicalize(d.path().join(CLAUDE_MD)).ok()
            == fs::canonicalize(d.path().join("CLAUDE.MD")).ok();
        if !same_file {
            assert!(agents.contains("# Claude lower"));
            assert!(agents.contains("### CLAUDE.MD"));
        }
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
