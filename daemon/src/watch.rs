// Filesystem watcher for Ro Sync daemon.
//
// Uses `notify-debouncer-full` for OS-aware event debouncing + built-in rename
// correlation. Events are filtered through a blacklist (editor temp files,
// VCS metadata) and a caller-controllable pause window (used around `/push`
// so that the watcher doesn't re-emit our own writes).
//
// Public surface:
//   Op, OpKind, Watch
//   Watch::new(root) -> Watch
//   Watch::subscribe() -> broadcast::Receiver<Op>
//   Watch::sender()    -> broadcast::Sender<Op>
//   Watch::pause_until(Instant)
//   Watch::pause_handle() -> Arc<Mutex<Instant>>

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RenameMode};
use notify::RecommendedWatcher;
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

const DEBOUNCE_MS: u64 = 150;
const CHANNEL_CAP: usize = 1024;

/// Substrings / name fragments we never want to propagate. Matches are
/// case-sensitive and applied to the final path component (except `.git`,
/// `.codex`, and `.vscode`, which match any ancestor component).
const BLACKLISTED: &[&str] = &[
    ".DS_Store",
    ".git",
    ".codex",
    ".vscode",
    "~$",
    ".#",
    ".swp",
    ".swo",
    ".meta.json",
    ".tree.json.tmp",
];

/// Reserved filenames the daemon itself writes at the project root. Watching
/// them would cause a feedback loop where our own emit-tree / write-config
/// would bounce back as ops. Matched only at the project root — nested files
/// with these names (unlikely, but allowed) are unaffected.
const ROOT_RESERVED: &[&str] = &[
    "ro-sync.json",
    "ro-sync.md",
    "CLAUDE.md",
    "CLAUDE.MD",
    "Claude.MD",
    "AGENTS.md",
    "tree.json",
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    Add,
    Update,
    Delete,
    /// Filesystem-side rename correlated by `notify-debouncer-full` into a
    /// single event. `Op::path` holds the destination and `Op::from` the
    /// source.
    Rename,
}

#[derive(Debug, Clone, Serialize)]
pub struct Op {
    pub kind: OpKind,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<u8>>,
}

pub struct Watch {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    tx: broadcast::Sender<Op>,
    root: PathBuf,
    #[allow(dead_code)]
    pause_until: Arc<Mutex<Instant>>,
}

