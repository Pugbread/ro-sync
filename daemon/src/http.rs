use axum::body::Bytes;
use axum::{
    extract::{DefaultBodyLimit, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use crate::conflict::{hash, Resolution, Resolved, StudioDecision};
use crate::fs_map::{
    classify_script_file, encode_name, instance_to_path, normalize_line_endings,
    parse_disambiguated, parse_init_file, path_to_instance_meta, InstanceDescriptor, PathInstance,
    ScriptClass, META_FILE,
};

/// Roblox classes the daemon will materialize on disk. Everything else is
/// Studio-authoritative and shows up only via the plugin-emitted tree.json.
const SCOPED_CLASSES: &[&str] = &["Folder", "Script", "LocalScript", "ModuleScript"];

fn is_scoped_class(class: &str) -> bool {
    SCOPED_CLASSES.contains(&class)
}
use crate::initial_sync::{compute_disk_stats, new_choice_id, Choice, PendingInitial, Stats};
use crate::snapshot;
use crate::watch::{Op, OpKind};
use crate::{AppState, PUSH_QUIET_MS};

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Axum's default body limit is 2 MiB — a full-place bootstrap from the
    // plugin easily exceeds that. Lift it to 512 MiB so large places fit.
    const MAX_BODY: usize = 512 * 1024 * 1024;

    Router::new()
        .route("/hello", get(hello))
        .route("/snapshot", get(snapshot))
        .route("/push", post(push))
        .route("/poll", get(poll))
        .route("/events", get(events))
        .route("/ws", get(crate::ws::ws_upgrade))
        .route("/resolve", post(resolve))
        .route("/initial-compare", post(initial_compare))
        .route("/initial-decision", get(initial_decision))
        .route("/initial-choice", post(initial_choice))
        .route("/tree", post(tree_post))
        .route("/writelog", post(writelog))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
        .layer(cors)
}

// ---------------------------------------------------------------------------
// POST /tree — plugin-emitted read-only Studio tree skeleton.
// Body is written to `<project>/.tree.json.tmp` then atomically renamed to
// `tree.json`. The watcher blacklists both names, so these writes never bounce
// back as ops.
// ---------------------------------------------------------------------------

/// Append one JSONL line to `~/.terminal64/widgets/ro-sync/writes.log`.
/// Creates the directory and file if they don't exist. The body is written
/// verbatim (after a timestamp is merged in) — callers should post a JSON
/// object describing the write they just performed.
async fn writelog(body: Json<Value>) -> Json<Value> {
    let home = match rosync_home_dir() {
        Some(h) => h,
        None => {
            return Json(json!({ "ok": false, "error": "home directory not found" }));
        }
    };
    let dir = home.join(".terminal64").join("widgets").join("ro-sync");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Json(json!({ "ok": false, "error": format!("mkdir {}: {e}", dir.display()) }));
    }
    let log_path = dir.join("writes.log");
    // Rotate when writes.log grows past 10 MiB. Preserve exactly one prior
    // generation: writes.log → writes.log.1, overwriting any previous .1. We
    // check before writing rather than after so a single giant record can't
    // push the file arbitrarily far over the threshold.
    const WRITES_LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() >= WRITES_LOG_ROTATE_BYTES {
            let rotated = dir.join("writes.log.1");
            // Windows will not rename over an existing destination, so remove
            // the previous generation first. Any failure is best-effort: the
            // append below should still be allowed to proceed.
            let _ = std::fs::remove_file(&rotated);
            let _ = std::fs::rename(&log_path, &rotated);
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut merged = match body.0 {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("entry".into(), other);
            m
        }
    };
    merged.entry("ts".to_string()).or_insert(Value::from(now));
    let line = match serde_json::to_string(&Value::Object(merged)) {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({ "ok": false, "error": format!("serialize: {e}") }));
        }
    };
    use std::io::Write;
    let mut f = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            return Json(
                json!({ "ok": false, "error": format!("open {}: {e}", log_path.display()) }),
            );
        }
    };
    if let Err(e) = writeln!(f, "{line}") {
        return Json(json!({ "ok": false, "error": format!("write: {e}") }));
    }
    Json(json!({ "ok": true }))
}

fn rosync_home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(home) = std::env::var_os("ROSYNC_TEST_HOME") {
        return Some(PathBuf::from(home));
    }

    dirs::home_dir()
}

async fn tree_post(State(state): State<AppState>, body: Bytes) -> Json<Value> {
    let root = state.canonical_project.as_path();
    let tmp = root.join(".tree.json.tmp");
    let final_path = root.join("tree.json");
    let bytes = body.len();
    if let Err(e) = std::fs::write(&tmp, &body) {
        return Json(json!({ "ok": false, "error": format!("write tmp: {e}") }));
    }
    if let Err(e) = std::fs::rename(&tmp, &final_path) {
        let _ = std::fs::remove_file(&tmp);
        return Json(json!({ "ok": false, "error": format!("rename: {e}") }));
    }
    Json(json!({ "ok": true, "bytes": bytes }))
}

#[derive(Serialize)]
struct Hello {
    name: String,
    version: &'static str,
    project: String,
    #[serde(rename = "gameId")]
    game_id: Option<String>,
    #[serde(rename = "placeIds")]
    place_ids: Vec<String>,
}

async fn hello(State(state): State<AppState>) -> Json<Hello> {
    Json(Hello {
        name: state.project_name.read().unwrap().clone(),
        version: env!("CARGO_PKG_VERSION"),
        project: state.project.display().to_string(),
        game_id: state.game_id.read().unwrap().clone(),
        place_ids: state.place_ids.read().unwrap().clone(),
    })
}

// ---------------------------------------------------------------------------
// /initial-compare, /initial-decision, /initial-choice
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct InitialCompareBody {
    #[serde(rename = "studioStats")]
    studio_stats: Stats,
}

