//! Initial-sync decision state shared between `/initial-compare`,
//! `/initial-decision` (long-poll), and `/initial-choice` (plugin UI).

use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::sync::oneshot;

use crate::fs_map::{classify_script_file, parse_init_file, META_FILE};
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

pub struct PendingInitial {
    pub choice_id: String,
    pub disk_stats: Stats,
    pub studio_stats: Stats,
    pub waker: Option<oneshot::Sender<Choice>>,
}

/// Walk the project root and count tracked scripts + instances.
///
/// * script_count: files whose extension is `.luau`, `.server.luau`, or
///   `.client.luau` (excluding the script-with-children `init (...)` files,
///   since those are counted as part of their parent directory's instance).
/// * instance_count: every filesystem node that `path_to_instance_meta`
///   recognises as an instance (scripts + directories + meta-driven instances).
pub fn compute_disk_stats(root: &Path) -> std::io::Result<Stats> {
    let mut script_count: u32 = 0;
    let mut instance_count: u32 = 0;
    if !root.is_dir() {
        return Ok(Stats::default());
    }
    // Only descend into known top-level services; anything else at the project
    // root is out of scope (stray tooling dirs, build outputs, etc.).
    for svc in SYNCED_SERVICES {
        let svc_dir = root.join(svc);
        if !svc_dir.is_dir() {
            continue;
        }
        instance_count += 1;
        walk(&svc_dir, &mut script_count, &mut instance_count)?;
    }
    Ok(Stats {
        script_count,
        instance_count,
    })
}

fn walk(dir: &Path, scripts: &mut u32, instances: &mut u32) -> std::io::Result<()> {
    let iter = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return Ok(()),
    };
    for entry in iter {
        let e = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let fname = e.file_name();
        let Some(name) = fname.to_str() else { continue };
        // Skip generated / control files at any depth. .meta.json is also
        // out of scope — the daemon no longer reads or writes it.
        if name == META_FILE || name == CONFIG_FILE || name == RO_SYNC_MD || name == TREE_JSON {
            continue;
        }
        let path = e.path();
        let file_type = match e.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_file() {
            // An on-disk script instance is a .luau/.server.luau/.client.luau
            // file whose name isn't the parent dir's `init (...)` marker.
            if classify_script_file(name).is_some() && parse_init_file(name).is_none() {
                *scripts += 1;
                *instances += 1;
            }
        } else if file_type.is_dir() {
            // Every nested directory is either a Folder or a script-with-children
            // instance under the new whitelist — both count once.
            *instances += 1;
            walk(&path, scripts, instances)?;
        }
    }
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
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let p = std::env::temp_dir().join(format!(
                "rosync-init-{}-{}-{:x}",
                tag,
                std::process::id(),
                nanos
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
        let shared = rs.join("Shared");
        fs::create_dir(&shared).unwrap();
        fs::write(shared.join("Util.luau"), b"return {}").unwrap();

        let s = compute_disk_stats(d.path()).unwrap();
        assert_eq!(s.script_count, 3);
        // instances: ReplicatedStorage, Config, Main, Shared, Util = 5
        assert_eq!(s.instance_count, 5);
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

    #[test]
    fn choice_id_is_12_hex_chars() {
        let a = new_choice_id();
        let b = new_choice_id();
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
