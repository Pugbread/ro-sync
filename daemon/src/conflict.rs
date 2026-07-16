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

#[derive(Debug, Clone, PartialEq, Eq)]
enum FsDestructiveAction {
    Delete {
        path: PathBuf,
        is_dir: bool,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        is_dir: bool,
        retained_bytes: Option<Vec<u8>>,
    },
}

impl FsDestructiveAction {
    fn source(&self) -> &Path {
        match self {
            Self::Delete { path, .. } => path,
            Self::Rename { from, .. } => from,
        }
    }

    fn is_dir(&self) -> bool {
        match self {
            Self::Delete { is_dir, .. } | Self::Rename { is_dir, .. } => *is_dir,
        }
    }

    fn retained_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Rename { retained_bytes, .. } => retained_bytes.as_deref(),
            Self::Delete { .. } => None,
        }
    }

    fn affects(&self, path: &Path) -> bool {
        fn intersects(left: &Path, right: &Path) -> bool {
            let left = stable_path(left);
            let right = stable_path(right);
            left == right || left.starts_with(&right) || right.starts_with(&left)
        }

        match self {
            Self::Delete { path: source, .. } => intersects(path, source),
            Self::Rename { from, to, .. } => intersects(path, from) || intersects(path, to),
        }
    }
}

#[derive(Debug, Clone)]
struct ParkedConflict {
    fs_bytes: Vec<u8>,
    fs_hash: Hash,
    fs_mtime: u64,
    studio_bytes: Vec<u8>,
    studio_hash: Hash,
    studio_mtime: u64,
    fs_is_dir: bool,
    studio_deleted: bool,
    fs_destructive_action: Option<FsDestructiveAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub path: PathBuf,
    pub local: String,
    pub studio: String,
    pub fs_hash: String,
    pub fs_mtime: u64,
    pub studio_hash: String,
    pub studio_mtime: u64,
    #[serde(rename = "studioDeleted")]
    pub studio_deleted: bool,
    #[serde(rename = "localDeleted")]
    pub local_deleted: bool,
    #[serde(rename = "localRenamedTo", skip_serializing_if = "Option::is_none")]
    pub local_renamed_to: Option<PathBuf>,
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
    PushStudio {
        bytes: Vec<u8>,
        is_dir: bool,
        rejected_studio: Option<Vec<u8>>,
    },
    /// Caller should delete the retained filesystem path.
    DeleteFs { bytes: Vec<u8>, is_dir: bool },
    /// The disk-side winner deleted `path`; caller should now delete the
    /// corresponding Studio instance.
    DeleteStudio {
        path: PathBuf,
        conflict_path: PathBuf,
        studio_bytes: Vec<u8>,
        is_dir: bool,
    },
    /// The disk-side winner renamed `from` to `to`; caller should mirror the
    /// rename in Studio, then re-apply the retained disk tree at `to`.
    RenameStudio {
        from: PathBuf,
        to: PathBuf,
        is_dir: bool,
        conflict_path: PathBuf,
        studio_bytes: Vec<u8>,
        local_bytes: Vec<u8>,
    },
    /// Studio won after a disk delete. Recreate the conflicted source at its
    /// original path with Studio's bytes.
    RestoreFsDelete {
        delete_root: PathBuf,
        conflict_path: PathBuf,
        studio_bytes: Vec<u8>,
        is_dir: bool,
    },
    /// Studio won after a disk rename. Move the retained disk path back to its
    /// original location, then write Studio's bytes to the conflicted source.
    RestoreFsRename {
        from: PathBuf,
        to: PathBuf,
        conflict_path: PathBuf,
        studio_bytes: Vec<u8>,
        is_dir: bool,
        local_bytes: Vec<u8>,
    },
}

#[derive(Default)]
pub struct ConflictEngine {
    baselines: Mutex<HashMap<PathBuf, Baseline>>,
    conflicts: Mutex<HashMap<PathBuf, ParkedConflict>>,
    pending_fs_destructive: Mutex<HashMap<PathBuf, FsDestructiveAction>>,
}