async fn initial_compare(
    State(state): State<AppState>,
    Json(body): Json<InitialCompareBody>,
) -> Json<Value> {
    let disk_stats = match compute_disk_stats(state.canonical_project.as_path()) {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({
                "ok": false,
                "error": format!("scan: {e}"),
            }));
        }
    };
    let disk_empty = disk_stats.is_empty();
    let studio_empty = body.studio_stats.is_empty();

    if disk_empty && !studio_empty {
        return Json(json!({
            "action": "push",
            "diskStats": disk_stats,
        }));
    }
    if studio_empty && !disk_empty {
        return Json(json!({
            "action": "pull",
            "diskStats": disk_stats,
        }));
    }
    if disk_empty && studio_empty {
        return Json(json!({
            "action": "push",
            "diskStats": disk_stats,
        }));
    }

    // Already-synced fast-path: when disk and Studio agree on both counts
    // (script files + instances), skip the modal and go straight to hooks +
    // WS. This is the common case for a reconnect after the first sync — the
    // previous behavior prompted every single time, which was noisy.
    // A small tolerance absorbs minor drift from transient objects the
    // plugin now suppresses (Camera, Terrain, PackageLink, PlayerScripts).
    let d = disk_stats;
    let s = body.studio_stats;
    let script_drift = (d.script_count as i64 - s.script_count as i64).abs();
    let instance_drift = (d.instance_count as i64 - s.instance_count as i64).abs();
    if script_drift == 0 && instance_drift <= 2 {
        return Json(json!({
            "action": "in-sync",
            "diskStats": disk_stats,
        }));
    }

    // Both non-empty → park a pending decision and tell the plugin to drive the UI.
    let choice_id = new_choice_id();
    let (tx, rx) = oneshot::channel::<Choice>();
    let pending = PendingInitial {
        choice_id: choice_id.clone(),
        disk_stats,
        studio_stats: body.studio_stats,
        waker: Some(tx),
    };
    {
        let mut slot = state.pending_initial.lock().unwrap();
        *slot = Some(pending);
    }
    // Stash the receiver under the same choice_id so /initial-decision can
    // await it. (The waker is stored on PendingInitial; the receiver lives on
    // the SSE side — we keep it here in a second slot so the long-poll endpoint
    // can find it by choice_id.)
    {
        let mut rxs = PENDING_RX.lock().unwrap();
        rxs.retain(|(_, _, ready)| !ready.load(std::sync::atomic::Ordering::Relaxed));
        rxs.push((
            choice_id.clone(),
            rx,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ));
    }
    let evt = json!({
        "type": "initial-choice-needed",
        "choiceId": choice_id,
        "diskStats": disk_stats,
        "studioStats": body.studio_stats,
    });
    if let Ok(s) = serde_json::to_string(&evt) {
        let _ = state.events.send(s);
    }
    Json(json!({
        "action": "decide",
        "choiceId": choice_id,
        "diskStats": disk_stats,
    }))
}

// Parked oneshot receivers keyed by choice_id. Using a module-scope static so
// the axum handler doesn't have to Send a !Send oneshot::Receiver through the
// AppState. The third tuple slot ("consumed" flag) lets us evict stale entries
// on the next compare.
type PendingRxEntry = (
    String,
    oneshot::Receiver<Choice>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
);
static PENDING_RX: once_cell_stub::Lazy<std::sync::Mutex<Vec<PendingRxEntry>>> =
    once_cell_stub::Lazy::new(|| std::sync::Mutex::new(Vec::new()));

// Tiny hand-rolled once_cell shim so we don't add a dep.
mod once_cell_stub {
    use std::cell::UnsafeCell;
    use std::sync::Once;

    pub struct Lazy<T> {
        once: Once,
        cell: UnsafeCell<Option<T>>,
        init: fn() -> T,
    }

    unsafe impl<T: Send + Sync> Sync for Lazy<T> {}

    impl<T> Lazy<T> {
        pub const fn new(init: fn() -> T) -> Self {
            Self {
                once: Once::new(),
                cell: UnsafeCell::new(None),
                init,
            }
        }
    }

    impl<T> std::ops::Deref for Lazy<T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.once.call_once(|| unsafe {
                *self.cell.get() = Some((self.init)());
            });
            unsafe { (*self.cell.get()).as_ref().unwrap() }
        }
    }
}

#[derive(Deserialize)]
struct InitialDecisionParams {
    #[serde(rename = "choiceId")]
    choice_id: String,
}

async fn initial_decision(
    State(state): State<AppState>,
    Query(params): Query<InitialDecisionParams>,
) -> impl IntoResponse {
    // Validate that the pending decision still matches.
    {
        let slot = state.pending_initial.lock().unwrap();
        match slot.as_ref() {
            Some(p) if p.choice_id == params.choice_id => {}
            _ => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "unknown choiceId" })),
                )
                    .into_response();
            }
        }
    }

    // Pull the receiver out of the side table.
    let rx_opt = {
        let mut rxs = PENDING_RX.lock().unwrap();
        let idx = rxs.iter().position(|(id, _, _)| id == &params.choice_id);
        idx.map(|i| rxs.remove(i))
    };
    let Some((id, mut rx, _consumed)) = rx_opt else {
        // No receiver (already taken by an earlier long-poll that timed out and
        // dropped it). Tell the plugin to keep polling; if the decision has
        // since been made, the /initial-choice handler will have cleared the
        // PendingInitial — but we can still detect that separately.
        let slot = state.pending_initial.lock().unwrap();
        if slot.is_none() {
            // Decision already delivered elsewhere and PendingInitial cleared —
            // but we can't know *what* was chosen. The plugin should drop its
            // current choiceId and re-run /initial-compare.
            return Json(json!({ "error": "unknown choiceId" })).into_response();
        }
        return Json(json!({ "pending": true })).into_response();
    };

    // Borrow &mut rx so `timeout` doesn't consume it; restore on timeout.
    let sleep = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(sleep);
    tokio::select! {
        res = &mut rx => {
            match res {
                Ok(choice) => {
                    let s = match choice {
                        Choice::Disk => "disk",
                        Choice::Studio => "studio",
                        Choice::Cancel => "cancel",
                    };
                    Json(json!({ "choice": s })).into_response()
                }
                Err(_) => {
                    // Sender dropped without sending — treat as still-pending.
                    Json(json!({ "pending": true })).into_response()
                }
            }
        }
        _ = &mut sleep => {
            // Timeout — return the receiver so the plugin's reconnect finds it.
            let mut rxs = PENDING_RX.lock().unwrap();
            if !rxs.iter().any(|(other, _, _)| other == &id) {
                rxs.push((id, rx, std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))));
            }
            Json(json!({ "pending": true })).into_response()
        }
    }
}