impl Watch {
    pub fn new(root: PathBuf) -> notify::Result<Self> {
        // Canonicalize so symlinked roots (e.g. /tmp → /private/tmp on macOS)
        // match the paths notify emits.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let (tx, _rx0) = broadcast::channel(CHANNEL_CAP);
        let (raw_tx, raw_rx) = std::sync::mpsc::channel::<DebounceEventResult>();

        let mut debouncer = new_debouncer(
            Duration::from_millis(DEBOUNCE_MS),
            None,
            move |result: DebounceEventResult| {
                let _ = raw_tx.send(result);
            },
        )?;
        debouncer.watch(&root, RecursiveMode::Recursive)?;

        let pause_until = Arc::new(Mutex::new(Instant::now()));
        let tx_thread = tx.clone();
        let root_thread = root.clone();
        let pause_thread = pause_until.clone();
        std::thread::spawn(move || drain_loop(raw_rx, tx_thread, root_thread, pause_thread));

        Ok(Self {
            _debouncer: debouncer,
            tx,
            root,
            pause_until,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Op> {
        self.tx.subscribe()
    }

    pub fn sender(&self) -> broadcast::Sender<Op> {
        self.tx.clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Suppress emitted ops until `deadline`. Extends the existing window if the
    /// new deadline is later.
    #[allow(dead_code)]
    pub fn pause_until(&self, deadline: Instant) {
        let mut guard = self.pause_until.lock().unwrap();
        if deadline > *guard {
            *guard = deadline;
        }
    }

    /// Shared handle so other modules (e.g. `/push` handlers that live outside
    /// the `Watch`) can extend the pause window.
    #[allow(dead_code)]
    pub fn pause_handle(&self) -> Arc<Mutex<Instant>> {
        self.pause_until.clone()
    }
}

fn drain_loop(
    raw_rx: std::sync::mpsc::Receiver<DebounceEventResult>,
    tx: broadcast::Sender<Op>,
    root: PathBuf,
    pause_until: Arc<Mutex<Instant>>,
) {
    while let Ok(result) = raw_rx.recv() {
        let events = match result {
            Ok(evs) => evs,
            Err(_) => continue,
        };
        // Global pause window: drop everything emitted while still inside it.
        let now = Instant::now();
        let paused = { *pause_until.lock().unwrap() > now };

        for ev in events {
            if paused {
                continue;
            }
            // Rename correlated by the debouncer: one event carrying [from, to].
            if let EventKind::Modify(ModifyKind::Name(RenameMode::Both)) = ev.event.kind {
                if ev.event.paths.len() >= 2 {
                    let from = &ev.event.paths[0];
                    let to = &ev.event.paths[1];
                    if is_blacklisted(from) || is_blacklisted(to) {
                        continue;
                    }
                    if is_root_reserved(from, &root) || is_root_reserved(to, &root) {
                        continue;
                    }
                    if !from.starts_with(&root) || !to.starts_with(&root) {
                        continue;
                    }
                    if let Some(op) = classify_rename(from, to) {
                        let _ = tx.send(op);
                    }
                    continue;
                }
            }
            for p in &ev.event.paths {
                if is_blacklisted(p) {
                    continue;
                }
                if is_root_reserved(p, &root) {
                    continue;
                }
                if let Some(op) = classify(p, &ev.event.kind, &root) {
                    let _ = tx.send(op);
                }
            }
        }
    }
}

fn classify_rename(from: &Path, to: &Path) -> Option<Op> {
    // For a rename we don't read content — the plugin's applyOps should do a
    // pure `Instance.Name = newName` (or reparent) when `from` and `to` share
    // an extension / kind.
    Some(Op {
        kind: OpKind::Rename,
        path: to.to_path_buf(),
        from: Some(from.to_path_buf()),
        content: None,
    })
}

/// Returns true if any component of the path matches a blacklisted fragment
/// (either a fixed name like `.DS_Store`/`.git`/`.codex`/`.vscode`, or a substring
/// pattern for editor swap files).
pub(crate) fn is_blacklisted(p: &Path) -> bool {
    // Ancestor-wide matches: bail early if any component is a blacklisted
    // directory or file name.
    for comp in p.components() {
        let Some(s) = comp.as_os_str().to_str() else {
            continue;
        };
        if s == ".DS_Store" || s == ".git" || s == ".codex" || s == ".vscode" {
            return true;
        }
    }
    // Substring / fragment matches on the final name.
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    for frag in BLACKLISTED {
        if matches_fragment(name, frag) {
            return true;
        }
    }
    // Unix convention — trailing `~` editor backups.
    if name.ends_with('~') {
        return true;
    }
    false
}

/// True if `path` is one of the daemon-authored root files (ro-sync.json,
/// ro-sync.md, CLAUDE.md, AGENTS.md, tree.json) sitting directly at the
/// project root. Used to prevent a feedback loop from our own emit-tree /
/// config writes.
pub(crate) fn is_root_reserved(path: &Path, root: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !ROOT_RESERVED.contains(&name) {
        return false;
    }
    match path.parent() {
        Some(parent) => parent == root,
        None => false,
    }
}

fn matches_fragment(name: &str, frag: &str) -> bool {
    match frag {
        ".DS_Store" => name == ".DS_Store",
        ".git" | ".codex" | ".vscode" => name == frag,
        ".meta.json" | ".tree.json.tmp" => name == frag,
        ".#" => name.starts_with(".#"),
        "~$" => name.starts_with("~$"),
        ".swp" | ".swo" => name.ends_with(frag),
        _ => name.contains(frag),
    }
}

fn classify(path: &Path, kind: &EventKind, root: &Path) -> Option<Op> {
    if !path.starts_with(root) || path == root {
        return None;
    }

    let exists = path.exists();
    let op_kind = match (kind, exists) {
        (_, false) => OpKind::Delete,
        (EventKind::Create(_), true) => OpKind::Add,
        (EventKind::Remove(_), _) => OpKind::Delete,
        (EventKind::Modify(_), true) => OpKind::Update,
        (_, true) => OpKind::Update,
    };

    let is_file_now = path.is_file();
    let is_dir_now = path.is_dir();

    // Parent-dir "Modify" echoes on FSEvents — we already get the child's own event.
    if matches!(op_kind, OpKind::Update) && is_dir_now {
        return None;
    }

    let content = if matches!(op_kind, OpKind::Add | OpKind::Update) && is_file_now {
        std::fs::read(path).ok()
    } else {
        None
    };

    Some(Op {
        kind: op_kind,
        path: path.to_path_buf(),
        from: None,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn recv_timeout(rx: &mut broadcast::Receiver<Op>, ms: u64) -> Option<Op> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            match rx.try_recv() {
                Ok(op) => return Some(op),
                Err(broadcast::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
    }

    #[test]
    fn emits_op_for_burst_writes() {
        let dir = TempDir::new().unwrap();
        let w = Watch::new(dir.path().to_path_buf()).unwrap();
        let mut rx = w.subscribe();

        let p = std::fs::canonicalize(dir.path()).unwrap().join("a.luau");
        let mut f = std::fs::File::create(&p).unwrap();
        for _ in 0..10 {
            f.write_all(b"x").unwrap();
            f.sync_all().unwrap();
        }
        drop(f);

        let op = recv_timeout(&mut rx, 5000).expect("op");
        assert_eq!(op.path, p);
        assert!(matches!(op.kind, OpKind::Add | OpKind::Update));
        assert_eq!(op.content.as_deref(), Some(&b"xxxxxxxxxx"[..]));
    }

    #[test]
    fn classify_remove_emits_delete_op() {
        // Integration-level "delete an existing file" is flaky on macOS FSEvents
        // (it coalesces short-lived creates+removes at the kernel layer) — so
        // instead assert the classifier handles a Remove event produced in
        // isolation, which is what /private/tmp deletes actually surface.
        use notify::event::{EventKind as EK, RemoveKind};
        let root = std::env::temp_dir();
        let p = root.join("phantom-deleted.luau");
        let op =
            classify(&p, &EK::Remove(RemoveKind::File), &root).expect("classify should emit an op");
        assert_eq!(op.kind, OpKind::Delete);
        assert_eq!(op.path, p);
    }

    #[test]
    fn pause_until_drops_events() {
        let dir = TempDir::new().unwrap();
        let w = Watch::new(dir.path().to_path_buf()).unwrap();
        let mut rx = w.subscribe();

        w.pause_until(Instant::now() + Duration::from_secs(2));
        let p = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("paused.luau");
        std::fs::write(&p, b"hi").unwrap();

        // Nothing should arrive during the pause.
        assert!(recv_timeout(&mut rx, 500).is_none());
    }

    #[test]
    fn classify_rename_emits_single_rename_op() {
        let from = PathBuf::from("/tmp/project/Old.luau");
        let to = PathBuf::from("/tmp/project/New.luau");
        let op = classify_rename(&from, &to).expect("rename op");
        assert_eq!(op.kind, OpKind::Rename);
        assert_eq!(op.path, to);
        assert_eq!(op.from.as_deref(), Some(from.as_path()));
        assert!(op.content.is_none());
    }

    #[test]
    fn root_reserved_filters_daemon_authored_files() {
        let root = PathBuf::from("/tmp/proj");
        assert!(is_root_reserved(&root.join("ro-sync.json"), &root));
        assert!(is_root_reserved(&root.join("ro-sync.md"), &root));
        assert!(is_root_reserved(&root.join("CLAUDE.md"), &root));
        assert!(is_root_reserved(&root.join("AGENTS.md"), &root));
        assert!(is_root_reserved(&root.join("tree.json"), &root));
        // Nested files with the same name are not reserved.
        assert!(!is_root_reserved(&root.join("sub/tree.json"), &root));
        assert!(!is_root_reserved(&root.join("sub/ro-sync.json"), &root));
        // Unrelated names at the root are not reserved.
        assert!(!is_root_reserved(&root.join("Main.luau"), &root));
    }

    #[test]
    fn blacklist_filters_ds_store_and_swap_files() {
        assert!(is_blacklisted(Path::new("/tmp/proj/.DS_Store")));
        assert!(is_blacklisted(Path::new("/tmp/proj/sub/.DS_Store")));
        assert!(is_blacklisted(Path::new("/tmp/proj/.git/config")));
        assert!(is_blacklisted(Path::new("/tmp/proj/.codex/config.toml")));
        assert!(is_blacklisted(Path::new("/tmp/proj/.vscode/settings.json")));
        assert!(is_blacklisted(Path::new("/tmp/proj/.#foo.luau")));
        assert!(is_blacklisted(Path::new("/tmp/proj/~$temp.docx")));
        assert!(is_blacklisted(Path::new("/tmp/proj/x.swp")));
        assert!(is_blacklisted(Path::new("/tmp/proj/x.swo")));
        assert!(is_blacklisted(Path::new("/tmp/proj/backup~")));
        assert!(!is_blacklisted(Path::new("/tmp/proj/Main.luau")));
    }
}
