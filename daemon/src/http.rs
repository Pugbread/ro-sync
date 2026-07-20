use axum::body::Bytes;
use axum::http::{header, request::Parts, HeaderValue, Method};
use axum::{
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
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
use std::collections::{BTreeSet, HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::conflict::{hash, Resolution, Resolved, StudioDecision};
use crate::diff;
use crate::fs_map::{
    classify_script_file, encode_name, instance_to_path, is_empty_plain_folder, is_init_file,
    normalize_line_endings, parse_disambiguated, parse_init_file, parse_plain_init_file,
    path_to_instance_meta, InstanceDescriptor, PathInstance, ScriptClass, META_FILE,
};

/// Roblox classes the daemon will materialize on disk. Everything else is
/// Studio-authoritative and must be inspected through the live plugin bridge.
fn is_scoped_class(class: &str) -> bool {
    crate::sync_scope::contains(class)
}

#[derive(Clone)]
struct AvoidSyncCache {
    root: PathBuf,
    paths: Vec<Vec<String>>,
}

static AVOID_SYNC_CACHE: OnceLock<Mutex<Option<AvoidSyncCache>>> = OnceLock::new();
use crate::initial_sync::{compute_disk_stats, new_choice_id, Choice, PendingInitial, Stats};
use crate::snapshot;
use crate::watch::{Op, OpKind};
use crate::{AppState, PUSH_QUIET_MS};

pub fn router(state: AppState) -> Router {
    // The Terminal 64 widget is rendered in an embedded browser, but ordinary
    // web pages must not be able to call this privileged localhost API. Browser
    // origins need both an allowlisted app origin (or file-webview `null`) and
    // the owning widget's capability; native Studio/CLI requests carry no
    // Origin header and bypass this browser-only CORS gate.
    let cors_managed = state.managed;
    let cors_owner_token = state.manager_owner_token.clone();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(
            move |origin: &HeaderValue, request: &Parts| {
                is_authorized_widget_browser_request(
                    origin,
                    &request.uri,
                    cors_managed,
                    cors_owner_token.as_ref().as_deref(),
                )
            },
        ))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE]);

    // Axum's default body limit is 2 MiB — a full-place bootstrap from the
    // plugin easily exceeds that. Lift it to 512 MiB so large places fit.
    const MAX_BODY: usize = 512 * 1024 * 1024;

    const ARTIFACT_CONTROL_BODY: usize = 4 * 1024;
    const ARTIFACT_CHUNK_BODY: usize = 768 * 1024;
    const PROJECT_INIT_BODY: usize = 16 * 1024;

    Router::new()
        .route("/hello", get(hello))
        .route(
            "/projects/init",
            post(project_init).layer(DefaultBodyLimit::max(PROJECT_INIT_BODY)),
        )
        .route("/snapshot", get(snapshot))
        .route("/snapshot/selective", post(selective_snapshot))
        .route("/push", post(push))
        .route("/poll", get(poll))
        .route("/events", get(events))
        .route("/ws", get(crate::ws::ws_upgrade))
        .route("/resolve", get(resolve_list).post(resolve))
        .route("/initial-compare", post(initial_compare))
        .route("/initial-decision", get(initial_decision))
        .route(
            "/initial-choice",
            get(initial_choice_status).post(initial_choice),
        )
        .route("/tree", post(tree_post))
        .route("/writelog", post(writelog))
        .route(
            "/artifacts/lease",
            post(artifact_lease).layer(DefaultBodyLimit::max(ARTIFACT_CONTROL_BODY)),
        )
        .route("/artifacts/:id", get(artifact_lookup))
        .route(
            "/artifacts/:id/read",
            post(artifact_read).layer(DefaultBodyLimit::max(ARTIFACT_CONTROL_BODY)),
        )
        .route(
            "/artifacts/:id/chunk",
            post(artifact_chunk).layer(DefaultBodyLimit::max(ARTIFACT_CHUNK_BODY)),
        )
        .route(
            "/artifacts/:id/finalize",
            post(artifact_finalize).layer(DefaultBodyLimit::max(ARTIFACT_CONTROL_BODY)),
        )
        .route(
            "/artifacts/:id/abort",
            post(artifact_abort).layer(DefaultBodyLimit::max(ARTIFACT_CONTROL_BODY)),
        )
        .route(
            "/artifacts/:id/consume",
            post(artifact_consume).layer(DefaultBodyLimit::max(ARTIFACT_CONTROL_BODY)),
        )
        .route("/widget-heartbeat", post(widget_heartbeat))
        .route("/widget-close", post(widget_close))
        .route("/manager-heartbeat", post(manager_heartbeat))
        .route("/manager-close", post(manager_close))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
        .layer(cors)
}

// ---------------------------------------------------------------------------
// Bounded binary artifact channel. A CLI creates a lease, hands its opaque
// token to the Studio plugin, and the plugin appends base64 chunks directly to
// localhost HTTP. Final metadata contains an absolute private file path, size,
// and SHA-256; image/audio/model bytes never need to cross the command WS.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactLeaseBody {
    filename: String,
    mime: Option<String>,
    expected_size: Option<u64>,
}

async fn artifact_lease(
    State(state): State<AppState>,
    Json(body): Json<ArtifactLeaseBody>,
) -> Json<Value> {
    match state
        .artifacts
        .create_lease(&body.filename, body.mime.as_deref(), body.expected_size)
    {
        Ok(lease) => Json(json!({ "ok": true, "lease": lease })),
        Err(error) => artifact_error_json(error),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactChunkBody {
    token: String,
    offset: u64,
    bytes_base64: String,
}

async fn artifact_chunk(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ArtifactChunkBody>,
) -> Json<Value> {
    use base64::Engine as _;
    const MAX_CHUNK_BYTES: usize = 512 * 1024;
    const MAX_ENCODED_CHUNK_BYTES: usize = MAX_CHUNK_BYTES.div_ceil(3) * 4;
    if body.bytes_base64.len() > MAX_ENCODED_CHUNK_BYTES {
        return Json(json!({
            "ok": false,
            "error": {
                "code": "ARTIFACT_CHUNK_TOO_LARGE",
                "message": format!(
                    "encoded artifact chunk is {} bytes; maximum is {MAX_ENCODED_CHUNK_BYTES}",
                    body.bytes_base64.len()
                ),
                "retryable": false,
            }
        }));
    }
    let bytes = match base64::engine::general_purpose::STANDARD.decode(&body.bytes_base64) {
        Ok(bytes) if bytes.len() <= MAX_CHUNK_BYTES => bytes,
        Ok(bytes) => {
            return Json(json!({
                "ok": false,
                "error": {
                    "code": "ARTIFACT_CHUNK_TOO_LARGE",
                    "message": format!("artifact chunk is {} bytes; maximum is {MAX_CHUNK_BYTES}", bytes.len()),
                    "retryable": false,
                }
            }));
        }
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": {
                    "code": "INVALID_ARTIFACT_BASE64",
                    "message": error.to_string(),
                    "retryable": false,
                }
            }));
        }
    };
    match state
        .artifacts
        .append(&id, &body.token, body.offset, &bytes)
    {
        Ok(receipt) => Json(json!({ "ok": true, "receipt": receipt })),
        Err(error) => artifact_error_json(error),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactFinalizeBody {
    token: String,
    expected_sha256: Option<String>,
}

async fn artifact_finalize(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ArtifactFinalizeBody>,
) -> Json<Value> {
    match state
        .artifacts
        .finalize(&id, &body.token, body.expected_sha256.as_deref())
    {
        Ok(artifact) => Json(json!({ "ok": true, "artifact": artifact })),
        Err(error) => artifact_error_json(error),
    }
}

#[derive(Deserialize)]
struct ArtifactAbortBody {
    token: String,
}

async fn artifact_abort(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ArtifactAbortBody>,
) -> Json<Value> {
    match state.artifacts.abort(&id, &body.token) {
        Ok(()) => Json(json!({ "ok": true, "aborted": true })),
        Err(error) => artifact_error_json(error),
    }
}

async fn artifact_lookup(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Value> {
    match state.artifacts.lookup(&id) {
        Ok(Some(artifact)) => Json(json!({ "ok": true, "artifact": artifact })),
        Ok(None) => Json(json!({
            "ok": false,
            "error": {
                "code": "ARTIFACT_NOT_FOUND",
                "message": "artifact was not found or is not finalized",
                "retryable": false,
            }
        })),
        Err(error) => artifact_error_json(error),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactReadBody {
    offset: u64,
    max_bytes: Option<u64>,
}

/// Read one bounded chunk from a finalized artifact.
///
/// Artifact ids contain 192 bits of randomness, the server is loopback-only,
/// and only finalized files registered by [`ArtifactStore`] can be addressed.
/// This gives the Studio plugin a bounded download path for native `.rbxm`
/// clipboard payloads without putting large binary values on the command WS.
async fn artifact_read(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ArtifactReadBody>,
) -> Json<Value> {
    use base64::Engine as _;
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

    const MAX_READ_BYTES: u64 = 512 * 1024;
    let metadata = match state.artifacts.lookup(&id) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
            return Json(json!({
                "ok": false,
                "error": {
                    "code": "ARTIFACT_NOT_FOUND",
                    "message": "artifact was not found or is not finalized",
                    "retryable": false,
                }
            }));
        }
        Err(error) => return artifact_error_json(error),
    };
    if body.offset >= metadata.size {
        return Json(json!({
            "ok": false,
            "error": {
                "code": "ARTIFACT_READ_RANGE",
                "message": format!(
                    "artifact read offset {} is outside {} bytes",
                    body.offset, metadata.size
                ),
                "retryable": false,
            }
        }));
    }
    let requested = body.max_bytes.unwrap_or(MAX_READ_BYTES);
    if requested == 0 || requested > MAX_READ_BYTES {
        return Json(json!({
            "ok": false,
            "error": {
                "code": "ARTIFACT_READ_RANGE",
                "message": format!("maxBytes must be between 1 and {MAX_READ_BYTES}"),
                "retryable": false,
            }
        }));
    }
    let count = requested.min(metadata.size - body.offset);
    let mut file = match tokio::fs::File::open(&metadata.path).await {
        Ok(file) => file,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": {
                    "code": "ARTIFACT_IO",
                    "message": format!("open finalized artifact: {error}"),
                    "retryable": true,
                }
            }));
        }
    };
    if let Err(error) = file.seek(std::io::SeekFrom::Start(body.offset)).await {
        return Json(json!({
            "ok": false,
            "error": {
                "code": "ARTIFACT_IO",
                "message": format!("seek finalized artifact: {error}"),
                "retryable": true,
            }
        }));
    }
    let Ok(count_usize) = usize::try_from(count) else {
        return Json(json!({
            "ok": false,
            "error": {
                "code": "ARTIFACT_READ_RANGE",
                "message": "artifact chunk length does not fit this platform",
                "retryable": false,
            }
        }));
    };
    let mut bytes = vec![0u8; count_usize];
    if let Err(error) = file.read_exact(&mut bytes).await {
        return Json(json!({
            "ok": false,
            "error": {
                "code": "ARTIFACT_IO",
                "message": format!("read finalized artifact: {error}"),
                "retryable": true,
            }
        }));
    }
    let next_offset = body.offset + count;
    Json(json!({
        "ok": true,
        "chunk": {
            "offset": body.offset,
            "nextOffset": next_offset,
            "eof": next_offset == metadata.size,
            "byteLength": metadata.size,
            "sha256": metadata.sha256,
            "bytesBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }))
}

async fn artifact_consume(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Value> {
    match state.artifacts.consume(&id) {
        Ok(Some(artifact)) => Json(json!({ "ok": true, "artifact": artifact, "consumed": true })),
        Ok(None) => Json(json!({
            "ok": false,
            "error": {
                "code": "ARTIFACT_NOT_FOUND",
                "message": "artifact was not found or is not finalized",
                "retryable": false,
            }
        })),
        Err(error) => artifact_error_json(error),
    }
}

fn artifact_error_json(error: crate::artifact::ArtifactError) -> Json<Value> {
    use crate::artifact::ArtifactError;
    let (code, retryable) = match &error {
        ArtifactError::LeaseNotFound => ("ARTIFACT_LEASE_NOT_FOUND", false),
        ArtifactError::InvalidToken => ("ARTIFACT_INVALID_TOKEN", false),
        ArtifactError::LeaseExpired => ("ARTIFACT_LEASE_EXPIRED", true),
        ArtifactError::OffsetMismatch { .. } => ("ARTIFACT_OFFSET_MISMATCH", true),
        ArtifactError::ByteLimitExceeded { .. }
        | ArtifactError::ExpectedSizeExceeded { .. }
        | ArtifactError::PendingByteLimitExceeded { .. }
        | ArtifactError::SizeMismatch { .. }
        | ArtifactError::ChecksumMismatch { .. } => ("ARTIFACT_VALIDATION_FAILED", false),
        ArtifactError::PendingLeaseLimitExceeded { .. } => ("ARTIFACT_CAPACITY", true),
        ArtifactError::Io(_) | ArtifactError::LockPoisoned => ("ARTIFACT_IO", true),
        _ => ("ARTIFACT_INVALID_REQUEST", false),
    };
    Json(json!({
        "ok": false,
        "error": {
            "code": code,
            "message": error.to_string(),
            "retryable": retryable,
        }
    }))
}

/// Return whether a browser Origin belongs to the local application shell.
/// Terminal 64 serves widget iframes from an ephemeral loopback HTTP port, while
/// packaged shells may use one of the custom origins below. This is only an
/// eligibility check: every browser request must additionally present the
/// per-daemon widget capability.
pub(crate) fn is_trusted_local_app_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let normalized = origin.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "t64://widget"
            | "terminal64://widget"
            | "app://localhost"
            | "tauri://localhost"
            | "http://tauri.localhost"
            | "wry://localhost"
    ) {
        return true;
    }

    let Ok(url) = reqwest::Url::parse(origin.trim()) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    )
}

/// Authorize a browser request from an eligible app origin (or the opaque
/// `null` origin used by a file-backed webview) with the widget owner token in
/// `?widgetToken=...`. Native Studio and CLI requests have no Origin header and
/// bypass this browser-only gate.
pub(crate) fn is_authorized_widget_browser_request(
    origin: &HeaderValue,
    uri: &axum::http::Uri,
    widget_owned: bool,
    expected_token: Option<&str>,
) -> bool {
    if !widget_owned {
        return false;
    }
    let Ok(origin_text) = origin.to_str() else {
        return false;
    };
    if !origin_text.trim().eq_ignore_ascii_case("null") && !is_trusted_local_app_origin(origin) {
        return false;
    }
    let Some(expected_token) = expected_token else {
        return false;
    };
    let candidate = uri.query().and_then(|query| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == "widgetToken").then_some(value)
        })
    });
    candidate.is_some_and(|candidate| constant_time_text_eq(candidate, expected_token))
}

fn constant_time_text_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (&left, &right) in left.as_bytes().iter().zip(right.as_bytes()) {
        difference |= left ^ right;
    }
    difference == 0
}

// ---------------------------------------------------------------------------
// POST /tree — plugin-emitted read-only Studio tree skeleton.
// The daemon keeps only the AvoidSync boundaries it needs for watcher
// filtering. The full live Explorer shape stays Studio-authoritative and is
// read through `rosync tree` / `rosync ls` rather than a project cache file.
// ---------------------------------------------------------------------------

/// Append one JSONL line to the platform-native Ro Sync `writes.log`.
/// Creates the directory and file if they don't exist. The body is written
/// verbatim (after a timestamp is merged in) — callers should post a JSON
/// object describing the write they just performed.
async fn writelog(body: Json<Value>) -> Json<Value> {
    write_log_entry(body)
}

pub(crate) fn write_log_entry(body: Json<Value>) -> Json<Value> {
    let dir = match writes_log_dir() {
        Ok(dir) => dir,
        Err(error) => return Json(json!({ "ok": false, "error": error })),
    };
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
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut f = match options.open(&log_path) {
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

fn writes_log_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(home) = std::env::var_os("ROSYNC_TEST_HOME") {
        let dir = PathBuf::from(home)
            .join(".terminal64")
            .join("widgets")
            .join("ro-sync");
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("mkdir {}: {error}", dir.display()))?;
        return Ok(dir);
    }

    if let Ok(dir) = crate::lifecycle::state_dir(None) {
        if crate::lifecycle::create_private_dir(&dir).is_ok() {
            return Ok(dir);
        }
    }
    if let Some(dir) = crate::lifecycle::legacy_widget_dir() {
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("mkdir {}: {error}", dir.display()))?;
        return Ok(dir);
    }
    Err("Ro Sync data directory not found".to_string())
}

#[derive(Default, Deserialize)]
struct WidgetControlBody {
    token: Option<String>,
    reason: Option<String>,
}

fn parse_widget_control_body(body: &Bytes) -> WidgetControlBody {
    if body.is_empty() {
        return WidgetControlBody::default();
    }
    serde_json::from_slice(body).unwrap_or_default()
}

fn authorize_widget_control(state: &AppState, token: Option<&str>) -> Result<(), &'static str> {
    if !state.widget_owned {
        return Err("daemon is not widget-owned");
    }
    let Some(expected) = state.widget_owner_token.as_ref().as_deref() else {
        return Err("missing daemon owner token");
    };
    if token != Some(expected) {
        return Err("invalid daemon owner token");
    }
    Ok(())
}

fn authorize_manager_control(state: &AppState, token: Option<&str>) -> Result<(), &'static str> {
    if !state.managed {
        return Err("daemon is not lifecycle-managed");
    }
    let Some(expected) = state.manager_owner_token.as_ref().as_deref() else {
        return Err("missing daemon control token");
    };
    if token != Some(expected) {
        return Err("invalid daemon control token");
    }
    Ok(())
}

async fn manager_heartbeat(State(state): State<AppState>, body: Bytes) -> Json<Value> {
    let body = parse_widget_control_body(&body);
    if let Err(error) = authorize_manager_control(&state, body.token.as_deref()) {
        return Json(json!({ "ok": false, "error": error }));
    }
    *state.manager_last_seen.lock().unwrap() = Some(Instant::now());
    Json(json!({ "ok": true }))
}

async fn manager_close(State(state): State<AppState>, body: Bytes) -> Json<Value> {
    let body = parse_widget_control_body(&body);
    if let Err(error) = authorize_manager_control(&state, body.token.as_deref()) {
        return Json(json!({ "ok": false, "error": error }));
    }
    let reason = body
        .reason
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "lifecycle manager requested shutdown".to_string());
    let plugin_connected = state.active_plugin.lock().unwrap().is_some();
    let _ = state.shutdown_tx.send(Some(reason.clone()));
    Json(json!({
        "ok": true,
        "reason": reason,
        "pluginConnected": plugin_connected,
    }))
}

async fn widget_heartbeat(State(state): State<AppState>, body: Bytes) -> Json<Value> {
    let body = parse_widget_control_body(&body);
    if let Err(error) = authorize_widget_control(&state, body.token.as_deref()) {
        return Json(json!({ "ok": false, "error": error }));
    }
    *state.widget_last_seen.lock().unwrap() = Some(Instant::now());
    *state.manager_last_seen.lock().unwrap() = Some(Instant::now());
    Json(json!({ "ok": true }))
}

async fn widget_close(State(state): State<AppState>, body: Bytes) -> Json<Value> {
    let body = parse_widget_control_body(&body);
    if let Err(error) = authorize_widget_control(&state, body.token.as_deref()) {
        return Json(json!({ "ok": false, "error": error }));
    }
    let reason = body
        .reason
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "widget closed".to_string());
    if state.active_plugin.lock().unwrap().is_some() {
        return Json(json!({
            "ok": true,
            "reason": reason,
            "keptAlive": true,
            "pluginConnected": true,
        }));
    }
    let _ = state.shutdown_tx.send(Some(reason.clone()));
    Json(json!({ "ok": true, "reason": reason }))
}

async fn tree_post(State(state): State<AppState>, body: Bytes) -> Json<Value> {
    let root = state.canonical_project.as_path();
    let bytes = body.len();
    let tree = match serde_json::from_slice::<Value>(&body) {
        Ok(tree) => tree,
        Err(e) => return Json(json!({ "ok": false, "error": format!("parse tree: {e}") })),
    };
    let mut paths = Vec::new();
    collect_avoid_sync_paths(&tree, &[], &mut paths);
    set_avoid_sync_paths(root, paths.clone());
    Json(json!({ "ok": true, "bytes": bytes, "avoidSyncPaths": paths.len() }))
}

fn set_avoid_sync_paths(root: &Path, paths: Vec<Vec<String>>) {
    let cache = AVOID_SYNC_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(AvoidSyncCache {
            root: root.to_path_buf(),
            paths,
        });
    }
}