#[derive(Deserialize)]
struct InitialChoiceBody {
    #[serde(rename = "choiceId")]
    choice_id: String,
    choice: String,
}

async fn initial_choice(
    State(state): State<AppState>,
    Json(body): Json<InitialChoiceBody>,
) -> Json<Value> {
    let choice = match body.choice.as_str() {
        "disk" => Choice::Disk,
        "studio" => Choice::Studio,
        "cancel" => Choice::Cancel,
        other => {
            return Json(json!({
                "ok": false,
                "error": format!("unknown choice: {other}"),
            }));
        }
    };

    let waker = {
        let mut slot = state.pending_initial.lock().unwrap();
        match slot.as_mut() {
            Some(p) if p.choice_id == body.choice_id => p.waker.take(),
            _ => {
                return Json(json!({
                    "ok": false,
                    "error": "no pending decision",
                }));
            }
        }
    };

    if let Some(tx) = waker {
        let _ = tx.send(choice);
    }
    {
        let mut slot = state.pending_initial.lock().unwrap();
        *slot = None;
    }

    let choice_str = match choice {
        Choice::Disk => "disk",
        Choice::Studio => "studio",
        Choice::Cancel => "cancel",
    };
    let evt = json!({ "type": "initial-choice-made", "choice": choice_str });
    if let Ok(s) = serde_json::to_string(&evt) {
        let _ = state.events.send(s);
    }

    Json(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// /snapshot
// ---------------------------------------------------------------------------
//
// The plugin expects either:
//   { services: [service_node...], bootstrap: bool, strict: bool }
// or { ops: [...] }.
//
// We emit the `services` form. `bootstrap: true` tells the plugin the
// filesystem is empty, so it should send its current Studio state back as an
// initial push instead of applying our (empty) snapshot over its live tree.

async fn snapshot(State(state): State<AppState>) -> Json<Value> {
    let services = match snapshot::emit_services(state.canonical_project.as_path()) {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({ "ok": false, "error": format!("snapshot: {e}") }));
        }
    };
    let bootstrap = services.is_empty();
    Json(json!({
        "services": services,
        "bootstrap": bootstrap,
        "strict": false,
    }))
}

// ---------------------------------------------------------------------------
// /push — plugin → filesystem
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PushBody {
    #[serde(default)]
    ops: Vec<Value>,
    #[serde(default)]
    bootstrap: bool,
    #[serde(default)]
    services: Vec<Value>,
}

async fn push(State(state): State<AppState>, Json(body): Json<PushBody>) -> Json<Value> {
    let root = state.canonical_project.as_path();
    let ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
    };
    let mut res = PushApplyResult::default();

    if body.bootstrap {
        for svc in &body.services {
            match apply_service_node(root, svc, &ctx) {
                Ok(n) => res.applied += n,
                Err(e) => res.errors.push(format!("bootstrap: {e}")),
            }
        }
    }

    apply_ops_into(root, &body.ops, &ctx, &mut res);

    Json(json!({
        "ok": res.errors.is_empty(),
        "applied": res.applied,
        "skipped": res.skipped,
        "conflicts": res.conflicts,
        "errors": res.errors,
    }))
}

/// Aggregate result of applying a batch of plugin push ops.
#[derive(Default, Debug)]
pub(crate) struct PushApplyResult {
    pub applied: usize,
    pub skipped: usize,
    pub conflicts: Vec<String>,
    pub errors: Vec<String>,
}

/// Apply a slice of plugin-shape ops against the project root, folding each
/// outcome into `out`. Shared between the HTTP `/push` handler and the
/// WebSocket `push` frame handler.
pub(crate) fn apply_ops_into(
    root: &Path,
    ops: &[Value],
    ctx: &PushCtx<'_>,
    out: &mut PushApplyResult,
) {
    for op in ops {
        match apply_op(root, op, ctx) {
            Ok(ApplyOutcome::Applied(n)) => out.applied += n,
            Ok(ApplyOutcome::Skipped) => out.skipped += 1,
            Ok(ApplyOutcome::Conflict(p)) => out.conflicts.push(p.display().to_string()),
            Err(e) => out.errors.push(e),
        }
    }
}

/// Apply a batch of plugin push ops using `state`. Used by the WebSocket
/// handler; constructs a `PushCtx` internally so callers don't have to touch
/// the conflict/quiet machinery.
pub(crate) fn apply_push_ops(state: &AppState, ops: &[Value]) -> PushApplyResult {
    let root = state.canonical_project.as_path();
    let ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
    };
    let mut out = PushApplyResult::default();
    apply_ops_into(root, ops, &ctx, &mut out);
    out
}

/// Handles wired into every /push sub-handler so writes can (a) consult the
/// conflict engine and (b) mark paths as "we just wrote this" to suppress the
/// watcher's echo (Argon `SYNCBACK_DEBOUNCE_TIME`).
pub(crate) struct PushCtx<'a> {
    pub conflicts: &'a crate::conflict::ConflictEngine,
    pub push_quiet: &'a Mutex<HashMap<PathBuf, Instant>>,
}

impl<'a> PushCtx<'a> {
    fn mark_quiet(&self, path: &Path) {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let deadline = Instant::now() + Duration::from_millis(PUSH_QUIET_MS);
        let mut guard = self.push_quiet.lock().unwrap();
        guard.insert(canon, deadline);
    }
}

// ---------------------------------------------------------------------------
// /poll — long-poll filesystem → plugin
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PollParams {
    #[serde(default)]
    #[allow(dead_code)]
    since: Option<u64>,
}

