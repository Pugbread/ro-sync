//! Initial-sync decision state shared between `/initial-compare`,
//! `/initial-decision` (long-poll), and `/initial-choice` (plugin UI).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

use crate::fs_map::{classify_script_file, is_init_file, META_FILE};
use crate::fs_safety::{
    metadata_no_follow, validate_rojo_project_in_index, validate_service_path,
    PortableDirectoryIndex, SafeEntryKind, MAX_SERVICE_TREE_DEPTH, MAX_SERVICE_TREE_NODES,
};
use crate::project_config::CONFIG_FILE;
use crate::snapshot::{RO_SYNC_MD, SYNCED_SERVICES, TREE_JSON};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Stats {
    #[serde(rename = "scriptCount")]
    pub script_count: u32,
    #[serde(rename = "instanceCount")]
    pub instance_count: u32,
}

impl Stats {
    pub fn is_empty(&self) -> bool {
        self.script_count == 0 && self.instance_count == 0
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Choice {
    Disk,
    Studio,
    Cancel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InitialChoiceAction {
    Create,
    Overwrite,
    Remove,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InitialChoiceItem {
    pub id: u32,
    pub action: InitialChoiceAction,
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub studio_class: Option<String>,
    pub class_changed: bool,
    pub source_changed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InitialChoiceSummary {
    pub new_files: usize,
    pub changed_files: usize,
    pub removed_files: usize,
}

#[derive(Debug, Clone)]
pub struct InitialSelectionReceipt {
    pub chunk_index: u32,
    pub final_chunk: bool,
    pub restart: bool,
    pub ids: Vec<u32>,
    pub selected_count: usize,
    pub committed: bool,
}

#[derive(Debug, Clone)]
pub struct InitialSelectionAccumulator {
    pub submission_id: String,
    pub next_chunk: u32,
    pub selected_ids: BTreeSet<u32>,
    pub receipts: Vec<InitialSelectionReceipt>,
    pub updated_at: Instant,
}

pub struct PendingInitial {
    pub choice_id: String,
    pub disk_stats: Stats,
    pub studio_stats: Stats,
    pub choice: Option<Choice>,
    /// Immutable, path-sorted divergence metadata. IDs are dense indexes into
    /// this vector and are the only selective-write authority accepted from
    /// the widget.
    pub details: Vec<InitialChoiceItem>,
    /// Compact counts used by initial-choice status and realtime events.
    pub summary: InitialChoiceSummary,
    /// `None` preserves the legacy/full "Keep Disk" behavior. `Some` is the
    /// explicit subset chosen in the two-pane divergence picker.
    pub selected_disk_paths: Option<Vec<String>>,
    /// At most one uncommitted selective submission may append to this exact
    /// choice. A fresh chunk zero with `restart:true` replaces it atomically.
    pub selection: Option<InitialSelectionAccumulator>,
}

/// Walk the project root and count tracked scripts + instances.
///
/// * script_count: `.luau`/`.lua` scripts and their `.server.*` / `.client.*`
///   variants (excluding the script-with-children `init (...)` files, since
///   those are counted as part of their parent directory's instance).
/// * instance_count: every filesystem node that `path_to_instance_meta`
///   recognises as an instance (scripts + directories + meta-driven instances).
pub fn compute_disk_stats(root: &Path) -> std::io::Result<Stats> {
    let mut script_count: u32 = 0;
    let mut instance_count: u32 = 0;
    let Some(root_metadata) = metadata_no_follow(root)? else {
        return Ok(Stats::default());
    };
    if !root_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("project root is not a directory: {}", root.display()),
        ));
    }
    // Only descend into known top-level services; anything else at the project
    // root is out of scope (stray tooling dirs, build outputs, etc.).
    for svc in SYNCED_SERVICES {
        let service_path = validate_service_path(root, svc, true)?;
        let Some(service_metadata) = metadata_no_follow(&service_path)? else {
            continue;
        };
        if !service_metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "synced service root is not a directory: {}",
                    service_path.display()
                ),
            ));
        }

        checked_increment(&mut instance_count, "instance count exceeds u32")?;
        let mut visited = 0usize;
        // `(path, depth, count_directory)`; service roots were counted above.
        let mut stack = vec![(service_path, 0usize, false)];
        while let Some((directory, depth, count_directory)) = stack.pop() {
            if depth > MAX_SERVICE_TREE_DEPTH {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "service {svc} exceeds maximum depth {MAX_SERVICE_TREE_DEPTH} at {}",
                        directory.display()
                    ),
                ));
            }

            let index = PortableDirectoryIndex::read(&directory)?;
            validate_rojo_project_in_index(&directory, &index)?;
            if count_directory {
                let projects_as_instance = index.unique_init_source().is_some()
                    || index
                        .entries()
                        .iter()
                        .any(|entry| entry.fragment != META_FILE && entry.fragment != ".DS_Store");
                if projects_as_instance {
                    checked_increment(&mut instance_count, "instance count exceeds u32")?;
                }
            }

            for entry in index.entries() {
                visited = visited.checked_add(1).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "service node count exceeds usize",
                    )
                })?;
                if visited > MAX_SERVICE_TREE_NODES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "service {svc} exceeds maximum node count {MAX_SERVICE_TREE_NODES}"
                        ),
                    ));
                }

                let name = &entry.fragment;
                let is_control = name == META_FILE
                    || name == CONFIG_FILE
                    || name == RO_SYNC_MD
                    || name == TREE_JSON;
                match entry.kind {
                    SafeEntryKind::File
                        if !is_control
                            && classify_script_file(name).is_some()
                            && !is_init_file(name) =>
                    {
                        checked_increment(&mut script_count, "script count exceeds u32")?;
                        checked_increment(&mut instance_count, "instance count exceeds u32")?;
                    }
                    SafeEntryKind::Directory => {
                        // Reverse-sorted push below preserves the deterministic
                        // ascending traversal produced by the shared index.
                    }
                    SafeEntryKind::File => {}
                }
            }
            for entry in index.entries().iter().rev() {
                if entry.kind == SafeEntryKind::Directory {
                    stack.push((entry.path.clone(), depth + 1, {
                        let name = entry.fragment.as_str();
                        name != CONFIG_FILE
                            && name != RO_SYNC_MD
                            && name != TREE_JSON
                            && name != META_FILE
                    }));
                }
            }
        }
    }
    Ok(Stats {
        script_count,
        instance_count,
    })
}

