// Filesystem watcher for Ro Sync daemon.
//
// Uses `notify-debouncer-full` for OS-aware event debouncing + built-in rename
// correlation. Events are filtered through a blacklist (editor temp files,
// VCS metadata) and a caller-controllable pause window (used around `/push`
// so that the watcher doesn't re-emit our own writes).
//
// Public surface:
//   Op, OpKind, WatchEvent, Watch
//   Watch::new(root) -> Watch
//   Watch::subscribe() -> broadcast::Receiver<WatchEvent>
//   Watch::pause_until(Instant)
//   Watch::pause_handle() -> Arc<Mutex<Instant>>

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::RecommendedWatcher;
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{
    new_debouncer, DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

const DEBOUNCE_MS: u64 = 150;
// Wally installs can touch several thousand files in one burst. Queue entries
// contain path/kind/shape only (never source bytes), so this bounds retained
// watcher memory. Exceeding the bound intentionally uses the receiver's tested
// Lagged -> retryable full-resync fallback rather than emitting a partial guess.
const CHANNEL_CAP: usize = 16384;
/// Notify callback ingress is intentionally tiny and nonblocking. Each entry
/// may itself be a large debounced batch; retaining more than a handful would
/// defeat the watcher's memory bound.
const RAW_INGRESS_CAP: usize = 4;
/// One batch may represent a realistic 25k-file install (one event + one path
/// per file) while pathological batches are quarantined before queueing.
const RAW_BATCH_WORK_CAP: usize = 131_072;

/// Substrings / name fragments we never want to propagate. Matches are
/// case-sensitive and applied to the final path component.
const BLACKLISTED: &[&str] = &[
    ".DS_Store",
    "~$",
    ".#",
    ".swp",
    ".swo",
    ".meta.json",
    ".tree.json.tmp",
    ".luaurc",
];

/// Project-root tooling directories. These names are valid Roblox instance
/// names below a synced service, so only the first component relative to the
/// project root is excluded.
const ROOT_TOOLING_DIRS: &[&str] = &[
    ".git",
    ".codex",
    ".vscode",
    ".t64",
    ".rosync-artifacts",
    ".rosync-backups",
    ".rosync-workflows",
    "tools",
];
const ROOT_TRANSIENT_PREFIXES: &[&str] = &[".rosync-stage-"];

/// Reserved filenames the daemon itself writes at the project root. Watching
/// them would cause a feedback loop where our own emit-tree / write-config
/// would bounce back as ops. Matched only at the project root — nested files
/// with these names (unlikely, but allowed) are unaffected.
const ROOT_RESERVED: &[&str] = &[
    ".stylua.toml",
    ".luaurc",
    "aftman.toml",
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op {
    pub kind: OpKind,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<u8>>,
    /// Filesystem shape captured from the event while it is still knowable.
    /// Remove events need this to distinguish a directory literally ending in
    /// a script suffix from a deleted script file with the same fragment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_dir: Option<bool>,
}

/// Internal watcher delivery. A resync request is deliberately a single,
/// lightweight typed item: ambiguous identity batches must never be expanded
/// into guessed operations or an artificial channel-overflow burst.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    Op(Op),
    Resync { reason: String },
}

#[derive(Debug)]
enum RawIngress {
    Batch {
        generation: u64,
        events: Vec<DebouncedEvent>,
    },
    Wake,
}

#[derive(Debug, Default)]
struct RawIngressState {
    generation: u64,
    quarantine: Option<RawQuarantine>,
}

#[derive(Debug)]
struct RawQuarantine {
    reason: String,
    emit_resync: bool,
}

#[derive(Debug, Default)]
struct RawIngressBarrier {
    state: Mutex<RawIngressState>,
}

impl RawIngressBarrier {
    fn snapshot(&self) -> (u64, bool) {
        let state = self.state.lock().unwrap();
        (state.generation, state.quarantine.is_some())
    }

    fn activate(&self, reason: impl Into<String>, emit_resync: bool) -> u64 {
        let mut state = self.state.lock().unwrap();
        if state.quarantine.is_none() {
            state.generation = state.generation.wrapping_add(1);
            state.quarantine = Some(RawQuarantine {
                reason: reason.into(),
                emit_resync,
            });
        } else if !emit_resync {
            // A downstream failure has already emitted its own shutdown. It
            // subsumes a concurrent raw-ingress fault, so drain the same
            // generation without publishing a second shutdown trigger.
            state.generation = state.generation.wrapping_add(1);
            state.quarantine = Some(RawQuarantine {
                reason: reason.into(),
                emit_resync: false,
            });
        }
        state.generation
    }
}

pub struct Watch {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    tx: broadcast::Sender<WatchEvent>,
    raw_tx: std::sync::mpsc::SyncSender<RawIngress>,
    raw_barrier: Arc<RawIngressBarrier>,
    root: PathBuf,
    #[allow(dead_code)]
    pause_until: Arc<Mutex<Instant>>,
}

impl Watch {
    pub fn new(root: PathBuf) -> notify::Result<Self> {
        // Validate the project object itself before canonicalizing. This still
        // normalizes platform aliases in ancestors (for example
        // `/var` -> `/private/var` on macOS), but a project-root symlink or
        // Windows junction cannot be smuggled through canonicalization.
        let root_metadata =
            crate::fs_safety::require_metadata_no_follow(&root).map_err(notify::Error::io)?;
        if !root_metadata.is_dir() {
            return Err(notify::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("watch root is not a physical directory: {}", root.display()),
            )));
        }
        let root =
            crate::fs_safety::stable_canonical_directory(&root).map_err(notify::Error::io)?;
        let (tx, _rx0) = broadcast::channel(CHANNEL_CAP);
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<RawIngress>(RAW_INGRESS_CAP);
        let raw_barrier = Arc::new(RawIngressBarrier::default());
        let callback_barrier = raw_barrier.clone();
        let callback_tx = raw_tx.clone();

        let mut debouncer = new_debouncer(
            Duration::from_millis(DEBOUNCE_MS),
            None,
            move |result: DebounceEventResult| {
                enqueue_raw_result(&callback_tx, &callback_barrier, result);
            },
        )?;
        debouncer.watch(&root, RecursiveMode::Recursive)?;

        let pause_until = Arc::new(Mutex::new(Instant::now()));
        let tx_thread = tx.clone();
        let root_thread = root.clone();
        let pause_thread = pause_until.clone();
        let thread_barrier = raw_barrier.clone();
        std::thread::spawn(move || {
            drain_loop(raw_rx, thread_barrier, tx_thread, root_thread, pause_thread)
        });

        Ok(Self {
            _debouncer: debouncer,
            tx,
            raw_tx,
            raw_barrier,
            root,
            pause_until,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WatchEvent> {
        self.tx.subscribe()
    }

    /// Replace a receiver with a fresh subscription positioned after every
    /// currently retained watcher event, and quarantine every raw native batch
    /// that was accepted before this downstream rebuild barrier.
    pub fn discard_retained_tail(&self, receiver: &mut broadcast::Receiver<WatchEvent>) {
        activate_raw_quarantine_silent(
            &self.raw_tx,
            &self.raw_barrier,
            "downstream watcher rebuild discarded retained raw ingress",
        );
        replace_with_fresh_subscription(&self.tx, receiver);
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

fn replace_with_fresh_subscription(
    tx: &broadcast::Sender<WatchEvent>,
    receiver: &mut broadcast::Receiver<WatchEvent>,
) {
    *receiver = tx.subscribe();
}

fn raw_batch_work(events: &[DebouncedEvent]) -> Option<usize> {
    events.iter().try_fold(0usize, |work, event| {
        work.checked_add(1usize.checked_add(event.event.paths.len())?)
    })
}

fn notify_error_reason(errors: &[notify::Error]) -> String {
    let summary = errors
        .iter()
        .take(3)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "filesystem watcher backend reported {} error(s): {summary}",
        errors.len()
    )
}

fn wake_raw_drain(tx: &std::sync::mpsc::SyncSender<RawIngress>) {
    match tx.try_send(RawIngress::Wake) {
        Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
    }
}

fn activate_raw_quarantine(
    tx: &std::sync::mpsc::SyncSender<RawIngress>,
    barrier: &RawIngressBarrier,
    reason: impl Into<String>,
) {
    barrier.activate(reason, true);
    // If the queue is empty this is the required wake. If it is full, an
    // already-retained item will wake the drain, which checks the shared
    // barrier before touching that item.
    wake_raw_drain(tx);
}

fn activate_raw_quarantine_silent(
    tx: &std::sync::mpsc::SyncSender<RawIngress>,
    barrier: &RawIngressBarrier,
    reason: impl Into<String>,
) {
    barrier.activate(reason, false);
    wake_raw_drain(tx);
}

fn enqueue_raw_result(
    tx: &std::sync::mpsc::SyncSender<RawIngress>,
    barrier: &RawIngressBarrier,
    result: DebounceEventResult,
) {
    enqueue_raw_result_with_cap(tx, barrier, result, RAW_BATCH_WORK_CAP);
}

fn enqueue_raw_result_with_cap(
    tx: &std::sync::mpsc::SyncSender<RawIngress>,
    barrier: &RawIngressBarrier,
    result: DebounceEventResult,
    batch_work_cap: usize,
) {
    // Tag the callback at ingress, before any validation work. If a rebuild
    // barrier races this callback, the complete batch retains the older
    // generation and cannot be relabeled as post-rebuild work.
    let (generation, quarantined) = barrier.snapshot();
    if quarantined {
        wake_raw_drain(tx);
        return;
    }

    let events = match result {
        Ok(events) => events,
        Err(errors) => {
            activate_raw_quarantine(tx, barrier, notify_error_reason(&errors));
            return;
        }
    };
    if events.iter().any(|event| event.event.need_rescan()) {
        activate_raw_quarantine(
            tx,
            barrier,
            "filesystem watcher requested a full rescan after missing native events",
        );
        return;
    }
    let Some(work) = raw_batch_work(&events) else {
        activate_raw_quarantine(
            tx,
            barrier,
            "filesystem watcher batch work count overflowed",
        );
        return;
    };
    if work > batch_work_cap {
        activate_raw_quarantine(
            tx,
            barrier,
            format!(
                "filesystem watcher batch exceeded bounded work cap ({work} > {batch_work_cap})"
            ),
        );
        return;
    }

    match tx.try_send(RawIngress::Batch { generation, events }) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            activate_raw_quarantine(
                tx,
                barrier,
                "filesystem watcher raw ingress queue overflowed",
            );
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
    }
}

fn flush_raw_quarantine(
    raw_rx: &std::sync::mpsc::Receiver<RawIngress>,
    barrier: &RawIngressBarrier,
    tx: &broadcast::Sender<WatchEvent>,
    discarded_through: &mut Option<u64>,
) -> bool {
    // Hold the state lock through queue draining and publication. This gives a
    // downstream silent barrier a strict ordering point: it either precedes
    // this flush (and suppresses its redundant Resync) or follows the publish
    // and subscribes after it.
    let mut state = barrier.state.lock().unwrap();
    let Some(quarantine) = state.quarantine.take() else {
        return false;
    };
    let generation = state.generation;
    while raw_rx.try_recv().is_ok() {}
    if quarantine.emit_resync {
        request_full_resync(tx, quarantine.reason);
    }
    *discarded_through =
        Some(discarded_through.map_or(generation, |discarded| discarded.max(generation)));
    state.generation = state.generation.wrapping_add(1);
    true
}

fn send_op_unless_quarantined(
    barrier: &RawIngressBarrier,
    tx: &broadcast::Sender<WatchEvent>,
    op: Op,
) -> bool {
    // Serialize the final publication point with barrier activation. If the
    // send wins, a downstream rebuild subscribes after this item; if the
    // barrier wins, this pre-barrier item is never published.
    let state = barrier.state.lock().unwrap();
    if state.quarantine.is_some() {
        return false;
    }
    let _ = tx.send(WatchEvent::Op(op));
    true
}

fn is_projected_path(path: &Path, root: &Path) -> bool {
    is_synced_path(path, root) && !is_blacklisted(path, root) && !is_root_reserved(path, root)
}

fn collect_raw_pending(
    events: Vec<DebouncedEvent>,
    root: &Path,
) -> Result<Vec<RawPending>, String> {
    let mut pending = Vec::<RawPending>::new();
    let mut by_path = HashMap::<PathBuf, usize>::new();
    for event in events {
        if let EventKind::Modify(ModifyKind::Name(mode)) = event.event.kind {
            if mode == RenameMode::Both && event.event.paths.len() == 2 {
                let from = &event.event.paths[0];
                let to = &event.event.paths[1];
                match (is_projected_path(from, root), is_projected_path(to, root)) {
                    (true, true) => push_raw_rename(&mut pending, &mut by_path, from, to),
                    (false, false) => {}
                    _ => {
                        return Err(format!(
                            "filesystem rename crossed the synced projection boundary: {} -> {}",
                            from.display(),
                            to.display()
                        ));
                    }
                }
                continue;
            }
            // Unpaired or single-path renames are routine on macOS: FSEvents
            // reports RenameMode::Any under load — including for this daemon's
            // OWN atomic temp-file swaps when it writes synced files. Treating
            // that as a fatal ambiguity quarantined the watcher moments after
            // every burst of writes, tearing down the plugin connection in a
            // loop. The path's true state is knowable — ask the filesystem:
            // present → ingest as a create/modify (content is re-read anyway),
            // absent → ingest as a removal.
            let _ = mode;
            for path in &event.event.paths {
                if !is_projected_path(path, root) {
                    continue;
                }
                let resolved_kind = if path.symlink_metadata().is_ok() {
                    EventKind::Create(CreateKind::Any)
                } else {
                    EventKind::Remove(RemoveKind::Any)
                };
                push_raw_path(&mut pending, &mut by_path, path, &resolved_kind);
            }
            continue;
        }

        for path in &event.event.paths {
            if is_projected_path(path, root) {
                push_raw_path(&mut pending, &mut by_path, path, &event.event.kind);
            }
        }
    }
    Ok(pending)
}

fn drain_loop(
    raw_rx: std::sync::mpsc::Receiver<RawIngress>,
    raw_barrier: Arc<RawIngressBarrier>,
    tx: broadcast::Sender<WatchEvent>,
    root: PathBuf,
    pause_until: Arc<Mutex<Instant>>,
) {
    let mut discarded_through = None;
    while let Ok(item) = raw_rx.recv() {
        if flush_raw_quarantine(&raw_rx, &raw_barrier, &tx, &mut discarded_through) {
            continue;
        }
        let (generation, events) = match item {
            RawIngress::Batch { generation, events } => (generation, events),
            RawIngress::Wake => continue,
        };
        if discarded_through.is_some_and(|discarded| generation <= discarded) {
            continue;
        }

        // Global pause window: drop everything emitted while still inside it.
        let now = Instant::now();
        let paused = { *pause_until.lock().unwrap() > now };
        if paused {
            continue;
        }

        // Collapse only raw event sequences whose classified semantics are
        // invariant: repeated creates/updates after one create, repeated
        // updates, and repeated deletes on the exact same path. Rename source
        // and destination paths are barriers, so identity-bearing ordering is
        // never collapsed across a move.
        let raw_pending = match collect_raw_pending(events, &root) {
            Ok(pending) => pending,
            Err(reason) => {
                raw_barrier.activate(reason, true);
                flush_raw_quarantine(&raw_rx, &raw_barrier, &tx, &mut discarded_through);
                continue;
            }
        };
        if raw_pending.is_empty() {
            continue;
        }
        let raw_pending = match compose_raw_rename_chains(raw_pending) {
            Ok(pending) => pending,
            Err(reason) => {
                raw_barrier.activate(reason, true);
                flush_raw_quarantine(&raw_rx, &raw_barrier, &tx, &mut discarded_through);
                continue;
            }
        };

        let mut validation = match crate::fs_safety::SyncedPathValidationCache::new(&root) {
            Ok(validation) => validation,
            Err(error) => {
                raw_barrier.activate(
                    format!(
                        "filesystem watcher could not validate project root {}: {error}",
                        root.display()
                    ),
                    true,
                );
                flush_raw_quarantine(&raw_rx, &raw_barrier, &tx, &mut discarded_through);
                continue;
            }
        };

        let mut pending = Vec::<Op>::new();
        let mut resync_reason = None;
        for raw in raw_pending {
            let op = match raw {
                RawPending::Path { path, kind } => {
                    classify_with_cache(&path, &kind, &root, &mut validation)
                }
                RawPending::Rename { from, to } => {
                    match classify_rename_with_cache(&from, &to, &mut validation) {
                        Some(op) => Some(op),
                        None => {
                            resync_reason = Some(format!(
                                "filesystem rename could not be classified exactly: {} -> {}",
                                from.display(),
                                to.display()
                            ));
                            break;
                        }
                    }
                }
            };
            if let Some(op) = op {
                pending.push(op);
            }
        }
        if let Some(reason) = resync_reason {
            raw_barrier.activate(reason, true);
            flush_raw_quarantine(&raw_rx, &raw_barrier, &tx, &mut discarded_through);
            continue;
        }
        for op in pending {
            if flush_raw_quarantine(&raw_rx, &raw_barrier, &tx, &mut discarded_through) {
                break;
            }
            if !send_op_unless_quarantined(&raw_barrier, &tx, op) {
                flush_raw_quarantine(&raw_rx, &raw_barrier, &tx, &mut discarded_through);
                break;
            }
        }
    }
}

fn request_full_resync(tx: &broadcast::Sender<WatchEvent>, reason: impl Into<String>) {
    let _ = tx.send(WatchEvent::Resync {
        reason: reason.into(),
    });
}

#[derive(Debug, Clone)]
enum RawPending {
    Path { path: PathBuf, kind: EventKind },
    Rename { from: PathBuf, to: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawMergeClass {
    Create,
    Modify,
    Remove,
}

fn raw_merge_class(kind: &EventKind) -> Option<RawMergeClass> {
    match kind {
        EventKind::Create(_) => Some(RawMergeClass::Create),
        EventKind::Modify(ModifyKind::Name(_)) => None,
        EventKind::Modify(_) => Some(RawMergeClass::Modify),
        EventKind::Remove(_) => Some(RawMergeClass::Remove),
        _ => None,
    }
}

fn merged_raw_kind(previous: &EventKind, next: &EventKind) -> Option<EventKind> {
    match (raw_merge_class(previous), raw_merge_class(next)) {
        (Some(RawMergeClass::Create), Some(RawMergeClass::Create | RawMergeClass::Modify)) => {
            Some(*previous)
        }
        (Some(RawMergeClass::Modify), Some(RawMergeClass::Modify))
        | (Some(RawMergeClass::Remove), Some(RawMergeClass::Remove)) => Some(*next),
        _ => None,
    }
}

fn push_raw_path(
    pending: &mut Vec<RawPending>,
    by_path: &mut HashMap<PathBuf, usize>,
    path: &Path,
    kind: &EventKind,
) {
    if let Some(index) = by_path.get(path).copied() {
        if let Some(RawPending::Path { kind: previous, .. }) = pending.get_mut(index) {
            if let Some(merged) = merged_raw_kind(previous, kind) {
                *previous = merged;
                return;
            }
        }
    }
    by_path.insert(path.to_path_buf(), pending.len());
    pending.push(RawPending::Path {
        path: path.to_path_buf(),
        kind: *kind,
    });
}

fn push_raw_rename(
    pending: &mut Vec<RawPending>,
    by_path: &mut HashMap<PathBuf, usize>,
    from: &Path,
    to: &Path,
) {
    by_path.remove(from);
    by_path.remove(to);
    pending.push(RawPending::Rename {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
    });
}

#[derive(Debug)]
struct RawRenameChain {
    source: PathBuf,
    destination: PathBuf,
    rename_indices: Vec<usize>,
    vertices: Vec<PathBuf>,
    modifies: Vec<(usize, EventKind)>,
    terminal_remove: Option<(usize, EventKind)>,
}

/// Compose stable rename chains against the batch's final filesystem shape.
///
/// `A→B, B→C` must be classified as `A→C`: by drain time `B` is gone, so
/// independently classifying the first edge would drop it and leave Studio
/// trying to rename a nonexistent `B`. Only exclusive, acyclic, temporally
/// ordered paths are safe. Any competing edge, cycle, prior destination
/// activity, or destructive identity barrier rejects the whole batch so the
/// receiver can rebuild from a fresh snapshot.
///
/// A terminal `A→…→Z, Remove(Z)` has no retained destination to rename. It is
/// exactly a delete of the original identity and is rewritten to `Remove(A)`.
fn compose_raw_rename_chains(pending: Vec<RawPending>) -> Result<Vec<RawPending>, String> {
    let mut outgoing = HashMap::<PathBuf, usize>::new();
    let mut incoming = HashMap::<PathBuf, usize>::new();
    let mut rename_count = 0usize;

    for (index, item) in pending.iter().enumerate() {
        let RawPending::Rename { from, to } = item else {
            continue;
        };
        rename_count += 1;
        if from == to {
            return Err(format!(
                "ambiguous filesystem rename batch contains a self-cycle at {}",
                from.display()
            ));
        }
        if outgoing.insert(from.clone(), index).is_some() {
            return Err(format!(
                "ambiguous filesystem rename batch has competing moves from {}",
                from.display()
            ));
        }
        if incoming.insert(to.clone(), index).is_some() {
            return Err(format!(
                "ambiguous filesystem rename batch has competing destinations at {}",
                to.display()
            ));
        }
    }
    if rename_count == 0 {
        return Ok(pending);
    }

    let roots = outgoing
        .keys()
        .filter(|path| !incoming.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    let mut visited = HashSet::<usize>::with_capacity(rename_count);
    let mut chains = Vec::<RawRenameChain>::new();

    for source in roots {
        let mut current = source.clone();
        let mut destination = source.clone();
        let mut vertices = vec![source.clone()];
        let mut rename_indices = Vec::new();
        let mut previous_index = None;
        while let Some(index) = outgoing.get(&current).copied() {
            if !visited.insert(index) {
                return Err(format!(
                    "ambiguous filesystem rename batch revisits {}",
                    current.display()
                ));
            }
            if previous_index.is_some_and(|previous| previous >= index) {
                return Err(format!(
                    "ambiguous filesystem rename chain arrived out of order at {}",
                    current.display()
                ));
            }
            let RawPending::Rename { to, .. } = &pending[index] else {
                unreachable!("rename index must address a rename item");
            };
            destination = to.clone();
            current = to.clone();
            vertices.push(current.clone());
            rename_indices.push(index);
            previous_index = Some(index);
        }
        chains.push(RawRenameChain {
            source,
            destination,
            rename_indices,
            vertices,
            modifies: Vec::new(),
            terminal_remove: None,
        });
    }
    if visited.len() != rename_count {
        return Err(
            "ambiguous filesystem rename batch contains a swap, rotation, or cycle".to_string(),
        );
    }

    let mut vertex_owner = HashMap::<PathBuf, (usize, usize)>::new();
    for (chain_index, chain) in chains.iter().enumerate() {
        for (position, vertex) in chain.vertices.iter().enumerate() {
            if vertex_owner
                .insert(vertex.clone(), (chain_index, position))
                .is_some()
            {
                return Err(format!(
                    "ambiguous filesystem rename chains share identity path {}",
                    vertex.display()
                ));
            }
        }
    }

    for (index, item) in pending.iter().enumerate() {
        let RawPending::Path { path, kind } = item else {
            continue;
        };
        let Some((chain_index, position)) = vertex_owner.get(path).copied() else {
            continue;
        };
        let chain = &mut chains[chain_index];
        let incoming_index = position
            .checked_sub(1)
            .map(|edge| chain.rename_indices[edge]);
        let outgoing_index = chain.rename_indices.get(position).copied();

        if incoming_index.is_some_and(|incoming| index < incoming) {
            return Err(format!(
                "ambiguous filesystem rename destination had prior activity: {}",
                path.display()
            ));
        }
        if outgoing_index.is_some_and(|outgoing| index > outgoing) {
            return Err(format!(
                "ambiguous filesystem rename source had activity after it moved: {}",
                path.display()
            ));
        }
        if chain
            .terminal_remove
            .is_some_and(|(removed_at, _)| index > removed_at)
        {
            return Err(format!(
                "ambiguous filesystem rename destination changed after removal: {}",
                path.display()
            ));
        }

        match raw_merge_class(kind) {
            Some(RawMergeClass::Modify) => chain.modifies.push((index, *kind)),
            Some(RawMergeClass::Remove)
                if position + 1 == chain.vertices.len()
                    && incoming_index.is_some_and(|incoming| index > incoming)
                    && chain.terminal_remove.is_none() =>
            {
                chain.terminal_remove = Some((index, *kind));
            }
            _ => {
                return Err(format!(
                    "ambiguous filesystem identity barrier in rename chain at {}",
                    path.display()
                ));
            }
        }
    }

    let mut removed = vec![false; pending.len()];
    let mut replacements = HashMap::<usize, Vec<RawPending>>::new();
    for chain in chains {
        let first_index = chain.rename_indices[0];
        for index in &chain.rename_indices {
            removed[*index] = true;
        }
        for (index, _) in &chain.modifies {
            removed[*index] = true;
        }

        let replacement = if let Some((remove_index, remove_kind)) = chain.terminal_remove {
            removed[remove_index] = true;
            vec![RawPending::Path {
                path: chain.source,
                kind: remove_kind,
            }]
        } else {
            let mut replacement = vec![RawPending::Rename {
                from: chain.source,
                to: chain.destination.clone(),
            }];
            if let Some((_, modify_kind)) = chain.modifies.last() {
                replacement.push(RawPending::Path {
                    path: chain.destination,
                    kind: *modify_kind,
                });
            }
            replacement
        };
        replacements.insert(first_index, replacement);
    }

    let mut composed = Vec::with_capacity(pending.len());
    for (index, item) in pending.into_iter().enumerate() {
        if let Some(replacement) = replacements.remove(&index) {
            composed.extend(replacement);
        }
        if !removed[index] {
            composed.push(item);
        }
    }
    Ok(composed)
}

#[cfg(test)]
fn classify_rename(from: &Path, to: &Path, root: &Path) -> Option<Op> {
    let mut validation = match crate::fs_safety::SyncedPathValidationCache::new(root) {
        Ok(validation) => validation,
        Err(error) => {
            eprintln!(
                "rosync: ignored rename for unsafe project root {}: {error}",
                root.display()
            );
            return None;
        }
    };
    classify_rename_with_cache(from, to, &mut validation)
}

fn classify_rename_with_cache(
    from: &Path,
    to: &Path,
    validation: &mut crate::fs_safety::SyncedPathValidationCache,
) -> Option<Op> {
    // For a rename we don't read content — the plugin's applyOps should do a
    // pure `Instance.Name = newName` (or reparent) when `from` and `to` share
    // an extension / kind.
    if let Err(error) = validation.validate(from, true) {
        eprintln!(
            "rosync: ignored unsafe rename source {}: {error}",
            from.display()
        );
        return None;
    }
    if let Err(error) = validation.validate(to, false) {
        eprintln!(
            "rosync: ignored unsafe rename destination {}: {error}",
            to.display()
        );
        return None;
    }
    let destination = match crate::fs_safety::metadata_no_follow(to) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!(
                "rosync: ignored unsafe rename destination {}: {error}",
                to.display()
            );
            return None;
        }
    };
    if let Err(error) = validation.validate(from, true) {
        eprintln!(
            "rosync: ignored rename whose source parent chain changed during inspection: {error}"
        );
        return None;
    }
    if let Err(error) = validation.validate(to, false) {
        eprintln!(
            "rosync: ignored rename whose destination parent chain changed during inspection: {error}"
        );
        return None;
    }
    Some(Op {
        kind: OpKind::Rename,
        path: to.to_path_buf(),
        from: Some(from.to_path_buf()),
        content: None,
        is_dir: destination.map(|metadata| metadata.is_dir()),
    })
}

/// Returns true for project-root tooling directories or a blacklisted final
/// filename. Nested folders named `tools`, `.codex`, or `.vscode` are valid
/// Studio names and must continue syncing.
pub(crate) fn is_blacklisted(p: &Path, root: &Path) -> bool {
    if let Ok(relative) = p.strip_prefix(root) {
        if let Some(name) = relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str())
        {
            if ROOT_TOOLING_DIRS.contains(&name)
                || ROOT_TRANSIENT_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            {
                return true;
            }
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

/// True if `path` is one of the daemon-authored root files sitting directly at
/// the project root. Used to prevent a feedback loop from our own emit-tree /
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
        ".meta.json" | ".tree.json.tmp" => name == frag,
        ".#" => name.starts_with(".#"),
        "~$" => name.starts_with("~$"),
        ".swp" | ".swo" => name.ends_with(frag),
        _ => name.contains(frag),
    }
}

#[cfg(test)]
fn classify(path: &Path, kind: &EventKind, root: &Path) -> Option<Op> {
    let mut validation = match crate::fs_safety::SyncedPathValidationCache::new(root) {
        Ok(validation) => validation,
        Err(error) => {
            eprintln!(
                "rosync: ignored filesystem event for unsafe project root {}: {error}",
                root.display()
            );
            return None;
        }
    };
    classify_with_cache(path, kind, root, &mut validation)
}

fn classify_with_cache(
    path: &Path,
    kind: &EventKind,
    root: &Path,
    validation: &mut crate::fs_safety::SyncedPathValidationCache,
) -> Option<Op> {
    if !is_synced_path(path, root) {
        return None;
    }
    // Notify events are queued; an existing ancestor can change into a Unix
    // symlink or Windows junction before this batch is drained. Validate every
    // still-existing component from the exact service root before inspecting
    // or reading the leaf. Missing tails are valid for delete notifications.
    if let Err(error) = validation.validate(path, true) {
        eprintln!(
            "rosync: ignored filesystem event through unsafe parent chain {}: {error}",
            path.display()
        );
        return None;
    }

    let observed = match crate::fs_safety::metadata_no_follow(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!(
                "rosync: ignored unsafe filesystem event {}: {error}",
                path.display()
            );
            return None;
        }
    };
    let exists = observed.is_some();
    let op_kind = match (kind, exists) {
        (_, false) => OpKind::Delete,
        (EventKind::Create(_), true) => OpKind::Add,
        (EventKind::Remove(_), _) => OpKind::Delete,
        (EventKind::Modify(_), true) => OpKind::Update,
        (_, true) => OpKind::Update,
    };

    let is_dir_now = observed.as_ref().is_some_and(|metadata| metadata.is_dir());
    let captured_is_dir = if exists {
        Some(is_dir_now)
    } else {
        match kind {
            EventKind::Remove(RemoveKind::File) => Some(false),
            EventKind::Remove(RemoveKind::Folder) => Some(true),
            _ => None,
        }
    };

    // Parent-dir "Modify" echoes on FSEvents — we already get the child's own event.
    if matches!(op_kind, OpKind::Update) && is_dir_now {
        return None;
    }

    if let Err(error) = validation.validate(path, !exists) {
        eprintln!(
            "rosync: ignored filesystem event whose parent chain changed during inspection {}: {error}",
            path.display()
        );
        return None;
    }

    Some(Op {
        kind: op_kind,
        path: path.to_path_buf(),
        from: None,
        // Files are hydrated by the single async receiver immediately before
        // conflict handling. Keeping source bytes out of this broadcast queue
        // bounds memory even when a project emits tens of thousands of paths.
        content: None,
        is_dir: captured_is_dir,
    })
}

fn is_synced_path(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if relative.as_os_str().is_empty() {
        return false;
    }
    relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|service| crate::fs_safety::SYNCED_SERVICES.contains(&service))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn debounced_event(kind: EventKind, paths: &[&Path]) -> DebouncedEvent {
        let event = paths.iter().fold(notify::Event::new(kind), |event, path| {
            event.add_path((*path).to_path_buf())
        });
        DebouncedEvent::new(event, Instant::now())
    }

    fn assert_one_raw_resync(
        raw_rx: &std::sync::mpsc::Receiver<RawIngress>,
        barrier: &RawIngressBarrier,
        event_tx: &broadcast::Sender<WatchEvent>,
        event_rx: &mut broadcast::Receiver<WatchEvent>,
        reason_fragment: &str,
    ) {
        let mut discarded_through = None;
        assert!(flush_raw_quarantine(
            raw_rx,
            barrier,
            event_tx,
            &mut discarded_through
        ));
        match event_rx.try_recv().unwrap() {
            WatchEvent::Resync { reason } => {
                assert!(reason.contains(reason_fragment), "{reason}");
            }
            WatchEvent::Op(_) => panic!("expected one typed full-resync request"),
        }
        assert!(matches!(
            event_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(!flush_raw_quarantine(
            raw_rx,
            barrier,
            event_tx,
            &mut discarded_through
        ));
    }

    fn recv_timeout(rx: &mut broadcast::Receiver<WatchEvent>, ms: u64) -> Option<Op> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            match rx.try_recv() {
                Ok(WatchEvent::Op(op)) => return Some(op),
                Ok(WatchEvent::Resync { reason }) => {
                    panic!("unexpected watcher resync request: {reason}")
                }
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
        std::fs::create_dir(dir.path().join("Workspace")).unwrap();
        let w = Watch::new(dir.path().to_path_buf()).unwrap();
        let mut rx = w.subscribe();

        // FSEvents registers the stream asynchronously after `watch()` returns.
        // Give the backend one debounce window before the first write so this
        // test exercises burst coalescing rather than startup scheduling.
        std::thread::sleep(Duration::from_millis(DEBOUNCE_MS * 2));
        let p = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("Workspace")
            .join("a.luau");
        let mut f = std::fs::File::create(&p).unwrap();
        for _ in 0..10 {
            f.write_all(b"x").unwrap();
            f.sync_all().unwrap();
        }
        drop(f);

        let op = recv_timeout(&mut rx, 5000).expect("op");
        assert_eq!(op.path, p);
        assert!(matches!(op.kind, OpKind::Add | OpKind::Update));
        assert_eq!(op.is_dir, Some(false));
        assert!(
            op.content.is_none(),
            "watcher queues must retain path/shape only; the receiver hydrates source"
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_project_root_is_rejected_before_watcher_registration() {
        use std::os::unix::fs::symlink;

        let parent = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let linked = parent.path().join("project");
        symlink(outside.path(), &linked).unwrap();

        let error = Watch::new(linked).err().expect("linked root must fail");
        assert!(
            error.to_string().contains("symbolic link"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn classify_remove_emits_delete_op() {
        // Integration-level "delete an existing file" is flaky on macOS FSEvents
        // (it coalesces short-lived creates+removes at the kernel layer) — so
        // instead assert the classifier handles a Remove event produced in
        // isolation, which is what /private/tmp deletes actually surface.
        use notify::event::{EventKind as EK, RemoveKind};
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("Workspace")).unwrap();
        let p = root.path().join("Workspace").join("phantom-deleted.luau");
        let op = classify(&p, &EK::Remove(RemoveKind::File), root.path())
            .expect("classify should emit an op");
        assert_eq!(op.kind, OpKind::Delete);
        assert_eq!(op.path, p);
        assert_eq!(op.is_dir, Some(false));
    }

    #[test]
    fn classify_remove_preserves_deleted_directory_shape() {
        use notify::event::{EventKind as EK, RemoveKind};
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("Workspace")).unwrap();
        let path = root
            .path()
            .join("Workspace")
            .join("folder-that-looks-like-a-script.luau");
        let op = classify(&path, &EK::Remove(RemoveKind::Folder), root.path()).unwrap();
        assert_eq!(op.kind, OpKind::Delete);
        assert_eq!(op.is_dir, Some(true));
    }

    #[test]
    fn pause_until_drops_events() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("Workspace")).unwrap();
        let w = Watch::new(dir.path().to_path_buf()).unwrap();
        let mut rx = w.subscribe();

        w.pause_until(Instant::now() + Duration::from_secs(2));
        let p = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("Workspace")
            .join("paused.luau");
        std::fs::write(&p, b"hi").unwrap();

        // Nothing should arrive during the pause.
        assert!(recv_timeout(&mut rx, 500).is_none());
    }

    #[test]
    fn classify_rename_emits_single_rename_op() {
        let project = TempDir::new().unwrap();
        let workspace = project.path().join("Workspace");
        std::fs::create_dir(&workspace).unwrap();
        let from = workspace.join("Old.luau");
        let to = workspace.join("New.luau");
        std::fs::write(&to, b"return true").unwrap();
        let op = classify_rename(&from, &to, project.path()).expect("rename op");
        assert_eq!(op.kind, OpKind::Rename);
        assert_eq!(op.path, to);
        assert_eq!(op.from.as_deref(), Some(from.as_path()));
        assert!(op.content.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn queued_events_cannot_read_through_a_parent_swapped_to_a_link() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        use std::os::unix::fs::symlink;

        let project = TempDir::new().unwrap();
        let workspace = project.path().join("Workspace");
        let container = workspace.join("Container");
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(&container).unwrap();
        std::fs::write(outside.path().join("Main.luau"), b"external sentinel").unwrap();
        symlink(outside.path(), container.join("Swapped")).unwrap();

        let through_link = container.join("Swapped").join("Main.luau");
        for kind in [
            EventKind::Create(CreateKind::Any),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Remove(RemoveKind::File),
        ] {
            assert!(
                classify(&through_link, &kind, project.path()).is_none(),
                "queued create/update/delete must fail closed after parent becomes a link"
            );
        }

        let old = workspace.join("Old.luau");
        assert!(
            classify_rename(&old, &through_link, project.path()).is_none(),
            "queued rename must validate the entire destination chain"
        );
        let retained = workspace.join("Retained.luau");
        std::fs::write(&retained, b"safe").unwrap();
        assert!(
            classify_rename(&through_link, &retained, project.path()).is_none(),
            "queued rename must validate the entire source chain"
        );
        assert_eq!(
            std::fs::read(outside.path().join("Main.luau")).unwrap(),
            b"external sentinel"
        );
    }

    #[test]
    fn raw_coalescing_reduces_only_semantics_safe_repeats() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};

        let path = Path::new("/project/Workspace/Main.luau");
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_path(
            &mut pending,
            &mut by_path,
            path,
            &EventKind::Create(CreateKind::File),
        );
        for _ in 0..10 {
            push_raw_path(
                &mut pending,
                &mut by_path,
                path,
                &EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            );
        }
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0],
            RawPending::Path {
                kind: EventKind::Create(_),
                ..
            }
        ));

        push_raw_path(
            &mut pending,
            &mut by_path,
            path,
            &EventKind::Remove(RemoveKind::File),
        );
        push_raw_path(
            &mut pending,
            &mut by_path,
            path,
            &EventKind::Create(CreateKind::File),
        );
        assert_eq!(
            pending.len(),
            3,
            "create/delete/create ordering must remain observable"
        );
    }

    #[test]
    fn raw_rename_is_a_source_and_destination_coalescing_barrier() {
        use notify::event::{CreateKind, ModifyKind};

        let from = Path::new("/project/Workspace/A.luau");
        let to = Path::new("/project/Workspace/B.luau");
        let modify = EventKind::Modify(ModifyKind::Any);
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_path(&mut pending, &mut by_path, from, &modify);
        push_raw_path(
            &mut pending,
            &mut by_path,
            to,
            &EventKind::Create(CreateKind::File),
        );
        push_raw_rename(&mut pending, &mut by_path, from, to);
        push_raw_path(&mut pending, &mut by_path, from, &modify);
        push_raw_path(&mut pending, &mut by_path, to, &modify);

        assert_eq!(pending.len(), 5);
        assert!(matches!(pending[2], RawPending::Rename { .. }));
    }

    #[test]
    fn raw_rename_chain_composes_to_the_final_destination() {
        let a = Path::new("/project/Workspace/A.luau");
        let b = Path::new("/project/Workspace/B.luau");
        let c = Path::new("/project/Workspace/C.luau");
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_rename(&mut pending, &mut by_path, a, b);
        push_raw_rename(&mut pending, &mut by_path, b, c);

        let composed = compose_raw_rename_chains(pending).unwrap();
        assert_eq!(composed.len(), 1);
        assert!(matches!(
            &composed[0],
            RawPending::Rename { from, to } if from == a && to == c
        ));
    }

    #[test]
    fn raw_rename_chain_retargets_an_intervening_destination_modify() {
        use notify::event::ModifyKind;

        let a = Path::new("/project/Workspace/A.luau");
        let b = Path::new("/project/Workspace/B.luau");
        let c = Path::new("/project/Workspace/C.luau");
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_rename(&mut pending, &mut by_path, a, b);
        push_raw_path(
            &mut pending,
            &mut by_path,
            b,
            &EventKind::Modify(ModifyKind::Any),
        );
        push_raw_rename(&mut pending, &mut by_path, b, c);

        let composed = compose_raw_rename_chains(pending).unwrap();
        assert_eq!(composed.len(), 2);
        assert!(matches!(
            &composed[0],
            RawPending::Rename { from, to } if from == a && to == c
        ));
        assert!(matches!(
            &composed[1],
            RawPending::Path {
                path,
                kind: EventKind::Modify(_)
            } if path == c
        ));
    }

    #[test]
    fn raw_rename_chain_does_not_cross_a_destructive_identity_barrier() {
        use notify::event::RemoveKind;

        let a = Path::new("/project/Workspace/A.luau");
        let b = Path::new("/project/Workspace/B.luau");
        let c = Path::new("/project/Workspace/C.luau");
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_rename(&mut pending, &mut by_path, a, b);
        push_raw_path(
            &mut pending,
            &mut by_path,
            b,
            &EventKind::Remove(RemoveKind::File),
        );
        push_raw_rename(&mut pending, &mut by_path, b, c);

        let error = compose_raw_rename_chains(pending).unwrap_err();
        assert!(error.contains("identity barrier"), "{error}");
    }

    #[test]
    fn raw_rename_chain_ending_in_remove_deletes_the_original_identity() {
        use notify::event::RemoveKind;

        let a = Path::new("/project/Workspace/A.luau");
        let b = Path::new("/project/Workspace/B.luau");
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_rename(&mut pending, &mut by_path, a, b);
        push_raw_path(
            &mut pending,
            &mut by_path,
            b,
            &EventKind::Remove(RemoveKind::File),
        );

        let composed = compose_raw_rename_chains(pending).unwrap();
        assert_eq!(composed.len(), 1);
        assert!(matches!(
            &composed[0],
            RawPending::Path {
                path,
                kind: EventKind::Remove(RemoveKind::File)
            } if path == a
        ));
    }

    #[test]
    fn raw_rename_rejects_prior_destination_activity() {
        use notify::event::ModifyKind;

        let a = Path::new("/project/Workspace/A.luau");
        let b = Path::new("/project/Workspace/B.luau");
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_path(
            &mut pending,
            &mut by_path,
            b,
            &EventKind::Modify(ModifyKind::Any),
        );
        push_raw_rename(&mut pending, &mut by_path, a, b);

        let error = compose_raw_rename_chains(pending).unwrap_err();
        assert!(error.contains("prior activity"), "{error}");
    }

    #[test]
    fn raw_rename_rejects_temporary_swaps_and_rotations() {
        let a = Path::new("/project/Workspace/A.luau");
        let b = Path::new("/project/Workspace/B.luau");
        let c = Path::new("/project/Workspace/C.luau");
        let temp = Path::new("/project/Workspace/.swap.luau");

        for edges in [
            vec![(a, temp), (b, a), (temp, b)],
            vec![(a, temp), (c, a), (b, c), (temp, b)],
        ] {
            let mut pending = Vec::new();
            let mut by_path = HashMap::new();
            for (from, to) in edges {
                push_raw_rename(&mut pending, &mut by_path, from, to);
            }
            let error = compose_raw_rename_chains(pending).unwrap_err();
            assert!(
                error.contains("swap") || error.contains("rotation") || error.contains("cycle"),
                "{error}"
            );
        }
    }

    #[test]
    fn raw_rename_rejects_competing_destinations() {
        let a = Path::new("/project/Workspace/A.luau");
        let b = Path::new("/project/Workspace/B.luau");
        let destination = Path::new("/project/Workspace/Destination.luau");
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_rename(&mut pending, &mut by_path, a, destination);
        push_raw_rename(&mut pending, &mut by_path, b, destination);

        let error = compose_raw_rename_chains(pending).unwrap_err();
        assert!(error.contains("competing destinations"), "{error}");
    }

    #[test]
    fn cross_boundary_renames_still_require_exact_resync() {
        let root = Path::new("/project");
        let inside = root.join("Workspace/Main.luau");
        let outside = PathBuf::from("/outside/Main.luau");

        for (from, to) in [(&inside, &outside), (&outside, &inside)] {
            let event = debounced_event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &[from, to],
            );
            let error = collect_raw_pending(vec![event], root).unwrap_err();
            assert!(error.contains("crossed"), "{error}");
        }
    }

    #[test]
    fn unpaired_renames_resolve_by_probing_disk() {
        // FSEvents delivers RenameMode::Any/From without a partner under load
        // (including for the daemon's own atomic temp-file swaps). Instead of
        // quarantining the watcher, the batch asks the filesystem: a path that
        // still exists ingests as a create, a missing one as a removal.
        let project = TempDir::new().unwrap();
        let workspace = project.path().join("Workspace");
        std::fs::create_dir(&workspace).unwrap();
        let present = workspace.join("Present.luau");
        std::fs::write(&present, b"return true\n").unwrap();
        let missing = workspace.join("Missing.luau");

        for (path, expect_create) in [(&present, true), (&missing, false)] {
            let unpaired = debounced_event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &[path.as_path()],
            );
            let resolved = collect_raw_pending(vec![unpaired], project.path()).unwrap();
            assert_eq!(resolved.len(), 1, "{resolved:?}");
            match &resolved[0] {
                RawPending::Path { path: seen, kind } => {
                    assert_eq!(seen, path.as_path());
                    let is_create = matches!(kind, EventKind::Create(_));
                    assert_eq!(is_create, expect_create, "{kind:?}");
                }
                other => panic!("expected a resolved path, got {other:?}"),
            }
        }
    }

    #[test]
    fn initial_generation_batch_is_delivered_before_any_barrier() {
        let project = TempDir::new().unwrap();
        let workspace = project.path().join("Workspace");
        std::fs::create_dir(&workspace).unwrap();
        let source = workspace.join("Initial.luau");
        std::fs::write(&source, b"return true\n").unwrap();

        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(1);
        let barrier = Arc::new(RawIngressBarrier::default());
        let (event_tx, mut event_rx) = broadcast::channel(4);
        let root = project.path().to_path_buf();
        let drain_barrier = barrier.clone();
        let drain = std::thread::spawn(move || {
            drain_loop(
                raw_rx,
                drain_barrier,
                event_tx,
                root,
                Arc::new(Mutex::new(Instant::now())),
            );
        });

        enqueue_raw_result(
            &raw_tx,
            &barrier,
            Ok(vec![debounced_event(
                EventKind::Create(notify::event::CreateKind::File),
                &[source.as_path()],
            )]),
        );
        let op = recv_timeout(&mut event_rx, 1_000).expect("initial generation op");
        assert_eq!(op.kind, OpKind::Add);
        assert_eq!(op.path, source);
        drop(raw_tx);
        drain.join().unwrap();
    }

    #[test]
    fn raw_ingress_queue_full_drops_tail_and_emits_one_resync() {
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(1);
        let barrier = RawIngressBarrier::default();
        let (event_tx, mut event_rx) = broadcast::channel(4);
        enqueue_raw_result(&raw_tx, &barrier, Ok(Vec::new()));
        enqueue_raw_result(&raw_tx, &barrier, Ok(Vec::new()));

        assert_one_raw_resync(
            &raw_rx,
            &barrier,
            &event_tx,
            &mut event_rx,
            "queue overflowed",
        );
        assert!(matches!(
            raw_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn raw_oversize_batch_wakes_empty_queue_and_emits_one_resync() {
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(1);
        let barrier = RawIngressBarrier::default();
        let (event_tx, mut event_rx) = broadcast::channel(4);
        let paths = [
            Path::new("/project/Workspace/A.luau"),
            Path::new("/project/Workspace/B.luau"),
        ];
        enqueue_raw_result_with_cap(
            &raw_tx,
            &barrier,
            Ok(vec![debounced_event(EventKind::Any, &paths)]),
            2,
        );

        assert!(matches!(raw_rx.try_recv(), Ok(RawIngress::Wake)));
        assert_one_raw_resync(&raw_rx, &barrier, &event_tx, &mut event_rx, "work cap");
    }

    #[test]
    fn raw_rescan_and_notify_error_each_emit_exactly_one_resync() {
        for (result, reason_fragment) in [
            (
                Ok(vec![DebouncedEvent::new(
                    notify::Event::new(EventKind::Any).set_flag(notify::event::Flag::Rescan),
                    Instant::now(),
                )]),
                "full rescan",
            ),
            (
                Err(vec![notify::Error::generic("native queue failed")]),
                "backend reported",
            ),
        ] {
            let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(1);
            let barrier = RawIngressBarrier::default();
            let (event_tx, mut event_rx) = broadcast::channel(4);
            enqueue_raw_result(&raw_tx, &barrier, result);
            // A second fault while quarantine is active must not create a
            // second shutdown request.
            enqueue_raw_result(
                &raw_tx,
                &barrier,
                Err(vec![notify::Error::generic("duplicate fault")]),
            );
            assert_one_raw_resync(&raw_rx, &barrier, &event_tx, &mut event_rx, reason_fragment);
        }
    }

    #[test]
    fn fresh_subscription_discards_destructive_tail_after_resync() {
        let (tx, mut rx) = broadcast::channel(8);
        request_full_resync(&tx, "quarantine");
        tx.send(WatchEvent::Op(Op {
            kind: OpKind::Delete,
            path: PathBuf::from("/project/Workspace/Stale.luau"),
            from: None,
            content: None,
            is_dir: Some(false),
        }))
        .unwrap();

        assert!(matches!(rx.try_recv(), Ok(WatchEvent::Resync { .. })));
        replace_with_fresh_subscription(&tx, &mut rx);
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn downstream_barrier_discards_queued_raw_work_and_broadcast_tail() {
        let project = TempDir::new().unwrap();
        let workspace = project.path().join("Workspace");
        std::fs::create_dir(&workspace).unwrap();
        let deleted = workspace.join("Stale.luau");
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(2);
        let barrier = Arc::new(RawIngressBarrier::default());
        let (event_tx, mut event_rx) = broadcast::channel(4);

        enqueue_raw_result(
            &raw_tx,
            &barrier,
            Ok(vec![debounced_event(
                EventKind::Remove(notify::event::RemoveKind::File),
                &[deleted.as_path()],
            )]),
        );
        event_tx
            .send(WatchEvent::Op(Op {
                kind: OpKind::Delete,
                path: deleted,
                from: None,
                content: None,
                is_dir: Some(false),
            }))
            .unwrap();

        activate_raw_quarantine_silent(&raw_tx, &barrier, "downstream hydration failure");
        replace_with_fresh_subscription(&event_tx, &mut event_rx);
        let event_keepalive = event_tx.clone();
        let drain_barrier = barrier.clone();
        let root = project.path().to_path_buf();
        let drain = std::thread::spawn(move || {
            drain_loop(
                raw_rx,
                drain_barrier,
                event_tx,
                root,
                Arc::new(Mutex::new(Instant::now())),
            );
        });
        drop(raw_tx);
        drain.join().unwrap();

        let _event_keepalive = event_keepalive;
        assert!(matches!(
            event_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn downstream_barrier_serializes_with_final_op_publication() {
        let (raw_tx, _raw_rx) = std::sync::mpsc::sync_channel(1);
        let barrier = RawIngressBarrier::default();
        let (event_tx, mut event_rx) = broadcast::channel(4);
        activate_raw_quarantine_silent(&raw_tx, &barrier, "downstream failure");
        replace_with_fresh_subscription(&event_tx, &mut event_rx);

        assert!(!send_op_unless_quarantined(
            &barrier,
            &event_tx,
            Op {
                kind: OpKind::Delete,
                path: PathBuf::from("/project/Workspace/Stale.luau"),
                from: None,
                content: None,
                is_dir: Some(false),
            },
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn ambiguous_batch_requests_exactly_one_bounded_full_resync() {
        let (tx, mut rx) = broadcast::channel(CHANNEL_CAP);
        let (_raw_tx, raw_rx) = std::sync::mpsc::sync_channel(1);
        let barrier = RawIngressBarrier::default();
        let a = Path::new("/project/Workspace/A.luau");
        let b = Path::new("/project/Workspace/B.luau");
        let destination = Path::new("/project/Workspace/Destination.luau");
        let mut pending = Vec::new();
        let mut by_path = HashMap::new();
        push_raw_rename(&mut pending, &mut by_path, a, destination);
        push_raw_rename(&mut pending, &mut by_path, b, destination);
        let error = compose_raw_rename_chains(pending).unwrap_err();
        barrier.activate(error, true);
        assert_one_raw_resync(&raw_rx, &barrier, &tx, &mut rx, "competing destinations");
    }

    #[test]
    fn bounded_channel_lag_is_an_explicit_full_resync_fallback() {
        let (tx, mut rx) = broadcast::channel(CHANNEL_CAP);
        let lightweight = WatchEvent::Op(Op {
            kind: OpKind::Update,
            path: PathBuf::from("/project/Workspace/Main.luau"),
            from: None,
            content: None,
            is_dir: Some(false),
        });
        for _ in 0..=CHANNEL_CAP {
            tx.send(lightweight.clone()).unwrap();
        }

        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(1))
        ));
    }

    fn assert_wide_batch_scans_parent_once(entry_count: usize) {
        use notify::event::ModifyKind;

        let project = TempDir::new().unwrap();
        let workspace = project.path().join("Workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut paths = Vec::with_capacity(entry_count);
        let realistic_source = b"--!strict\nlocal enabled = true\nreturn { enabled = enabled }\n";
        for index in 0..entry_count {
            let path = workspace.join(format!("Item{index:05}.luau"));
            std::fs::write(&path, realistic_source).unwrap();
            paths.push(path);
        }

        let mut validation =
            crate::fs_safety::SyncedPathValidationCache::new(project.path()).unwrap();
        for path in &paths {
            let op = classify_with_cache(
                path,
                &EventKind::Modify(ModifyKind::Any),
                project.path(),
                &mut validation,
            )
            .expect("wide-parent update should classify");
            assert_eq!(op.kind, OpKind::Update);
            assert!(
                op.content.is_none(),
                "even nonempty sources stay out of the watcher queue"
            );
        }
        assert_eq!(
            validation.completed_scans(),
            2,
            "one project-root scan plus one stable service scan"
        );
    }

    #[test]
    fn wide_batch_classification_reuses_one_parent_index() {
        assert_wide_batch_scans_parent_once(1_024);
    }

    #[test]
    #[ignore = "25k-event watcher burst benchmark"]
    fn benchmark_twenty_five_thousand_wide_updates() {
        assert_wide_batch_scans_parent_once(25_000);
    }

    #[test]
    fn root_reserved_filters_daemon_authored_files() {
        let root = PathBuf::from("/tmp/proj");
        assert!(is_root_reserved(&root.join(".stylua.toml"), &root));
        assert!(is_root_reserved(&root.join("aftman.toml"), &root));
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
        let root = Path::new("/tmp/proj");
        assert!(is_blacklisted(&root.join(".DS_Store"), root));
        assert!(is_blacklisted(&root.join("sub/.DS_Store"), root));
        assert!(is_blacklisted(&root.join(".git/config"), root));
        assert!(is_blacklisted(&root.join(".codex/config.toml"), root));
        assert!(is_blacklisted(&root.join(".vscode/settings.json"), root));
        assert!(is_blacklisted(
            &root.join(".rosync-backups/123/Workspace/Main.luau"),
            root
        ));
        assert!(is_blacklisted(
            &root.join(".rosync-stage-abc/Workspace/Main.luau"),
            root
        ));
        assert!(is_blacklisted(
            &root.join(".rosync-workflows/run.json"),
            root
        ));
        assert!(is_blacklisted(&root.join(".t64/session.json"), root));
        assert!(is_blacklisted(&root.join("tools/linter"), root));
        assert!(is_blacklisted(&root.join(".#foo.luau"), root));
        assert!(is_blacklisted(&root.join("~$temp.docx"), root));
        assert!(is_blacklisted(&root.join("x.swp"), root));
        assert!(is_blacklisted(&root.join("x.swo"), root));
        assert!(is_blacklisted(&root.join("backup~"), root));
        assert!(!is_blacklisted(&root.join("Main.luau"), root));
        assert!(!is_blacklisted(
            &root.join("Workspace/tools/Main.luau"),
            root
        ));
        assert!(!is_blacklisted(
            &root.join("Workspace/.codex/Main.luau"),
            root
        ));
        assert!(!is_blacklisted(
            &root.join("Workspace/.vscode/Main.luau"),
            root
        ));
        assert!(!is_blacklisted(
            &root.join("Workspace/.rosync-stage-valid/Main.luau"),
            root
        ));
    }
}