async fn poll(State(state): State<AppState>, Query(_params): Query<PollParams>) -> Json<Value> {
    let mut rx = state.watch_tx.subscribe();
    let root = state.canonical_project.as_path();
    let mut out: Vec<Value> = Vec::new();

    // Wait up to 30s for the first op, then drain anything else that arrived
    // within a brief coalesce window so bursts go out together.
    let first = tokio::time::timeout(Duration::from_secs(30), rx.recv()).await;
    match first {
        Ok(Ok(op)) => {
            if let Some(po) = fs_op_to_plugin_op(root, &op) {
                out.push(po);
            }
        }
        Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
        Ok(Err(broadcast::error::RecvError::Closed)) => {}
        Err(_) => {
            // Timeout — return empty, plugin re-polls immediately.
            return Json(json!({ "ok": true, "ops": out }));
        }
    }

    // Brief drain window.
    loop {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Ok(op)) => {
                if let Some(po) = fs_op_to_plugin_op(root, &op) {
                    out.push(po);
                }
            }
            _ => break,
        }
    }

    Json(json!({ "ok": true, "ops": out }))
}

// ---------------------------------------------------------------------------
// /events — SSE stream
// ---------------------------------------------------------------------------

async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(msg) => Some(Ok(Event::default().data(msg))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// /resolve
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ResolveBody {
    path: String,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    choice: Option<String>,
}

async fn resolve(
    State(state): State<AppState>,
    Json(body): Json<ResolveBody>,
) -> impl IntoResponse {
    let raw = body.resolution.or(body.choice).unwrap_or_default();
    let resolution = match raw.as_str() {
        "keep-local" | "keep_fs" | "fs" | "local" => Resolution::KeepLocal,
        "keep-studio" | "keep_studio" | "studio" => Resolution::KeepStudio,
        other => {
            return Json(json!({
                "ok": false,
                "error": format!("unknown resolution: {other}"),
            }));
        }
    };

    let target = PathBuf::from(&body.path);
    let Some(decision) = state.conflict.resolve(&target, resolution) else {
        return Json(json!({
            "ok": false,
            "error": "no parked conflict for that path",
            "path": body.path,
        }));
    };

    match decision {
        Resolved::WriteFs(bytes) => {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&target, &bytes) {
                return Json(json!({ "ok": false, "error": format!("write: {e}") }));
            }
            state
                .conflict
                .record_sync(&target, hash(&bytes), fs_mtime(&target));
            // Quiet window so the watcher doesn't re-emit our own write.
            {
                let canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
                let deadline = Instant::now() + Duration::from_millis(PUSH_QUIET_MS);
                state.push_quiet.lock().unwrap().insert(canon, deadline);
            }
            Json(json!({ "ok": true, "action": "wrote-fs", "path": body.path }))
        }
        Resolved::PushStudio(bytes) => {
            let op = Op {
                kind: OpKind::Update,
                path: target.clone(),
                from: None,
                content: Some(bytes.clone()),
            };
            let _ = state.watch_tx.send(op);
            state
                .conflict
                .record_sync(&target, hash(&bytes), fs_mtime(&target));
            Json(json!({ "ok": true, "action": "pushed-studio", "path": body.path }))
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin op → filesystem
// ---------------------------------------------------------------------------

enum ApplyOutcome {
    Applied(usize),
    Skipped,
    Conflict(PathBuf),
}

fn op_kind(op: &Value) -> &str {
    op.get("op")
        .and_then(|v| v.as_str())
        .or_else(|| op.get("type").and_then(|v| v.as_str()))
        .unwrap_or("")
}

fn path_segments(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn apply_op(root: &Path, op: &Value, ctx: &PushCtx<'_>) -> Result<ApplyOutcome, String> {
    match op_kind(op) {
        "set" | "replace" => {
            let parent_segs = op.get("path").map(path_segments).unwrap_or_default();
            let node = op.get("node").ok_or("set: missing node")?;
            apply_set(root, &parent_segs, node, ctx)
        }
        "delete" | "remove" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            apply_delete(root, &segs, ctx).map(ApplyOutcome::Applied)
        }
        "update" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            let props = op.get("properties").cloned();
            let name = op.get("name").and_then(|v| v.as_str()).map(str::to_string);
            apply_update(root, &segs, props, name, ctx)
        }
        "rename" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            let new_name = op
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("rename: missing name")?;
            apply_rename(root, &segs, new_name, ctx).map(ApplyOutcome::Applied)
        }
        "move" => {
            let from_segs = op.get("from").map(path_segments).unwrap_or_default();
            let to_segs = op.get("to").map(path_segments).unwrap_or_default();
            apply_move(root, &from_segs, &to_segs, ctx).map(ApplyOutcome::Applied)
        }
        other if other.is_empty() => Err("op missing kind".to_string()),
        other => Err(format!("unknown op: {other}")),
    }
}

fn apply_service_node(root: &Path, node: &Value, ctx: &PushCtx<'_>) -> Result<usize, String> {
    let name = node
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("service: missing name")?;
    let svc_dir = root.join(encode_name(name));
    std::fs::create_dir_all(&svc_dir).map_err(|e| format!("mkdir {}: {e}", svc_dir.display()))?;
    ctx.mark_quiet(&svc_dir);
    // Materialize children of the service node.
    let mut n = 0usize;
    if let Some(kids) = node.get("children").and_then(|v| v.as_array()) {
        for child in kids {
            match apply_set(root, &[name.to_string()], child, ctx)? {
                ApplyOutcome::Applied(k) => n += k,
                _ => {}
            }
        }
    }
    Ok(n)
}

fn apply_set(
    root: &Path,
    parent_segs: &[String],
    node: &Value,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    let conflicts = ctx.conflicts;
    let name = node
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("set: node missing name")?;
    let class = node
        .get("class")
        .and_then(|v| v.as_str())
        .ok_or("set: node missing class")?;
    // Scope: daemon only materializes scripts + folders. Anything else is
    // Studio-authoritative and silently skipped (not errored).
    if !is_scoped_class(class) {
        return Ok(ApplyOutcome::Skipped);
    }
    let parent_dir = resolve_segments_to_dir(root, parent_segs)?;
    std::fs::create_dir_all(&parent_dir)
        .map_err(|e| format!("mkdir {}: {e}", parent_dir.display()))?;

    let children = node.get("children").and_then(|v| v.as_array()).cloned();
    let has_children = children.as_ref().map(|c| !c.is_empty()).unwrap_or(false);

    // If a node with this name already exists on disk, reuse its path; otherwise
    // compute a fresh fragment.
    let existing = find_child_fragment_by_name(&parent_dir, name).map_err(|e| e.to_string())?;
    let taken = siblings_except(&parent_dir, existing.as_deref())?;

    let frag = match &existing {
        Some(f) => {
            let p = parent_dir.join(f);
            let is_dir = p.is_dir();
            crate::fs_map::PathFragment {
                fragment: f.clone(),
                is_dir,
            }
        }
        None => instance_to_path(
            &InstanceDescriptor {
                class,
                name,
                has_children,
            },
            &taken,
        ),
    };

    let target = parent_dir.join(&frag.fragment);

    // Script content lives in properties.Source.
    let source = node
        .get("properties")
        .and_then(|p| p.get("Source"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let sc = ScriptClass::from_class(class);
    let mut applied = 0usize;

    match (sc, has_children) {
        (Some(_), false) => {
            // Leaf script file. Normalize CRLF→LF so comparisons against FS
            // bytes and cached hashes line up regardless of checkout style.
            let raw_bytes = source.unwrap_or_default().into_bytes();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            let current = if target.is_file() {
                Some((
                    std::fs::read(&target).unwrap_or_default(),
                    fs_mtime(&target),
                ))
            } else {
                None
            };
            let normalized_current: Option<Vec<u8>> = current
                .as_ref()
                .map(|(b, _)| normalize_line_endings(b).into_owned());
            let current_ref = current
                .as_ref()
                .zip(normalized_current.as_ref())
                .map(|((_, m), nb)| (nb.as_slice(), *m));
            match conflicts.on_studio_push(&target, &bytes, current_ref) {
                StudioDecision::Apply => {
                    std::fs::write(&target, &bytes)
                        .map_err(|e| format!("write {}: {e}", target.display()))?;
                    conflicts.record_sync(&target, hash(&bytes), fs_mtime(&target));
                    ctx.mark_quiet(&target);
                    applied += 1;
                }
                StudioDecision::NoChange => {}
                StudioDecision::Conflict => {
                    return Ok(ApplyOutcome::Conflict(target));
                }
            }
        }
        (Some(sc), true) => {
            // Script-with-children directory.
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("mkdir {}: {e}", target.display()))?;
            ctx.mark_quiet(&target);
            let init_name = format!("init ({}){}", encode_name(name), sc.suffix());
            let init_path = target.join(&init_name);
            let raw_bytes = source.unwrap_or_default().into_bytes();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            std::fs::write(&init_path, &bytes)
                .map_err(|e| format!("write {}: {e}", init_path.display()))?;
            conflicts.record_sync(&init_path, hash(&bytes), fs_mtime(&init_path));
            ctx.mark_quiet(&init_path);
            applied += 1;
            if let Some(kids) = children {
                let mut child_segs: Vec<String> = parent_segs.to_vec();
                child_segs.push(name.to_string());
                for child in kids {
                    if let ApplyOutcome::Applied(n) = apply_set(root, &child_segs, &child, ctx)? {
                        applied += n;
                    }
                }
            }
        }
        (None, _) => {
            // Folder (the only surviving non-script whitelisted class).
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("mkdir {}: {e}", target.display()))?;
            ctx.mark_quiet(&target);
            if let Some(kids) = children {
                let mut child_segs: Vec<String> = parent_segs.to_vec();
                child_segs.push(name.to_string());
                for child in kids {
                    if let ApplyOutcome::Applied(n) = apply_set(root, &child_segs, &child, ctx)? {
                        applied += n;
                    }
                }
            }
            applied += 1;
        }
    }
    Ok(ApplyOutcome::Applied(applied))
}

fn apply_delete(root: &Path, segs: &[String], ctx: &PushCtx<'_>) -> Result<usize, String> {
    if segs.is_empty() {
        return Err("delete: empty path".into());
    }
    let target = match resolve_segments_to_path(root, segs)? {
        Some(p) => p,
        None => return Ok(0),
    };
    if target.is_dir() {
        std::fs::remove_dir_all(&target).map_err(|e| format!("rmdir {}: {e}", target.display()))?;
    } else if target.is_file() {
        std::fs::remove_file(&target).map_err(|e| format!("rm {}: {e}", target.display()))?;
    }
    ctx.mark_quiet(&target);
    Ok(1)
}

fn apply_update(
    root: &Path,
    segs: &[String],
    properties: Option<Value>,
    _new_name: Option<String>,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    let conflicts = ctx.conflicts;
    let Some(target) = resolve_segments_to_path(root, segs)? else {
        return Ok(ApplyOutcome::Skipped);
    };

    let Some(props) = properties.and_then(|v| v.as_object().cloned()) else {
        return Ok(ApplyOutcome::Skipped);
    };

    // Script leaf: properties.Source replaces file contents.
    if target.is_file() {
        if let Some(source) = props.get("Source").and_then(|v| v.as_str()) {
            let raw_bytes = source.as_bytes().to_vec();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            let current = Some((
                std::fs::read(&target).unwrap_or_default(),
                fs_mtime(&target),
            ));
            let normalized_current: Option<Vec<u8>> = current
                .as_ref()
                .map(|(b, _)| normalize_line_endings(b).into_owned());
            let current_ref = current
                .as_ref()
                .zip(normalized_current.as_ref())
                .map(|((_, m), nb)| (nb.as_slice(), *m));
            match conflicts.on_studio_push(&target, &bytes, current_ref) {
                StudioDecision::Apply => {
                    std::fs::write(&target, &bytes)
                        .map_err(|e| format!("write {}: {e}", target.display()))?;
                    conflicts.record_sync(&target, hash(&bytes), fs_mtime(&target));
                    ctx.mark_quiet(&target);
                    return Ok(ApplyOutcome::Applied(1));
                }
                StudioDecision::NoChange => return Ok(ApplyOutcome::Skipped),
                StudioDecision::Conflict => return Ok(ApplyOutcome::Conflict(target)),
            }
        }
        return Ok(ApplyOutcome::Skipped);
    }

    // Directory-backed instances (folders / script-with-children dirs) no
    // longer carry property updates. Script-source-in-dir updates arrive via
    // `set`, not `update` — scripts-with-children have their init file set in
    // apply_set. Anything else is Studio-authoritative.
    Ok(ApplyOutcome::Skipped)
}

fn apply_rename(
    root: &Path,
    segs: &[String],
    new_name: &str,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    let Some(target) = resolve_segments_to_path(root, segs)? else {
        return Ok(0);
    };
    let parent_dir = target
        .parent()
        .ok_or_else(|| format!("rename: no parent for {}", target.display()))?
        .to_path_buf();

    let (class, has_children) = match path_to_instance_meta(&target).map_err(|e| e.to_string())? {
        Some(inst) => (
            inst.class,
            inst.is_dir && !inst.is_script_with_children
                || inst.is_script_with_children && children_exist(&target),
        ),
        None => ("Folder".to_string(), target.is_dir()),
    };
    let current_frag = target
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let taken = siblings_except(&parent_dir, current_frag.as_deref())?;
    let new_frag = instance_to_path(
        &InstanceDescriptor {
            class: &class,
            name: new_name,
            has_children,
        },
        &taken,
    );
    let new_path = parent_dir.join(&new_frag.fragment);
    std::fs::rename(&target, &new_path)
        .map_err(|e| format!("rename {} → {}: {e}", target.display(), new_path.display()))?;
    ctx.mark_quiet(&target);
    ctx.mark_quiet(&new_path);

    // If this was a script-with-children dir, also rename the init file.
    if new_path.is_dir() {
        if let Ok(iter) = std::fs::read_dir(&new_path) {
            for e in iter.flatten() {
                let fname = e.file_name();
                let Some(n) = fname.to_str() else { continue };
                if let Some((sc, _)) = parse_init_file(n) {
                    let new_init = format!("init ({}){}", encode_name(new_name), sc.suffix());
                    let old_init_path = e.path();
                    let new_init_path = new_path.join(new_init);
                    let _ = std::fs::rename(&old_init_path, &new_init_path);
                    ctx.mark_quiet(&old_init_path);
                    ctx.mark_quiet(&new_init_path);
                    break;
                }
            }
        }
    }
    Ok(1)
}

fn apply_move(
    root: &Path,
    from_segs: &[String],
    to_segs: &[String],
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    let Some(src) = resolve_segments_to_path(root, from_segs)? else {
        return Ok(0);
    };
    // `to` is the new full path (including the target's new name as the last seg).
    if to_segs.is_empty() {
        return Err("move: empty 'to' path".into());
    }
    let to_parent_segs = &to_segs[..to_segs.len() - 1];
    let new_name = &to_segs[to_segs.len() - 1];
    let parent_dir = resolve_segments_to_dir(root, to_parent_segs)?;
    std::fs::create_dir_all(&parent_dir)
        .map_err(|e| format!("mkdir {}: {e}", parent_dir.display()))?;
    let (class, has_children) = match path_to_instance_meta(&src).map_err(|e| e.to_string())? {
        Some(inst) => (inst.class, inst.is_dir),
        None => ("Folder".to_string(), src.is_dir()),
    };
    let taken = siblings_except(&parent_dir, None)?;
    let frag = instance_to_path(
        &InstanceDescriptor {
            class: &class,
            name: new_name,
            has_children,
        },
        &taken,
    );
    let dest = parent_dir.join(&frag.fragment);
    std::fs::rename(&src, &dest)
        .map_err(|e| format!("mv {} → {}: {e}", src.display(), dest.display()))?;
    ctx.mark_quiet(&src);
    ctx.mark_quiet(&dest);
    Ok(1)
}

// ---------------------------------------------------------------------------
// Path resolution helpers
// ---------------------------------------------------------------------------

/// Resolve `segs` (Studio instance names, last segment included) to a filesystem
/// path if it exists. Returns Ok(None) if any segment doesn't resolve.
fn resolve_segments_to_path(root: &Path, segs: &[String]) -> Result<Option<PathBuf>, String> {
    let mut cur = root.to_path_buf();
    for (i, seg) in segs.iter().enumerate() {
        let lookup_dir = if i == 0 {
            root.to_path_buf()
        } else {
            cur.clone()
        };
        match find_child_fragment_by_name(&lookup_dir, seg).map_err(|e| e.to_string())? {
            Some(frag) => cur = lookup_dir.join(frag),
            None => {
                // Fallback: encoded segment literally (top-level services).
                let candidate = lookup_dir.join(encode_name(seg));
                if candidate.exists() {
                    cur = candidate;
                } else {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(cur))
}

/// Resolve the segments to a filesystem *directory* to be used as a parent
/// (creating-along-the-way is deferred to the caller).
fn resolve_segments_to_dir(root: &Path, segs: &[String]) -> Result<PathBuf, String> {
    if segs.is_empty() {
        return Ok(root.to_path_buf());
    }
    if let Some(p) = resolve_segments_to_path(root, segs)? {
        if p.is_dir() {
            return Ok(p);
        }
        return Err(format!(
            "path {} is a file, not a directory (needed as parent)",
            p.display()
        ));
    }
    // Doesn't exist yet — build the literal encoded path.
    let mut p = root.to_path_buf();
    for seg in segs {
        p = p.join(encode_name(seg));
    }
    Ok(p)
}

/// Scan `dir` for a child whose instance name is `name`. Returns the fragment
/// (file/dir name) if found.
fn find_child_fragment_by_name(dir: &Path, name: &str) -> std::io::Result<Option<String>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(dir)? {
        let e = entry?;
        let fname = e.file_name();
        let Some(fstr) = fname.to_str() else { continue };
        if fstr == META_FILE {
            continue;
        }
        let inst = path_to_instance_meta(&e.path())?;
        if let Some(i) = inst {
            if i.name == name {
                return Ok(Some(fstr.to_string()));
            }
        }
    }
    Ok(None)
}

fn siblings_except(dir: &Path, except: Option<&str>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let iter = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in iter {
        let e = entry.map_err(|e| e.to_string())?;
        let fname = e.file_name();
        let Some(s) = fname.to_str() else { continue };
        if Some(s) == except {
            continue;
        }
        out.push(s.to_string());
    }
    Ok(out)
}

fn children_exist(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|it| {
            it.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n != META_FILE && parse_init_file(n).is_none())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Filesystem op → plugin op translation
// ---------------------------------------------------------------------------

/// Convert a watcher `Op` into a plugin-facing op (`set` / `delete` / `update`).
/// Directories (add/update) produce `set` ops with a minimal node envelope;
/// leaf scripts produce `set` ops carrying `properties.Source`.
pub(crate) fn fs_op_to_plugin_op(root: &Path, op: &Op) -> Option<Value> {
    let rel = op.path.strip_prefix(root).ok()?;
    let segs: Vec<String> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(String::from))
        .collect();
    if segs.is_empty() {
        return None;
    }

    // Ignore generated files (daemon-authored at the project root).
    if segs.last().map(|s| s.as_str()) == Some(snapshot::RO_SYNC_MD)
        || segs.last().map(|s| s.as_str()) == Some(snapshot::TREE_JSON)
        || segs.last().map(|s| s.as_str()) == Some(".tree.json.tmp")
    {
        return None;
    }

    match op.kind {
        OpKind::Delete => {
            let target_segs = segs_to_instance_path(&segs)?;
            Some(json!({ "op": "delete", "path": target_segs }))
        }
        OpKind::Rename => {
            // `op.path` is the destination (new) path; `op.from` is the source.
            let from_path = op.from.as_ref()?;
            let from_rel = from_path.strip_prefix(root).ok()?;
            let from_segs_fs: Vec<String> = from_rel
                .components()
                .filter_map(|c| c.as_os_str().to_str().map(String::from))
                .collect();
            if from_segs_fs.is_empty() {
                return None;
            }
            let from_inst = segs_to_instance_path(&from_segs_fs)?;
            let to_inst = segs_to_instance_path(&segs)?;
            // Two cases the plugin handles with one op:
            //   (a) same-parent rename → just `Instance.Name = last(to_inst)`.
            //   (b) cross-parent move  → reparent + maybe rename.
            Some(json!({
                "op": "rename",
                "from": from_inst,
                "to": to_inst,
            }))
        }
        OpKind::Add | OpKind::Update => {
            let fname = segs.last()?.clone();
            // Skip the `init (...).luau` files — they describe their parent dir.
            if parse_init_file(&fname).is_some() {
                // Translate into an update of the parent dir (Source on the script-with-children).
                let parent_path = op.path.parent()?;
                let parent_inst = path_to_instance_meta(parent_path).ok().flatten()?;
                if let Some(PathInstance {
                    is_script_with_children: true,
                    ..
                }) = Some(&parent_inst).filter(|i| i.is_script_with_children)
                {
                    let parent_segs_fs: Vec<String> = segs[..segs.len() - 1].to_vec();
                    let inst_segs = segs_to_instance_path(&parent_segs_fs)?;
                    let content = op.content.as_deref().unwrap_or(b"");
                    let source = String::from_utf8_lossy(content).to_string();
                    return Some(json!({
                        "op": "update",
                        "path": inst_segs,
                        "properties": { "Source": source },
                    }));
                }
                return None;
            }
            // `.meta.json` is blacklisted at the watcher — if one still slips
            // through, swallow it here.
            if fname == META_FILE {
                return None;
            }

            // Regular file or directory: classify and emit `set` with a node.
            // Scripts carry their Source; non-scripts emit an empty properties
            // map (property sync is Studio-authoritative via tree.json).
            let inst = path_to_instance_meta(&op.path).ok().flatten()?;
            let parent_segs_fs: Vec<String> = segs[..segs.len() - 1].to_vec();
            let parent_inst_segs = segs_to_instance_path(&parent_segs_fs).unwrap_or_default();

            let mut props: Map<String, Value> = Map::new();
            if !inst.is_dir {
                if let Some(bytes) = &op.content {
                    let src = String::from_utf8_lossy(bytes).to_string();
                    props.insert("Source".to_string(), Value::String(src));
                }
            }
            Some(json!({
                "op": "set",
                "path": parent_inst_segs,
                "node": {
                    "class": inst.class,
                    "name": inst.name,
                    "properties": Value::Object(props),
                    "children": Value::Array(Vec::new()),
                },
            }))
        }
    }
}

/// Translate a slice of filesystem segments (possibly disambiguated / encoded)
/// into their corresponding instance names. Returns None if any segment can't
/// be understood.
fn segs_to_instance_path(segs: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(segs.len());
    for (i, s) in segs.iter().enumerate() {
        if i == 0 {
            // Top-level is a service: name == segment (possibly disambiguated).
            out.push(match parse_disambiguated(s) {
                Some((n, _)) => crate::fs_map::decode_name(&n),
                None => crate::fs_map::decode_name(s),
            });
            continue;
        }
        // File: strip .luau variants.
        if let Some((_, stem)) = classify_script_file(s) {
            let name = match parse_disambiguated(&stem) {
                Some((n, _)) => n,
                None => stem,
            };
            out.push(crate::fs_map::decode_name(&name));
            continue;
        }
        // Directory fragment.
        let name = match parse_disambiguated(s) {
            Some((n, _)) => n,
            None => s.clone(),
        };
        out.push(crate::fs_map::decode_name(&name));
    }
    Some(out)
}

fn fs_mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
//
// These drive `apply_set` / `apply_delete` / `apply_rename` / `apply_move`
// directly against a scratch project root, which covers the same code path
// `/push` takes without needing an axum harness.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict::ConflictEngine;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "rosync-http-{}-{}-{}",
                tag,
                std::process::id(),
                rand_tok()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(std::fs::canonicalize(&p).unwrap_or(p))
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn rand_tok() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:x}", n)
    }

    fn harness<'a>(
        engine: &'a ConflictEngine,
        quiet: &'a Mutex<HashMap<PathBuf, Instant>>,
    ) -> PushCtx<'a> {
        PushCtx {
            conflicts: engine,
            push_quiet: quiet,
        }
    }

    fn push_quiet() -> Mutex<HashMap<PathBuf, Instant>> {
        Mutex::new(HashMap::new())
    }

    // Out-of-scope classes are silently skipped: `Part` is not in the four-class
    // whitelist, so `apply_set` returns `Skipped` instead of materializing
    // anything on disk. Property sync is ripped out — anything beyond
    // Folder/Script/LocalScript/ModuleScript is Studio-authoritative via tree.json.
    #[test]
    fn apply_set_skips_out_of_scope_class() {
        let d = TempDir::new("scope");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);

        let ws = d.path().join("Workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let node = serde_json::json!({
            "name": "Box",
            "class": "Part",
            "properties": { "Anchored": true },
            "children": []
        });
        let out = apply_set(d.path(), &["Workspace".into()], &node, &ctx).unwrap();
        assert!(matches!(out, ApplyOutcome::Skipped));
        assert!(!ws.join("Box").exists());
    }

    // POST /tree writes body to `.tree.json.tmp` then atomically renames it to
    // `tree.json`. Verifies round-trip bytes and that the watcher ignores both
    // paths so the write never bounces back as an op.
    #[tokio::test]
    async fn tree_post_round_trip() {
        use crate::watch::{is_blacklisted, is_root_reserved};

        let d = TempDir::new("tree-post");
        let root = d.path();
        let skeleton = serde_json::json!({
            "name": "Workspace",
            "class": "Workspace",
            "children": [
                { "name": "Camera", "class": "Camera", "children": [] }
            ]
        });
        let bytes = serde_json::to_vec(&skeleton).unwrap();

        // Write via the same path the handler uses.
        let tmp = root.join(".tree.json.tmp");
        let final_path = root.join("tree.json");
        std::fs::write(&tmp, &bytes).unwrap();
        std::fs::rename(&tmp, &final_path).unwrap();

        assert!(final_path.exists(), "tree.json should exist after rename");
        assert!(!tmp.exists(), ".tree.json.tmp should be gone after rename");

        let reloaded: Value = serde_json::from_slice(&std::fs::read(&final_path).unwrap()).unwrap();
        assert_eq!(reloaded, skeleton);

        // The watcher blacklist should ignore both filenames — proving that a
        // POST /tree round-trip never fires a watcher op.
        assert!(
            is_root_reserved(&final_path, root),
            "tree.json at project root should be reserved"
        );
        assert!(
            is_blacklisted(&tmp) || is_root_reserved(&tmp, root),
            ".tree.json.tmp should be filtered out of watcher ops"
        );
    }

    // `writelog` reads a test-only home override at call-time, so pointing it
    // at a TempDir completely contains the side effects. Environment mutation
    // is process-global though, so the writelog tests serialize on this mutex.
    static WRITELOG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn writes_log_paths(fake_home: &Path) -> (PathBuf, PathBuf) {
        let dir = fake_home.join(".terminal64/widgets/ro-sync");
        (dir.join("writes.log"), dir.join("writes.log.1"))
    }

    #[tokio::test]
    async fn writelog_appends_under_fake_home() {
        let _guard = WRITELOG_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let d = TempDir::new("writelog-append");
        std::env::set_var("ROSYNC_TEST_HOME", d.path());
        let (log, _rot) = writes_log_paths(d.path());
        let resp = writelog(Json(json!({ "op": "set", "ok": true }))).await;
        assert_eq!(resp.0["ok"], true, "writelog should succeed");
        let body = std::fs::read_to_string(&log).unwrap();
        // Exactly one JSONL line, and it should carry a `ts` field we merged in.
        let line_count = body.lines().count();
        assert_eq!(line_count, 1, "one append = one line");
        let parsed: Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(parsed["op"], "set");
        assert!(parsed["ts"].is_u64());
    }

    #[tokio::test]
    async fn writelog_rotates_when_over_10mib() {
        let _guard = WRITELOG_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let d = TempDir::new("writelog-rotate");
        std::env::set_var("ROSYNC_TEST_HOME", d.path());
        let (log, rotated) = writes_log_paths(d.path());
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        // Pre-fill writes.log past the 10 MiB threshold so the next POST
        // triggers rotation. The content is irrelevant — only the size matters.
        let big = vec![b'x'; 10 * 1024 * 1024 + 64];
        std::fs::write(&log, &big).unwrap();
        let before_len = std::fs::metadata(&log).unwrap().len();
        assert!(before_len >= 10 * 1024 * 1024);

        let resp = writelog(Json(json!({ "op": "set", "ok": true }))).await;
        assert_eq!(resp.0["ok"], true);

        // Old content has been moved aside...
        assert!(rotated.exists(), "rotation should produce writes.log.1");
        let rotated_len = std::fs::metadata(&rotated).unwrap().len();
        assert_eq!(rotated_len, before_len, "rotated file keeps original bytes");

        // ...and the fresh writes.log holds exactly the one new entry.
        let fresh = std::fs::read_to_string(&log).unwrap();
        assert_eq!(fresh.lines().count(), 1);
        assert!(fresh.contains("\"op\":\"set\""));
    }

    #[tokio::test]
    async fn writelog_rotation_overwrites_prior_generation() {
        let _guard = WRITELOG_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let d = TempDir::new("writelog-rotate-overwrite");
        std::env::set_var("ROSYNC_TEST_HOME", d.path());
        let (log, rotated) = writes_log_paths(d.path());
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        // A prior rotation exists with distinctive content...
        std::fs::write(&rotated, b"OLD_ROTATION\n").unwrap();
        // ...and the live log is over threshold with new-ish content.
        let mut marker = b"NEW_ROTATION\n".to_vec();
        marker.extend_from_slice(&vec![b'y'; 10 * 1024 * 1024]);
        std::fs::write(&log, &marker).unwrap();

        let resp = writelog(Json(json!({ "op": "eval", "ok": true }))).await;
        assert_eq!(resp.0["ok"], true);

        // The .1 file must now start with NEW_ROTATION — old generation gone.
        let rotated_body = std::fs::read(&rotated).unwrap();
        assert!(
            rotated_body.starts_with(b"NEW_ROTATION"),
            "writes.log.1 should be overwritten by the prior writes.log"
        );
    }
}