#[derive(Serialize)]
struct Hello {
    name: String,
    version: &'static str,
    project: String,
    #[serde(rename = "gameId")]
    game_id: Option<String>,
    #[serde(rename = "groupId")]
    group_id: Option<String>,
    #[serde(rename = "placeIds")]
    place_ids: Vec<String>,
    #[serde(rename = "wallyEnabled")]
    wally_enabled: bool,
    #[serde(rename = "wallyFolder")]
    wally_folder: Option<String>,
    #[serde(rename = "widgetOwned")]
    widget_owned: bool,
    managed: bool,
    #[serde(rename = "managedBy")]
    managed_by: String,
    #[serde(rename = "bootId")]
    boot_id: String,
    pid: u32,
    port: u16,
    #[serde(rename = "startedAt")]
    started_at: u64,
    #[serde(rename = "pluginConnected")]
    plugin_connected: bool,
    #[serde(rename = "pluginProtocol")]
    plugin_protocol: u64,
    #[serde(rename = "pluginCapability")]
    plugin_capability: &'static str,
    #[serde(rename = "projectInit")]
    project_init: ProjectInitHello,
}

#[derive(Serialize)]
struct ProjectInitHello {
    available: bool,
    #[serde(rename = "projectsRoot", skip_serializing_if = "Option::is_none")]
    projects_root: Option<String>,
    endpoint: &'static str,
}

async fn hello(State(state): State<AppState>) -> Json<Hello> {
    let plugin_connected = state.active_plugin.lock().unwrap().is_some();
    let projects_root = state
        .projects_root
        .as_ref()
        .as_ref()
        .map(|path| path.display().to_string());
    Json(Hello {
        name: state.project_name.read().unwrap().clone(),
        version: env!("CARGO_PKG_VERSION"),
        project: state.project.display().to_string(),
        game_id: state.game_id.read().unwrap().clone(),
        group_id: state.group_id.read().unwrap().clone(),
        place_ids: state.place_ids.read().unwrap().clone(),
        wally_enabled: *state.wally_enabled.read().unwrap(),
        wally_folder: state.wally_folder.read().unwrap().clone(),
        widget_owned: state.widget_owned,
        managed: state.managed,
        managed_by: state.managed_by.as_ref().clone(),
        boot_id: state.boot_id.as_ref().clone(),
        pid: state.process_id,
        port: state.listen_port,
        started_at: state.started_at,
        plugin_connected,
        plugin_protocol: crate::ws::PLUGIN_PROTOCOL_VERSION,
        plugin_capability: crate::ws::plugin_capability(),
        project_init: ProjectInitHello {
            available: projects_root.is_some(),
            projects_root,
            endpoint: "/projects/init",
        },
    })
}

// ---------------------------------------------------------------------------
// POST /projects/init — explicit Studio request to initialize one safe project
// below the desktop-authorized projects root. The request contains metadata,
// never a filesystem path. A per-boot plugin capability prevents arbitrary
// localhost callers from creating directories.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ProjectInitBody {
    #[serde(rename = "pluginCapability")]
    plugin_capability: String,
    #[serde(rename = "gameName")]
    game_name: String,
    #[serde(rename = "placeName")]
    place_name: String,
    #[serde(rename = "gameId")]
    game_id: String,
    #[serde(rename = "placeId")]
    place_id: String,
    #[serde(rename = "creatorType", default)]
    creator_type: Option<String>,
    #[serde(rename = "creatorId", default)]
    creator_id: Option<String>,
    #[serde(rename = "groupId", default)]
    group_id: Option<String>,
}

async fn project_init(State(state): State<AppState>, body: Bytes) -> Json<Value> {
    project_init_inner(&state, &body)
}

fn project_init_inner(state: &AppState, body: &[u8]) -> Json<Value> {
    let Some(projects_root) = state.projects_root.as_ref().as_ref() else {
        return Json(json!({
            "ok": false,
            "error": {
                "code": "PROJECT_INIT_UNAVAILABLE",
                "message": "this daemon was not started with a desktop-authorized projects root",
            },
        }));
    };
    let body = match serde_json::from_slice::<ProjectInitBody>(body) {
        Ok(body) => body,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": {
                    "code": "INVALID_REQUEST",
                    "message": format!("parse project initialization request: {error}"),
                },
            }));
        }
    };
    if !constant_time_text_eq(
        body.plugin_capability.as_str(),
        crate::ws::plugin_capability(),
    ) {
        return Json(json!({
            "ok": false,
            "error": {
                "code": "UNAUTHORIZED",
                "message": "the advertised Studio plugin capability is missing or stale",
            },
        }));
    }

    let outcome = match crate::project_init::initialize_project(
        projects_root,
        crate::project_init::ProjectInitRequest {
            game_name: body.game_name,
            place_name: body.place_name,
            game_id: body.game_id,
            place_id: body.place_id,
            creator_type: body.creator_type,
            creator_id: body.creator_id,
            group_id: body.group_id,
        },
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": {
                    "code": error.code(),
                    "message": error.message(),
                    "suggestedDirectoryName": error.suggested_directory_name(),
                },
            }));
        }
    };

    let status = if outcome.created {
        "created"
    } else {
        "existing"
    };
    let event = json!({
        "type": "project-init",
        "status": status,
        "project": outcome.project,
        "directoryName": outcome.directory_name,
        "name": outcome.name,
        "metadata": outcome.metadata,
        "changed": outcome.changed,
    });
    let _ = state.events.send(event.to_string());
    let _ = write_log_entry(Json(json!({
        "source": "studio-plugin",
        "action": "project-init",
        "outcome": status,
        "project": outcome.project,
        "directoryName": outcome.directory_name,
        "name": outcome.name,
        "metadata": outcome.metadata,
        "changed": outcome.changed,
    })));

    Json(json!({
        "ok": true,
        "status": status,
        "created": outcome.created,
        "project": outcome.project,
        "directoryName": outcome.directory_name,
        "name": outcome.name,
        "metadata": outcome.metadata,
        "changed": outcome.changed,
        "reconnectRequired": true,
    }))
}

// ---------------------------------------------------------------------------
// /initial-compare, /initial-decision, /initial-choice
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct InitialCompareBody {
    #[serde(rename = "studioStats")]
    studio_stats: Stats,
    #[serde(rename = "studioSnapshot", default)]
    studio_snapshot: Vec<Value>,
    #[serde(rename = "pluginProtocol", default)]
    plugin_protocol: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct InitialComparison {
    summary: InitialComparisonSummary,
    #[serde(rename = "newFiles")]
    new_files: Vec<diff::DiffItem>,
    #[serde(rename = "changedFiles")]
    changed_files: Vec<diff::ChangedItem>,
    #[serde(rename = "removedFiles")]
    removed_files: Vec<diff::DiffItem>,
}

impl InitialComparison {
    fn is_clean(&self) -> bool {
        self.summary.new_files == 0
            && self.summary.changed_files == 0
            && self.summary.removed_files == 0
    }

    fn divergent_paths(&self) -> Vec<String> {
        let mut paths = self
            .new_files
            .iter()
            .map(|item| item.path.clone())
            .chain(self.changed_files.iter().map(|item| item.path.clone()))
            .chain(self.removed_files.iter().map(|item| item.path.clone()))
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }
}

#[derive(Debug, Clone, Serialize)]
struct InitialComparisonSummary {
    #[serde(rename = "newFiles")]
    new_files: usize,
    #[serde(rename = "changedFiles")]
    changed_files: usize,
    #[serde(rename = "removedFiles")]
    removed_files: usize,
}

async fn initial_compare(
    State(state): State<AppState>,
    Json(body): Json<InitialCompareBody>,
) -> Json<Value> {
    if body.plugin_protocol != Some(crate::ws::PLUGIN_PROTOCOL_VERSION) {
        return Json(json!({
            "ok": false,
            "error": format!(
                "incompatible Studio plugin protocol; expected {}. Reinstall the Studio plugin.",
                crate::ws::PLUGIN_PROTOCOL_VERSION
            ),
        }));
    }
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

    if body.studio_snapshot.is_empty() {
        return Json(json!({
            "ok": false,
            "error": "Studio snapshot is required to compare two non-empty sync trees",
        }));
    }
    let comparison =
        match initial_snapshot_comparison(state.canonical_project.as_path(), &body.studio_snapshot)
        {
            Ok(report) if report.is_clean() => {
                if let Err(error) = seed_clean_script_baselines(
                    state.canonical_project.as_path(),
                    state.conflict.as_ref(),
                ) {
                    return Json(json!({
                        "ok": false,
                        "error": format!("seed conflict baselines: {error}"),
                    }));
                }
                return Json(json!({
                    "action": "in-sync",
                    "diskStats": disk_stats,
                }));
            }
            Ok(report) => Some(report),
            Err(e) => {
                return Json(json!({
                    "ok": false,
                    "error": format!("snapshot compare: {e}"),
                }));
            }
        };

    // Both non-empty → park a pending decision and tell the plugin to drive the UI.
    let choice_id = new_choice_id();
    let pending = PendingInitial {
        choice_id: choice_id.clone(),
        disk_stats,
        studio_stats: body.studio_stats,
        choice: None,
        allowed_disk_paths: comparison
            .as_ref()
            .map(InitialComparison::divergent_paths)
            .unwrap_or_default(),
        selected_disk_paths: None,
        comparison: comparison
            .as_ref()
            .and_then(|report| serde_json::to_value(report).ok()),
    };
    {
        let mut slot = state.pending_initial.lock().unwrap();
        *slot = Some(pending);
    }
    let evt = json!({
        "type": "initial-choice-needed",
        "choiceId": choice_id,
        "diskStats": disk_stats,
        "studioStats": body.studio_stats,
        "comparison": comparison.clone(),
    });
    if let Ok(s) = serde_json::to_string(&evt) {
        let _ = state.events.send(s);
    }
    Json(json!({
        "action": "decide",
        "choiceId": choice_id,
        "diskStats": disk_stats,
        "comparison": comparison,
    }))
}

fn seed_clean_script_baselines(
    root: &Path,
    conflicts: &crate::conflict::ConflictEngine,
) -> Result<usize, String> {
    let mut seeded = 0usize;
    for service in snapshot::SYNCED_SERVICES {
        let service_dir = root.join(service);
        if service_dir.is_dir() {
            seeded += seed_script_baselines_in_dir(&service_dir, conflicts)?;
        }
    }
    Ok(seeded)
}

fn seed_script_baselines_in_dir(
    dir: &Path,
    conflicts: &crate::conflict::ConflictEngine,
) -> Result<usize, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("read dir {}: {error}", dir.display()))?;
    let mut seeded = 0usize;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read dir {}: {error}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            seeded += seed_script_baselines_in_dir(&path, conflicts)?;
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if classify_script_file(&name).is_none() && !is_init_file(&name) {
            continue;
        }
        let bytes =
            std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let normalized = normalize_line_endings(&bytes).into_owned();
        conflicts.record_sync(&path, hash(&normalized), fs_mtime(&path));
        seeded += 1;
    }
    Ok(seeded)
}

#[cfg(test)]
fn initial_snapshots_match(root: &Path, studio_services: &[Value]) -> Result<bool, String> {
    Ok(initial_snapshot_comparison(root, studio_services)?.is_clean())
}

fn initial_snapshot_comparison(
    root: &Path,
    studio_services: &[Value],
) -> Result<InitialComparison, String> {
    let local_services =
        snapshot::emit_services(root).map_err(|e| format!("scan {}: {e}", root.display()))?;
    let mut local = diff::collect_local_nodes(&local_services);
    let ignored = avoid_sync_paths_from_nodes(studio_services);
    if !ignored.is_empty() {
        local.retain(|path, _| !diff_path_is_avoid_synced(path, &ignored));
    }
    let studio = diff::collect_studio_tree_nodes(&json!({
        "class": "DataModel",
        "name": "game",
        "children": studio_services,
    }));
    let report = diff::compare(&local, &studio);
    Ok(InitialComparison {
        summary: InitialComparisonSummary {
            new_files: report.summary.added,
            changed_files: report.summary.changed,
            removed_files: report.summary.removed,
        },
        new_files: report.added,
        changed_files: report.changed,
        removed_files: report.removed,
    })
}

fn avoid_sync_paths_from_nodes(nodes: &[Value]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for node in nodes {
        collect_avoid_sync_paths(node, &[], &mut out);
    }
    out
}

fn diff_path_is_avoid_synced(path: &str, ignored: &[Vec<String>]) -> bool {
    if path.is_empty() {
        return false;
    }
    let segs: Vec<&str> = path.split('/').collect();
    ignored.iter().any(|prefix| {
        prefix.len() <= segs.len()
            && prefix
                .iter()
                .zip(segs.iter())
                .all(|(a, b)| a.as_str() == *b)
    })
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
    let started = Instant::now();
    loop {
        let decision = {
            let slot = state.pending_initial.lock().unwrap();
            match slot.as_ref() {
                Some(p) if p.choice_id == params.choice_id => p
                    .choice
                    .map(|choice| (choice, p.selected_disk_paths.clone())),
                _ => {
                    return Json(json!({
                        "choice": "stale",
                        "error": "unknown choiceId",
                    }))
                    .into_response();
                }
            }
        };

        if let Some((choice, selected_disk_paths)) = decision {
            {
                let mut slot = state.pending_initial.lock().unwrap();
                if slot.as_ref().map(|p| p.choice_id.as_str()) == Some(params.choice_id.as_str()) {
                    *slot = None;
                }
            }
            let s = match choice {
                Choice::Disk => "disk",
                Choice::Studio => "studio",
                Choice::Cancel => "cancel",
            };
            return match (choice, selected_disk_paths) {
                (Choice::Disk, Some(paths)) => {
                    Json(json!({ "choice": s, "paths": paths })).into_response()
                }
                _ => Json(json!({ "choice": s })).into_response(),
            };
        }

        if started.elapsed() >= Duration::from_secs(60) {
            return Json(json!({ "pending": true })).into_response();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn initial_choice_status(State(state): State<AppState>) -> Json<Value> {
    let pending = state.pending_initial.lock().unwrap();
    let Some(pending) = pending.as_ref() else {
        return Json(json!({ "pending": false }));
    };
    let choice = pending.choice.map(|choice| match choice {
        Choice::Disk => "disk",
        Choice::Studio => "studio",
        Choice::Cancel => "cancel",
    });
    Json(json!({
        "pending": true,
        "choiceId": pending.choice_id,
        "diskStats": pending.disk_stats,
        "studioStats": pending.studio_stats,
        "choice": choice,
        "comparisonPaths": pending.allowed_disk_paths,
        "selectedPaths": pending.selected_disk_paths,
        "comparison": pending.comparison,
    }))
}

#[derive(Deserialize)]
struct InitialChoiceBody {
    #[serde(rename = "choiceId")]
    choice_id: String,
    choice: String,
    #[serde(default)]
    paths: Option<Vec<String>>,
}

fn normalize_initial_disk_selection(
    requested: Vec<String>,
    allowed: &[String],
) -> Result<Vec<String>, String> {
    if requested.is_empty() {
        return Err("choose at least one divergent path before finishing".into());
    }
    let allowed: HashSet<&str> = allowed.iter().map(String::as_str).collect();
    let mut selected = Vec::with_capacity(requested.len());
    for path in requested {
        if path.is_empty() || !allowed.contains(path.as_str()) {
            return Err(format!(
                "path is not part of the current initial divergence: {path:?}"
            ));
        }
        selected.push(path);
    }
    selected.sort();
    selected.dedup();
    Ok(selected)
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

    {
        let mut slot = state.pending_initial.lock().unwrap();
        match slot.as_mut() {
            Some(p) if p.choice_id == body.choice_id => {
                if choice != Choice::Disk && body.paths.is_some() {
                    return Json(json!({
                        "ok": false,
                        "error": "selected paths are only valid for a disk-to-Studio choice",
                    }));
                }
                p.selected_disk_paths = match body.paths {
                    Some(paths) => {
                        match normalize_initial_disk_selection(paths, &p.allowed_disk_paths) {
                            Ok(paths) => Some(paths),
                            Err(error) => return Json(json!({ "ok": false, "error": error })),
                        }
                    }
                    None => None,
                };
                p.choice = Some(choice);
            }
            _ => {
                return Json(json!({
                    "ok": false,
                    "error": "no pending decision",
                }));
            }
        }
    }

    let choice_str = match choice {
        Choice::Disk => "disk",
        Choice::Studio => "studio",
        Choice::Cancel => "cancel",
    };
    let evt = json!({
        "type": "initial-choice-made",
        "choiceId": body.choice_id,
        "choice": choice_str,
    });
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

#[derive(Deserialize, Default)]
struct SnapshotParams {
    #[serde(default)]
    strict: bool,
    #[serde(rename = "forcePrune", default)]
    force_prune: bool,
}

async fn snapshot(
    State(state): State<AppState>,
    Query(params): Query<SnapshotParams>,
) -> Json<Value> {
    let services = match snapshot::emit_services(state.canonical_project.as_path()) {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({ "ok": false, "error": format!("snapshot: {e}") }));
        }
    };
    let bootstrap = services.is_empty();
    let plugin_connected = state.active_plugin.lock().unwrap().is_some();
    Json(json!({
        "services": services,
        "bootstrap": bootstrap,
        "strict": params.strict,
        "forcePrune": params.force_prune,
        "pluginConnected": plugin_connected,
    }))
}

#[derive(Deserialize)]
struct SelectiveSnapshotBody {
    paths: Vec<String>,
    #[serde(rename = "pluginProtocol", default)]
    plugin_protocol: Option<u64>,
}

fn compact_selected_paths(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Err("selective snapshot requires at least one path".into());
    }
    if paths.len() > 100_000 {
        return Err("selective snapshot path count exceeds 100000".into());
    }

    let mut ordered = paths.to_vec();
    ordered.sort_by(|left, right| {
        left.split('/')
            .count()
            .cmp(&right.split('/').count())
            .then_with(|| left.cmp(right))
    });
    ordered.dedup();

    let mut compacted: Vec<String> = Vec::with_capacity(ordered.len());
    for path in ordered {
        if path.len() > 4096 {
            return Err("selective snapshot path exceeds 4096 bytes".into());
        }
        let segments = path.split('/').collect::<Vec<_>>();
        if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
            return Err(format!("invalid selective snapshot path: {path:?}"));
        }
        if !snapshot::SYNCED_SERVICES.contains(&segments[0]) {
            return Err(format!(
                "selective snapshot path is outside a synced service: {path}"
            ));
        }
        if compacted.iter().any(|ancestor| {
            path.len() > ancestor.len()
                && path.starts_with(ancestor)
                && path.as_bytes().get(ancestor.len()) == Some(&b'/')
        }) {
            continue;
        }
        compacted.push(path);
    }
    Ok(compacted)
}

fn shallow_snapshot_node(node: &Value) -> Value {
    let mut node = node.clone();
    if let Some(object) = node.as_object_mut() {
        object.insert("properties".into(), Value::Object(Map::new()));
        object.insert("children".into(), Value::Array(Vec::new()));
    }
    node
}

fn build_selective_snapshot(root: &Path, paths: &[String]) -> Result<Value, String> {
    let selected_paths = compact_selected_paths(paths)?;
    let services = snapshot::emit_services(root)
        .map_err(|error| format!("selective snapshot scan {}: {error}", root.display()))?;
    let disk_nodes = diff::collect_local_snapshot_values(&services);
    let mut ops = Vec::new();
    let mut emitted_ancestors = BTreeSet::new();

    for path in &selected_paths {
        let segments = path.split('/').map(str::to_string).collect::<Vec<_>>();
        if let Some(node) = disk_nodes.get(path) {
            // A selected child may live below a disk-only parent that does not
            // exist in Studio yet. Create only the container shells required
            // to reach it; empty properties ensure an unselected ancestor's
            // script source is never overwritten as a side effect.
            for depth in 2..segments.len() {
                let ancestor_path = segments[..depth].join("/");
                let Some(ancestor) = disk_nodes.get(&ancestor_path) else {
                    continue;
                };
                if !emitted_ancestors.insert(ancestor_path) {
                    continue;
                }
                ops.push(json!({
                    "op": "ensure",
                    "path": segments[..depth - 1].to_vec(),
                    "node": shallow_snapshot_node(ancestor),
                    "strict": false,
                    "forcePrune": false,
                }));
            }
            ops.push(json!({
                "op": "set",
                "path": segments[..segments.len() - 1].to_vec(),
                "node": node,
                "strict": true,
                "forcePrune": true,
            }));
        } else {
            // The selected path exists only in Studio. Applying the disk state
            // therefore means removing that synced instance from Studio.
            ops.push(json!({
                "op": "delete",
                "path": segments,
                "forcePrune": true,
            }));
        }
    }

    Ok(json!({
        "ops": ops,
        "strict": true,
        "forcePrune": true,
        "selectedPaths": selected_paths,
    }))
}

