// Conflict engine for Ro Sync daemon.
//
// All runtime deps (sha2, serde, serde_json) are already in daemon/Cargo.toml
// per Agent 1's scaffold. No dev-deps required.
//
// Baseline model: per file we store the last value that both sides agreed on
// (`last_plugin_push_hash` plus the `fs_mtime` when that push landed on disk).
// A conflict arises when BOTH sides diverge from that baseline before the
// other side has had a chance to apply the peer's change.
//
// Flow the endpoint layer is expected to wire up:
//   * On plugin push  -> engine.on_studio_push(path, bytes, fs_mtime_now)
//   * On fs  change   -> engine.on_fs_change(path, bytes, fs_mtime)
//   * On plugin poll sending a pending studio push through -> engine.record_sync(...)
//   * GET  /events     -> engine.list()
//   * POST /resolve    -> engine.resolve(path, Resolution::*)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];

pub fn hash(bytes: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

fn hex(h: &Hash) -> String {
    let mut s = String::with_capacity(64);
    for b in h {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolution {
    KeepLocal,
    KeepStudio,
}

#[derive(Debug, Clone, Copy)]
struct Baseline {
    #[allow(dead_code)]
    fs_mtime: u64,
    last_plugin_push_hash: Hash,
}

#[derive(Debug, Clone)]
struct ParkedConflict {
    fs_bytes: Vec<u8>,
    fs_hash: Hash,
    fs_mtime: u64,
    studio_bytes: Vec<u8>,
    studio_hash: Hash,
    studio_mtime: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub path: PathBuf,
    pub fs_hash: String,
    pub fs_mtime: u64,
    pub studio_hash: String,
    pub studio_mtime: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsDecision {
    /// Hash matches the baseline — nothing actually changed (probably our own write echo).
    NoChange,
    /// Real FS-side change, safe to enqueue toward Studio.
    Propagate,
    /// Conflict parked: Studio has an unapplied push whose content differs from FS.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StudioDecision {
    /// Safe to apply: FS still matches baseline.
    Apply,
    /// Bytes are identical to what's on disk — drop, but refresh baseline.
    NoChange,
    /// Conflict parked: FS drifted from baseline before Studio's push arrived.
    Conflict,
}

#[derive(Debug, Clone)]
pub enum Resolved {
    /// Caller should write these bytes to FS (and replay that as baseline once done).
    WriteFs(Vec<u8>),
    /// Caller should push these bytes to Studio (and replay as baseline once acked).
    PushStudio(Vec<u8>),
}

#[derive(Default)]
pub struct ConflictEngine {
    baselines: Mutex<HashMap<PathBuf, Baseline>>,
    conflicts: Mutex<HashMap<PathBuf, ParkedConflict>>,
}

/// Resolve `path` to whichever key actually exists in `map`, trying:
///   1. the canonicalized path
///   2. each ancestor of the canonicalized path (Argon's MultiMap parent-walk —
///      lets a rename/move surface against the closest known baseline)
///   3. the raw path as supplied
/// Returns the raw path if nothing matched, so callers can still insert.
fn resolve_key<V>(map: &HashMap<PathBuf, V>, path: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if map.contains_key(&canon) {
        return canon;
    }
    let mut cur = canon.as_path();
    while let Some(parent) = cur.parent() {
        if map.contains_key(parent) {
            return parent.to_path_buf();
        }
        cur = parent;
    }
    if map.contains_key(path) {
        return path.to_path_buf();
    }
    canon
}

impl ConflictEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set or refresh the agreed baseline for `path`. Call after either side's
    /// change has been successfully applied on the peer.
    pub fn record_sync(&self, path: &Path, content_hash: Hash, fs_mtime: u64) {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut b = self.baselines.lock().unwrap();
        b.insert(
            key,
            Baseline { fs_mtime, last_plugin_push_hash: content_hash },
        );
    }

    /// An FS-side change was observed. Returns what the caller should do.
    pub fn on_fs_change(&self, path: &Path, fs_bytes: &[u8], fs_mtime: u64) -> FsDecision {
        let fs_h = hash(fs_bytes);

        // If there's already a parked studio push for this path, fold in fresh FS info.
        {
            let mut c = self.conflicts.lock().unwrap();
            let key = resolve_key(&*c, path);
            if let Some(existing) = c.get_mut(&key) {
                if existing.studio_hash == fs_h {
                    // FS caught up to studio's version; clear conflict and baseline.
                    c.remove(&key);
                    drop(c);
                    self.record_sync(path, fs_h, fs_mtime);
                    return FsDecision::NoChange;
                }
                existing.fs_bytes = fs_bytes.to_vec();
                existing.fs_hash = fs_h;
                existing.fs_mtime = fs_mtime;
                return FsDecision::Conflict;
            }
        }

        let baseline = {
            let b = self.baselines.lock().unwrap();
            let key = resolve_key(&*b, path);
            b.get(&key).copied()
        };
        match baseline {
            Some(b) if b.last_plugin_push_hash == fs_h => {
                // Just update mtime; content didn't actually change.
                self.record_sync(path, fs_h, fs_mtime);
                FsDecision::NoChange
            }
            _ => FsDecision::Propagate,
        }
    }

    /// Studio pushed new content. Returns what the caller should do.
    pub fn on_studio_push(
        &self,
        path: &Path,
        studio_bytes: &[u8],
        current_fs: Option<(&[u8], u64)>,
    ) -> StudioDecision {
        let studio_h = hash(studio_bytes);

        let baseline = {
            let b = self.baselines.lock().unwrap();
            let key = resolve_key(&*b, path);
            b.get(&key).copied()
        };
        let (fs_bytes, fs_mtime) = match current_fs {
            Some((b, m)) => (b.to_vec(), m),
            None => {
                // No FS file → treat as clean apply (add).
                return StudioDecision::Apply;
            }
        };
        let fs_h = hash(&fs_bytes);

        if fs_h == studio_h {
            self.record_sync(path, studio_h, fs_mtime);
            return StudioDecision::NoChange;
        }

        let fs_matches_baseline = matches!(baseline, Some(b) if b.last_plugin_push_hash == fs_h);
        if fs_matches_baseline || baseline.is_none() {
            // FS hasn't drifted (or we never had a baseline: first push wins cleanly).
            return StudioDecision::Apply;
        }

        // Both sides diverged — park.
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut c = self.conflicts.lock().unwrap();
        c.insert(
            key,
            ParkedConflict {
                fs_bytes,
                fs_hash: fs_h,
                fs_mtime,
                studio_bytes: studio_bytes.to_vec(),
                studio_hash: studio_h,
                studio_mtime: now_secs(),
            },
        );
        StudioDecision::Conflict
    }

    pub fn list(&self) -> Vec<Conflict> {
        self.conflicts
            .lock()
            .unwrap()
            .iter()
            .map(|(p, c)| Conflict {
                path: p.clone(),
                fs_hash: hex(&c.fs_hash),
                fs_mtime: c.fs_mtime,
                studio_hash: hex(&c.studio_hash),
                studio_mtime: c.studio_mtime,
            })
            .collect()
    }

    pub fn has_conflict(&self, path: &Path) -> bool {
        let c = self.conflicts.lock().unwrap();
        let key = resolve_key(&*c, path);
        c.contains_key(&key)
    }

    /// Resolve a parked conflict. Returns the action the caller must perform;
    /// caller is responsible for invoking `record_sync` with the resulting hash
    /// once the write/push is acknowledged.
    pub fn resolve(&self, path: &Path, resolution: Resolution) -> Option<Resolved> {
        let parked = {
            let mut c = self.conflicts.lock().unwrap();
            let key = resolve_key(&*c, path);
            c.remove(&key)?
        };
        Some(match resolution {
            Resolution::KeepLocal => Resolved::PushStudio(parked.fs_bytes),
            Resolution::KeepStudio => Resolved::WriteFs(parked.studio_bytes),
        })
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn fs_noop_when_content_matches_baseline() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        assert_eq!(e.on_fs_change(&p("/x/a.luau"), b"hello", 200), FsDecision::NoChange);
    }

    #[test]
    fn fs_change_propagates_when_no_studio_push_pending() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        assert_eq!(e.on_fs_change(&p("/x/a.luau"), b"world", 200), FsDecision::Propagate);
    }

    #[test]
    fn studio_push_applies_when_fs_matches_baseline() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        let d = e.on_studio_push(&p("/x/a.luau"), b"world", Some((b"hello", 100)));
        assert_eq!(d, StudioDecision::Apply);
    }

    #[test]
    fn both_sides_diverge_parks_conflict() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        let d = e.on_studio_push(&p("/x/a.luau"), b"studio-edit", Some((b"fs-edit", 200)));
        assert_eq!(d, StudioDecision::Conflict);
        assert_eq!(e.list().len(), 1);
        assert!(e.has_conflict(&p("/x/a.luau")));
    }

    #[test]
    fn resolve_keep_local_returns_fs_bytes_to_push() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        e.on_studio_push(&p("/x/a.luau"), b"studio-edit", Some((b"fs-edit", 200)));

        match e.resolve(&p("/x/a.luau"), Resolution::KeepLocal) {
            Some(Resolved::PushStudio(b)) => assert_eq!(b, b"fs-edit"),
            other => panic!("got {:?}", other),
        }
        assert!(!e.has_conflict(&p("/x/a.luau")));
    }

    #[test]
    fn resolve_keep_studio_returns_studio_bytes_to_write() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        e.on_studio_push(&p("/x/a.luau"), b"studio-edit", Some((b"fs-edit", 200)));

        match e.resolve(&p("/x/a.luau"), Resolution::KeepStudio) {
            Some(Resolved::WriteFs(b)) => assert_eq!(b, b"studio-edit"),
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn fs_change_matching_parked_studio_version_clears_conflict() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        e.on_studio_push(&p("/x/a.luau"), b"studio-edit", Some((b"fs-edit", 200)));
        // user manually makes FS match studio
        let d = e.on_fs_change(&p("/x/a.luau"), b"studio-edit", 300);
        assert_eq!(d, FsDecision::NoChange);
        assert!(!e.has_conflict(&p("/x/a.luau")));
    }

    #[test]
    fn identical_push_is_noop() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        let d = e.on_studio_push(&p("/x/a.luau"), b"hello", Some((b"hello", 100)));
        assert_eq!(d, StudioDecision::NoChange);
    }
}