fn checked_increment(value: &mut u32, message: &'static str) -> std::io::Result<()> {
    *value = value
        .checked_add(1)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;
    Ok(())
}

/// Generate a 12-character hex choice-id from system time + a monotonic counter.
/// Enough entropy for pairing within a single daemon lifetime.
pub fn new_choice_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mix = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(n);
    // 12 hex chars = 48 bits.
    format!("{:012x}", mix & 0x0000_FFFF_FFFF_FFFFu64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(tempfile::TempDir);
    impl TempDir {
        fn new(tag: &str) -> Self {
            TempDir(
                tempfile::Builder::new()
                    .prefix(&format!("rosync-init-{tag}-"))
                    .tempdir()
                    .unwrap(),
            )
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    #[test]
    fn empty_root_empty_stats() {
        let d = TempDir::new("empty");
        let s = compute_disk_stats(d.path()).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn counts_scripts_and_instances() {
        let d = TempDir::new("count");
        let rs = d.path().join("ReplicatedStorage");
        fs::create_dir_all(&rs).unwrap();
        fs::write(rs.join("Config.luau"), b"return {}").unwrap();
        fs::write(rs.join("Main.server.luau"), b"-- svr").unwrap();
        fs::write(rs.join("React.lua"), b"return {}").unwrap();
        let shared = rs.join("Shared");
        fs::create_dir(&shared).unwrap();
        fs::write(shared.join("Util.luau"), b"return {}").unwrap();

        let s = compute_disk_stats(d.path()).unwrap();
        assert_eq!(s.script_count, 4);
        // instances: ReplicatedStorage, Config, Main, React, Shared, Util = 6
        assert_eq!(s.instance_count, 6);
    }

    #[test]
    fn counts_twenty_five_thousand_scripts_without_generation_allocation() {
        const WIDTH: u32 = 25_000;
        let d = TempDir::new("wide-count");
        let rs = d.path().join("ReplicatedStorage");
        fs::create_dir_all(&rs).unwrap();
        for index in 0..WIDTH {
            fs::write(rs.join(format!("Item{index:05}.luau")), b"return true").unwrap();
        }

        let stats = compute_disk_stats(d.path()).unwrap();
        assert_eq!(stats.script_count, WIDTH);
        assert_eq!(stats.instance_count, WIDTH + 1);
    }

    #[test]
    fn ignores_control_files() {
        let d = TempDir::new("ignore");
        fs::write(d.path().join("ro-sync.md"), b"x").unwrap();
        fs::write(d.path().join("ro-sync.json"), b"{}").unwrap();
        fs::write(d.path().join("tree.json"), b"{}").unwrap();
        let s = compute_disk_stats(d.path()).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn counts_only_folders_and_scripts() {
        // A stray `.meta.json` at any depth and a non-service top-level subdir
        // must not bump either counter — the walk is scoped to SYNCED_SERVICES
        // and ignores `.meta.json` by name.
        let d = TempDir::new("scope");

        let rs = d.path().join("ReplicatedStorage");
        fs::create_dir_all(&rs).unwrap();
        fs::write(rs.join("Util.luau"), b"return {}").unwrap();
        // Stray .meta.json inside a service — must be ignored.
        fs::write(rs.join(".meta.json"), br#"{"className":"Anything"}"#).unwrap();

        // Non-service top-level directory — must not be walked at all.
        fs::create_dir_all(d.path().join("RandomTool")).unwrap();
        fs::write(
            d.path().join("RandomTool").join("Script.luau"),
            b"return {}",
        )
        .unwrap();

        let s = compute_disk_stats(d.path()).unwrap();
        assert_eq!(s.script_count, 1);
        // ReplicatedStorage + Util = 2; .meta.json and RandomTool excluded.
        assert_eq!(s.instance_count, 2);
    }

    #[test]
    fn init_file_does_not_double_count() {
        let d = TempDir::new("initfile");
        let sss = d.path().join("ServerScriptService");
        fs::create_dir_all(&sss).unwrap();
        let net = sss.join("Net");
        fs::create_dir(&net).unwrap();
        fs::write(net.join("init (Net).server.luau"), b"-- root").unwrap();
        fs::write(net.join("Helper.luau"), b"return {}").unwrap();

        let s = compute_disk_stats(d.path()).unwrap();
        // scripts: Helper.luau only (init file describes parent dir).
        assert_eq!(s.script_count, 1);
        // instances: ServerScriptService, Net, Helper = 3
        assert_eq!(s.instance_count, 3);
    }

    #[cfg(unix)]
    #[test]
    fn linked_service_root_is_rejected_without_touching_external_tree() {
        use std::os::unix::fs::symlink;
        let d = TempDir::new("linked-service");
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), "keep").unwrap();
        symlink(outside.path(), d.path().join("Workspace")).unwrap();

        let error = compute_disk_stats(d.path()).unwrap_err();
        assert!(error.to_string().contains("linked/reparse"));
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn choice_id_is_12_hex_chars() {
        let a = new_choice_id();
        let b = new_choice_id();
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