async fn selective_snapshot(
    State(state): State<AppState>,
    Json(body): Json<SelectiveSnapshotBody>,
) -> Json<Value> {
    if body.plugin_protocol != Some(crate::ws::PLUGIN_PROTOCOL_VERSION) {
        return Json(json!({
            "ok": false,
            "error": format!(
                "incompatible Studio plugin protocol; expected {}. Reinstall the Studio plugin.",
                crate::ws::PLUGIN_PROTOCOL_VERSION
            ),
        }));
    }
    match build_selective_snapshot(state.canonical_project.as_path(), &body.paths) {
        Ok(payload) => Json(payload),
        Err(error) => Json(json!({ "ok": false, "error": error })),
    }
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
    strict: bool,
    #[serde(rename = "forcePrune", default)]
    force_prune: bool,
    #[serde(default)]
    services: Vec<Value>,
    #[serde(rename = "pluginProtocol", default)]
    plugin_protocol: Option<u64>,
}

async fn push(State(state): State<AppState>, Json(body): Json<PushBody>) -> Json<Value> {
    if body.bootstrap && body.plugin_protocol != Some(crate::ws::PLUGIN_PROTOCOL_VERSION) {
        return Json(json!({
            "ok": false,
            "applied": 0,
            "skipped": 0,
            "conflicts": [],
            "errors": [format!(
                "incompatible Studio plugin protocol; expected {}. Reinstall the Studio plugin.",
                crate::ws::PLUGIN_PROTOCOL_VERSION
            )],
        }));
    }
    let root = state.canonical_project.as_path();
    let ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: false,
        strict: false,
        force_prune: false,
    };
    let mut res = PushApplyResult::default();

    if body.bootstrap {
        let bootstrap_ctx = PushCtx {
            conflicts: state.conflict.as_ref(),
            push_quiet: state.push_quiet.as_ref(),
            force_overwrite: true,
            strict: body.strict,
            force_prune: body.force_prune,
        };
        for svc in &body.services {
            match apply_service_node(root, svc, &bootstrap_ctx) {
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
        force_overwrite: false,
        strict: false,
        force_prune: false,
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
    pub force_overwrite: bool,
    pub strict: bool,
    pub force_prune: bool,
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
    let mut rx = state.events.subscribe();
    let root = state.canonical_project.as_path();
    let mut out: Vec<Value> = Vec::new();

    // Wait up to 30s for the first conflict-filtered op, then drain anything
    // else that arrived within a brief coalesce window so bursts go together.
    let first = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Some(op) = event_to_plugin_op(root, &event) {
                        return Some(op);
                    }
                }
                Err(_) => return None,
            }
        }
    })
    .await;
    match first {
        Ok(Some(op)) => out.push(op),
        Ok(None) => {}
        Err(_) => {
            // Timeout — return empty, plugin re-polls immediately.
            return Json(json!({ "ok": true, "ops": out }));
        }
    }

    // Brief drain window.
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        if let Some(op) = event_to_plugin_op(root, &event) {
            out.push(op);
        }
    }

    Json(json!({ "ok": true, "ops": out }))
}

pub(crate) fn event_to_plugin_op(root: &Path, event: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(event).ok()?;
    if value.get("type").and_then(Value::as_str) == Some("plugin-op") {
        return value.get("op").cloned();
    }
    if value.get("type").and_then(Value::as_str) != Some("op") {
        return None;
    }
    let op: Op = serde_json::from_value(value.get("op")?.clone()).ok()?;
    fs_op_to_plugin_op(root, &op)
}

fn broadcast_filtered_op(events: &broadcast::Sender<String>, op: &Op) -> Result<(), String> {
    let payload = serde_json::to_string(&json!({ "type": "op", "op": op }))
        .map_err(|error| format!("serialize op: {error}"))?;
    events
        .send(payload)
        .map(|_| ())
        .map_err(|_| "no connected client can receive the resolved conflict".to_string())
}

fn broadcast_plugin_op(events: &broadcast::Sender<String>, op: Value) -> Result<(), String> {
    let payload = serde_json::to_string(&json!({ "type": "plugin-op", "op": op }))
        .map_err(|error| format!("serialize plugin op: {error}"))?;
    events
        .send(payload)
        .map(|_| ())
        .map_err(|_| "no connected client can receive the resolved conflict".to_string())
}

fn deliver_prepared_rename(
    events: &broadcast::Sender<String>,
    rename: &Value,
    retained: &[Value],
) -> Result<usize, (String, usize)> {
    deliver_prepared_rename_with(rename, retained, |op| {
        broadcast_plugin_op(events, op.clone())
    })
}

fn deliver_prepared_rename_with<F>(
    rename: &Value,
    retained: &[Value],
    mut deliver: F,
) -> Result<usize, (String, usize)>
where
    F: FnMut(&Value) -> Result<(), String>,
{
    let mut delivered = 0usize;
    for op in std::iter::once(rename).chain(retained.iter()) {
        if let Err(error) = deliver(op) {
            return Err((error, delivered));
        }
        delivered += 1;
    }
    Ok(delivered)
}

fn compensate_studio_rename(
    events: &broadcast::Sender<String>,
    root: &Path,
    applied_rename: &Value,
    from: &Path,
    to: &Path,
    conflict_path: &Path,
    studio_bytes: &[u8],
) -> bool {
    if applied_rename.get("op").and_then(Value::as_str) != Some("rename") {
        // Class-changing renames require reconstructing the original class and
        // cannot be safely guessed after a partial apply.
        return false;
    }
    let Some(rename_from) = applied_rename.get("from").cloned() else {
        return false;
    };
    let Some(rename_to) = applied_rename.get("to").cloned() else {
        return false;
    };

    let destination_conflict = if conflict_path == from {
        to.to_path_buf()
    } else if let Ok(suffix) = conflict_path.strip_prefix(from) {
        to.join(suffix)
    } else {
        return false;
    };
    let Ok(relative) = destination_conflict.strip_prefix(root) else {
        return false;
    };
    let segments = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(String::from))
        .collect::<Vec<_>>();
    let Some(destination_lookup) = segs_to_lookup_path(&segments) else {
        return false;
    };
    let Ok(studio_source) = std::str::from_utf8(studio_bytes) else {
        return false;
    };

    let restore_source = json!({
        "op": "update",
        "path": destination_lookup,
        "properties": { "Source": studio_source },
    });
    let reverse_rename = json!({
        "op": "rename",
        "from": rename_to,
        "to": rename_from,
    });
    broadcast_plugin_op(events, restore_source).is_ok()
        && broadcast_plugin_op(events, reverse_rename).is_ok()
}

fn mark_conflict_resolution_quiet(state: &AppState, path: &Path) {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let deadline = Instant::now() + Duration::from_millis(PUSH_QUIET_MS);
    state.push_quiet.lock().unwrap().insert(canon, deadline);
}

fn audit_conflict_resolution(action: &str, fields: Value) {
    let mut entry = json!({
        "source": "filesystem-sync-conflict",
        "action": action,
        "outcome": "resolved",
    });
    if let (Some(entry), Some(fields)) = (entry.as_object_mut(), fields.as_object()) {
        entry.extend(fields.clone());
    }
    let _ = write_log_entry(Json(entry));
}

fn conflict_swap_path(parent: &Path, label: &str) -> Result<PathBuf, String> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    use std::sync::atomic::Ordering;
    for _ in 0..64 {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".rosync-conflict-{label}-{}-{sequence}.swp",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not allocate conflict rollback path in {}",
        parent.display()
    ))
}

fn write_conflict_temp(parent: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write as _;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create conflict temp parent {}: {error}", parent.display()))?;
    for _ in 0..64 {
        let path = conflict_swap_path(parent, "write")?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("write conflict temp {}: {error}", path.display()));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("create conflict temp {}: {error}", path.display()));
            }
        }
    }
    Err(format!(
        "could not create conflict temp file in {}",
        parent.display()
    ))
}

fn restore_fs_rename_transactional(
    from: &Path,
    to: &Path,
    conflict_path: &Path,
    studio_bytes: &[u8],
) -> Result<(), String> {
    restore_fs_rename_transactional_with(from, to, conflict_path, studio_bytes, |from, to| {
        std::fs::rename(from, to)
    })
}

fn restore_fs_deleted_source(path: &Path, studio_bytes: &[u8]) -> Result<(), String> {
    restore_fs_deleted_source_with(path, studio_bytes, |from, to| std::fs::rename(from, to))
}

const DIRECTORY_DELETE_RESTORE_ERROR: &str =
    "cannot safely restore a directory deleted from disk from one conflicted source; no files were written and the conflict remains parked. Restore the full subtree from Studio before resolving it";

fn validate_fs_delete_restore(is_dir: bool) -> Result<(), &'static str> {
    if is_dir {
        Err(DIRECTORY_DELETE_RESTORE_ERROR)
    } else {
        Ok(())
    }
}

fn restore_fs_deleted_source_with<R>(
    path: &Path,
    studio_bytes: &[u8],
    mut rename: R,
) -> Result<(), String>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    if path.exists() {
        return Err(format!(
            "refusing to restore deleted source because {} already exists",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("restored source has no parent: {}", path.display()))?;
    let temporary = write_conflict_temp(parent, studio_bytes)?;
    if path.exists() {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "refusing to restore deleted source because {} appeared during restore",
            path.display()
        ));
    }
    if let Err(error) = rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "install restored source {}: {error}",
            path.display()
        ));
    }
    Ok(())
}

fn restore_fs_rename_transactional_with<R>(
    from: &Path,
    to: &Path,
    conflict_path: &Path,
    studio_bytes: &[u8],
    mut rename: R,
) -> Result<(), String>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    if from.exists() || !to.exists() {
        return Err(format!(
            "restore rename requires only the retained destination to exist (from={}, to={})",
            from.exists(),
            to.exists()
        ));
    }
    let temp_parent = from.parent().ok_or_else(|| {
        format!(
            "restore rename has no parent for original path {}",
            from.display()
        )
    })?;
    let write_temp = write_conflict_temp(temp_parent, studio_bytes)?;

    if let Err(error) = rename(to, from) {
        let _ = std::fs::remove_file(&write_temp);
        return Err(format!(
            "restore rename {} -> {}: {error}",
            to.display(),
            from.display()
        ));
    }

    if let Some(parent) = conflict_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            let rollback = rename(from, to);
            let _ = std::fs::remove_file(&write_temp);
            return Err(format!(
                "create restored source parent {}: {error}; directory rollback: {}",
                parent.display(),
                rollback
                    .map(|_| "ok".to_string())
                    .unwrap_or_else(|rollback| rollback.to_string())
            ));
        }
    }

    let backup = if conflict_path.exists() {
        let parent = conflict_path.parent().unwrap_or(temp_parent);
        let backup = match conflict_swap_path(parent, "backup") {
            Ok(backup) => backup,
            Err(error) => {
                let rollback = rename(from, to);
                let _ = std::fs::remove_file(&write_temp);
                return Err(format!(
                    "{error}; directory rollback: {}",
                    rollback
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|rollback| rollback.to_string())
                ));
            }
        };
        if let Err(error) = rename(conflict_path, &backup) {
            let rollback = rename(from, to);
            let _ = std::fs::remove_file(&write_temp);
            return Err(format!(
                "backup restored source {}: {error}; directory rollback: {}",
                conflict_path.display(),
                rollback
                    .map(|_| "ok".to_string())
                    .unwrap_or_else(|rollback| rollback.to_string())
            ));
        }
        Some(backup)
    } else {
        None
    };

    if let Err(error) = rename(&write_temp, conflict_path) {
        let source_rollback = backup.as_ref().map(|backup| rename(backup, conflict_path));
        let directory_rollback = rename(from, to);
        let _ = std::fs::remove_file(&write_temp);
        return Err(format!(
            "install restored Studio source {}: {error}; source rollback: {}; directory rollback: {}",
            conflict_path.display(),
            source_rollback
                .map(|result| result.map(|_| "ok".to_string()).unwrap_or_else(|error| error.to_string()))
                .unwrap_or_else(|| "not-needed".to_string()),
            directory_rollback
                .map(|_| "ok".to_string())
                .unwrap_or_else(|error| error.to_string())
        ));
    }

    if let Some(backup) = backup {
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

fn collect_tree_update_ops(path: &Path, out: &mut Vec<Op>) -> Result<(), String> {
    let is_dir = path.is_dir();
    let content = if is_dir {
        None
    } else {
        Some(std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?)
    };
    out.push(Op {
        kind: OpKind::Update,
        path: path.to_path_buf(),
        from: None,
        content,
    });

    if !is_dir {
        return Ok(());
    }

    let mut children = std::fs::read_dir(path)
        .map_err(|error| format!("read dir {}: {error}", path.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("read dir {}: {error}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_tree_update_ops(&child, out)?;
            continue;
        }
        let Some(name) = child.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if classify_script_file(name).is_some() || is_init_file(name) {
            collect_tree_update_ops(&child, out)?;
        }
    }
    Ok(())
}

async fn wait_for_source_acks(
    conflicts: &crate::conflict::ConflictEngine,
    ops: &[Op],
    timeout: Duration,
) -> bool {
    let expected: Vec<(&Path, Vec<u8>)> = ops
        .iter()
        .filter_map(|op| {
            op.content.as_deref().map(|content| {
                (
                    op.path.as_path(),
                    normalize_line_endings(content).into_owned(),
                )
            })
        })
        .collect();
    if expected.is_empty() {
        return true;
    }

    let deadline = Instant::now() + timeout;
    loop {
        if expected
            .iter()
            .all(|(path, content)| conflicts.matches_baseline(path, content))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn restore_resolved_conflict(
    state: &AppState,
    target: &Path,
    bytes: Vec<u8>,
    is_dir: bool,
    rejected_studio: Option<Vec<u8>>,
) {
    if let Some(studio_bytes) = rejected_studio {
        state
            .conflict
            .park_studio_update(target, bytes, studio_bytes, fs_mtime(target));
    } else {
        state
            .conflict
            .park_studio_delete(target, bytes, fs_mtime(target), is_dir);
    }
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

async fn resolve_list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "conflicts": state.conflict.list(),
    }))
}

#[derive(Deserialize)]
struct ResolveBody {
    path: String,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    choice: Option<String>,
}

fn parse_resolution(raw: &str) -> Result<Resolution, String> {
    match raw {
        "keep-local" | "keep-disk" | "keep_disk" | "keep_fs" | "fs" | "local" | "disk" => {
            Ok(Resolution::KeepLocal)
        }
        "keep-studio" | "keep_studio" | "studio" => Ok(Resolution::KeepStudio),
        other => Err(format!("unknown resolution: {other}")),
    }
}

fn resolve_conflict_target(project: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        project.join(path)
    }
}