/// Resolve `path` to whichever key actually exists in `map`, trying:
///   1. the canonicalized path
///   2. each ancestor of the canonicalized path (Argon's MultiMap parent-walk —
///      lets a rename/move surface against the closest known baseline)
///   3. the raw path as supplied
///
/// Returns the raw path if nothing matched, so callers can still insert.
fn resolve_key<V>(map: &HashMap<PathBuf, V>, path: &Path) -> PathBuf {
    let canon = stable_path(path);
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
        let key = stable_path(path);
        {
            let mut b = self.baselines.lock().unwrap();
            b.insert(
                key.clone(),
                Baseline {
                    fs_mtime,
                    last_plugin_push_hash: content_hash,
                },
            );
        }
        let mut c = self.conflicts.lock().unwrap();
        let conflict_key = resolve_key(&*c, &key);
        c.remove(&conflict_key);
    }

    /// Whether the current filesystem bytes still match the last value both
    /// sides acknowledged. Missing baselines deliberately return false: after
    /// a daemon restart, unknown state must not be treated as permission to
    /// destroy an existing local edit.
    pub fn matches_baseline(&self, path: &Path, bytes: &[u8]) -> bool {
        let baselines = self.baselines.lock().unwrap();
        let key = resolve_key(&*baselines, path);
        baselines
            .get(&key)
            .is_some_and(|baseline| baseline.last_plugin_push_hash == hash(bytes))
    }

    /// Forget baselines and parked conflicts for a path and all descendants.
    pub fn forget_path(&self, path: &Path) {
        let key = stable_path(path);
        self.baselines
            .lock()
            .unwrap()
            .retain(|candidate, _| candidate != &key && !candidate.starts_with(&key));
        self.conflicts
            .lock()
            .unwrap()
            .retain(|candidate, _| candidate != &key && !candidate.starts_with(&key));
    }

    /// Park a Studio-side deletion instead of deleting local content whose
    /// baseline is missing or stale. The caller keeps the path on disk until
    /// the user chooses which side wins.
    pub fn park_studio_delete(
        &self,
        path: &Path,
        fs_bytes: Vec<u8>,
        fs_mtime: u64,
        fs_is_dir: bool,
    ) {
        let key = stable_path(path);
        let fs_hash = hash(&fs_bytes);
        self.conflicts.lock().unwrap().insert(
            key,
            ParkedConflict {
                fs_bytes,
                fs_hash,
                fs_mtime,
                studio_bytes: Vec::new(),
                studio_hash: hash(&[]),
                studio_mtime: now_secs(),
                fs_is_dir,
                studio_deleted: true,
                fs_destructive_action: None,
            },
        );
    }

    /// Restore a source-update conflict when delivery of a Keep Local
    /// resolution fails because the plugin disconnected mid-request.
    pub fn park_studio_update(
        &self,
        path: &Path,
        fs_bytes: Vec<u8>,
        studio_bytes: Vec<u8>,
        fs_mtime: u64,
    ) {
        let key = stable_path(path);
        let fs_hash = hash(&fs_bytes);
        let studio_hash = hash(&studio_bytes);
        self.conflicts.lock().unwrap().insert(
            key,
            ParkedConflict {
                fs_bytes,
                fs_hash,
                fs_mtime,
                studio_bytes,
                studio_hash,
                studio_mtime: now_secs(),
                fs_is_dir: false,
                studio_deleted: false,
                fs_destructive_action: None,
            },
        );
    }

    /// Re-park a filesystem-delete conflict if conflict resolution could not
    /// be delivered or applied. This mirrors `park_studio_update` but retains
    /// the destructive disk intent.
    pub fn park_fs_delete_conflict(
        &self,
        conflict_path: &Path,
        delete_root: &Path,
        studio_bytes: Vec<u8>,
        is_dir: bool,
    ) {
        self.park_fs_destructive_conflict(
            conflict_path,
            Vec::new(),
            studio_bytes,
            FsDestructiveAction::Delete {
                path: delete_root.to_path_buf(),
                is_dir,
            },
        );
    }

    /// Re-park a filesystem-rename conflict after an incomplete resolution.
    pub fn park_fs_rename_conflict(
        &self,
        conflict_path: &Path,
        from: &Path,
        to: &Path,
        local_bytes: Vec<u8>,
        studio_bytes: Vec<u8>,
        is_dir: bool,
    ) {
        self.park_fs_destructive_conflict(
            conflict_path,
            local_bytes.clone(),
            studio_bytes,
            FsDestructiveAction::Rename {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                is_dir,
                retained_bytes: (!is_dir).then_some(local_bytes),
            },
        );
    }

    fn park_fs_destructive_conflict(
        &self,
        conflict_path: &Path,
        fs_bytes: Vec<u8>,
        studio_bytes: Vec<u8>,
        action: FsDestructiveAction,
    ) {
        let key = stable_path(conflict_path);
        let fs_hash = hash(&fs_bytes);
        let studio_hash = hash(&studio_bytes);
        self.conflicts.lock().unwrap().insert(
            key,
            ParkedConflict {
                fs_bytes,
                fs_hash,
                fs_mtime: 0,
                studio_bytes,
                studio_hash,
                studio_mtime: now_secs(),
                fs_is_dir: action.is_dir(),
                studio_deleted: false,
                fs_destructive_action: Some(action),
            },
        );
    }

    /// Begin a bounded fail-closed window for a filesystem-originated delete.
    /// The watcher calls this before waiting briefly for an in-flight Studio
    /// source push. If Studio still matches the baseline the delete remains
    /// clean; if Studio has diverged, `on_studio_push` parks a resolvable
    /// conflict instead of recreating the just-deleted disk path.
    pub fn begin_fs_delete(&self, path: &Path, is_dir: bool) {
        self.begin_fs_destructive(FsDestructiveAction::Delete {
            path: path.to_path_buf(),
            is_dir,
        });
    }

    /// Begin the same bounded window for a filesystem-originated rename.
    pub fn begin_fs_rename(
        &self,
        from: &Path,
        to: &Path,
        is_dir: bool,
        retained_bytes: Option<Vec<u8>>,
    ) {
        self.begin_fs_destructive(FsDestructiveAction::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            is_dir,
            retained_bytes,
        });
    }

    fn begin_fs_destructive(&self, action: FsDestructiveAction) {
        let source = stable_path(action.source());
        self.pending_fs_destructive
            .lock()
            .unwrap()
            .insert(source, action.clone());

        // A Studio-vs-disk source conflict may already have been parked before
        // notify delivered the delete/rename. Attach the new disk intent to it
        // so either resolution remains deterministic.
        let mut conflicts = self.conflicts.lock().unwrap();
        for (path, conflict) in conflicts.iter_mut() {
            if action.affects(path) {
                if let Some(bytes) = action.retained_bytes() {
                    conflict.fs_bytes = bytes.to_vec();
                    conflict.fs_hash = hash(bytes);
                }
                conflict.fs_destructive_action = Some(action.clone());
                conflict.fs_is_dir = action.is_dir();
            }
        }
    }

    /// Finish a filesystem destructive preflight. `Conflict` means the caller
    /// must emit only a conflict notification; the delete/rename must not be
    /// sent to Studio. `Propagate` means no divergent Studio push was observed
    /// during the bounded window.
    pub fn finish_fs_destructive(&self, source: &Path) -> FsDecision {
        let source = stable_path(source);
        let action = self.pending_fs_destructive.lock().unwrap().remove(&source);
        let Some(action) = action else {
            // Unknown destructive ops fail closed if an intersecting conflict
            // is already present, but preserve legacy clean propagation.
            return if self.conflict_intersects(&source) {
                FsDecision::Conflict
            } else {
                FsDecision::Propagate
            };
        };
        if self
            .conflicts
            .lock()
            .unwrap()
            .iter()
            .any(|(path, conflict)| {
                action.affects(path) && conflict.fs_destructive_action.as_ref() == Some(&action)
            })
        {
            FsDecision::Conflict
        } else {
            FsDecision::Propagate
        }
    }

    fn conflict_intersects(&self, path: &Path) -> bool {
        self.conflicts.lock().unwrap().keys().any(|candidate| {
            candidate == path || candidate.starts_with(path) || path.starts_with(candidate)
        })
    }

    /// Commit baseline bookkeeping after a clean disk delete has been emitted.
    pub fn commit_fs_delete(&self, path: &Path) {
        self.forget_path(path);
    }

    /// Move agreed baselines along with a clean disk rename. This prevents the
    /// next Studio source acknowledgement at the destination from looking like
    /// an unrelated baseline-less edit.
    pub fn commit_fs_rename(&self, from: &Path, to: &Path) {
        let from = stable_path(from);
        let to = stable_path(to);
        let mut baselines = self.baselines.lock().unwrap();
        let moved = baselines
            .iter()
            .filter_map(|(path, baseline)| {
                path.strip_prefix(&from)
                    .ok()
                    .map(|suffix| (path.clone(), to.join(suffix), *baseline))
            })
            .collect::<Vec<_>>();
        for (old, new, baseline) in moved {
            baselines.remove(&old);
            baselines.insert(new, baseline);
        }
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

        // A blocked rename can be followed by a separate destination update
        // notification on some watcher backends. Do not let that second event
        // materialize the renamed script in Studio while the rename itself is
        // waiting on conflict resolution.
        if self.conflicts.lock().unwrap().values().any(|conflict| {
            conflict
                .fs_destructive_action
                .as_ref()
                .is_some_and(|action| action.affects(path))
        }) {
            return FsDecision::Conflict;
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

        // A filesystem delete/rename has already happened, but its watcher op
        // is held for a short grace window. Treat a concurrent Studio push as
        // a preflight response: unchanged Studio state permits the destructive
        // op; divergent Studio state becomes an explicit, resolvable conflict.
        let pending_action = {
            let pending = self.pending_fs_destructive.lock().unwrap();
            pending
                .values()
                .find(|action| action.affects(path))
                .cloned()
        };
        if let Some(action) = pending_action {
            let baseline = {
                let baselines = self.baselines.lock().unwrap();
                let key = resolve_key(&*baselines, path);
                baselines.get(&key).copied()
            };
            if baseline.is_some_and(|baseline| baseline.last_plugin_push_hash == studio_h) {
                // Studio has not diverged. Do not recreate the deleted/renamed
                // source on disk; the pending watcher op will update Studio.
                return StudioDecision::NoChange;
            }

            let key = stable_path(path);
            let (fs_bytes, fs_mtime) = if let Some((bytes, mtime)) = current_fs {
                (bytes.to_vec(), mtime)
            } else if let Some(bytes) = action.retained_bytes() {
                (bytes.to_vec(), 0)
            } else {
                // A delete has no local source by definition; a directory
                // rename keeps its actual sources in the retained destination
                // tree and re-reads them fallibly during Keep Disk. These empty
                // bytes are conflict metadata only and are never pushed as a
                // replacement script source.
                (Vec::new(), 0)
            };
            let fs_hash = hash(&fs_bytes);
            self.conflicts.lock().unwrap().insert(
                key,
                ParkedConflict {
                    fs_bytes,
                    fs_hash,
                    fs_mtime,
                    studio_bytes: studio_bytes.to_vec(),
                    studio_hash: studio_h,
                    studio_mtime: now_secs(),
                    fs_is_dir: action.is_dir(),
                    studio_deleted: false,
                    fs_destructive_action: Some(action),
                },
            );
            return StudioDecision::Conflict;
        }

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
        if fs_matches_baseline {
            // FS hasn't drifted from the last value both sides acknowledged.
            return StudioDecision::Apply;
        }

        // Both sides diverged, or this daemon has no trustworthy baseline for
        // an existing file (for example after restart) — park rather than let
        // the first Studio push silently win.
        let key = stable_path(path);
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
                fs_is_dir: false,
                studio_deleted: false,
                fs_destructive_action: None,
            },
        );
        StudioDecision::Conflict
    }

    pub fn list(&self) -> Vec<Conflict> {
        self.conflicts
            .lock()
            .unwrap()
            .iter()
            .map(|(p, c)| {
                let (local_deleted, local_renamed_to) = match &c.fs_destructive_action {
                    Some(FsDestructiveAction::Delete { .. }) => (true, None),
                    Some(FsDestructiveAction::Rename { to, .. }) => (false, Some(to.clone())),
                    None => (false, None),
                };
                Conflict {
                    path: p.clone(),
                    local: if local_deleted {
                        "[deleted on disk]".to_string()
                    } else if let Some(to) = &local_renamed_to {
                        format!("[renamed on disk to {}]", to.display())
                    } else {
                        String::from_utf8_lossy(&c.fs_bytes).into_owned()
                    },
                    studio: String::from_utf8_lossy(&c.studio_bytes).into_owned(),
                    fs_hash: hex(&c.fs_hash),
                    fs_mtime: c.fs_mtime,
                    studio_hash: hex(&c.studio_hash),
                    studio_mtime: c.studio_mtime,
                    studio_deleted: c.studio_deleted,
                    local_deleted,
                    local_renamed_to,
                }
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
        let (conflict_path, parked) = {
            let mut c = self.conflicts.lock().unwrap();
            let key = resolve_key(&*c, path);
            let parked = c.remove(&key)?;
            (key, parked)
        };
        Some(match (resolution, parked.fs_destructive_action) {
            (Resolution::KeepLocal, Some(FsDestructiveAction::Delete { path, is_dir })) => {
                Resolved::DeleteStudio {
                    path,
                    conflict_path,
                    studio_bytes: parked.studio_bytes,
                    is_dir,
                }
            }
            (
                Resolution::KeepLocal,
                Some(FsDestructiveAction::Rename {
                    from, to, is_dir, ..
                }),
            ) => Resolved::RenameStudio {
                from,
                to,
                is_dir,
                conflict_path,
                studio_bytes: parked.studio_bytes,
                local_bytes: parked.fs_bytes,
            },
            (Resolution::KeepStudio, Some(FsDestructiveAction::Delete { path, is_dir })) => {
                Resolved::RestoreFsDelete {
                    delete_root: path,
                    conflict_path,
                    studio_bytes: parked.studio_bytes,
                    is_dir,
                }
            }
            (
                Resolution::KeepStudio,
                Some(FsDestructiveAction::Rename {
                    from, to, is_dir, ..
                }),
            ) => Resolved::RestoreFsRename {
                from,
                to,
                conflict_path,
                studio_bytes: parked.studio_bytes,
                is_dir,
                local_bytes: parked.fs_bytes,
            },
            (Resolution::KeepLocal, None) => Resolved::PushStudio {
                bytes: parked.fs_bytes,
                is_dir: parked.fs_is_dir,
                rejected_studio: (!parked.studio_deleted).then_some(parked.studio_bytes),
            },
            (Resolution::KeepStudio, None) if parked.studio_deleted => Resolved::DeleteFs {
                bytes: parked.fs_bytes,
                is_dir: parked.fs_is_dir,
            },
            (Resolution::KeepStudio, None) => Resolved::WriteFs(parked.studio_bytes),
        })
    }
}

fn stable_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    // Deleted and renamed paths no longer canonicalize directly. Canonicalize
    // the nearest surviving ancestor (important on macOS where /var resolves
    // to /private/var), then append the missing suffix unchanged.
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while let Some(name) = ancestor.file_name() {
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
        if let Ok(mut canonical) = std::fs::canonicalize(ancestor) {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
    }
    path.to_path_buf()
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
        assert_eq!(
            e.on_fs_change(&p("/x/a.luau"), b"hello", 200),
            FsDecision::NoChange
        );
    }

    #[test]
    fn fs_change_propagates_when_no_studio_push_pending() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        assert_eq!(
            e.on_fs_change(&p("/x/a.luau"), b"world", 200),
            FsDecision::Propagate
        );
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
        let conflicts = e.list();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].local, "fs-edit");
        assert_eq!(conflicts[0].studio, "studio-edit");
        assert!(e.has_conflict(&p("/x/a.luau")));
    }

    #[test]
    fn resolve_keep_local_returns_fs_bytes_to_push() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        e.on_studio_push(&p("/x/a.luau"), b"studio-edit", Some((b"fs-edit", 200)));

        match e.resolve(&p("/x/a.luau"), Resolution::KeepLocal) {
            Some(Resolved::PushStudio {
                bytes,
                is_dir,
                rejected_studio,
            }) => {
                assert_eq!(bytes, b"fs-edit");
                assert!(!is_dir);
                assert_eq!(rejected_studio.as_deref(), Some(&b"studio-edit"[..]));
            }
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
    fn record_sync_clears_stale_parked_conflict() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        e.on_studio_push(&p("/x/a.luau"), b"studio-edit", Some((b"fs-edit", 200)));
        assert!(e.has_conflict(&p("/x/a.luau")));

        e.record_sync(&p("/x/a.luau"), hash(b"fs-edit"), 300);
        assert!(!e.has_conflict(&p("/x/a.luau")));
    }

    #[test]
    fn identical_push_is_noop() {
        let e = ConflictEngine::new();
        e.record_sync(&p("/x/a.luau"), hash(b"hello"), 100);
        let d = e.on_studio_push(&p("/x/a.luau"), b"hello", Some((b"hello", 100)));
        assert_eq!(d, StudioDecision::NoChange);
    }

    #[test]
    fn existing_file_without_baseline_is_parked_after_restart() {
        let e = ConflictEngine::new();
        let d = e.on_studio_push(&p("/x/a.luau"), b"studio-edit", Some((b"disk-edit", 200)));
        assert_eq!(d, StudioDecision::Conflict);
        assert!(e.has_conflict(&p("/x/a.luau")));
    }

    #[test]
    fn studio_delete_can_be_resolved_without_losing_local_bytes() {
        let e = ConflictEngine::new();
        e.park_studio_delete(&p("/x/a.luau"), b"local-edit".to_vec(), 200, false);
        let listed = e.list();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].studio_deleted);

        match e.resolve(&p("/x/a.luau"), Resolution::KeepLocal) {
            Some(Resolved::PushStudio {
                bytes,
                is_dir,
                rejected_studio,
            }) => {
                assert_eq!(bytes, b"local-edit");
                assert!(!is_dir);
                assert!(rejected_studio.is_none());
            }
            other => panic!("got {:?}", other),
        }

        e.park_studio_delete(&p("/x/a.luau"), b"local-edit".to_vec(), 200, false);
        assert!(matches!(
            e.resolve(&p("/x/a.luau"), Resolution::KeepStudio),
            Some(Resolved::DeleteFs { .. })
        ));
    }

    #[test]
    fn disk_delete_of_divergent_studio_source_is_parked_and_resolvable() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Workspace/Controller.server.luau");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"disk edit\n").unwrap();

        let engine = ConflictEngine::new();
        engine.record_sync(&source, hash(b"agreed\n"), 1);
        assert_eq!(
            engine.on_studio_push(&source, b"studio edit\n", Some((b"disk edit\n", 2))),
            StudioDecision::Conflict
        );

        std::fs::remove_file(&source).unwrap();
        engine.begin_fs_delete(&source, false);
        assert_eq!(engine.finish_fs_destructive(&source), FsDecision::Conflict);

        let listed = engine.list();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].local_deleted);
        assert_eq!(listed[0].local, "[deleted on disk]");
        assert_eq!(listed[0].studio, "studio edit\n");
        assert!(matches!(
            engine.resolve(&source, Resolution::KeepLocal),
            Some(Resolved::DeleteStudio { path, .. }) if path == source
        ));

        // The opposite choice carries Studio's retained bytes back to the
        // exact original disk path.
        engine.park_studio_update(
            &source,
            b"disk edit\n".to_vec(),
            b"studio edit\n".to_vec(),
            2,
        );
        engine.begin_fs_delete(&source, false);
        let _ = engine.finish_fs_destructive(&source);
        assert!(matches!(
            engine.resolve(&source, Resolution::KeepStudio),
            Some(Resolved::RestoreFsDelete {
                delete_root,
                conflict_path,
                studio_bytes,
                ..
            }) if delete_root == source
                && conflict_path == stable_path(&source)
                && studio_bytes == b"studio edit\n"
        ));
    }

    #[test]
    fn disk_rename_of_divergent_studio_source_is_parked_and_resolvable() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("ReplicatedStorage/Old.luau");
        let to = dir.path().join("ReplicatedStorage/New.luau");
        std::fs::create_dir_all(from.parent().unwrap()).unwrap();
        std::fs::write(&from, b"disk edit\n").unwrap();

        let engine = ConflictEngine::new();
        engine.record_sync(&from, hash(b"agreed\n"), 1);
        assert_eq!(
            engine.on_studio_push(&from, b"studio edit\n", Some((b"disk edit\n", 2))),
            StudioDecision::Conflict
        );

        std::fs::rename(&from, &to).unwrap();
        engine.begin_fs_rename(&from, &to, false, Some(b"disk edit\n".to_vec()));
        assert_eq!(engine.finish_fs_destructive(&from), FsDecision::Conflict);

        let listed = engine.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].local_renamed_to.as_deref(), Some(to.as_path()));
        assert!(matches!(
            engine.resolve(&from, Resolution::KeepLocal),
            Some(Resolved::RenameStudio {
                from: resolved_from,
                to: resolved_to,
                is_dir: false,
                ..
            }) if resolved_from == from && resolved_to == to
        ));

        engine.park_studio_update(&from, b"disk edit\n".to_vec(), b"studio edit\n".to_vec(), 2);
        engine.begin_fs_rename(&from, &to, false, Some(b"disk edit\n".to_vec()));
        let _ = engine.finish_fs_destructive(&from);
        assert!(matches!(
            engine.resolve(&from, Resolution::KeepStudio),
            Some(Resolved::RestoreFsRename {
                from: resolved_from,
                to: resolved_to,
                conflict_path,
                studio_bytes,
                ..
            }) if resolved_from == from
                && resolved_to == to
                && conflict_path == stable_path(&from)
                && studio_bytes == b"studio edit\n"
        ));
    }

    #[test]
    fn bounded_destructive_preflight_catches_in_flight_studio_edit() {
        let source = p("/x/race.luau");
        let engine = ConflictEngine::new();
        engine.record_sync(&source, hash(b"agreed"), 1);
        engine.begin_fs_delete(&source, false);

        assert_eq!(
            engine.on_studio_push(&source, b"studio edit", None),
            StudioDecision::Conflict
        );
        assert_eq!(engine.finish_fs_destructive(&source), FsDecision::Conflict);
        assert!(engine.list()[0].local_deleted);
    }

    #[test]
    fn clean_disk_delete_and_rename_still_propagate() {
        let source = p("/x/clean.luau");
        let renamed = p("/x/renamed.luau");
        let engine = ConflictEngine::new();
        engine.record_sync(&source, hash(b"agreed"), 1);

        engine.begin_fs_delete(&source, false);
        assert_eq!(
            engine.on_studio_push(&source, b"agreed", None),
            StudioDecision::NoChange
        );
        assert_eq!(engine.finish_fs_destructive(&source), FsDecision::Propagate);

        engine.record_sync(&source, hash(b"agreed"), 1);
        engine.begin_fs_rename(&source, &renamed, false, Some(b"agreed".to_vec()));
        assert_eq!(engine.finish_fs_destructive(&source), FsDecision::Propagate);
        engine.commit_fs_rename(&source, &renamed);
        assert!(engine.matches_baseline(&renamed, b"agreed"));
    }
}