async fn resolve(
    State(state): State<AppState>,
    Json(body): Json<ResolveBody>,
) -> impl IntoResponse {
    let raw = body.resolution.or(body.choice).unwrap_or_default();
    let resolution = match parse_resolution(&raw) {
        Ok(resolution) => resolution,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": error,
            }));
        }
    };

    let target = resolve_conflict_target(&state.canonical_project, &body.path);
    if resolution == Resolution::KeepLocal && state.active_plugin.lock().unwrap().is_none() {
        return Json(json!({
            "ok": false,
            "error": "cannot keep local while the Studio plugin is disconnected",
            "path": body.path,
        }));
    }
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
            {
                let canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
                let deadline = Instant::now() + Duration::from_millis(PUSH_QUIET_MS);
                state.push_quiet.lock().unwrap().insert(canon, deadline);
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
        Resolved::PushStudio {
            bytes,
            is_dir,
            rejected_studio,
        } => {
            let ops = if is_dir {
                let mut ops = Vec::new();
                if let Err(error) = collect_tree_update_ops(&target, &mut ops) {
                    state
                        .conflict
                        .park_studio_delete(&target, bytes, fs_mtime(&target), true);
                    return Json(json!({ "ok": false, "error": error }));
                }
                ops
            } else {
                vec![Op {
                    kind: OpKind::Update,
                    path: target.clone(),
                    from: None,
                    content: Some(bytes.clone()),
                }]
            };
            let delivery = ops
                .iter()
                .try_for_each(|op| broadcast_filtered_op(&state.events, op));
            if let Err(error) = delivery {
                restore_resolved_conflict(&state, &target, bytes, is_dir, rejected_studio);
                return Json(json!({ "ok": false, "error": error }));
            }
            if !wait_for_source_acks(state.conflict.as_ref(), &ops, Duration::from_secs(5)).await {
                restore_resolved_conflict(&state, &target, bytes, is_dir, rejected_studio);
                return Json(json!({
                    "ok": false,
                    "error": "Studio did not acknowledge the resolved source; conflict remains parked",
                }));
            }
            Json(json!({ "ok": true, "action": "pushed-studio", "path": body.path }))
        }
        Resolved::DeleteFs { bytes, is_dir } => {
            {
                let canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
                let deadline = Instant::now() + Duration::from_millis(PUSH_QUIET_MS);
                state.push_quiet.lock().unwrap().insert(canon, deadline);
            }
            let result = if target.is_dir() {
                std::fs::remove_dir_all(&target)
            } else {
                std::fs::remove_file(&target)
            };
            if let Err(error) = result {
                state
                    .conflict
                    .park_studio_delete(&target, bytes, fs_mtime(&target), is_dir);
                return Json(json!({ "ok": false, "error": format!("delete: {error}") }));
            }
            state.conflict.forget_path(&target);
            Json(json!({ "ok": true, "action": "deleted-fs", "path": body.path }))
        }
        Resolved::DeleteStudio {
            path,
            conflict_path,
            studio_bytes,
            is_dir,
        } => {
            let op = Op {
                kind: OpKind::Delete,
                path: path.clone(),
                from: None,
                content: None,
            };
            if fs_op_to_plugin_op(state.canonical_project.as_path(), &op).is_none() {
                state
                    .conflict
                    .park_fs_delete_conflict(&conflict_path, &path, studio_bytes, is_dir);
                return Json(json!({
                    "ok": false,
                    "error": format!("cannot map disk delete {} to a Studio path", path.display()),
                }));
            }
            if let Err(error) = broadcast_filtered_op(&state.events, &op) {
                state
                    .conflict
                    .park_fs_delete_conflict(&conflict_path, &path, studio_bytes, is_dir);
                return Json(json!({ "ok": false, "error": error }));
            }
            state.conflict.commit_fs_delete(&path);
            audit_conflict_resolution(
                "delete-studio",
                json!({ "path": path, "resolution": "keep-disk" }),
            );
            Json(json!({
                "ok": true,
                "action": "deleted-studio",
                "path": body.path,
            }))
        }
        Resolved::RenameStudio {
            from,
            to,
            is_dir,
            conflict_path,
            studio_bytes,
            local_bytes,
        } => {
            let rename = Op {
                kind: OpKind::Rename,
                path: to.clone(),
                from: Some(from.clone()),
                content: None,
            };
            let mut ops = Vec::new();
            if let Err(error) = collect_tree_update_ops(&to, &mut ops) {
                state.conflict.park_fs_rename_conflict(
                    &conflict_path,
                    &from,
                    &to,
                    local_bytes,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({ "ok": false, "error": error }));
            }
            let mut retained_plugin_ops = Vec::with_capacity(ops.len());
            for op in &ops {
                let Some(plugin_op) = fs_op_to_plugin_op(state.canonical_project.as_path(), op)
                else {
                    state.conflict.park_fs_rename_conflict(
                        &conflict_path,
                        &from,
                        &to,
                        local_bytes,
                        studio_bytes,
                        is_dir,
                    );
                    return Json(json!({
                        "ok": false,
                        "error": format!(
                            "cannot map retained rename source {} to Studio",
                            op.path.display()
                        ),
                    }));
                };
                retained_plugin_ops.push(plugin_op);
            }
            let Some(plugin_op) = fs_op_to_plugin_op(state.canonical_project.as_path(), &rename)
            else {
                state.conflict.park_fs_rename_conflict(
                    &conflict_path,
                    &from,
                    &to,
                    local_bytes,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({
                    "ok": false,
                    "error": format!(
                        "cannot map disk rename {} -> {} to Studio paths",
                        from.display(),
                        to.display()
                    ),
                }));
            };
            // Every retained source was read and translated before the first
            // Studio mutation. Rename first, then re-apply the destination
            // tree so Keep Disk means both name and source win.
            if let Err((error, delivered)) =
                deliver_prepared_rename(&state.events, &plugin_op, &retained_plugin_ops)
            {
                let compensated = delivered > 0
                    && compensate_studio_rename(
                        &state.events,
                        state.canonical_project.as_path(),
                        &plugin_op,
                        &from,
                        &to,
                        &conflict_path,
                        &studio_bytes,
                    );
                state.conflict.park_fs_rename_conflict(
                    &conflict_path,
                    &from,
                    &to,
                    local_bytes,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({
                    "ok": false,
                    "error": format!(
                        "{error} after {delivered} queued op(s); Studio rename compensation {}",
                        if compensated { "was queued" } else { "was unavailable" }
                    ),
                }));
            }
            if !wait_for_source_acks(state.conflict.as_ref(), &ops, Duration::from_secs(5)).await {
                let compensated = compensate_studio_rename(
                    &state.events,
                    state.canonical_project.as_path(),
                    &plugin_op,
                    &from,
                    &to,
                    &conflict_path,
                    &studio_bytes,
                );
                state.conflict.park_fs_rename_conflict(
                    &conflict_path,
                    &from,
                    &to,
                    local_bytes,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({
                    "ok": false,
                    "error": format!(
                        "Studio did not acknowledge retained disk source after rename; compensation {}",
                        if compensated { "was queued" } else { "was unavailable" }
                    ),
                }));
            }
            state.conflict.forget_path(&from);
            audit_conflict_resolution(
                "rename-studio",
                json!({
                    "from": from,
                    "to": to,
                    "isDirectory": is_dir,
                    "resolution": "keep-disk",
                }),
            );
            Json(json!({
                "ok": true,
                "action": "renamed-studio",
                "path": body.path,
            }))
        }
        Resolved::RestoreFsDelete {
            delete_root,
            conflict_path,
            studio_bytes,
            is_dir,
        } => {
            if let Err(error) = validate_fs_delete_restore(is_dir) {
                let _ = write_log_entry(Json(json!({
                    "source": "filesystem-sync-conflict",
                    "action": "restore-disk-delete",
                    "deleteRoot": &delete_root,
                    "path": &conflict_path,
                    "resolution": "keep-studio",
                    "outcome": "blocked-directory-restore",
                    "error": error,
                })));
                state.conflict.park_fs_delete_conflict(
                    &conflict_path,
                    &delete_root,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({
                    "ok": false,
                    "code": "DIRECTORY_DELETE_RESTORE_REQUIRES_STUDIO_PULL",
                    "error": error,
                    "conflictRemains": true,
                }));
            }
            mark_conflict_resolution_quiet(&state, &conflict_path);
            if let Err(error) = restore_fs_deleted_source(&conflict_path, &studio_bytes) {
                state.conflict.park_fs_delete_conflict(
                    &conflict_path,
                    &delete_root,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({ "ok": false, "error": error }));
            }
            state.conflict.record_sync(
                &conflict_path,
                hash(&studio_bytes),
                fs_mtime(&conflict_path),
            );
            mark_conflict_resolution_quiet(&state, &conflict_path);
            audit_conflict_resolution(
                "restore-disk-delete",
                json!({
                    "deleteRoot": delete_root,
                    "path": conflict_path,
                    "resolution": "keep-studio",
                }),
            );
            Json(json!({
                "ok": true,
                "action": "restored-fs",
                "path": body.path,
            }))
        }
        Resolved::RestoreFsRename {
            from,
            to,
            conflict_path,
            studio_bytes,
            is_dir,
            local_bytes,
        } => {
            let repark = || {
                if from.exists() && !to.exists() {
                    state.conflict.park_studio_update(
                        &conflict_path,
                        local_bytes.clone(),
                        studio_bytes.clone(),
                        fs_mtime(&conflict_path),
                    );
                } else {
                    state.conflict.park_fs_rename_conflict(
                        &conflict_path,
                        &from,
                        &to,
                        local_bytes.clone(),
                        studio_bytes.clone(),
                        is_dir,
                    );
                }
            };
            mark_conflict_resolution_quiet(&state, &from);
            mark_conflict_resolution_quiet(&state, &to);
            if let Err(error) =
                restore_fs_rename_transactional(&from, &to, &conflict_path, &studio_bytes)
            {
                repark();
                return Json(json!({ "ok": false, "error": error }));
            }
            state.conflict.record_sync(
                &conflict_path,
                hash(&studio_bytes),
                fs_mtime(&conflict_path),
            );
            mark_conflict_resolution_quiet(&state, &from);
            mark_conflict_resolution_quiet(&state, &to);
            mark_conflict_resolution_quiet(&state, &conflict_path);
            audit_conflict_resolution(
                "restore-disk-rename",
                json!({
                    "from": from,
                    "to": to,
                    "path": conflict_path,
                    "resolution": "keep-studio",
                }),
            );
            Json(json!({
                "ok": true,
                "action": "restored-fs-rename",
                "path": body.path,
            }))
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

struct ChildAssignment<'a> {
    node: &'a Value,
    fragment: String,
    fallback_by_name: bool,
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
            apply_delete(root, &segs, ctx)
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
        "" => Err("op missing kind".to_string()),
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
    let children = node
        .get("children")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let wanted = wanted_child_names_for_prune(&children);
    for child in child_fragment_assignments(&children) {
        if let ApplyOutcome::Applied(k) = apply_set_in_dir(
            &svc_dir,
            child.node,
            ctx,
            Some((&child.fragment, child.fallback_by_name)),
        )? {
            n += k
        }
    }
    if ctx.strict && ctx.force_prune {
        n += prune_dir_to_names(&svc_dir, &wanted, false, ctx)?;
    }
    Ok(n)
}

fn apply_set(
    root: &Path,
    parent_segs: &[String],
    node: &Value,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    let parent_dir = resolve_segments_to_dir(root, parent_segs)?;
    apply_set_in_dir(&parent_dir, node, ctx, None)
}

fn apply_set_in_dir(
    parent_dir: &Path,
    node: &Value,
    ctx: &PushCtx<'_>,
    preferred_fragment: Option<(&str, bool)>,
) -> Result<ApplyOutcome, String> {
    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(ApplyOutcome::Skipped);
    }
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
    let children = node
        .get("children")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let has_children = !children.is_empty();
    if class == "Folder" && !has_children {
        return Ok(ApplyOutcome::Skipped);
    }
    std::fs::create_dir_all(parent_dir)
        .map_err(|e| format!("mkdir {}: {e}", parent_dir.display()))?;

    // If a node with this name already exists on disk, reuse its path; otherwise
    // compute a fresh fragment.
    let mut existing = match preferred_fragment {
        Some((fragment, fallback_by_name)) => {
            if parent_dir.join(fragment).exists() {
                Some(fragment.to_string())
            } else if fallback_by_name {
                find_child_fragment_by_name(parent_dir, name).map_err(|e| e.to_string())?
            } else {
                None
            }
        }
        None => find_child_fragment_by_name(parent_dir, name).map_err(|e| e.to_string())?,
    };
    if let Some(fragment) = existing.as_deref() {
        let existing_path = parent_dir.join(fragment);
        if !existing_fragment_compatible(&existing_path, class, has_children) {
            if ctx.force_overwrite {
                remove_path_for_replace(&existing_path, ctx)?;
                existing = None;
            } else {
                return Ok(ApplyOutcome::Skipped);
            }
        }
    }
    let taken = siblings_except(parent_dir, existing.as_deref())?;

    let frag = match &existing {
        Some(f) => {
            let p = parent_dir.join(f);
            let is_dir = p.is_dir();
            crate::fs_map::PathFragment {
                fragment: f.clone(),
                is_dir,
            }
        }
        None => match preferred_fragment {
            Some((fragment, _)) => crate::fs_map::PathFragment {
                fragment: fragment.to_string(),
                is_dir: class == "Folder" || has_children,
            },
            None => instance_to_path(
                &InstanceDescriptor {
                    class,
                    name,
                    has_children,
                },
                &taken,
            ),
        },
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
    let wanted = wanted_child_names_for_prune(&children);

    match (sc, has_children) {
        (Some(_), false) => {
            // Leaf script file. Normalize CRLF→LF so comparisons against FS
            // bytes and cached hashes line up regardless of checkout style.
            let raw_bytes = source.unwrap_or_default().into_bytes();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            match apply_source_bytes(&target, &bytes, ctx)? {
                SourceWriteOutcome::Applied => applied += 1,
                SourceWriteOutcome::Skipped => {}
                SourceWriteOutcome::Conflict(path) => return Ok(ApplyOutcome::Conflict(path)),
            }
        }
        (Some(sc), true) => {
            // Script-with-children directory.
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("mkdir {}: {e}", target.display()))?;
            ctx.mark_quiet(&target);
            let init_name = format!("init ({}){}", encode_name(name), sc.suffix());
            let preferred_init_path = target.join(&init_name);
            let init_path = if preferred_init_path.exists() {
                preferred_init_path
            } else {
                find_existing_init_source(&target, name, sc)?.unwrap_or(preferred_init_path)
            };
            let raw_bytes = source.unwrap_or_default().into_bytes();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            match apply_source_bytes(&init_path, &bytes, ctx)? {
                SourceWriteOutcome::Applied => applied += 1,
                SourceWriteOutcome::Skipped => {}
                SourceWriteOutcome::Conflict(path) => return Ok(ApplyOutcome::Conflict(path)),
            }
            for child in child_fragment_assignments(&children) {
                if let ApplyOutcome::Applied(n) = apply_set_in_dir(
                    &target,
                    child.node,
                    ctx,
                    Some((&child.fragment, child.fallback_by_name)),
                )? {
                    applied += n;
                }
            }
            if ctx.strict && ctx.force_prune {
                applied += prune_dir_to_names(&target, &wanted, true, ctx)?;
            }
        }
        (None, _) => {
            // Folder (the only surviving non-script whitelisted class).
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("mkdir {}: {e}", target.display()))?;
            ctx.mark_quiet(&target);
            for child in child_fragment_assignments(&children) {
                if let ApplyOutcome::Applied(n) = apply_set_in_dir(
                    &target,
                    child.node,
                    ctx,
                    Some((&child.fragment, child.fallback_by_name)),
                )? {
                    applied += n;
                }
            }
            if ctx.strict && ctx.force_prune {
                applied += prune_dir_to_names(&target, &wanted, false, ctx)?;
            }
            applied += 1;
        }
    }
    Ok(ApplyOutcome::Applied(applied))
}

/// Find the source file for an existing script-with-children without assuming
/// it already uses the latest portable filename encoding. Older projects may
/// have a literal-Unicode `init (<Name>)` file; reuse it instead of creating a
/// second encoded init file beside it.
fn find_existing_init_source(
    dir: &Path,
    expected_name: &str,
    expected_class: ScriptClass,
) -> Result<Option<PathBuf>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("read dir {}: {error}", dir.display()))?;
    let mut named_matches = Vec::new();
    let mut plain_match = None;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read dir {}: {error}", dir.display()))?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some((class, name)) = parse_init_file(&file_name) {
            if class == expected_class && name == expected_name {
                named_matches.push(entry.path());
            }
            continue;
        }
        if parse_plain_init_file(&file_name) == Some(expected_class) {
            plain_match = Some(entry.path());
        }
    }
    if named_matches.len() > 1 {
        return Err(format!(
            "multiple init sources in {} map to {}",
            dir.display(),
            expected_name
        ));
    }
    Ok(named_matches.pop().or(plain_match))
}

fn child_fragment_assignments(children: &[Value]) -> Vec<ChildAssignment<'_>> {
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for child in children {
        if !node_should_materialize(child) {
            continue;
        }
        if let Some(name) = child.get("name").and_then(|v| v.as_str()) {
            *name_counts.entry(name.to_string()).or_insert(0) += 1;
        }
    }

    let mut relevant = Vec::new();
    for child in children {
        if !node_should_materialize(child) {
            continue;
        }
        let Some(name) = child.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(class) = child.get("class").and_then(|v| v.as_str()) else {
            continue;
        };
        let has_children = child
            .get("children")
            .and_then(|v| v.as_array())
            .is_some_and(|children| !children.is_empty());
        relevant.push((
            diff::snapshot_sibling_sort_key(child, class),
            child,
            name,
            class,
            has_children,
            name_counts.get(name).copied().unwrap_or(0) == 1,
        ));
    }
    relevant.sort_by(|a, b| a.0.cmp(&b.0));

    let mut taken = Vec::new();
    let mut out = Vec::new();
    for (_sort_key, node, name, class, has_children, fallback_by_name) in relevant {
        let fragment = instance_to_path(
            &InstanceDescriptor {
                class,
                name,
                has_children,
            },
            &taken,
        );
        taken.push(fragment.fragment.clone());
        out.push(ChildAssignment {
            node,
            fragment: fragment.fragment,
            fallback_by_name,
        });
    }
    out
}

fn node_should_materialize(node: &Value) -> bool {
    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }

    let class = node.get("class").and_then(|v| v.as_str()).unwrap_or("");
    if !is_scoped_class(class) {
        return false;
    }

    let has_children = node
        .get("children")
        .and_then(|v| v.as_array())
        .map(|children| !children.is_empty())
        .unwrap_or(false);
    class != "Folder" || has_children
}

fn wanted_child_names_for_prune(children: &[Value]) -> Vec<String> {
    children
        .iter()
        .filter(|child| node_should_keep_disk_path(child))
        .filter_map(|child| {
            child
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect()
}

fn node_should_keep_disk_path(node: &Value) -> bool {
    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }

    let class = node.get("class").and_then(|v| v.as_str()).unwrap_or("");
    if !is_scoped_class(class) {
        return false;
    }

    let has_children = node
        .get("children")
        .and_then(|v| v.as_array())
        .map(|children| !children.is_empty())
        .unwrap_or(false);
    class != "Folder" || has_children
}

fn existing_fragment_compatible(path: &Path, class: &str, has_children: bool) -> bool {
    let Ok(Some(inst)) = path_to_instance_meta(path) else {
        return false;
    };
    if class == "Folder" {
        return inst.class == "Folder" && !inst.is_script_with_children;
    }
    if ScriptClass::from_class(class).is_some() {
        if has_children {
            return inst.is_dir && inst.is_script_with_children && inst.class == class;
        }
        return !inst.is_dir && inst.class == class;
    }
    false
}

fn prune_dir_to_names(
    dir: &Path,
    wanted_names: &[String],
    keep_init_files: bool,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir {}: {e}", dir.display()))?;
        let path = entry.path();
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if file_name == META_FILE || file_name == ".DS_Store" {
            continue;
        }
        if is_init_file(&file_name) {
            if keep_init_files {
                continue;
            }
            remove_path_for_replace(&path, ctx)?;
            removed += 1;
            continue;
        }
        let Some(inst) =
            path_to_instance_meta(&path).map_err(|e| format!("scan {}: {e}", path.display()))?
        else {
            continue;
        };
        if wanted_names.iter().any(|wanted| wanted == &inst.name) {
            continue;
        }
        if !disk_path_is_sync_owned(&path) {
            continue;
        }
        remove_path_for_replace(&path, ctx)?;
        removed += 1;
    }
    Ok(removed)
}

fn disk_path_is_sync_owned(path: &Path) -> bool {
    let Ok(Some(inst)) = path_to_instance_meta(path) else {
        return false;
    };
    if inst.script_class.is_some() {
        return true;
    }
    if inst.class == "Folder" && inst.is_dir {
        return folder_contains_sync_owned_path(path);
    }
    false
}

fn folder_contains_sync_owned_path(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if file_name == META_FILE || file_name == ".DS_Store" {
            continue;
        }
        if is_init_file(&file_name) {
            return true;
        }
        if disk_path_is_sync_owned(&path) {
            return true;
        }
    }
    false
}

fn remove_path_for_replace(path: &Path, ctx: &PushCtx<'_>) -> Result<(), String> {
    if path.exists() && (ctx.force_overwrite || (ctx.strict && ctx.force_prune)) {
        backup_forced_removal(path)?;
    }
    mark_quiet_tree(path, ctx);
    if path.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| format!("rmdir {}: {e}", path.display()))?;
    } else if path.exists() {
        std::fs::remove_file(path).map_err(|e| format!("rm {}: {e}", path.display()))?;
    }
    ctx.mark_quiet(path);
    Ok(())
}

fn backup_forced_removal(path: &Path) -> Result<PathBuf, String> {
    let service_dir = path
        .ancestors()
        .find(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| snapshot::SYNCED_SERVICES.contains(&name))
        })
        .ok_or_else(|| {
            format!(
                "refusing destructive write outside a synced service: {}",
                path.display()
            )
        })?;
    let project_root = service_dir
        .parent()
        .ok_or_else(|| format!("cannot locate project root for {}", path.display()))?;
    let relative = path
        .strip_prefix(project_root)
        .map_err(|error| format!("backup path {}: {error}", path.display()))?;
    static BACKUP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = BACKUP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let destination = project_root
        .join(".rosync-backups")
        .join(format!("{stamp}-{sequence}"))
        .join(relative);
    copy_backup_path(path, &destination)?;
    Ok(destination)
}

fn copy_backup_path(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("backup metadata {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing destructive write through symlink {}; move it manually",
            source.display()
        ));
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(destination)
            .map_err(|error| format!("backup mkdir {}: {error}", destination.display()))?;
        let entries = std::fs::read_dir(source)
            .map_err(|error| format!("backup read dir {}: {error}", source.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("backup read dir {}: {error}", source.display()))?;
            copy_backup_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("backup mkdir {}: {error}", parent.display()))?;
        }
        std::fs::copy(source, destination).map_err(|error| {
            format!(
                "backup copy {} to {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn mark_quiet_tree(path: &Path, ctx: &PushCtx<'_>) {
    ctx.mark_quiet(path);
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            mark_quiet_tree(&entry.path(), ctx);
        }
    }
}

enum SourceWriteOutcome {
    Applied,
    Skipped,
    Conflict(PathBuf),
}

fn apply_source_bytes(
    target: &Path,
    bytes: &[u8],
    ctx: &PushCtx<'_>,
) -> Result<SourceWriteOutcome, String> {
    let conflicts = ctx.conflicts;
    if ctx.force_overwrite {
        ctx.mark_quiet(target);
        std::fs::write(target, bytes).map_err(|e| format!("write {}: {e}", target.display()))?;
        conflicts.record_sync(target, hash(bytes), fs_mtime(target));
        ctx.mark_quiet(target);
        return Ok(SourceWriteOutcome::Applied);
    }

    let current = if target.is_file() {
        Some((
            std::fs::read(target).map_err(|e| format!("read {}: {e}", target.display()))?,
            fs_mtime(target),
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
    match conflicts.on_studio_push(target, bytes, current_ref) {
        StudioDecision::Apply => {
            ctx.mark_quiet(target);
            std::fs::write(target, bytes)
                .map_err(|e| format!("write {}: {e}", target.display()))?;
            conflicts.record_sync(target, hash(bytes), fs_mtime(target));
            ctx.mark_quiet(target);
            Ok(SourceWriteOutcome::Applied)
        }
        StudioDecision::NoChange => Ok(SourceWriteOutcome::Skipped),
        StudioDecision::Conflict => Ok(SourceWriteOutcome::Conflict(target.to_path_buf())),
    }
}

fn apply_delete(root: &Path, segs: &[String], ctx: &PushCtx<'_>) -> Result<ApplyOutcome, String> {
    if segs.is_empty() {
        return Err("delete: empty path".into());
    }
    let target = match resolve_segments_to_path(root, segs)? {
        Some(p) => p,
        None => return Ok(ApplyOutcome::Skipped),
    };
    if target.is_dir() && !disk_path_is_sync_owned(&target) {
        return Ok(ApplyOutcome::Skipped);
    }
    if !ctx.force_overwrite && !path_tree_matches_baselines(&target, ctx.conflicts)? {
        let is_dir = target.is_dir();
        let local = if is_dir {
            format!("[directory retained on disk: {}]", target.display()).into_bytes()
        } else {
            std::fs::read(&target).map_err(|e| format!("read {}: {e}", target.display()))?
        };
        ctx.conflicts
            .park_studio_delete(&target, local, fs_mtime(&target), is_dir);
        return Ok(ApplyOutcome::Conflict(target));
    }
    if target.is_dir() {
        std::fs::remove_dir_all(&target).map_err(|e| format!("rmdir {}: {e}", target.display()))?;
    } else if target.is_file() {
        std::fs::remove_file(&target).map_err(|e| format!("rm {}: {e}", target.display()))?;
    }
    ctx.conflicts.forget_path(&target);
    ctx.mark_quiet(&target);
    Ok(ApplyOutcome::Applied(1))
}

fn path_tree_matches_baselines(
    path: &Path,
    conflicts: &crate::conflict::ConflictEngine,
) -> Result<bool, String> {
    if path.is_file() {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        return Ok(conflicts.matches_baseline(path, &normalize_line_endings(&bytes)));
    }
    let entries =
        std::fs::read_dir(path).map_err(|e| format!("read dir {}: {e}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir {}: {e}", path.display()))?;
        let child = entry.path();
        if child.is_dir() {
            if !path_tree_matches_baselines(&child, conflicts)? {
                return Ok(false);
            }
            continue;
        }
        let Some(name) = child.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if classify_script_file(name).is_none() && !is_init_file(name) {
            continue;
        }
        let bytes = std::fs::read(&child).map_err(|e| format!("read {}: {e}", child.display()))?;
        if !conflicts.matches_baseline(&child, &normalize_line_endings(&bytes)) {
            return Ok(false);
        }
    }
    Ok(true)
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
                std::fs::read(&target).map_err(|e| format!("read {}: {e}", target.display()))?,
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
                    ctx.mark_quiet(&target);
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

    let (class, has_children, script_with_children) =
        match path_to_instance_meta(&target).map_err(|e| e.to_string())? {
            Some(inst) => (
                inst.class,
                inst.is_dir && !inst.is_script_with_children
                    || inst.is_script_with_children && children_exist(&target),
                inst.is_script_with_children,
            ),
            None => ("Folder".to_string(), target.is_dir(), false),
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
    rename_path_and_init(&target, &new_path, new_name, script_with_children, ctx)?;
    // The source bytes did not change, but conflict baselines are keyed by
    // filesystem path. Leaving them under the old name makes the next clean
    // Studio edit/delete look like an unknown post-restart divergence. Rebase
    // only after the outer + named-init rename has completed successfully.
    ctx.conflicts.forget_path(&target);
    if new_path.is_dir() {
        seed_script_baselines_in_dir(&new_path, ctx.conflicts)?;
    } else if classify_script_file(
        new_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default(),
    )
    .is_some()
    {
        let bytes = std::fs::read(&new_path)
            .map_err(|error| format!("read renamed source {}: {error}", new_path.display()))?;
        let normalized = normalize_line_endings(&bytes).into_owned();
        ctx.conflicts
            .record_sync(&new_path, hash(&normalized), fs_mtime(&new_path));
    }
    Ok(1)
}

#[derive(Debug)]
struct InitRenamePlan {
    old_name: std::ffi::OsString,
    new_name: String,
}

fn prepare_init_rename(
    dir: &Path,
    new_instance_name: &str,
    script_with_children: bool,
) -> Result<Option<InitRenamePlan>, String> {
    if !script_with_children {
        return Ok(None);
    }
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("read dir {}: {error}", dir.display()))?;
    let mut named = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read dir {}: {error}", dir.display()))?;
        if !entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?
            .is_file()
        {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some((class, _)) = parse_init_file(name) {
            named.push((file_name, class));
        }
    }
    if named.len() > 1 {
        return Err(format!(
            "rename: multiple named init sources found in {}",
            dir.display()
        ));
    }
    let Some((old_name, class)) = named.pop() else {
        // Plain Wally/Rojo `init.lua` roots derive their identity from the
        // directory name and therefore need no inner rename.
        return Ok(None);
    };
    let new_name = format!(
        "init ({}){}",
        encode_name(new_instance_name),
        class.suffix()
    );
    if old_name == std::ffi::OsStr::new(&new_name) {
        return Ok(None);
    }

    Ok(Some(InitRenamePlan { old_name, new_name }))
}

fn init_rename_temp_path(dir: &Path) -> Result<PathBuf, String> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    for _ in 0..32 {
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = dir.join(format!(
            ".rosync-init-rename-{}-{sequence}.tmp",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "rename: could not allocate a temporary init path in {}",
        dir.display()
    ))
}

fn rename_path_and_init(
    target: &Path,
    new_path: &Path,
    new_instance_name: &str,
    script_with_children: bool,
    ctx: &PushCtx<'_>,
) -> Result<(), String> {
    rename_path_and_init_with(
        target,
        new_path,
        new_instance_name,
        script_with_children,
        ctx,
        |from, to| std::fs::rename(from, to),
    )
}

fn rename_path_and_init_with<R>(
    target: &Path,
    new_path: &Path,
    new_instance_name: &str,
    script_with_children: bool,
    ctx: &PushCtx<'_>,
    mut rename: R,
) -> Result<(), String>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let init_plan = prepare_init_rename(target, new_instance_name, script_with_children)?;
    let temp_name = if init_plan.is_some() {
        Some(
            init_rename_temp_path(target)?
                .file_name()
                .ok_or_else(|| {
                    format!("rename: invalid temporary path under {}", target.display())
                })?
                .to_os_string(),
        )
    } else {
        None
    };
    ctx.mark_quiet(target);
    ctx.mark_quiet(new_path);
    rename(target, new_path).map_err(|error| {
        format!(
            "rename {} → {}: {error}",
            target.display(),
            new_path.display()
        )
    })?;

    let Some(init_plan) = init_plan else {
        return Ok(());
    };
    let old_init = new_path.join(&init_plan.old_name);
    let new_init = new_path.join(&init_plan.new_name);
    let temp_init = new_path.join(temp_name.expect("init plan allocates a temporary name"));
    ctx.mark_quiet(&old_init);
    ctx.mark_quiet(&new_init);
    ctx.mark_quiet(&temp_init);

    if let Err(init_error) = rename(&old_init, &temp_init) {
        let rollback = rename(new_path, target);
        return match rollback {
            Ok(()) => Err(format!(
                "rename init {} → {}: {init_error}; outer rename was rolled back",
                old_init.display(),
                new_init.display()
            )),
            Err(rollback_error) => Err(format!(
                "rename init {} → {}: {init_error}; rollback {} → {} also failed: {rollback_error}",
                old_init.display(),
                new_init.display(),
                new_path.display(),
                target.display()
            )),
        };
    }

    // Check only after moving the old file aside. On case-insensitive
    // filesystems a case-only destination aliases the old path and disappears
    // at this point; on case-sensitive filesystems a genuinely distinct
    // destination remains and must never be overwritten.
    if new_init.exists() {
        let restore_init = rename(&temp_init, &old_init);
        let rollback_outer = rename(new_path, target);
        return Err(format!(
            "rename: init destination already exists: {}; init rollback: {}; outer rollback: {}",
            new_init.display(),
            restore_init
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "ok".to_string()),
            rollback_outer
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "ok".to_string())
        ));
    }

    if let Err(init_error) = rename(&temp_init, &new_init) {
        let restore_init = rename(&temp_init, &old_init);
        let rollback_outer = rename(new_path, target);
        if restore_init.is_ok() && rollback_outer.is_ok() {
            return Err(format!(
                "rename init {} → {}: {init_error}; init and outer rename were rolled back",
                old_init.display(),
                new_init.display()
            ));
        }
        return Err(format!(
            "rename init {} → {}: {init_error}; init rollback: {}; outer rollback: {}",
            old_init.display(),
            new_init.display(),
            restore_init
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "ok".to_string()),
            rollback_outer
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "ok".to_string())
        ));
    }
    Ok(())
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
    // Resolve each existing segment before appending a missing one. Rebuilding
    // the whole path after the first miss would discard a legacy literal-
    // Unicode or disambiguated prefix and create a second encoded branch.
    let mut p = root.to_path_buf();
    for seg in segs {
        let next = match find_child_fragment_by_name(&p, seg).map_err(|e| e.to_string())? {
            Some(fragment) => p.join(fragment),
            None => p.join(encode_name(seg)),
        };
        if next.exists() && !next.is_dir() {
            return Err(format!(
                "path {} is a file, not a directory (needed as parent)",
                next.display()
            ));
        }
        p = next;
    }
    Ok(p)
}

/// Scan `dir` for a child whose instance name is `name`. Returns the fragment
/// (file/dir name) if found.
fn find_child_fragment_by_name(dir: &Path, name: &str) -> std::io::Result<Option<String>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut best: Option<(String, u8)> = None;
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
                let priority = fragment_lookup_priority(&e.path(), &i);
                if best.as_ref().map(|(_, p)| priority > *p).unwrap_or(true) {
                    best = Some((fstr.to_string(), priority));
                }
            }
        }
    }
    Ok(best.map(|(fragment, _)| fragment))
}

fn fragment_lookup_priority(path: &Path, inst: &PathInstance) -> u8 {
    if inst.is_script_with_children {
        return 4;
    }
    if inst.script_class.is_some() && !inst.is_dir {
        return 3;
    }
    if inst.class == "Folder" && is_empty_plain_folder(path).unwrap_or(false) {
        return 0;
    }
    if inst.class == "Folder" {
        return 1;
    }
    2
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
                    .map(|n| n != META_FILE && !is_init_file(n))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Filesystem op → plugin op translation
// ---------------------------------------------------------------------------

/// Convert a watcher `Op` into a plugin-facing op (`set` / `delete` / `update` /
/// `rename`). Directories (add/update) produce `set` ops with a minimal node
/// envelope; leaf scripts produce `set` ops carrying `properties.Source`.
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

    if !is_synced_service_segment(&segs[0]) {
        return None;
    }

    match op.kind {
        OpKind::Delete => {
            let target_lookup_segs = segs_to_lookup_path(&segs)?;
            let target_name_segs = segs_to_instance_path(&segs)?;
            if deleted_path_is_shadowed_ignored_folder(root, &segs, &op.path) {
                return None;
            }
            if path_is_avoid_synced(root, &target_name_segs) {
                return None;
            }
            Some(json!({ "op": "delete", "path": target_lookup_segs }))
        }
        OpKind::Rename => {
            if is_empty_plain_folder(&op.path).unwrap_or(false) {
                return None;
            }
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
            if !is_synced_service_segment(&from_segs_fs[0]) {
                return None;
            }
            let from_lookup = segs_to_lookup_path(&from_segs_fs)?;
            let to_naming = segs_to_naming_path(&segs)?;
            let from_name = segs_to_instance_path(&from_segs_fs)?;
            let to_name = segs_to_instance_path(&segs)?;
            if path_is_avoid_synced(root, &from_name) || path_is_avoid_synced(root, &to_name) {
                return None;
            }
            let from_script = script_identity_from_segments(root, &from_segs_fs, from_path);
            let to_script = script_identity_from_segments(root, &segs, &op.path);
            if let (Some((from_lookup_path, _, from_class)), Some((_, to_naming_path, to_class))) =
                (from_script, to_script)
            {
                if from_class != to_class {
                    let source = source_for_path(&op.path, op.content.as_deref())?;
                    return Some(json!({
                        "op": "class_change",
                        "path": from_lookup_path,
                        "to": to_naming_path,
                        "class": to_class,
                        "properties": { "Source": source },
                    }));
                }
            }
            // Two cases the plugin handles with one op:
            //   (a) same-parent rename → just `Instance.Name = last(to_inst)`.
            //   (b) cross-parent move  → reparent + maybe rename.
            Some(json!({
                "op": "rename",
                "from": from_lookup,
                "to": to_naming,
            }))
        }
        OpKind::Add | OpKind::Update => {
            let fname = segs.last()?.clone();
            // Skip init files — they describe their parent dir.
            if is_init_file(&fname) {
                // Translate into an update of the parent dir (Source on the script-with-children).
                let parent_path = op.path.parent()?;
                let parent_inst = path_to_instance_meta(parent_path).ok().flatten()?;
                if let Some(PathInstance {
                    is_script_with_children: true,
                    ..
                }) = Some(&parent_inst).filter(|i| i.is_script_with_children)
                {
                    let parent_segs_fs: Vec<String> = segs[..segs.len() - 1].to_vec();
                    let inst_lookup_segs = segs_to_lookup_path(&parent_segs_fs)?;
                    let inst_naming_segs = segs_to_naming_path(&parent_segs_fs)?;
                    let inst_name_segs = segs_to_instance_path(&parent_segs_fs)?;
                    if path_is_avoid_synced(root, &inst_name_segs) {
                        return None;
                    }
                    let content = op.content.as_deref().unwrap_or(b"");
                    let source = String::from_utf8_lossy(content).to_string();
                    return Some(json!({
                        "op": "class_change",
                        "path": inst_lookup_segs,
                        "to": inst_naming_segs,
                        "class": parent_inst.class,
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
            // map (property sync is Studio-authoritative via live Studio reads).
            let inst = path_to_instance_meta(&op.path).ok().flatten()?;
            if inst.class == "Folder" && is_empty_plain_folder(&op.path).unwrap_or(false) {
                return None;
            }
            let parent_segs_fs: Vec<String> = segs[..segs.len() - 1].to_vec();
            let parent_lookup_segs = segs_to_lookup_path(&parent_segs_fs).unwrap_or_default();
            let parent_name_segs = segs_to_instance_path(&parent_segs_fs).unwrap_or_default();
            let inst_name_segs = segs_to_instance_path(&segs)?;
            if path_is_avoid_synced(root, &parent_name_segs)
                || path_is_avoid_synced(root, &inst_name_segs)
            {
                return None;
            }

            let mut props: Map<String, Value> = Map::new();
            if !inst.is_dir {
                if let Some(bytes) = &op.content {
                    let src = String::from_utf8_lossy(bytes).to_string();
                    props.insert("Source".to_string(), Value::String(src));
                }
            }
            Some(json!({
                "op": "set",
                "path": parent_lookup_segs,
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

fn script_identity_from_segments(
    root: &Path,
    segs: &[String],
    fs_path: &Path,
) -> Option<(Vec<String>, Vec<String>, String)> {
    let fname = segs.last()?;
    if let Some((script_class, _)) = parse_init_file(fname) {
        let parent_segs = &segs[..segs.len().saturating_sub(1)];
        let mut naming_path = if let Some(parent) = fs_path.parent() {
            path_to_instance_meta(parent)
                .ok()
                .flatten()
                .and_then(|inst| {
                    let mut out = segs_to_naming_path(parent_segs)?;
                    let last = out.last_mut()?;
                    *last = inst.name;
                    Some(out)
                })
                .or_else(|| segs_to_naming_path(parent_segs))
        } else {
            segs_to_naming_path(parent_segs)
        }?;
        if let Some(parent) = fs_path.parent() {
            if let Ok(Some(inst)) = path_to_instance_meta(parent) {
                if let Some(last) = naming_path.last_mut() {
                    *last = inst.name;
                }
            }
        }
        return Some((
            segs_to_lookup_path(parent_segs)?,
            naming_path,
            script_class.class_name().to_string(),
        ));
    }

    if let Some((script_class, _)) = classify_script_file(fname) {
        return Some((
            segs_to_lookup_path(segs)?,
            segs_to_naming_path(segs)?,
            script_class.class_name().to_string(),
        ));
    }

    let rel = fs_path.strip_prefix(root).ok()?;
    let rel_segs: Vec<String> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(String::from))
        .collect();
    let inst = path_to_instance_meta(fs_path).ok().flatten()?;
    if inst.script_class.is_some() {
        return Some((
            segs_to_lookup_path(&rel_segs)?,
            segs_to_naming_path(&rel_segs)?,
            inst.class,
        ));
    }
    None
}

fn source_for_path(path: &Path, content: Option<&[u8]>) -> Option<String> {
    if let Some(content) = content {
        return Some(String::from_utf8_lossy(content).to_string());
    }
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .ok()
}

fn deleted_path_is_shadowed_ignored_folder(root: &Path, segs: &[String], path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    let Some(fname) = segs.last() else {
        return false;
    };
    if classify_script_file(fname).is_some() || is_init_file(fname) || fname == META_FILE {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(parent_rel) = parent.strip_prefix(root) else {
        return false;
    };
    if parent_rel.as_os_str().is_empty() || !parent.is_dir() {
        return false;
    }
    let instance_name = match parse_disambiguated(fname) {
        Some((name, _)) => crate::fs_map::decode_name(&name),
        None => crate::fs_map::decode_name(fname),
    };
    let Ok(Some(fragment)) = find_child_fragment_by_name(parent, &instance_name) else {
        return false;
    };
    fragment != *fname
}

fn is_synced_service_segment(segment: &str) -> bool {
    let service_name = match parse_disambiguated(segment) {
        Some((name, _)) => crate::fs_map::decode_name(&name),
        None => crate::fs_map::decode_name(segment),
    };
    snapshot::SYNCED_SERVICES
        .iter()
        .any(|service| *service == service_name)
}

fn path_is_avoid_synced(root: &Path, instance_path: &[String]) -> bool {
    if instance_path.is_empty() {
        return false;
    }
    let avoided = avoid_sync_paths(root);
    avoided
        .iter()
        .any(|path| path.len() <= instance_path.len() && path == &instance_path[..path.len()])
}

fn avoid_sync_paths(root: &Path) -> Vec<Vec<String>> {
    let cache = AVOID_SYNC_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref() {
            if cached.root == root {
                return cached.paths.clone();
            }
        }
    }
    Vec::new()
}

fn collect_avoid_sync_paths(node: &Value, parent: &[String], out: &mut Vec<Vec<String>>) {
    if let Some(nodes) = node.as_array() {
        for child in nodes {
            collect_avoid_sync_paths(child, parent, out);
        }
        return;
    }

    let Some(name) = node.get("name").and_then(|v| v.as_str()) else {
        if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
            for child in children {
                collect_avoid_sync_paths(child, parent, out);
            }
        }
        return;
    };

    let class = node.get("class").and_then(|v| v.as_str()).unwrap_or("");
    let is_data_model_root = parent.is_empty() && class == "DataModel";
    let mut path = parent.to_vec();
    if !is_data_model_root {
        path.push(name.to_string());
    }

    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        out.push(path);
        return;
    }

    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for child in children {
            collect_avoid_sync_paths(child, &path, out);
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

/// Convert filesystem segments to plugin lookup segments. Unlike
/// `segs_to_instance_path`, this preserves generated duplicate suffixes such as
/// `Foo [1]` so the Studio plugin can resolve the exact sibling.
fn segs_to_lookup_path(segs: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(segs.len());
    for (i, s) in segs.iter().enumerate() {
        if i == 0 {
            out.push(fs_segment_instance_name(s));
        } else {
            out.push(fs_segment_lookup_name(s));
        }
    }
    Some(out)
}

/// Convert filesystem segments to a path whose parents are lookup-safe but
/// whose final segment is the actual Roblox instance name. This is used for
/// rename/class-change destinations, where the parent may need disambiguation
/// but the final segment becomes `Instance.Name`.
fn segs_to_naming_path(segs: &[String]) -> Option<Vec<String>> {
    let mut out = segs_to_lookup_path(segs)?;
    if let (Some(last), Some(source_last)) = (out.last_mut(), segs.last()) {
        *last = fs_segment_instance_name(source_last);
    }
    Some(out)
}

fn fs_segment_lookup_name(segment: &str) -> String {
    if let Some((_, stem)) = classify_script_file(segment) {
        crate::fs_map::decode_name(&stem)
    } else {
        crate::fs_map::decode_name(segment)
    }
}

fn fs_segment_instance_name(segment: &str) -> String {
    if let Some((_, stem)) = classify_script_file(segment) {
        let name = match parse_disambiguated(&stem) {
            Some((n, _)) => n,
            None => stem,
        };
        crate::fs_map::decode_name(&name)
    } else {
        let name = match parse_disambiguated(segment) {
            Some((n, _)) => n,
            None => segment.to_string(),
        };
        crate::fs_map::decode_name(&name)
    }
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
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, RwLock};
    use tokio::sync::broadcast;
    use tower::ServiceExt as _;

    struct TempDir(tempfile::TempDir);
    impl TempDir {
        fn new(tag: &str) -> Self {
            TempDir(
                tempfile::Builder::new()
                    .prefix(&format!("rosync-http-{tag}-"))
                    .tempdir()
                    .unwrap(),
            )
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    fn harness<'a>(
        engine: &'a ConflictEngine,
        quiet: &'a Mutex<HashMap<PathBuf, Instant>>,
    ) -> PushCtx<'a> {
        PushCtx {
            conflicts: engine,
            push_quiet: quiet,
            force_overwrite: false,
            strict: false,
            force_prune: false,
        }
    }

    fn force_harness<'a>(
        engine: &'a ConflictEngine,
        quiet: &'a Mutex<HashMap<PathBuf, Instant>>,
    ) -> PushCtx<'a> {
        PushCtx {
            conflicts: engine,
            push_quiet: quiet,
            force_overwrite: true,
            strict: false,
            force_prune: false,
        }
    }

    fn strict_force_harness<'a>(
        engine: &'a ConflictEngine,
        quiet: &'a Mutex<HashMap<PathBuf, Instant>>,
    ) -> PushCtx<'a> {
        PushCtx {
            conflicts: engine,
            push_quiet: quiet,
            force_overwrite: true,
            strict: true,
            force_prune: true,
        }
    }

    fn push_quiet() -> Mutex<HashMap<PathBuf, Instant>> {
        Mutex::new(HashMap::new())
    }

    fn test_state(temp: &TempDir, projects_root: Option<PathBuf>) -> AppState {
        let project = std::fs::canonicalize(temp.path()).unwrap();
        let (events, _) = broadcast::channel::<String>(16);
        let (request_tx, _) = broadcast::channel::<crate::RequestEnvelope>(16);
        let (shutdown_tx, _) = tokio::sync::watch::channel::<Option<String>>(None);
        AppState {
            project: Arc::new(project.clone()),
            canonical_project: Arc::new(project.clone()),
            projects_root: Arc::new(projects_root),
            events,
            conflict: Arc::new(ConflictEngine::new()),
            artifacts: crate::artifact::ArtifactStore::new(
                project.join(".rosync-artifacts"),
                8 * 1024 * 1024,
                Duration::from_secs(60),
            )
            .unwrap(),
            project_name: Arc::new(RwLock::new("artifact-test".into())),
            game_id: Arc::new(RwLock::new(None)),
            group_id: Arc::new(RwLock::new(None)),
            place_ids: Arc::new(RwLock::new(Vec::new())),
            wally_enabled: Arc::new(RwLock::new(false)),
            wally_folder: Arc::new(RwLock::new(None)),
            pending_initial: Arc::new(Mutex::new(None)),
            push_quiet: Arc::new(Mutex::new(HashMap::new())),
            request_tx,
            pending_routes: Arc::new(Mutex::new(HashMap::new())),
            active_plugin: Arc::new(Mutex::new(None)),
            widget_owned: true,
            managed: true,
            managed_by: Arc::new("test-widget".into()),
            boot_id: Arc::new("test-boot".into()),
            listen_port: 0,
            process_id: std::process::id(),
            started_at: 1,
            manager_owner_token: Arc::new(Some("artifact-widget-token".into())),
            manager_last_seen: Arc::new(Mutex::new(None)),
            widget_owner_token: Arc::new(Some("artifact-widget-token".into())),
            widget_last_seen: Arc::new(Mutex::new(None)),
            shutdown_tx,
        }
    }

    fn artifact_test_app(temp: &TempDir) -> Router {
        router(test_state(temp, None))
    }

    async fn artifact_json_request(
        app: &Router,
        method: Method,
        uri: &str,
        body: Value,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap();
        (status, headers, body)
    }

    #[tokio::test]
    async fn hello_advertises_only_a_canonical_configured_projects_root() {
        let project = TempDir::new("project-init-hello-project");
        let projects = TempDir::new("project-init-hello-root");
        let canonical_projects = std::fs::canonicalize(projects.path()).unwrap();
        let app = router(test_state(&project, Some(canonical_projects.clone())));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/hello")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let hello: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(hello["projectInit"]["available"], true);
        assert_eq!(hello["projectInit"]["endpoint"], "/projects/init");
        assert_eq!(
            hello["projectInit"]["projectsRoot"],
            canonical_projects.display().to_string()
        );

        let disabled = router(test_state(&project, None));
        let response = disabled
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/hello")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let hello: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(hello["projectInit"]["available"], false);
        assert!(hello["projectInit"].get("projectsRoot").is_none());
    }

    #[tokio::test]
    async fn project_init_requires_availability_and_the_advertised_plugin_capability() {
        let project = TempDir::new("project-init-auth-project");
        let projects = TempDir::new("project-init-auth-root");
        let request = json!({
            "pluginCapability": "0".repeat(64),
            "gameName": "Race Stars",
            "placeName": "Main Place",
            "gameId": "123",
            "placeId": "456",
        });

        let disabled = router(test_state(&project, None));
        let (_, _, body) =
            artifact_json_request(&disabled, Method::POST, "/projects/init", request.clone()).await;
        assert_eq!(body["error"]["code"], "PROJECT_INIT_UNAVAILABLE");

        let enabled = router(test_state(
            &project,
            Some(std::fs::canonicalize(projects.path()).unwrap()),
        ));
        let (_, _, body) =
            artifact_json_request(&enabled, Method::POST, "/projects/init", request).await;
        assert_eq!(body["error"]["code"], "UNAUTHORIZED");
        assert!(std::fs::read_dir(projects.path()).unwrap().next().is_none());
    }

    #[test]
    fn project_init_creates_once_broadcasts_and_audits_without_exposing_capability() {
        let _guard = WRITELOG_ENV_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_test_home = std::env::var_os("ROSYNC_TEST_HOME");
        let fake_home = TempDir::new("project-init-audit-home");
        std::env::set_var("ROSYNC_TEST_HOME", fake_home.path());

        let project = TempDir::new("project-init-route-project");
        let projects = TempDir::new("project-init-route-root");
        let canonical_projects = std::fs::canonicalize(projects.path()).unwrap();
        let state = test_state(&project, Some(canonical_projects.clone()));
        let mut events = state.events.subscribe();
        let request = json!({
            "pluginCapability": crate::ws::plugin_capability(),
            "gameName": "Race Stars",
            "placeName": "Main Place",
            "gameId": "123",
            "placeId": "456",
            "creatorType": "Group",
            "creatorId": "789",
            "groupId": "789",
        });

        let created = project_init_inner(&state, &serde_json::to_vec(&request).unwrap()).0;
        assert_eq!(created["ok"], true);
        assert_eq!(created["status"], "created");
        assert_eq!(created["directoryName"], "race-stars");
        assert_eq!(created["name"], "Race Stars");
        let created_path = PathBuf::from(created["project"].as_str().unwrap());
        assert_eq!(created_path.parent(), Some(canonical_projects.as_path()));
        assert!(created_path
            .join(crate::project_config::CONFIG_FILE)
            .is_file());

        let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "project-init");
        assert_eq!(event["status"], "created");
        assert_eq!(event["name"], "Race Stars");
        assert_eq!(event["metadata"]["gameId"], "123");

        let existing = project_init_inner(&state, &serde_json::to_vec(&request).unwrap()).0;
        assert_eq!(existing["ok"], true);
        assert_eq!(existing["status"], "existing");
        assert_eq!(existing["project"], created["project"]);

        let (log_path, _) = writes_log_paths(fake_home.path());
        let audit = std::fs::read_to_string(log_path).unwrap();
        assert!(audit.contains("\"action\":\"project-init\""));
        assert!(!audit.contains(crate::ws::plugin_capability()));

        if let Some(previous) = previous_test_home {
            std::env::set_var("ROSYNC_TEST_HOME", previous);
        } else {
            std::env::remove_var("ROSYNC_TEST_HOME");
        }
    }

    async fn create_artifact_lease(
        app: &Router,
        filename: &str,
        expected_size: Option<usize>,
    ) -> (String, String) {
        let (_, _, response) = artifact_json_request(
            app,
            Method::POST,
            "/artifacts/lease",
            json!({
                "filename": filename,
                "mime": "application/octet-stream",
                "expectedSize": expected_size,
            }),
        )
        .await;
        assert_eq!(response["ok"], true, "lease response: {response}");
        (
            response["lease"]["id"].as_str().unwrap().to_owned(),
            response["lease"]["token"].as_str().unwrap().to_owned(),
        )
    }

    #[tokio::test]
    async fn artifact_routes_complete_lease_chunk_finalize_lookup_cycle() {
        let temp = TempDir::new("artifact-http-happy");
        let app = artifact_test_app(&temp);
        let payload = b"\x89PNG\r\n\x1a\nroute-test";
        let expected_sha256 = format!("{:x}", Sha256::digest(payload));
        let (id, token) = create_artifact_lease(&app, "capture.png", Some(payload.len())).await;

        let (status, _, chunk) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({
                "token": token,
                "offset": 0,
                "bytesBase64": base64::engine::general_purpose::STANDARD.encode(payload),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(chunk["ok"], true, "chunk response: {chunk}");
        assert_eq!(chunk["receipt"]["totalBytes"], payload.len());

        let (_, _, finalized) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/finalize"),
            json!({ "token": token, "expectedSha256": expected_sha256 }),
        )
        .await;
        assert_eq!(finalized["ok"], true, "finalize response: {finalized}");
        assert_eq!(finalized["artifact"]["id"], id);
        assert_eq!(finalized["artifact"]["size"], payload.len());
        assert_eq!(finalized["artifact"]["sha256"], expected_sha256);
        let artifact_path = PathBuf::from(finalized["artifact"]["path"].as_str().unwrap());
        assert!(artifact_path.is_absolute());
        assert_eq!(std::fs::read(&artifact_path).unwrap(), payload);

        let (_, _, lookup) =
            artifact_json_request(&app, Method::GET, &format!("/artifacts/{id}"), Value::Null)
                .await;
        assert_eq!(lookup["ok"], true, "lookup response: {lookup}");
        assert_eq!(lookup["artifact"], finalized["artifact"]);

        let (_, _, first_read) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/read"),
            json!({ "offset": 0, "maxBytes": 5 }),
        )
        .await;
        assert_eq!(first_read["ok"], true, "read response: {first_read}");
        assert_eq!(first_read["chunk"]["offset"], 0);
        assert_eq!(first_read["chunk"]["nextOffset"], 5);
        assert_eq!(first_read["chunk"]["eof"], false);
        assert_eq!(first_read["chunk"]["byteLength"], payload.len());
        assert_eq!(first_read["chunk"]["sha256"], expected_sha256);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(first_read["chunk"]["bytesBase64"].as_str().unwrap())
                .unwrap(),
            &payload[..5],
        );

        let (_, _, final_read) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/read"),
            json!({ "offset": 5, "maxBytes": payload.len() }),
        )
        .await;
        assert_eq!(final_read["ok"], true);
        assert_eq!(final_read["chunk"]["eof"], true);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(final_read["chunk"]["bytesBase64"].as_str().unwrap())
                .unwrap(),
            &payload[5..],
        );

        let (_, _, invalid_read) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/read"),
            json!({ "offset": payload.len(), "maxBytes": 1 }),
        )
        .await;
        assert_eq!(invalid_read["ok"], false);
        assert_eq!(invalid_read["error"]["code"], "ARTIFACT_READ_RANGE");

        let (_, _, consumed) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/consume"),
            json!({}),
        )
        .await;
        assert_eq!(consumed["ok"], true, "consume response: {consumed}");
        assert!(!artifact_path.exists());
        let (_, _, missing) =
            artifact_json_request(&app, Method::GET, &format!("/artifacts/{id}"), Value::Null)
                .await;
        assert_eq!(missing["ok"], false);
    }

    #[tokio::test]
    async fn artifact_chunk_rejects_the_wrong_lease_token() {
        let temp = TempDir::new("artifact-http-token");
        let app = artifact_test_app(&temp);
        let (id, token) = create_artifact_lease(&app, "token.bin", Some(2)).await;

        let (_, _, rejected) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({ "token": "not-the-token", "offset": 0, "bytesBase64": "b2s=" }),
        )
        .await;
        assert_eq!(rejected["ok"], false);
        assert_eq!(rejected["error"]["code"], "ARTIFACT_INVALID_TOKEN");

        let (_, _, accepted) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({ "token": token, "offset": 0, "bytesBase64": "b2s=" }),
        )
        .await;
        assert_eq!(accepted["ok"], true, "valid token must remain usable");
    }

    #[tokio::test]
    async fn artifact_chunks_enforce_the_exact_next_offset_without_corruption() {
        let temp = TempDir::new("artifact-http-offset");
        let app = artifact_test_app(&temp);
        let (id, token) = create_artifact_lease(&app, "ordered.bin", Some(6)).await;

        let (_, _, first) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({ "token": token, "offset": 0, "bytesBase64": "YWJj" }),
        )
        .await;
        assert_eq!(first["receipt"]["totalBytes"], 3);

        let (_, _, rejected) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({ "token": token, "offset": 1, "bytesBase64": "ZGVm" }),
        )
        .await;
        assert_eq!(rejected["error"]["code"], "ARTIFACT_OFFSET_MISMATCH");
        assert_eq!(rejected["error"]["retryable"], true);

        let (_, _, second) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({ "token": token, "offset": 3, "bytesBase64": "ZGVm" }),
        )
        .await;
        assert_eq!(second["receipt"]["totalBytes"], 6);

        let (_, _, finalized) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/finalize"),
            json!({ "token": token }),
        )
        .await;
        let path = finalized["artifact"]["path"].as_str().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
    }

    #[tokio::test]
    async fn artifact_chunk_rejects_invalid_base64_and_decoded_oversize_payloads() {
        const MAX_CHUNK_BYTES: usize = 512 * 1024;
        let temp = TempDir::new("artifact-http-chunk-validation");
        let app = artifact_test_app(&temp);
        let (id, token) = create_artifact_lease(&app, "validation.bin", None).await;

        let (_, _, invalid) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({ "token": token, "offset": 0, "bytesBase64": "%%%not-base64%%%" }),
        )
        .await;
        assert_eq!(invalid["error"]["code"], "INVALID_ARTIFACT_BASE64");

        let oversized =
            base64::engine::general_purpose::STANDARD.encode(vec![0x5a; MAX_CHUNK_BYTES + 1]);
        let (_, _, too_large) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({ "token": token, "offset": 0, "bytesBase64": oversized }),
        )
        .await;
        assert_eq!(too_large["error"]["code"], "ARTIFACT_CHUNK_TOO_LARGE");

        let (_, _, accepted) = artifact_json_request(
            &app,
            Method::POST,
            &format!("/artifacts/{id}/chunk"),
            json!({ "token": token, "offset": 0, "bytesBase64": "eg==" }),
        )
        .await;
        assert_eq!(accepted["receipt"]["totalBytes"], 1);
    }

    #[tokio::test]
    async fn artifact_chunk_route_rejects_large_json_before_deserialization() {
        let temp = TempDir::new("artifact-http-body-limit");
        let app = artifact_test_app(&temp);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/artifacts/not-a-real-id/chunk")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(vec![b'x'; 2 * 1024 * 1024]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn artifact_preflight_exposes_cors_only_to_trusted_local_app_origins() {
        let temp = TempDir::new("artifact-http-cors");
        let app = artifact_test_app(&temp);

        let preflight = |origin: &'static str, authorized: bool| {
            let uri = if authorized {
                "/artifacts/lease?widgetToken=artifact-widget-token"
            } else {
                "/artifacts/lease"
            };
            app.clone().oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri(uri)
                    .header(header::ORIGIN, origin)
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
                    .body(Body::empty())
                    .unwrap(),
            )
        };

        let denied_without_token = preflight("terminal64://widget", false).await.unwrap();
        assert!(denied_without_token
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());

        let trusted = preflight("terminal64://widget", true).await.unwrap();
        assert_eq!(trusted.status(), StatusCode::OK);
        assert_eq!(
            trusted
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "terminal64://widget"
        );

        let untrusted = preflight("https://attacker.example", true).await.unwrap();
        assert!(
            untrusted
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "untrusted web origins must not receive CORS permission"
        );
    }

    #[test]
    fn local_app_origin_policy_allows_custom_and_loopback_widget_origins() {
        for origin in [
            "t64://widget",
            "terminal64://widget",
            "app://localhost",
            "tauri://localhost",
            "http://tauri.localhost",
            "wry://localhost",
            "http://127.0.0.1:49173",
            "http://localhost:49174",
            "https://[::1]:4443",
        ] {
            assert!(
                is_trusted_local_app_origin(&HeaderValue::from_bytes(origin.as_bytes()).unwrap()),
                "expected trusted local-app origin: {origin}"
            );
        }

        for origin in [
            "null",
            "file://",
            "terminal64://attacker",
            "app://attacker",
            "https://attacker.example",
            "http://127.0.0.2:3000",
            "http://localhost.attacker.example:3000",
            "http://tauri.localhost.attacker.example",
            "http://user@localhost:3000",
            "chrome-extension://attacker",
            "moz-extension://attacker",
            "terminal64.example",
        ] {
            assert!(
                !is_trusted_local_app_origin(&HeaderValue::from_bytes(origin.as_bytes()).unwrap()),
                "expected untrusted browser origin: {origin}"
            );
        }
    }

    #[test]
    fn opaque_widget_origin_requires_the_exact_owner_token() {
        let origin = HeaderValue::from_static("null");
        assert!(!is_authorized_widget_browser_request(
            &origin,
            &"/hello".parse().unwrap(),
            true,
            Some("secret")
        ));
        assert!(!is_authorized_widget_browser_request(
            &origin,
            &"/hello?widgetToken=wrong".parse().unwrap(),
            true,
            Some("secret")
        ));
        assert!(is_authorized_widget_browser_request(
            &origin,
            &"/hello?widgetToken=secret".parse().unwrap(),
            true,
            Some("secret")
        ));
    }

    #[test]
    fn resolve_accepts_plan_disk_alias() {
        assert!(matches!(
            parse_resolution("disk"),
            Ok(Resolution::KeepLocal)
        ));
        assert!(matches!(
            parse_resolution("keep-disk"),
            Ok(Resolution::KeepLocal)
        ));
        assert!(matches!(
            parse_resolution("studio"),
            Ok(Resolution::KeepStudio)
        ));
    }

    #[test]
    fn resolve_conflict_target_accepts_project_relative_file_paths() {
        let project = PathBuf::from("/project");
        assert_eq!(
            resolve_conflict_target(&project, "ServerScriptService/Foo.luau"),
            PathBuf::from("/project/ServerScriptService/Foo.luau")
        );
        assert_eq!(
            resolve_conflict_target(&project, "/project/ServerScriptService/Foo.luau"),
            PathBuf::from("/project/ServerScriptService/Foo.luau")
        );
    }

    #[test]
    fn missing_nested_parent_preserves_existing_legacy_unicode_prefix() {
        let d = TempDir::new("resolve-parent-prefix");
        let workspace = d.path().join("Workspace");
        let legacy_parent = workspace.join("É");
        std::fs::create_dir_all(&legacy_parent).unwrap();

        let resolved =
            resolve_segments_to_dir(d.path(), &["Workspace".into(), "É".into(), "Nested".into()])
                .unwrap();

        assert_eq!(resolved, legacy_parent.join("Nested"));
        assert_ne!(resolved, workspace.join(encode_name("É")).join("Nested"));
    }

    // Out-of-scope classes are silently skipped: `Part` is not in the four-class
    // whitelist, so `apply_set` returns `Skipped` instead of materializing
    // anything on disk. Property sync is ripped out — anything beyond
    // Folder/Script/LocalScript/ModuleScript is Studio-authoritative via live Studio reads.
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

    #[test]
    fn bootstrap_skips_unchanged_script_with_children_sources() {
        let d = TempDir::new("bootstrap-unchanged-script-dir");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);

        let controller = d.path().join("ReplicatedStorage").join("Controller");
        std::fs::create_dir_all(&controller).unwrap();
        std::fs::write(controller.join("init (Controller).luau"), "print('same')\n").unwrap();
        std::fs::write(controller.join("Child.luau"), "return {}\n").unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "Controller",
                "class": "ModuleScript",
                "properties": { "Source": "print('same')\r\n" },
                "children": [{
                    "name": "Child",
                    "class": "ModuleScript",
                    "properties": { "Source": "return {}\r\n" },
                    "children": []
                }]
            }]
        });

        let applied = apply_service_node(d.path(), &service, &ctx).unwrap();
        assert_eq!(applied, 0);
    }

    #[test]
    fn bootstrap_reuses_legacy_literal_unicode_script_and_init_paths() {
        let d = TempDir::new("bootstrap-legacy-unicode-script-dir");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);

        let storage = d.path().join("ReplicatedStorage");
        let controller = storage.join("É");
        std::fs::create_dir_all(&controller).unwrap();
        let legacy_init = controller.join("init (É).luau");
        let child_path = controller.join("Child.luau");
        std::fs::write(&legacy_init, "return {}\n").unwrap();
        std::fs::write(&child_path, "return true\n").unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "É",
                "class": "ModuleScript",
                "properties": { "Source": "return {}\n" },
                "children": [{
                    "name": "Child",
                    "class": "ModuleScript",
                    "properties": { "Source": "return true\n" },
                    "children": []
                }]
            }]
        });

        let applied = apply_service_node(d.path(), &service, &ctx).unwrap();
        assert_eq!(applied, 0);
        assert!(legacy_init.is_file());
        assert!(child_path.is_file());
        assert!(!storage.join(encode_name("É")).exists());
        assert!(!controller
            .join(format!(
                "init ({}){}",
                encode_name("É"),
                ScriptClass::ModuleScript.suffix()
            ))
            .exists());
        let init_count = std::fs::read_dir(&controller)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_str().is_some_and(is_init_file))
            .count();
        assert_eq!(init_count, 1);
    }

    #[test]
    fn bootstrap_applies_changed_script_with_children_source_once() {
        let d = TempDir::new("bootstrap-changed-script-dir");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();

        let controller = d.path().join("ReplicatedStorage").join("Controller");
        std::fs::create_dir_all(&controller).unwrap();
        let init_path = controller.join("init (Controller).luau");
        let child_path = controller.join("Child.luau");
        std::fs::write(&init_path, "print('old')\n").unwrap();
        std::fs::write(&child_path, "return {}\n").unwrap();
        engine.record_sync(&init_path, hash(b"print('old')\n"), 1);
        engine.record_sync(&child_path, hash(b"return {}\n"), 1);
        let ctx = harness(&engine, &quiet);

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "Controller",
                "class": "ModuleScript",
                "properties": { "Source": "print('new')\n" },
                "children": [{
                    "name": "Child",
                    "class": "ModuleScript",
                    "properties": { "Source": "return {}\n" },
                    "children": []
                }]
            }]
        });

        let applied = apply_service_node(d.path(), &service, &ctx).unwrap();
        assert_eq!(applied, 1);
        assert_eq!(
            std::fs::read_to_string(init_path).unwrap(),
            "print('new')\n"
        );
    }

    #[test]
    fn script_with_children_rename_updates_directory_and_init_atomically() {
        let d = TempDir::new("rename-script-dir-atomic");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);
        let parent = d.path().join("ReplicatedStorage");
        let old_path = parent.join("Old");
        let new_path = parent.join("New");
        std::fs::create_dir_all(&old_path).unwrap();
        std::fs::write(old_path.join("init (Old).luau"), "return 42\n").unwrap();

        rename_path_and_init(&old_path, &new_path, "New", true, &ctx).unwrap();

        assert!(!old_path.exists());
        assert_eq!(
            std::fs::read_to_string(new_path.join("init (New).luau")).unwrap(),
            "return 42\n"
        );
    }

    #[test]
    fn studio_rename_rebases_leaf_baseline_for_followup_clean_delete() {
        let d = TempDir::new("rename-leaf-rebases-baseline");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);
        let storage = d.path().join("ServerStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let old_path = storage.join("Old.luau");
        let new_path = storage.join("New.luau");
        std::fs::write(&old_path, "return 42\n").unwrap();
        engine.record_sync(&old_path, hash(b"return 42\n"), 1);

        assert_eq!(
            apply_rename(
                d.path(),
                &["ServerStorage".into(), "Old".into()],
                "New",
                &ctx
            )
            .unwrap(),
            1
        );
        assert!(new_path.is_file());
        assert!(engine.matches_baseline(&new_path, b"return 42\n"));

        let deleted =
            apply_delete(d.path(), &["ServerStorage".into(), "New".into()], &ctx).unwrap();
        assert!(matches!(deleted, ApplyOutcome::Applied(1)));
        assert!(!new_path.exists());
        assert!(engine.list().is_empty());
    }

    #[test]
    fn script_with_children_rename_rolls_back_after_inner_failure() {
        let d = TempDir::new("rename-script-dir-rollback");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);
        let parent = d.path().join("ReplicatedStorage");
        let old_path = parent.join("Old");
        let new_path = parent.join("New");
        let old_init = old_path.join("init (Old).luau");
        std::fs::create_dir_all(&old_path).unwrap();
        std::fs::write(&old_init, "return 'preserved'\n").unwrap();

        let mut rename_calls = 0usize;
        let error =
            rename_path_and_init_with(&old_path, &new_path, "New", true, &ctx, |from, to| {
                rename_calls += 1;
                if rename_calls == 3 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected init rename failure",
                    ));
                }
                std::fs::rename(from, to)
            })
            .unwrap_err();

        assert!(error.contains("init and outer rename were rolled back"));
        assert_eq!(rename_calls, 5);
        assert!(old_path.is_dir());
        assert!(!new_path.exists());
        assert_eq!(
            std::fs::read_to_string(&old_init).unwrap(),
            "return 'preserved'\n"
        );
        assert!(std::fs::read_dir(&old_path).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".rosync-init-rename-")));
    }

    #[test]
    fn keep_studio_rename_restore_is_atomic_on_success() {
        let d = TempDir::new("resolve-rename-transaction-success");
        let parent = d.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&parent).unwrap();
        let from = parent.join("Old.luau");
        let to = parent.join("New.luau");
        std::fs::write(&to, b"disk edit\n").unwrap();

        restore_fs_rename_transactional(&from, &to, &from, b"studio edit\n").unwrap();

        assert_eq!(std::fs::read(&from).unwrap(), b"studio edit\n");
        assert!(!to.exists());
        assert!(std::fs::read_dir(&parent).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".swp")));
    }

    #[test]
    fn keep_studio_directory_rename_restores_the_entire_retained_tree() {
        let d = TempDir::new("resolve-directory-rename-transaction-success");
        let parent = d.path().join("ReplicatedStorage");
        let from = parent.join("Old");
        let to = parent.join("New");
        let conflict_path = from.join("Nested").join("Diverged.luau");
        std::fs::create_dir_all(to.join("Nested")).unwrap();
        std::fs::write(to.join("Nested").join("Diverged.luau"), b"disk edit\n").unwrap();
        std::fs::write(to.join("Sibling.luau"), b"return 'sibling'\n").unwrap();
        std::fs::write(to.join("Nested").join("Clean.luau"), b"return 'clean'\n").unwrap();

        restore_fs_rename_transactional(&from, &to, &conflict_path, b"return 'studio edit'\n")
            .unwrap();

        assert!(!to.exists());
        assert_eq!(
            std::fs::read(&conflict_path).unwrap(),
            b"return 'studio edit'\n"
        );
        assert_eq!(
            std::fs::read(from.join("Sibling.luau")).unwrap(),
            b"return 'sibling'\n"
        );
        assert_eq!(
            std::fs::read(from.join("Nested").join("Clean.luau")).unwrap(),
            b"return 'clean'\n"
        );
        assert!(std::fs::read_dir(&parent).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".swp")));
    }

    #[test]
    fn keep_studio_directory_delete_restore_is_explicitly_fail_closed() {
        assert_eq!(validate_fs_delete_restore(false), Ok(()));
        assert_eq!(
            validate_fs_delete_restore(true),
            Err(DIRECTORY_DELETE_RESTORE_ERROR)
        );
    }

    #[test]
    fn keep_studio_rename_restore_rolls_back_after_install_failure() {
        let d = TempDir::new("resolve-rename-transaction-rollback");
        let parent = d.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&parent).unwrap();
        let from = parent.join("Old.luau");
        let to = parent.join("New.luau");
        std::fs::write(&to, b"disk edit\n").unwrap();

        let mut calls = 0usize;
        let error = restore_fs_rename_transactional_with(
            &from,
            &to,
            &from,
            b"studio edit\n",
            |source, destination| {
                calls += 1;
                if calls == 3 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected install failure",
                    ));
                }
                std::fs::rename(source, destination)
            },
        )
        .unwrap_err();

        assert!(error.contains("source rollback: ok"), "{error}");
        assert!(error.contains("directory rollback: ok"), "{error}");
        assert_eq!(calls, 5);
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"disk edit\n");
        assert!(std::fs::read_dir(&parent).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".swp")));
    }

    #[test]
    fn keep_studio_delete_restore_leaves_no_partial_file_on_install_failure() {
        let d = TempDir::new("resolve-delete-transaction-rollback");
        let parent = d.path().join("ServerScriptService");
        std::fs::create_dir_all(&parent).unwrap();
        let source = parent.join("Deleted.server.luau");

        let error = restore_fs_deleted_source_with(&source, b"studio edit\n", |_from, _to| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected install failure",
            ))
        })
        .unwrap_err();

        assert!(error.contains("injected install failure"), "{error}");
        assert!(!source.exists());
        assert!(std::fs::read_dir(&parent).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".swp")));
    }

    #[test]
    fn keep_disk_rename_delivery_reports_partial_failure_position() {
        let rename = json!({ "op": "rename" });
        let retained = vec![
            json!({ "op": "update", "path": ["Workspace", "New"] }),
            json!({ "op": "update", "path": ["Workspace", "New", "Child"] }),
        ];
        let mut attempted = Vec::new();
        let result = deliver_prepared_rename_with(&rename, &retained, |op| {
            attempted.push(op["op"].as_str().unwrap().to_string());
            if attempted.len() == 2 {
                return Err("injected transport failure".to_string());
            }
            Ok(())
        });

        assert_eq!(result, Err(("injected transport failure".to_string(), 1)));
        assert_eq!(attempted, ["rename", "update"]);
    }

    #[test]
    fn partial_keep_disk_rename_queues_source_and_name_compensation() {
        let d = TempDir::new("resolve-rename-studio-compensation");
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let from = workspace.join("Old.luau");
        let to = workspace.join("New.luau");
        std::fs::write(&to, b"disk edit\n").unwrap();
        let applied = json!({
            "op": "rename",
            "from": ["Workspace", "Old"],
            "to": ["Workspace", "New"],
        });
        let (events, mut receiver) = broadcast::channel(4);

        assert!(compensate_studio_rename(
            &events,
            d.path(),
            &applied,
            &from,
            &to,
            &from,
            b"studio edit\n",
        ));

        let source_restore: Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
        assert_eq!(source_restore["type"], "plugin-op");
        assert_eq!(source_restore["op"]["op"], "update");
        assert_eq!(source_restore["op"]["path"], json!(["Workspace", "New"]));
        assert_eq!(
            source_restore["op"]["properties"]["Source"],
            "studio edit\n"
        );

        let reverse: Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
        assert_eq!(reverse["op"]["op"], "rename");
        assert_eq!(reverse["op"]["from"], json!(["Workspace", "New"]));
        assert_eq!(reverse["op"]["to"], json!(["Workspace", "Old"]));
    }

    #[test]
    fn fs_rename_op_to_plugin_uses_from_and_to_paths() {
        let d = TempDir::new("fs-rename-op");
        let from = d
            .path()
            .join("ReplicatedStorage")
            .join("Shared")
            .join("OldName.luau");
        let to = d
            .path()
            .join("ReplicatedStorage")
            .join("Shared")
            .join("NewName.luau");
        let op = Op {
            kind: OpKind::Rename,
            path: to,
            from: Some(from),
            content: None,
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).expect("rename plugin op");

        assert_eq!(plugin_op["op"], "rename");
        assert_eq!(
            plugin_op["from"],
            serde_json::json!(["ReplicatedStorage", "Shared", "OldName"])
        );
        assert_eq!(
            plugin_op["to"],
            serde_json::json!(["ReplicatedStorage", "Shared", "NewName"])
        );
    }

    #[test]
    fn retained_directory_resolution_emits_the_full_script_tree() {
        let d = TempDir::new("resolve-retained-tree");
        let root = d.path().join("Workspace").join("Feature");
        let nested = root.join("Nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Worker.server.luau"), "print('kept')\n").unwrap();

        let mut ops = Vec::new();
        collect_tree_update_ops(&root, &mut ops).unwrap();

        let plugin_ops: Vec<Value> = ops
            .iter()
            .filter_map(|op| fs_op_to_plugin_op(d.path(), op))
            .collect();
        assert_eq!(plugin_ops.len(), 3);
        assert_eq!(plugin_ops[0]["node"]["name"], "Feature");
        assert_eq!(plugin_ops[1]["node"]["name"], "Nested");
        assert_eq!(plugin_ops[2]["node"]["name"], "Worker");
        assert_eq!(
            plugin_ops[2]["node"]["properties"]["Source"],
            "print('kept')\n"
        );
    }

    #[test]
    fn fs_rename_op_to_plugin_converts_script_class() {
        let d = TempDir::new("fs-rename-class-op");
        let from = d
            .path()
            .join("ReplicatedStorage")
            .join("Shared")
            .join("CombatService.server.luau");
        let to = d
            .path()
            .join("ReplicatedStorage")
            .join("Shared")
            .join("CombatService.luau");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(&to, "return {}\n").unwrap();
        let op = Op {
            kind: OpKind::Rename,
            path: to,
            from: Some(from),
            content: None,
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).expect("class change op");

        assert_eq!(plugin_op["op"], "class_change");
        assert_eq!(
            plugin_op["path"],
            serde_json::json!(["ReplicatedStorage", "Shared", "CombatService"])
        );
        assert_eq!(
            plugin_op["to"],
            serde_json::json!(["ReplicatedStorage", "Shared", "CombatService"])
        );
        assert_eq!(plugin_op["class"], "ModuleScript");
        assert_eq!(plugin_op["properties"]["Source"], "return {}\n");
    }

    #[test]
    fn fs_init_update_to_plugin_converts_folder_to_script_with_children() {
        let d = TempDir::new("fs-init-class-op");
        let controller = d.path().join("ServerScriptService").join("Controller");
        std::fs::create_dir_all(&controller).unwrap();
        let init = controller.join("init (Controller).server.luau");
        let source = "print('controller')\n";
        std::fs::write(&init, source).unwrap();
        let op = Op {
            kind: OpKind::Update,
            path: init,
            from: None,
            content: Some(source.as_bytes().to_vec()),
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).expect("class change op");

        assert_eq!(plugin_op["op"], "class_change");
        assert_eq!(
            plugin_op["path"],
            serde_json::json!(["ServerScriptService", "Controller"])
        );
        assert_eq!(plugin_op["class"], "Script");
        assert_eq!(plugin_op["properties"]["Source"], source);
    }

    #[test]
    fn fs_empty_folder_op_is_ignored_and_cannot_shadow_script() {
        let d = TempDir::new("fs-empty-folder-shadow");
        let root = d.path().join("ReplicatedStorage");
        let empty = root.join("LuckyBlockHandler");
        let script = root.join("LuckyBlockHandler.luau");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(&script, "return {}\n").unwrap();

        let folder_op = Op {
            kind: OpKind::Add,
            path: empty,
            from: None,
            content: None,
        };
        assert!(fs_op_to_plugin_op(d.path(), &folder_op).is_none());
        assert_eq!(
            find_child_fragment_by_name(&root, "LuckyBlockHandler")
                .unwrap()
                .as_deref(),
            Some("LuckyBlockHandler.luau")
        );
    }

    #[test]
    fn fs_delete_of_shadowing_empty_folder_does_not_delete_script() {
        let d = TempDir::new("fs-empty-folder-delete-shadow");
        let root = d.path().join("ReplicatedStorage");
        let empty = root.join("LuckyBlockHandler");
        let script = root.join("LuckyBlockHandler.luau");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(&script, "return {}\n").unwrap();
        std::fs::remove_dir(&empty).unwrap();

        let op = Op {
            kind: OpKind::Delete,
            path: empty,
            from: None,
            content: None,
        };

        assert!(fs_op_to_plugin_op(d.path(), &op).is_none());
    }

    #[test]
    fn fs_delete_preserves_duplicate_lookup_segment() {
        let d = TempDir::new("fs-delete-duplicate-lookup");
        let root = d.path().join("Workspace");
        std::fs::create_dir_all(&root).unwrap();
        let duplicate = root.join("Controller [1].luau");

        let op = Op {
            kind: OpKind::Delete,
            path: duplicate,
            from: None,
            content: None,
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).unwrap();
        assert_eq!(plugin_op["op"], "delete");
        assert_eq!(
            plugin_op["path"],
            serde_json::json!(["Workspace", "Controller [1]"])
        );
    }

    #[test]
    fn fs_set_under_duplicate_parent_uses_lookup_parent() {
        let d = TempDir::new("fs-set-duplicate-parent");
        let duplicate_parent = d.path().join("Workspace").join("Rig [1]");
        std::fs::create_dir_all(&duplicate_parent).unwrap();
        let child = duplicate_parent.join("Animate.client.luau");
        std::fs::write(&child, "print('animate')\n").unwrap();

        let op = Op {
            kind: OpKind::Update,
            path: child,
            from: None,
            content: Some(b"print('animate')\n".to_vec()),
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).unwrap();
        assert_eq!(plugin_op["op"], "set");
        assert_eq!(
            plugin_op["path"],
            serde_json::json!(["Workspace", "Rig [1]"])
        );
        assert_eq!(plugin_op["node"]["name"], "Animate");
        assert_eq!(plugin_op["node"]["class"], "LocalScript");
    }

    #[test]
    fn fs_rename_uses_duplicate_lookup_source_and_actual_destination_name() {
        let d = TempDir::new("fs-rename-duplicate-lookup");
        let root = d.path().join("Workspace");
        std::fs::create_dir_all(&root).unwrap();
        let from = root.join("Controller [1].luau");
        let to = root.join("ControllerRenamed.luau");
        std::fs::write(&to, "return {}\n").unwrap();

        let op = Op {
            kind: OpKind::Rename,
            path: to,
            from: Some(from),
            content: Some(b"return {}\n".to_vec()),
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).unwrap();
        assert_eq!(plugin_op["op"], "rename");
        assert_eq!(
            plugin_op["from"],
            serde_json::json!(["Workspace", "Controller [1]"])
        );
        assert_eq!(
            plugin_op["to"],
            serde_json::json!(["Workspace", "ControllerRenamed"])
        );
    }

    #[test]
    fn fs_class_change_uses_duplicate_lookup_but_actual_destination_name() {
        let d = TempDir::new("fs-class-change-duplicate-lookup");
        let root = d.path().join("Workspace");
        std::fs::create_dir_all(&root).unwrap();
        let from = root.join("Controller [1].luau");
        let to = root.join("Controller [1].client.luau");
        std::fs::write(&to, "print('client')\n").unwrap();

        let op = Op {
            kind: OpKind::Rename,
            path: to,
            from: Some(from),
            content: Some(b"print('client')\n".to_vec()),
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).unwrap();
        assert_eq!(plugin_op["op"], "class_change");
        assert_eq!(
            plugin_op["path"],
            serde_json::json!(["Workspace", "Controller [1]"])
        );
        assert_eq!(
            plugin_op["to"],
            serde_json::json!(["Workspace", "Controller"])
        );
        assert_eq!(plugin_op["class"], "LocalScript");
    }

    #[test]
    fn fs_op_to_plugin_ignores_unknown_project_root() {
        let d = TempDir::new("fs-unknown-root");
        let op = Op {
            kind: OpKind::Update,
            path: d.path().join("RandomFolder"),
            from: None,
            content: None,
        };

        assert!(fs_op_to_plugin_op(d.path(), &op).is_none());
    }

    #[test]
    fn fs_op_to_plugin_ignores_avoid_sync_tree_paths() {
        let d = TempDir::new("fs-avoid-sync");
        let ignored = d.path().join("Workspace").join("Ignored");
        std::fs::create_dir_all(&ignored).unwrap();
        let script = ignored.join("Worker.server.luau");
        std::fs::write(&script, "print('nope')\n").unwrap();
        set_avoid_sync_paths(
            d.path(),
            vec![vec!["Workspace".to_string(), "Ignored".to_string()]],
        );
        let op = Op {
            kind: OpKind::Update,
            path: script,
            from: None,
            content: Some(b"print('nope')\n".to_vec()),
        };

        assert!(fs_op_to_plugin_op(d.path(), &op).is_none());
    }

    #[test]
    fn fs_rename_op_to_plugin_rejects_unknown_source_root() {
        let d = TempDir::new("fs-rename-unknown-root");
        let op = Op {
            kind: OpKind::Rename,
            path: d.path().join("Workspace").join("RandomFolder"),
            from: Some(d.path().join("RandomFolder")),
            content: None,
        };

        assert!(fs_op_to_plugin_op(d.path(), &op).is_none());
    }

    #[test]
    fn bootstrap_force_overwrites_sources_without_diffing_existing_files() {
        let d = TempDir::new("bootstrap-force-overwrite-script-dir");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = force_harness(&engine, &quiet);

        let controller = d.path().join("ReplicatedStorage").join("Controller");
        std::fs::create_dir_all(&controller).unwrap();
        let init_path = controller.join("init (Controller).luau");
        let child_path = controller.join("Child.luau");
        std::fs::write(&init_path, "print('same')\n").unwrap();
        std::fs::write(&child_path, "return {}\n").unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "Controller",
                "class": "ModuleScript",
                "properties": { "Source": "print('same')\r\n" },
                "children": [{
                    "name": "Child",
                    "class": "ModuleScript",
                    "properties": { "Source": "return {}\r\n" },
                    "children": []
                }]
            }]
        });

        let applied = apply_service_node(d.path(), &service, &ctx).unwrap();
        assert_eq!(applied, 2);
        assert_eq!(
            std::fs::read_to_string(init_path).unwrap(),
            "print('same')\n"
        );
        assert_eq!(std::fs::read_to_string(child_path).unwrap(), "return {}\n");
    }

    #[test]
    fn bootstrap_strict_prunes_disk_only_paths_when_keeping_studio() {
        let d = TempDir::new("bootstrap-strict-prune");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet);

        let disk_only = d
            .path()
            .join("ReplicatedStorage")
            .join("Assets")
            .join("EventVFX")
            .join("Galaxy");
        std::fs::create_dir_all(&disk_only).unwrap();
        std::fs::write(
            d.path()
                .join("ReplicatedStorage")
                .join("ClientOnly.server.luau"),
            "print('remove me')\n",
        )
        .unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": []
        });

        let applied = apply_service_node(d.path(), &service, &ctx).unwrap();

        assert!(applied >= 1);
        assert!(
            d.path().join("ReplicatedStorage").join("Assets").exists(),
            "folder-only Studio-adjacent disk trees are outside the sync surface"
        );
        assert!(!d
            .path()
            .join("ReplicatedStorage")
            .join("ClientOnly.server.luau")
            .exists());
        let backup_root = d.path().join(".rosync-backups");
        let preserved = std::fs::read_dir(&backup_root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| {
                entry
                    .path()
                    .join("ReplicatedStorage")
                    .join("ClientOnly.server.luau")
            })
            .find(|path| path.exists())
            .expect("strict prune must preserve removed source in a backup");
        assert_eq!(
            std::fs::read_to_string(preserved).unwrap(),
            "print('remove me')\n"
        );
    }

    #[test]
    fn bootstrap_strict_prunes_disk_only_folder_with_script_descendant() {
        let d = TempDir::new("bootstrap-strict-script-folder-prune");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet);

        let owned_tree = d
            .path()
            .join("ReplicatedStorage")
            .join("Assets")
            .join("EventVFX");
        std::fs::create_dir_all(&owned_tree).unwrap();
        std::fs::write(
            owned_tree.join("Emitter.client.luau"),
            "print('remove me')\n",
        )
        .unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": []
        });

        let applied = apply_service_node(d.path(), &service, &ctx).unwrap();

        assert!(applied >= 1);
        assert!(
            !d.path().join("ReplicatedStorage").join("Assets").exists(),
            "folders with script descendants remain sync-owned and pruneable"
        );
    }

    #[test]
    fn studio_delete_ignores_plain_disk_folder_without_script_sources() {
        let d = TempDir::new("delete-plain-folder");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);

        let folder = d.path().join("Workspace").join("StudioOnly").join("Nested");
        std::fs::create_dir_all(&folder).unwrap();

        let outcome =
            apply_delete(d.path(), &["Workspace".into(), "StudioOnly".into()], &ctx).unwrap();

        assert!(matches!(outcome, ApplyOutcome::Skipped));
        assert!(d.path().join("Workspace").join("StudioOnly").exists());
    }

    #[test]
    fn studio_delete_parks_when_local_script_changed() {
        let d = TempDir::new("delete-local-edit");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let script = workspace.join("Safe.server.luau");
        std::fs::write(&script, b"local edit\n").unwrap();
        engine.record_sync(&script, hash(b"agreed source\n"), 1);

        let outcome = apply_delete(d.path(), &["Workspace".into(), "Safe".into()], &ctx).unwrap();

        assert!(matches!(outcome, ApplyOutcome::Conflict(path) if path == script));
        assert!(script.exists(), "conflicting local source must be retained");
        let conflicts = engine.list();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].studio_deleted);
        assert_eq!(conflicts[0].local, "local edit\n");
    }

    #[test]
    fn studio_delete_applies_when_local_script_matches_baseline() {
        let d = TempDir::new("delete-clean");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet);
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let script = workspace.join("Safe.server.luau");
        std::fs::write(&script, b"agreed source\n").unwrap();
        engine.record_sync(&script, hash(b"agreed source\n"), 1);

        let outcome = apply_delete(d.path(), &["Workspace".into(), "Safe".into()], &ctx).unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied(1)));
        assert!(!script.exists());
    }

    #[test]
    fn clean_initial_compare_seeds_leaf_and_directory_script_baselines() {
        let d = TempDir::new("seed-baselines");
        let replicated = d.path().join("ReplicatedStorage");
        let controller = replicated.join("Controller");
        std::fs::create_dir_all(&controller).unwrap();
        let leaf = replicated.join("Config.luau");
        let init = controller.join("init (Controller).luau");
        std::fs::write(&leaf, b"return 1\n").unwrap();
        std::fs::write(&init, b"return 2\n").unwrap();
        std::fs::write(controller.join("Child.luau"), b"return 3\n").unwrap();
        let engine = ConflictEngine::new();

        let seeded = seed_clean_script_baselines(d.path(), &engine).unwrap();

        assert_eq!(seeded, 3);
        assert_eq!(
            engine.on_studio_push(&leaf, b"return 10\n", Some((b"return 1\n", 2))),
            StudioDecision::Apply
        );
        assert_eq!(
            engine.on_studio_push(&init, b"return 20\n", Some((b"return 2\n", 2))),
            StudioDecision::Apply
        );
    }

    #[test]
    fn bootstrap_force_replaces_stale_disk_class_when_keeping_studio() {
        let d = TempDir::new("bootstrap-force-class-replace");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet);

        let stale_folder = d
            .path()
            .join("ReplicatedStorage")
            .join("Client")
            .join("Dialog");
        std::fs::create_dir_all(&stale_folder).unwrap();
        std::fs::write(stale_folder.join("Child.luau"), "return {}\n").unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "Client",
                "class": "Folder",
                "properties": {},
                "children": [{
                    "name": "Dialog",
                    "class": "LocalScript",
                    "properties": { "Source": "print('studio')\n" },
                    "children": []
                }]
            }]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert!(!stale_folder.exists());
        assert_eq!(
            std::fs::read_to_string(
                d.path()
                    .join("ReplicatedStorage")
                    .join("Client")
                    .join("Dialog.client.luau")
            )
            .unwrap(),
            "print('studio')\n"
        );
    }

    #[test]
    fn bootstrap_strict_empty_studio_folder_does_not_protect_stale_disk_tree() {
        let d = TempDir::new("bootstrap-empty-studio-folder-prune");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet);

        let client = d.path().join("ReplicatedStorage").join("Client");
        std::fs::create_dir_all(&client).unwrap();
        std::fs::write(client.join("DialogueText.luau"), "return {}\n").unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "Client",
                "class": "Folder",
                "properties": {},
                "children": []
            }]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert!(
            !client.exists(),
            "empty Studio folder should not keep stale synced disk children alive"
        );
    }

    #[test]
    fn bootstrap_strict_prunes_missing_nested_child_under_kept_folder() {
        let d = TempDir::new("bootstrap-nested-prune");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet);

        let client = d.path().join("ReplicatedStorage").join("Client");
        std::fs::create_dir_all(&client).unwrap();
        std::fs::write(client.join("DialogueText.luau"), "return {}\n").unwrap();
        std::fs::write(client.join("WorldController.luau"), "return {}\n").unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "Client",
                "class": "Folder",
                "properties": {},
                "children": [{
                    "name": "WorldController",
                    "class": "ModuleScript",
                    "properties": { "Source": "return { Studio = true }\n" },
                    "children": []
                }]
            }]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert!(!client.join("DialogueText.luau").exists());
        assert_eq!(
            std::fs::read_to_string(client.join("WorldController.luau")).unwrap(),
            "return { Studio = true }\n"
        );
    }

    #[test]
    fn bootstrap_assigns_duplicate_siblings_by_stable_subtree() {
        let d = TempDir::new("bootstrap-duplicate-order");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet);

        let sell_npc = d
            .path()
            .join("Workspace")
            .join("SellNPC")
            .join("HumanoidRootPart");
        std::fs::create_dir_all(&sell_npc).unwrap();
        std::fs::write(sell_npc.join("DialogueDemo.client.luau"), "old dialogue\n").unwrap();
        let sell_npc_duplicate = d.path().join("Workspace").join("SellNPC [1]");
        std::fs::create_dir_all(&sell_npc_duplicate).unwrap();
        std::fs::write(
            sell_npc_duplicate.join("Animate.client.luau"),
            "old animate\n",
        )
        .unwrap();
        let stable_duplicate_target = d
            .path()
            .join("Workspace")
            .join("SellNPC [1]")
            .join("HumanoidRootPart");

        let service = serde_json::json!({
            "name": "Workspace",
            "class": "Workspace",
            "children": [
                { "name": "SellNPC", "class": "Folder", "properties": {}, "children": [
                    { "name": "HumanoidRootPart", "class": "Folder", "properties": {}, "children": [
                        { "name": "DialogueDemo", "class": "LocalScript", "properties": { "Source": "dialogue\n" }, "children": [] }
                    ] }
                ] },
                { "name": "SellNPC", "class": "Folder", "properties": {}, "children": [
                    { "name": "Animate", "class": "LocalScript", "properties": { "Source": "animate\n" }, "children": [] }
                ] }
            ]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert_eq!(
            std::fs::read_to_string(d.path().join("Workspace/SellNPC/Animate.client.luau"))
                .unwrap(),
            "animate\n"
        );
        assert_eq!(
            std::fs::read_to_string(stable_duplicate_target.join("DialogueDemo.client.luau"))
                .unwrap(),
            "dialogue\n"
        );
        assert!(!d
            .path()
            .join("Workspace/SellNPC/HumanoidRootPart/DialogueDemo.client.luau")
            .exists());
        assert!(!d
            .path()
            .join("Workspace/SellNPC [1]/Animate.client.luau")
            .exists());
    }

    #[test]
    fn tree_post_state_keeps_avoid_sync_without_tree_json() {
        let d = TempDir::new("tree-post");
        let root = d.path();
        let skeleton = serde_json::json!({
            "name": "Workspace",
            "class": "Workspace",
            "children": [
                { "name": "Ignored", "class": "Folder", "avoidSync": true, "children": [] },
                { "name": "Camera", "class": "Camera", "children": [] }
            ]
        });
        let mut paths = Vec::new();
        collect_avoid_sync_paths(&skeleton, &[], &mut paths);
        set_avoid_sync_paths(root, paths);

        assert!(path_is_avoid_synced(
            root,
            &["Workspace".to_string(), "Ignored".to_string()]
        ));
        assert!(
            !root.join("tree.json").exists(),
            "live tree posts should not create tree.json"
        );
    }

    #[test]
    fn initial_snapshot_compare_accepts_matching_script_with_children() {
        let d = TempDir::new("initial-match");
        let sss = d.path().join("ServerScriptService");
        let controller = sss.join("Controller");
        std::fs::create_dir_all(&controller).unwrap();
        std::fs::write(
            controller.join("init (Controller).server.luau"),
            "print('root')\n",
        )
        .unwrap();
        std::fs::write(controller.join("Child.luau"), "return {}\n").unwrap();

        let studio = vec![json!({
            "class": "ServerScriptService",
            "name": "ServerScriptService",
            "properties": {},
            "children": [{
                "class": "Script",
                "name": "Controller",
                "properties": { "Source": "print('root')\r\n" },
                "children": [{
                    "class": "ModuleScript",
                    "name": "Child",
                    "properties": { "Source": "return {}\r\n" },
                    "children": []
                }]
            }]
        })];

        assert!(initial_snapshots_match(d.path(), &studio).unwrap());
    }

    #[test]
    fn initial_snapshot_compare_uses_studio_tree_mapping() {
        let d = TempDir::new("initial-studio-tree-mapping");
        let model = d.path().join("Workspace").join("Rig");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("Animate.client.luau"), "animate\n").unwrap();

        let studio = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "children": [
                {
                    "class": "Model",
                    "name": "Rig",
                    "children": [{
                        "class": "LocalScript",
                        "name": "Animate",
                        "properties": { "Source": "animate\r\n" },
                        "children": []
                    }]
                },
                {
                    "class": "Folder",
                    "name": "StudioOnlyEmpty",
                    "children": []
                }
            ]
        })];

        let report = initial_snapshot_comparison(d.path(), &studio).unwrap();
        assert!(
            report.is_clean(),
            "initial compare should match Studio pass-through containers and ignore empty folders: {report:?}"
        );
    }

    #[test]
    fn initial_snapshot_compare_accepts_reordered_duplicate_siblings() {
        let d = TempDir::new("initial-duplicate-order");
        let packages = d.path().join("ReplicatedStorage").join("Packages");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::write(packages.join("Net.luau"), "return 'Net'\n").unwrap();
        std::fs::write(packages.join("net.lua"), "return 'net'\n").unwrap();

        let sell_npc = d.path().join("Workspace").join("SellNPC");
        std::fs::create_dir_all(&sell_npc).unwrap();
        std::fs::write(sell_npc.join("Animate.client.luau"), "animate\n").unwrap();
        let sell_npc_duplicate = d
            .path()
            .join("Workspace")
            .join("SellNPC [1]")
            .join("HumanoidRootPart");
        std::fs::create_dir_all(&sell_npc_duplicate).unwrap();
        std::fs::write(
            sell_npc_duplicate.join("DialogueDemo.client.luau"),
            "dialogue\n",
        )
        .unwrap();

        let studio = vec![
            json!({
                "class": "ReplicatedStorage",
                "name": "ReplicatedStorage",
                "properties": {},
                "children": [{
                    "class": "Folder",
                    "name": "Packages",
                    "properties": {},
                    "children": [
                        { "class": "ModuleScript", "name": "net", "properties": { "Source": "return 'net'\n" }, "children": [] },
                        { "class": "ModuleScript", "name": "Net", "properties": { "Source": "return 'Net'\n" }, "children": [] }
                    ]
                }]
            }),
            json!({
                "class": "Workspace",
                "name": "Workspace",
                "properties": {},
                "children": [
                    { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                        { "class": "Folder", "name": "HumanoidRootPart", "properties": {}, "children": [
                            { "class": "LocalScript", "name": "DialogueDemo", "properties": { "Source": "dialogue\n" }, "children": [] }
                        ] }
                    ] },
                    { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                        { "class": "LocalScript", "name": "Animate", "properties": { "Source": "animate\n" }, "children": [] }
                    ] }
                ]
            }),
        ];

        assert!(initial_snapshots_match(d.path(), &studio).unwrap());
    }

    #[test]
    fn initial_snapshot_compare_detects_real_source_change() {
        let d = TempDir::new("initial-diff");
        let sss = d.path().join("ServerScriptService");
        std::fs::create_dir_all(&sss).unwrap();
        std::fs::write(sss.join("Main.server.luau"), "print('disk')\n").unwrap();

        let studio = vec![json!({
            "class": "ServerScriptService",
            "name": "ServerScriptService",
            "properties": {},
            "children": [{
                "class": "Script",
                "name": "Main",
                "properties": { "Source": "print('studio')\n" },
                "children": []
            }]
        })];

        assert!(!initial_snapshots_match(d.path(), &studio).unwrap());
    }

    #[test]
    fn initial_snapshot_comparison_groups_changes_and_ignores_unsynced_junk() {
        let d = TempDir::new("initial-summary");
        std::fs::write(d.path().join("notes.md"), "not synced").unwrap();
        std::fs::create_dir_all(d.path().join("Plans")).unwrap();
        std::fs::write(d.path().join("Plans").join("Loose.luau"), "return 'junk'").unwrap();

        let rs = d.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&rs).unwrap();
        std::fs::write(rs.join("Config.luau"), "return 'disk'\n").unwrap();
        std::fs::write(rs.join("LocalOnly.luau"), "return true\n").unwrap();

        let studio = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [
                {
                    "class": "ModuleScript",
                    "name": "Config",
                    "properties": { "Source": "return 'studio'\n" },
                    "children": []
                },
                {
                    "class": "ModuleScript",
                    "name": "StudioOnly",
                    "properties": { "Source": "return false\n" },
                    "children": []
                }
            ]
        })];

        let report = initial_snapshot_comparison(d.path(), &studio).unwrap();
        assert_eq!(report.summary.new_files, 1);
        assert_eq!(report.new_files[0].path, "ReplicatedStorage/LocalOnly");
        assert_eq!(report.summary.changed_files, 1);
        assert_eq!(report.changed_files[0].path, "ReplicatedStorage/Config");
        assert_eq!(report.summary.removed_files, 1);
        assert_eq!(report.removed_files[0].path, "ReplicatedStorage/StudioOnly");

        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("notes.md"));
        assert!(!json.contains("Plans"));
        assert!(!json.contains("Loose"));
    }

    #[tokio::test]
    async fn initial_choice_status_replays_the_full_comparison() {
        let project = TempDir::new("initial-choice-replay");
        let state = test_state(&project, None);
        *state.pending_initial.lock().unwrap() = Some(PendingInitial {
            choice_id: "choice-replay".into(),
            disk_stats: Stats {
                script_count: 2,
                instance_count: 3,
            },
            studio_stats: Stats {
                script_count: 4,
                instance_count: 5,
            },
            choice: None,
            allowed_disk_paths: vec!["ReplicatedStorage/Config".into()],
            selected_disk_paths: None,
            comparison: Some(json!({
                "summary": { "newFiles": 0, "changedFiles": 1, "removedFiles": 0 },
                "newFiles": [],
                "changedFiles": [{
                    "path": "ReplicatedStorage/Config",
                    "kind": "script",
                    "localClass": "ModuleScript",
                    "studioClass": "ModuleScript",
                    "classChanged": false,
                    "sourceChanged": true
                }],
                "removedFiles": []
            })),
        });

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/initial-choice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["pending"], true);
        assert_eq!(body["choiceId"], "choice-replay");
        assert_eq!(
            body["comparison"]["changedFiles"][0]["path"],
            "ReplicatedStorage/Config"
        );
    }

    #[test]
    fn selective_initial_snapshot_contains_only_chosen_disk_changes() {
        let d = TempDir::new("initial-selective");
        let feature = d.path().join("ReplicatedStorage").join("Feature");
        std::fs::create_dir_all(&feature).unwrap();
        std::fs::write(feature.join("Chosen.luau"), "return 'chosen'\n").unwrap();
        std::fs::write(feature.join("Untouched.luau"), "return 'untouched'\n").unwrap();

        let payload = build_selective_snapshot(
            d.path(),
            &[
                "ReplicatedStorage/Feature/Chosen".into(),
                "StarterGui/StudioOnly".into(),
            ],
        )
        .unwrap();
        let ops = payload["ops"].as_array().unwrap();

        assert_eq!(ops.len(), 3, "one parent shell, one set, one delete");
        let parent = ops
            .iter()
            .find(|op| op["node"]["name"] == "Feature")
            .unwrap();
        let chosen = ops
            .iter()
            .find(|op| op["node"]["name"] == "Chosen")
            .unwrap();
        let removed = ops.iter().find(|op| op["op"] == "delete").unwrap();
        assert_eq!(parent["op"], "ensure");
        assert_eq!(parent["node"]["children"], json!([]));
        assert_eq!(parent["node"]["properties"], json!({}));
        assert_eq!(chosen["node"]["name"], "Chosen");
        assert_eq!(chosen["node"]["properties"]["Source"], "return 'chosen'\n");
        assert_eq!(chosen["forcePrune"], true);
        assert_eq!(removed["path"], json!(["StarterGui", "StudioOnly"]));
        assert!(
            !payload.to_string().contains("untouched"),
            "an unselected sibling source must not cross into Studio"
        );
    }

    #[test]
    fn selective_initial_snapshot_parent_selection_subsumes_children() {
        let d = TempDir::new("initial-selective-tree");
        let feature = d.path().join("Workspace").join("Feature");
        std::fs::create_dir_all(&feature).unwrap();
        std::fs::write(feature.join("Child.server.luau"), "print('child')\n").unwrap();

        let payload = build_selective_snapshot(
            d.path(),
            &["Workspace/Feature/Child".into(), "Workspace/Feature".into()],
        )
        .unwrap();
        let ops = payload["ops"].as_array().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0]["node"]["name"], "Feature");
        assert_eq!(ops[0]["node"]["children"][0]["name"], "Child");
        assert_eq!(payload["selectedPaths"], json!(["Workspace/Feature"]));
    }

    #[test]
    fn initial_disk_selection_rejects_paths_not_shown_by_the_compare() {
        let allowed = vec![
            "ReplicatedStorage/Config".to_string(),
            "Workspace/Feature".to_string(),
        ];
        let selected = normalize_initial_disk_selection(
            vec![
                "Workspace/Feature".into(),
                "ReplicatedStorage/Config".into(),
                "Workspace/Feature".into(),
            ],
            &allowed,
        )
        .unwrap();
        assert_eq!(
            selected,
            vec!["ReplicatedStorage/Config", "Workspace/Feature"]
        );
        assert!(normalize_initial_disk_selection(vec![], &allowed).is_err());
        assert!(
            normalize_initial_disk_selection(vec!["Workspace/Other".into()], &allowed).is_err()
        );
    }

    #[test]
    fn initial_snapshot_comparison_ignores_avoid_sync_local_subtree() {
        let d = TempDir::new("initial-avoid-sync");
        let ignored = d.path().join("Workspace").join("Ignored");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("LocalOnly.server.luau"), "print('local')\n").unwrap();

        let studio = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [{
                "class": "Folder",
                "name": "Ignored",
                "properties": {},
                "avoidSync": true,
                "children": []
            }]
        })];

        let report = initial_snapshot_comparison(d.path(), &studio).unwrap();
        assert!(report.is_clean());
    }

    // `writelog` reads a test-only home override at call-time, so pointing it
    // at a TempDir completely contains the side effects. Environment mutation
    // is process-global though, so the writelog tests serialize on this mutex.
    static WRITELOG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn writes_log_paths(fake_home: &Path) -> (PathBuf, PathBuf) {
        let dir = fake_home.join(".terminal64/widgets/ro-sync");
        (dir.join("writes.log"), dir.join("writes.log.1"))
    }

    #[test]
    fn writelog_appends_under_fake_home() {
        let _guard = WRITELOG_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let d = TempDir::new("writelog-append");
        std::env::set_var("ROSYNC_TEST_HOME", d.path());
        let (log, _rot) = writes_log_paths(d.path());
        let resp = write_log_entry(Json(json!({ "op": "set", "ok": true })));
        assert_eq!(resp.0["ok"], true, "writelog should succeed");
        let body = std::fs::read_to_string(&log).unwrap();
        // Exactly one JSONL line, and it should carry a `ts` field we merged in.
        let line_count = body.lines().count();
        assert_eq!(line_count, 1, "one append = one line");
        let parsed: Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(parsed["op"], "set");
        assert!(parsed["ts"].is_u64());
    }

    #[test]
    fn writelog_rotates_when_over_10mib() {
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

        let resp = write_log_entry(Json(json!({ "op": "set", "ok": true })));
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

    #[test]
    fn writelog_rotation_overwrites_prior_generation() {
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

        let resp = write_log_entry(Json(json!({ "op": "eval", "ok": true })));
        assert_eq!(resp.0["ok"], true);

        // The .1 file must now start with NEW_ROTATION — old generation gone.
        let rotated_body = std::fs::read(&rotated).unwrap();
        assert!(
            rotated_body.starts_with(b"NEW_ROTATION"),
            "writes.log.1 should be overwritten by the prior writes.log"
        );
    }
}
