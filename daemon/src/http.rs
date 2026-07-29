use axum::body::Bytes;
use axum::http::{header, request::Parts, HeaderValue, Method, StatusCode};
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
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
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
    path_to_instance_meta, portable_init_file_name, InstanceDescriptor, PathFragmentAllocator,
    PathInstance, ScriptClass, META_FILE,
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
use crate::initial_sync::{
    compute_disk_stats, new_choice_id, Choice, InitialChoiceAction, InitialChoiceItem,
    InitialChoiceSummary, InitialSelectionAccumulator, InitialSelectionReceipt, PendingInitial,
    Stats,
};
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

    // Protocol 5 streams large payloads through route-specific bounded chunks.
    // Keep every unclassified localhost route well below whole-place size.
    const MAX_BODY: usize = 4 * 1024 * 1024;

    const ARTIFACT_CONTROL_BODY: usize = 4 * 1024;
    const ARTIFACT_CHUNK_BODY: usize = 768 * 1024;
    const PROJECT_INIT_BODY: usize = 16 * 1024;
    // Protocol-5 bootstrap records are deliberately bounded. Keep malformed
    // requests from allocating against the much larger legacy snapshot cap
    // before record-count validation gets a chance to run.
    const INITIAL_COMPARE_BODY: usize = STREAM_REQUEST_BODY_BYTES;
    const INITIAL_CHOICE_BODY: usize = 16 * 1024;
    const INITIAL_SELECTION_BODY: usize = 64 * 1024;
    const STREAM_PUSH_BODY: usize = STREAM_REQUEST_BODY_BYTES;
    const SNAPSHOT_STREAM_BODY: usize = 1024 * 1024;

    Router::new()
        .route("/hello", get(hello))
        .route(
            "/projects/init",
            post(project_init).layer(DefaultBodyLimit::max(PROJECT_INIT_BODY)),
        )
        .route("/snapshot", get(snapshot))
        .route(
            "/snapshot/stream",
            post(snapshot_stream).layer(DefaultBodyLimit::max(SNAPSHOT_STREAM_BODY)),
        )
        .route("/snapshot/selective", post(selective_snapshot))
        .route(
            "/push",
            post(push).layer(DefaultBodyLimit::max(STREAM_PUSH_BODY)),
        )
        .route("/poll", get(poll))
        .route("/events", get(events))
        .route("/ws", get(crate::ws::ws_upgrade))
        .route("/resolve", get(resolve_list).post(resolve))
        .route(
            "/initial-compare",
            post(initial_compare).layer(DefaultBodyLimit::max(INITIAL_COMPARE_BODY)),
        )
        .route("/initial-decision", get(initial_decision))
        .route(
            "/initial-choice",
            get(initial_choice_status)
                .post(initial_choice)
                .layer(DefaultBodyLimit::max(INITIAL_CHOICE_BODY)),
        )
        .route("/initial-choice/details", get(initial_choice_details))
        .route(
            "/initial-choice/selection",
            post(initial_choice_selection).layer(DefaultBodyLimit::max(INITIAL_SELECTION_BODY)),
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
    #[serde(rename = "expectedBootId")]
    expected_boot_id: Option<String>,
    #[serde(rename = "expectedPid")]
    expected_pid: Option<u32>,
    #[serde(rename = "expectedPort")]
    expected_port: Option<u16>,
    #[serde(rename = "expectedCanonicalProject")]
    expected_canonical_project: Option<String>,
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

fn authorize_destructive_identity(
    state: &AppState,
    body: &WidgetControlBody,
) -> Result<(), &'static str> {
    let (
        Some(expected_boot_id),
        Some(expected_pid),
        Some(expected_port),
        Some(expected_canonical_project),
    ) = (
        body.expected_boot_id.as_deref(),
        body.expected_pid,
        body.expected_port,
        body.expected_canonical_project.as_deref(),
    )
    else {
        return Err("missing exact daemon close identity");
    };
    let current_canonical_project = state.canonical_project.display().to_string();
    if expected_boot_id != state.boot_id.as_str()
        || expected_pid != state.process_id
        || expected_port != state.listen_port
        || expected_canonical_project != current_canonical_project
    {
        return Err("daemon lifecycle identity changed");
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
    if let Err(error) = authorize_destructive_identity(&state, &body) {
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
    if let Err(error) = authorize_destructive_identity(&state, &body) {
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
    let paths = match compact_avoid_sync_paths(&tree) {
        Ok(Some(paths)) => paths,
        Ok(None) => {
            // Protocol compatibility: older Studio plugins posted the entire
            // class/name skeleton even though the daemon only retained
            // AvoidSync roots from it.
            if let Err(error) = validate_full_tree_value(&tree) {
                return Json(json!({ "ok": false, "error": format!("tree: {error}") }));
            }
            let mut paths = Vec::new();
            collect_avoid_sync_paths(&tree, &[], &mut paths);
            paths
        }
        Err(error) => return Json(json!({ "ok": false, "error": error })),
    };
    set_avoid_sync_paths(root, paths.clone());
    Json(json!({ "ok": true, "bytes": bytes, "avoidSyncPaths": paths.len() }))
}

fn compact_avoid_sync_paths(tree: &Value) -> Result<Option<Vec<Vec<String>>>, String> {
    let Some(raw_paths) = tree.get("avoidSyncPaths") else {
        return Ok(None);
    };
    let paths = raw_paths
        .as_array()
        .ok_or("compact tree avoidSyncPaths must be an array")?;
    const MAX_AVOID_SYNC_PATHS: usize = 100_000;
    const MAX_AVOID_SYNC_DEPTH: usize = 256;
    const MAX_AVOID_SYNC_PATH_BYTES: usize = 4096;
    if paths.len() > MAX_AVOID_SYNC_PATHS {
        return Err(format!(
            "compact tree avoidSyncPaths exceeds {MAX_AVOID_SYNC_PATHS} entries"
        ));
    }

    let mut out = Vec::with_capacity(paths.len());
    for raw_path in paths {
        let segments = raw_path
            .as_array()
            .ok_or("compact tree path must be an array of strings")?;
        if segments.is_empty() || segments.len() > MAX_AVOID_SYNC_DEPTH {
            return Err(format!(
                "compact tree path depth must be between 1 and {MAX_AVOID_SYNC_DEPTH}"
            ));
        }
        let mut path = Vec::with_capacity(segments.len());
        let mut path_bytes = 0usize;
        for segment in segments {
            let segment = segment
                .as_str()
                .ok_or("compact tree path segments must be strings")?;
            if segment.is_empty() || segment.as_bytes().contains(&0) {
                return Err("compact tree path segments must be non-empty and NUL-free".into());
            }
            path_bytes = path_bytes.saturating_add(segment.len());
            if path_bytes > MAX_AVOID_SYNC_PATH_BYTES {
                return Err(format!(
                    "compact tree path exceeds {MAX_AVOID_SYNC_PATH_BYTES} bytes"
                ));
            }
            path.push(segment.to_string());
        }
        if !snapshot::SYNCED_SERVICES.contains(&path[0].as_str()) {
            return Err(format!(
                "compact tree path is outside a synced service: {}",
                path.join("/")
            ));
        }
        out.push(path);
    }
    out.sort();
    out.dedup();
    Ok(Some(out))
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

#[derive(Deserialize, Serialize)]
struct InitialCompareBody {
    #[serde(rename = "studioStats")]
    studio_stats: Stats,
    #[serde(rename = "studioSnapshot", default)]
    studio_snapshot: Vec<Value>,
    #[serde(rename = "compareId", default)]
    compare_id: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(rename = "pluginProtocol", default)]
    plugin_protocol: Option<u64>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(rename = "chunkIndex", default)]
    chunk_index: Option<u64>,
    #[serde(rename = "finalChunk", default)]
    final_chunk: bool,
    #[serde(default)]
    records: Vec<snapshot::FlatSnapshotRecord>,
    #[serde(default)]
    hashes: Vec<StreamSourceHash>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamSourceHash {
    id: u64,
    sha256: String,
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

    fn merge(&mut self, mut other: Self) {
        self.new_files.append(&mut other.new_files);
        self.changed_files.append(&mut other.changed_files);
        self.removed_files.append(&mut other.removed_files);
        self.summary = InitialComparisonSummary {
            new_files: self.new_files.len(),
            changed_files: self.changed_files.len(),
            removed_files: self.removed_files.len(),
        };
    }
}

impl Default for InitialComparison {
    fn default() -> Self {
        Self {
            summary: InitialComparisonSummary {
                new_files: 0,
                changed_files: 0,
                removed_files: 0,
            },
            new_files: Vec::new(),
            changed_files: Vec::new(),
            removed_files: Vec::new(),
        }
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

fn initial_choice_details_from_comparison(
    comparison: &InitialComparison,
) -> Result<Vec<InitialChoiceItem>, String> {
    let total = comparison
        .new_files
        .len()
        .checked_add(comparison.changed_files.len())
        .and_then(|count| count.checked_add(comparison.removed_files.len()))
        .ok_or("initial divergence detail count exceeds usize")?;
    if total > u32::MAX as usize {
        return Err("initial divergence has more than u32::MAX paths".into());
    }

    let mut details = Vec::with_capacity(total);
    details.extend(comparison.new_files.iter().map(|item| InitialChoiceItem {
        id: 0,
        action: InitialChoiceAction::Create,
        path: item.path.clone(),
        kind: initial_choice_kind(item.kind),
        class: Some(item.class.clone()),
        local_class: None,
        studio_class: None,
        class_changed: false,
        source_changed: false,
    }));
    details.extend(
        comparison
            .changed_files
            .iter()
            .map(|item| InitialChoiceItem {
                id: 0,
                action: InitialChoiceAction::Overwrite,
                path: item.path.clone(),
                kind: initial_choice_kind(item.kind),
                class: None,
                local_class: Some(item.local_class.clone()),
                studio_class: Some(item.studio_class.clone()),
                class_changed: item.class_changed,
                source_changed: item.source_changed,
            }),
    );
    details.extend(
        comparison
            .removed_files
            .iter()
            .map(|item| InitialChoiceItem {
                id: 0,
                action: InitialChoiceAction::Remove,
                path: item.path.clone(),
                kind: initial_choice_kind(item.kind),
                class: Some(item.class.clone()),
                local_class: None,
                studio_class: None,
                class_changed: false,
                source_changed: false,
            }),
    );
    details.sort_by(|left, right| {
        left.path.cmp(&right.path).then_with(|| {
            initial_choice_action_rank(left.action).cmp(&initial_choice_action_rank(right.action))
        })
    });
    if details
        .windows(2)
        .any(|window| window[0].path == window[1].path)
    {
        return Err("initial divergence contains a duplicate path".into());
    }
    for (index, detail) in details.iter_mut().enumerate() {
        detail.id = u32::try_from(index).map_err(|_| "initial divergence detail ID exceeds u32")?;
    }
    Ok(details)
}

fn initial_choice_kind(kind: diff::DiffKind) -> String {
    match kind {
        diff::DiffKind::Folder => "folder",
        diff::DiffKind::Script => "script",
    }
    .into()
}

fn initial_choice_action_rank(action: InitialChoiceAction) -> u8 {
    match action {
        InitialChoiceAction::Create => 0,
        InitialChoiceAction::Overwrite => 1,
        InitialChoiceAction::Remove => 2,
    }
}

struct InitialCompareAccumulator {
    compare_id: String,
    disk_stats: Stats,
    studio_stats: Stats,
    next_service: usize,
    comparison: InitialComparison,
    staged_baselines: Vec<StagedScriptBaseline>,
    staged_service_generations: Vec<crate::fs_safety::TreeGeneration>,
    service_stream: Option<InitialCompareServiceStream>,
    last_service: Option<String>,
    last_request_hash: Option<crate::conflict::Hash>,
    last_response: Option<Value>,
    pending_choice_id: Option<String>,
    accepted_stream_bytes: usize,
    started_at: Instant,
    completed_at: Option<Instant>,
}

enum InitialCompareStreamPhase {
    Structure,
    DiskPrepare,
    Hashes,
}

type InitialComparePrepareResult = (
    Vec<snapshot::FlatSnapshotRecord>,
    Result<PreparedStreamedComparison, String>,
);

struct InitialCompareServiceStream {
    service: String,
    phase: InitialCompareStreamPhase,
    next_chunk: u64,
    records: Vec<snapshot::FlatSnapshotRecord>,
    accepted_structure_bytes: usize,
    final_structure_len: usize,
    final_structure_bytes: usize,
    final_structure_chunk: u64,
    prepare_result: Option<std::sync::mpsc::Receiver<InitialComparePrepareResult>>,
    local_nodes: Option<BTreeMap<String, diff::DiffNode>>,
    local_source_paths_by_path: HashMap<String, PathBuf>,
    studio_nodes: Option<BTreeMap<String, diff::DiffNode>>,
    studio_paths_by_id: HashMap<u64, String>,
    expected_hash_ids: HashSet<u64>,
    received_hash_ids: HashSet<u64>,
}

struct StagedScriptBaseline {
    path: PathBuf,
    source_hash: crate::conflict::Hash,
    fs_mtime: u64,
    generation: crate::fs_safety::FileGeneration,
}

struct StagedComparisonState {
    baselines: Vec<StagedScriptBaseline>,
    service_generations: Vec<crate::fs_safety::TreeGeneration>,
}

static INITIAL_COMPARE_ACCUMULATORS: OnceLock<
    Mutex<HashMap<PathBuf, Arc<Mutex<InitialCompareAccumulator>>>>,
> = OnceLock::new();
struct SelectiveTransferGrant {
    paths: Vec<String>,
    created_at: Instant,
}
static SELECTIVE_TRANSFER_GRANTS: OnceLock<
    Mutex<HashMap<(PathBuf, String), SelectiveTransferGrant>>,
> = OnceLock::new();
const INITIAL_COMPARE_SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const INITIAL_COMPARE_COMPLETED_TTL: Duration = Duration::from_secs(2 * 60);
const SELECTIVE_TRANSFER_GRANT_TTL: Duration = Duration::from_secs(2 * 60);

fn initial_compare_session_expired(session: &InitialCompareAccumulator) -> bool {
    match session.completed_at {
        Some(completed_at) => completed_at.elapsed() >= INITIAL_COMPARE_COMPLETED_TTL,
        None => session.started_at.elapsed() >= INITIAL_COMPARE_SESSION_TTL,
    }
}

fn prune_initial_compare_sessions(
    sessions: &mut HashMap<PathBuf, Arc<Mutex<InitialCompareAccumulator>>>,
) {
    sessions.retain(|_, session| {
        session
            .try_lock()
            .map(|session| !initial_compare_session_expired(&session))
            .unwrap_or(true)
    });
}

fn schedule_initial_compare_cleanup(
    project: PathBuf,
    session: &Arc<Mutex<InitialCompareAccumulator>>,
    wake_after: Duration,
) {
    // A weak handle is important here: once the map entry is removed, an old
    // timer must not keep a completed response (and its divergence metadata)
    // alive until the abandoned-session deadline.
    let session = Arc::downgrade(session);
    tokio::spawn(async move {
        tokio::time::sleep(wake_after).await;
        loop {
            let Some(session) = session.upgrade() else {
                return;
            };
            let remaining = {
                let session = session.lock().unwrap();
                match session.completed_at {
                    Some(completed_at) => {
                        INITIAL_COMPARE_COMPLETED_TTL.saturating_sub(completed_at.elapsed())
                    }
                    None => {
                        INITIAL_COMPARE_SESSION_TTL.saturating_sub(session.started_at.elapsed())
                    }
                }
            };
            if !remaining.is_zero() {
                drop(session);
                tokio::time::sleep(remaining).await;
                continue;
            }

            // Keep the established lock order (session, then map). Stats-first
            // pruning uses try_lock while holding the map, so it cannot deadlock
            // with a final response or this expiry path.
            let session_guard = session.lock().unwrap();
            if !initial_compare_session_expired(&session_guard) {
                continue;
            }
            let mut sessions = INITIAL_COMPARE_ACCUMULATORS
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap();
            if sessions
                .get(&project)
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                sessions.remove(&project);
            }
            return;
        }
    });
}

fn clear_completed_initial_compare_for_choice(project: &Path, choice_id: &str) {
    let session = INITIAL_COMPARE_ACCUMULATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(project)
        .cloned();
    let Some(session) = session else {
        return;
    };
    let session_guard = session.lock().unwrap();
    if session_guard.completed_at.is_none()
        || session_guard.pending_choice_id.as_deref() != Some(choice_id)
    {
        return;
    }
    let mut sessions = INITIAL_COMPARE_ACCUMULATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if sessions
        .get(project)
        .is_some_and(|current| Arc::ptr_eq(current, &session))
    {
        sessions.remove(project);
    }
}

struct InitialCompareHashWriter(Sha256);

impl std::io::Write for InitialCompareHashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn initial_compare_request_hash<T: Serialize>(
    value: &T,
) -> Result<crate::conflict::Hash, serde_json::Error> {
    let mut writer = InitialCompareHashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0.finalize().into())
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
    if body.compare_id.is_some() || body.service.is_some() {
        return initial_compare_service_chunk(&state, body);
    }

    let stats_root = state.canonical_project.clone();
    let disk_stats =
        match tokio::task::spawn_blocking(move || compute_disk_stats(stats_root.as_path())).await {
            Ok(Ok(stats)) => stats,
            Ok(Err(error)) => {
                return Json(json!({
                    "ok": false,
                    "error": format!("scan: {error}"),
                }));
            }
            Err(error) => {
                return Json(json!({
                    "ok": false,
                    "error": format!("scan worker failed: {error}"),
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
        let compare_id = new_choice_id();
        let project = state.canonical_project.as_ref().clone();
        let mut sessions = INITIAL_COMPARE_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        prune_initial_compare_sessions(&mut sessions);
        let session = Arc::new(Mutex::new(InitialCompareAccumulator {
            compare_id: compare_id.clone(),
            disk_stats,
            studio_stats: body.studio_stats,
            next_service: 0,
            comparison: InitialComparison::default(),
            staged_baselines: Vec::new(),
            staged_service_generations: Vec::new(),
            service_stream: None,
            last_service: None,
            last_request_hash: None,
            last_response: None,
            pending_choice_id: None,
            accepted_stream_bytes: 0,
            started_at: Instant::now(),
            completed_at: None,
        }));
        sessions.insert(project.clone(), session.clone());
        drop(sessions);
        schedule_initial_compare_cleanup(project, &session, INITIAL_COMPARE_SESSION_TTL);
        return Json(json!({
            // Compare one service per request. This bounds Studio JSON/source
            // memory and lets the daemon discard each tree after retaining
            // only its small divergence metadata.
            "action": "compare",
            "compareId": compare_id,
            "services": snapshot::SYNCED_SERVICES,
            "nextService": snapshot::SYNCED_SERVICES[0],
            "phase": "structure",
            "nextChunk": 0,
            "diskStats": disk_stats,
        }));
    }

    // Protocol 5 is mandatory above. Retain the monolithic shape only for
    // early protocol-5 clients that already supplied it; current clients are
    // always instructed to use bounded service/chunk streaming.
    match initial_snapshot_comparison(state.canonical_project.as_path(), &body.studio_snapshot) {
        Ok(comparison) => {
            finish_initial_comparison(&state, disk_stats, body.studio_stats, comparison, None)
        }
        Err(error) => Json(json!({
            "ok": false,
            "error": format!("snapshot compare: {error}"),
        })),
    }
}

fn initial_compare_service_chunk(state: &AppState, body: InitialCompareBody) -> Json<Value> {
    let Some(compare_id) = body.compare_id.as_deref() else {
        return Json(json!({
            "ok": false,
            "error": "service comparison requires compareId",
        }));
    };
    let Some(service) = body.service.as_deref() else {
        return Json(json!({
            "ok": false,
            "error": "service comparison requires service",
        }));
    };
    let streamed = body.phase.is_some();
    if !streamed
        && (body.studio_snapshot.len() != 1
            || body.studio_snapshot[0].get("name").and_then(Value::as_str) != Some(service))
    {
        return Json(json!({
            "ok": false,
            "error": format!(
                "initial comparison service {service} requires exactly one matching service node"
            ),
        }));
    }
    if streamed && !body.studio_snapshot.is_empty() {
        return Json(json!({
            "ok": false,
            "error": "streamed comparison chunks cannot include a nested studioSnapshot",
        }));
    }
    let request_hash = match initial_compare_request_hash(&body) {
        Ok(request_hash) => request_hash,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": format!("encode initial comparison service {service}: {error}"),
            }));
        }
    };

    let project = state.canonical_project.as_ref().clone();
    let session_handle = INITIAL_COMPARE_ACCUMULATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(&project)
        .cloned();
    let Some(session_handle) = session_handle else {
        return Json(json!({
            "ok": false,
            "error": "initial comparison session is stale; restart the stats scan",
        }));
    };
    let mut session = session_handle.lock().unwrap();
    if initial_compare_session_expired(&session) {
        return Json(json!({
            "ok": false,
            "error": "initial comparison session expired; restart the stats scan",
        }));
    }
    if session.compare_id != compare_id {
        return Json(json!({
            "ok": false,
            "error": "initial comparison compareId is stale",
        }));
    }

    // RequestAsync can time out after the daemon committed a service response.
    // Replay only the exact immediately preceding service body; a changed or
    // out-of-order duplicate remains an error and cannot double-count metadata.
    if session.last_service.as_deref() == Some(service)
        && session.last_request_hash == Some(request_hash)
    {
        if let Some(response) = session.last_response.clone() {
            return Json(response);
        }
    }

    let Some(expected_service) = snapshot::SYNCED_SERVICES.get(session.next_service).copied()
    else {
        return Json(json!({
            "ok": false,
            "error": "initial comparison session already completed",
        }));
    };
    if service != expected_service {
        return Json(json!({
            "ok": false,
            "error": format!(
                "initial comparison expected service {expected_service}, received {service}"
            ),
        }));
    }

    let response = if streamed {
        match process_streamed_initial_compare_chunk(
            state,
            &mut session,
            &body,
            service,
            &project,
            &session_handle,
        ) {
            Ok(response) => response,
            Err(error) => {
                return Json(json!({
                    "ok": false,
                    "error": format!("streamed snapshot compare {service}: {error}"),
                }));
            }
        }
    } else {
        let service_comparison = match initial_service_snapshot_comparison(
            state.canonical_project.as_path(),
            &body.studio_snapshot[0],
        ) {
            Ok(comparison) => comparison,
            Err(error) => {
                return Json(json!({
                    "ok": false,
                    "error": format!("snapshot compare {service}: {error}"),
                }));
            }
        };
        session.comparison.merge(service_comparison);
        session.next_service += 1;
        session.started_at = Instant::now();
        match next_initial_compare_response(
            state,
            &mut session,
            compare_id,
            &project,
            &session_handle,
            false,
        ) {
            Ok(response) => response,
            Err(error) => return Json(json!({ "ok": false, "error": error })),
        }
    };
    if !INITIAL_COMPARE_ACCUMULATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(&project)
        .is_some_and(|current| Arc::ptr_eq(current, &session_handle))
    {
        return Json(json!({
            "ok": false,
            "error": "initial comparison was superseded by a newer Studio scan",
        }));
    }
    session.last_service = Some(service.to_string());
    session.last_request_hash = Some(request_hash);
    session.pending_choice_id = response
        .get("choiceId")
        .and_then(Value::as_str)
        .map(str::to_string);
    session.last_response = Some(response.clone());
    if session.next_service == snapshot::SYNCED_SERVICES.len() {
        session.completed_at = Some(Instant::now());
        schedule_initial_compare_cleanup(project, &session_handle, INITIAL_COMPARE_COMPLETED_TTL);
    }
    Json(response)
}

fn next_initial_compare_response(
    state: &AppState,
    session: &mut InitialCompareAccumulator,
    compare_id: &str,
    project: &Path,
    session_handle: &Arc<Mutex<InitialCompareAccumulator>>,
    compact_comparison: bool,
) -> Result<Value, String> {
    if let Some(next_service) = snapshot::SYNCED_SERVICES.get(session.next_service).copied() {
        return Ok(if compact_comparison {
            json!({
                "action": "compare",
                "compareId": compare_id,
                "nextService": next_service,
                "phase": "structure",
                "nextChunk": 0,
            })
        } else {
            json!({
                "action": "compare",
                "compareId": compare_id,
                "nextService": next_service,
            })
        });
    }

    let sessions = INITIAL_COMPARE_ACCUMULATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if !sessions
        .get(project)
        .is_some_and(|current| Arc::ptr_eq(current, session_handle))
    {
        return Err("initial comparison was superseded by a newer Studio scan".into());
    }
    let comparison = std::mem::take(&mut session.comparison);
    let staged_comparison = StagedComparisonState {
        baselines: std::mem::take(&mut session.staged_baselines),
        service_generations: std::mem::take(&mut session.staged_service_generations),
    };
    let response = finish_initial_comparison(
        state,
        session.disk_stats,
        session.studio_stats,
        comparison,
        compact_comparison.then_some(staged_comparison),
    )
    .0;
    Ok(response)
}

fn process_streamed_initial_compare_chunk(
    state: &AppState,
    session: &mut InitialCompareAccumulator,
    body: &InitialCompareBody,
    service: &str,
    project: &Path,
    session_handle: &Arc<Mutex<InitialCompareAccumulator>>,
) -> Result<Value, String> {
    let phase = body
        .phase
        .as_deref()
        .ok_or("streamed comparison chunk is missing phase")?;
    let chunk_index = body
        .chunk_index
        .ok_or("streamed comparison chunk is missing chunkIndex")?;
    if session.service_stream.is_none() {
        if phase != "structure" || chunk_index != 0 {
            return Err("a service stream must begin with structure chunk 0".into());
        }
        session.service_stream = Some(InitialCompareServiceStream {
            service: service.to_string(),
            phase: InitialCompareStreamPhase::Structure,
            next_chunk: 0,
            records: Vec::new(),
            accepted_structure_bytes: 0,
            final_structure_len: 0,
            final_structure_bytes: 0,
            final_structure_chunk: 0,
            prepare_result: None,
            local_nodes: None,
            local_source_paths_by_path: HashMap::new(),
            studio_nodes: None,
            studio_paths_by_id: HashMap::new(),
            expected_hash_ids: HashSet::new(),
            received_hash_ids: HashSet::new(),
        });
    }
    let stream = session
        .service_stream
        .as_mut()
        .expect("service stream was initialized");
    if stream.service != service {
        return Err(format!(
            "active service stream is {}, not {service}",
            stream.service
        ));
    }
    if chunk_index != stream.next_chunk {
        return Err(format!(
            "streamed {service} {phase} expected chunk {}, received {chunk_index}",
            stream.next_chunk
        ));
    }

    match stream.phase {
        InitialCompareStreamPhase::Structure => {
            if phase != "structure" {
                return Err("service structure must finish before hashes begin".into());
            }
            if !body.hashes.is_empty() || !body.studio_snapshot.is_empty() {
                return Err("structure chunks may contain only flat records".into());
            }
            validate_stream_record_chunk_fields(&body.records)?;
            let chunk_bytes = encoded_stream_record_chunk_bytes(&body.records)?;
            if body.records.len() > STREAM_STRUCTURE_CHUNK_NODES {
                return Err(format!(
                    "structure chunks are limited to {STREAM_STRUCTURE_CHUNK_NODES} nodes"
                ));
            }
            if stream
                .records
                .len()
                .checked_add(body.records.len())
                .is_none_or(|count| count > MAX_BOOTSTRAP_NODES)
            {
                return Err(format!(
                    "streamed service contains more than the supported limit of {MAX_BOOTSTRAP_NODES} instances"
                ));
            }
            for (offset, record) in body.records.iter().enumerate() {
                let expected_id = (stream.records.len() + offset) as u64;
                if record.id != expected_id {
                    return Err(format!(
                        "structure IDs must be dense across chunks; expected {expected_id}, received {}",
                        record.id
                    ));
                }
            }
            let (service_bytes, session_bytes) = charge_stream_structure_bytes(
                stream.accepted_structure_bytes,
                session.accepted_stream_bytes,
                chunk_bytes,
            )?;
            stream.accepted_structure_bytes = service_bytes;
            session.accepted_stream_bytes = session_bytes;
            if !body.final_chunk {
                stream.records.extend(body.records.iter().cloned());
                stream.next_chunk += 1;
                session.started_at = Instant::now();
                return Ok(json!({
                    "action": "compare",
                    "compareId": session.compare_id,
                    "nextService": service,
                    "phase": "structure",
                    "nextChunk": stream.next_chunk,
                }));
            }
            stream.records.extend(body.records.iter().cloned());
            let records = std::mem::take(&mut stream.records);
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let root = state.canonical_project.as_ref().clone();
            let service_name = service.to_string();
            std::thread::spawn(move || {
                let prepared = (|| {
                    let validated = validate_flat_snapshot(&records, &service_name, false)?;
                    prepare_streamed_initial_service_comparison(&root, validated)
                })();
                let _ = send.send((records, prepared));
            });
            session.started_at = Instant::now();
            stream.final_structure_len = body.records.len();
            stream.final_structure_bytes = chunk_bytes;
            stream.final_structure_chunk = chunk_index;
            stream.prepare_result = Some(receive);
            stream.phase = InitialCompareStreamPhase::DiskPrepare;
            stream.next_chunk = 0;
            Ok(json!({
                "action": "compare",
                "compareId": session.compare_id,
                "nextService": service,
                "phase": "diskPrepare",
                "nextChunk": 0,
            }))
        }
        InitialCompareStreamPhase::DiskPrepare => {
            if phase != "diskPrepare"
                || !body.records.is_empty()
                || !body.hashes.is_empty()
                || !body.studio_snapshot.is_empty()
                || body.final_chunk
            {
                return Err("diskPrepare accepts only empty continuation ticks".into());
            }
            let result = stream
                .prepare_result
                .as_ref()
                .ok_or("diskPrepare worker is missing")?
                .try_recv();
            match result {
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    stream.next_chunk += 1;
                    session.started_at = Instant::now();
                    Ok(json!({
                        "action": "compare",
                        "compareId": session.compare_id,
                        "nextService": service,
                        "phase": "diskPrepare",
                        "nextChunk": stream.next_chunk,
                    }))
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err("diskPrepare worker disconnected".into())
                }
                Ok((mut records, Err(error))) => {
                    let retained = records.len().saturating_sub(stream.final_structure_len);
                    records.truncate(retained);
                    stream.records = records;
                    stream.prepare_result = None;
                    stream.final_structure_len = 0;
                    stream.accepted_structure_bytes = stream
                        .accepted_structure_bytes
                        .saturating_sub(stream.final_structure_bytes);
                    session.accepted_stream_bytes = session
                        .accepted_stream_bytes
                        .saturating_sub(stream.final_structure_bytes);
                    stream.final_structure_bytes = 0;
                    stream.phase = InitialCompareStreamPhase::Structure;
                    stream.next_chunk = stream.final_structure_chunk;
                    // The final structure request used this same cursor. A
                    // corrected retry can therefore rebuild the disk fence
                    // without restarting the preceding structure chunks.
                    Err(error)
                }
                Ok((_records, Ok(prepared))) => {
                    stream.prepare_result = None;
                    stream.final_structure_len = 0;
                    stream.final_structure_bytes = 0;
                    stream.final_structure_chunk = 0;
                    stream.local_nodes = Some(prepared.local_nodes);
                    stream.local_source_paths_by_path = prepared.local_source_paths_by_path;
                    stream.studio_nodes = Some(prepared.studio_nodes);
                    stream.studio_paths_by_id = prepared.studio_paths_by_id;
                    stream.expected_hash_ids = prepared.expected_hash_ids;
                    session
                        .staged_service_generations
                        .push(prepared.service_generation);
                    stream.phase = InitialCompareStreamPhase::Hashes;
                    stream.next_chunk = 0;
                    session.started_at = Instant::now();
                    Ok(json!({
                        "action": "compare",
                        "compareId": session.compare_id,
                        "nextService": service,
                        "phase": "hashes",
                        "nextChunk": 0,
                    }))
                }
            }
        }
        InitialCompareStreamPhase::Hashes => {
            if phase != "hashes" {
                return Err("service hash phase cannot return to structure".into());
            }
            if !body.records.is_empty() || !body.studio_snapshot.is_empty() {
                return Err("hash chunks may contain only script hashes".into());
            }
            if body.hashes.len() > STREAM_HASH_CHUNK_NODES {
                return Err(format!(
                    "hash chunks are limited to {STREAM_HASH_CHUNK_NODES} scripts"
                ));
            }
            let studio_nodes = stream
                .studio_nodes
                .as_ref()
                .ok_or("stream hash phase has no validated structure")?;
            let local_nodes = stream
                .local_nodes
                .as_ref()
                .ok_or("stream hash phase has no local comparison")?;
            let mut chunk_ids = HashSet::with_capacity(body.hashes.len());
            let mut validated_hashes = Vec::with_capacity(body.hashes.len());
            for source_hash in &body.hashes {
                if !stream.expected_hash_ids.contains(&source_hash.id) {
                    return Err(format!(
                        "hash references unknown or non-script stream ID {}",
                        source_hash.id
                    ));
                }
                if stream.received_hash_ids.contains(&source_hash.id)
                    || !chunk_ids.insert(source_hash.id)
                {
                    return Err(format!(
                        "hash for stream ID {} was sent more than once",
                        source_hash.id
                    ));
                }
                let path = stream
                    .studio_paths_by_id
                    .get(&source_hash.id)
                    .ok_or_else(|| {
                        format!(
                            "stream ID {} has no projected comparison path",
                            source_hash.id
                        )
                    })?;
                let studio_node = studio_nodes.get(path).ok_or_else(|| {
                    format!(
                        "projected comparison path disappeared for stream ID {}",
                        source_hash.id
                    )
                })?;
                let digest = parse_sha256_hex(&source_hash.sha256)?;
                let local_source_path = local_nodes
                    .get(path)
                    .filter(|local_node| {
                        local_node.kind == diff::DiffKind::Script
                            && local_node.class == studio_node.class
                    })
                    .map(|_| {
                        stream
                            .local_source_paths_by_path
                            .get(path)
                            .cloned()
                            .ok_or_else(|| format!("disk script {path} has no staged source path"))
                    })
                    .transpose()?;
                validated_hashes.push((source_hash.id, path.clone(), digest, local_source_path));
            }
            if body.final_chunk
                && stream.received_hash_ids.len() + chunk_ids.len()
                    != stream.expected_hash_ids.len()
            {
                return Err(format!(
                    "hash stream would end with {}/{} script hashes",
                    stream.received_hash_ids.len() + chunk_ids.len(),
                    stream.expected_hash_ids.len()
                ));
            }

            // Source IO is deliberately deferred until the matching Studio
            // hash chunk arrives. This keeps the final structure request
            // metadata-only and bounds daemon work to one small hash batch.
            let mut prepared_hashes = Vec::with_capacity(validated_hashes.len());
            for (id, path, studio_hash, local_source_path) in validated_hashes {
                let local_baseline = if let Some(source_path) = local_source_path {
                    let generation = crate::fs_safety::file_generation_no_follow(&source_path)?;
                    let source_hash = normalized_file_hash(project, &source_path)?;
                    if crate::fs_safety::file_generation_no_follow(&source_path)? != generation {
                        return Err(format!(
                            "disk script {} changed while it was hashed; restart the comparison",
                            source_path.display()
                        ));
                    }
                    Some(StagedScriptBaseline {
                        fs_mtime: fs_mtime(&source_path),
                        path: source_path,
                        source_hash,
                        generation,
                    })
                } else {
                    None
                };
                prepared_hashes.push((id, path, studio_hash, local_baseline));
            }

            // Commit only after every ID, path, digest, and disk read in the
            // chunk has succeeded. A corrected retry after any error therefore
            // sees exactly the same receive state.
            let studio_nodes = stream
                .studio_nodes
                .as_mut()
                .expect("stream structure was validated above");
            let local_nodes = stream
                .local_nodes
                .as_mut()
                .expect("stream local comparison was validated above");
            for (id, path, studio_hash, local_baseline) in prepared_hashes {
                studio_nodes
                    .get_mut(&path)
                    .expect("stream path was validated above")
                    .source_hash = Some(studio_hash);
                if let Some(local_baseline) = local_baseline {
                    local_nodes
                        .get_mut(&path)
                        .expect("matching local script was validated above")
                        .source_hash = Some(local_baseline.source_hash);
                    session.staged_baselines.push(local_baseline);
                }
                stream.received_hash_ids.insert(id);
            }
            stream.next_chunk += 1;
            session.started_at = Instant::now();
            if !body.final_chunk {
                return Ok(json!({
                    "action": "compare",
                    "compareId": session.compare_id,
                    "nextService": service,
                    "phase": "hashes",
                    "nextChunk": stream.next_chunk,
                }));
            }
            if stream.received_hash_ids != stream.expected_hash_ids {
                return Err(format!(
                    "hash stream ended with {}/{} script hashes",
                    stream.received_hash_ids.len(),
                    stream.expected_hash_ids.len()
                ));
            }
            let local_nodes = stream
                .local_nodes
                .take()
                .ok_or("stream hash phase has no local comparison")?;
            let studio_nodes = stream
                .studio_nodes
                .take()
                .ok_or("stream hash phase has no Studio comparison")?;
            let report = diff::compare(&local_nodes, &studio_nodes);
            session.comparison.merge(InitialComparison {
                summary: InitialComparisonSummary {
                    new_files: report.summary.added,
                    changed_files: report.summary.changed,
                    removed_files: report.summary.removed,
                },
                new_files: report.added,
                changed_files: report.changed,
                removed_files: report.removed,
            });
            session.service_stream = None;
            session.next_service += 1;
            next_initial_compare_response(
                state,
                session,
                body.compare_id
                    .as_deref()
                    .expect("stream compareId was validated"),
                project,
                session_handle,
                true,
            )
        }
    }
}

struct PreparedStreamedComparison {
    local_nodes: BTreeMap<String, diff::DiffNode>,
    local_source_paths_by_path: HashMap<String, PathBuf>,
    service_generation: crate::fs_safety::TreeGeneration,
    studio_nodes: BTreeMap<String, diff::DiffNode>,
    studio_paths_by_id: HashMap<u64, String>,
    expected_hash_ids: HashSet<u64>,
}

fn prepare_streamed_initial_service_comparison(
    root: &Path,
    studio: ValidatedFlatSnapshot,
) -> Result<PreparedStreamedComparison, String> {
    let service = studio
        .service
        .get("name")
        .and_then(Value::as_str)
        .ok_or("flat Studio service is missing its name")?
        .to_string();
    let service_generation = crate::fs_safety::capture_tree_metadata(root, &service)?;
    let disk = snapshot::emit_flat_service(root, &service)
        .map_err(|error| format!("scan {}: {error}", root.join(&service).display()))?;
    if crate::fs_safety::capture_tree_metadata(root, &service)? != service_generation {
        return Err(format!(
            "disk service {service} changed during initial comparison; restart the scan"
        ));
    }
    let disk_source_paths = disk.source_paths;
    let disk = validate_flat_snapshot(&disk.records, &service, true)?;
    let mut local_services = vec![disk.service];
    let studio_services = vec![studio.service];
    overlay_local_avoid_sync_reservations(root, &mut local_services, &studio_services)?;

    let local_nodes = diff::collect_local_nodes(&local_services);
    let mut local_source_paths_by_path = HashMap::new();
    for node in local_nodes.values() {
        if node.kind != diff::DiffKind::Script {
            continue;
        }
        let source_id = node
            .stream_id
            .ok_or_else(|| format!("disk script {} is missing its stream ID", node.path))?;
        let source_path = disk_source_paths.get(&source_id).ok_or_else(|| {
            format!(
                "disk script {} has no source file for stream ID {source_id}",
                node.path
            )
        })?;
        local_source_paths_by_path.insert(node.path.clone(), source_path.clone());
    }

    let studio_root = json!({
        "class": "DataModel",
        "name": "game",
        "children": studio_services,
    });
    let studio_nodes = diff::collect_studio_tree_nodes(&studio_root);
    let studio_paths_by_id = studio_nodes
        .iter()
        .filter_map(|(path, node)| node.stream_id.map(|id| (id, path.clone())))
        .collect::<HashMap<_, _>>();
    let expected_hash_ids = studio.script_ids.into_iter().collect::<HashSet<_>>();
    for source_id in &expected_hash_ids {
        if !studio_paths_by_id.contains_key(source_id) {
            return Err(format!(
                "Studio script stream ID {source_id} has no projected comparison path"
            ));
        }
    }
    Ok(PreparedStreamedComparison {
        local_nodes,
        local_source_paths_by_path,
        service_generation,
        studio_nodes,
        studio_paths_by_id,
        expected_hash_ids,
    })
}

fn parse_sha256_hex(value: &str) -> Result<crate::conflict::Hash, String> {
    if value.len() != 64 {
        return Err("script SHA-256 must contain exactly 64 hexadecimal characters".into());
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&value[offset..offset + 2], 16)
            .map_err(|_| "script SHA-256 contains a non-hexadecimal character".to_string())?;
    }
    Ok(out)
}

fn normalized_file_hash(project_root: &Path, path: &Path) -> Result<crate::conflict::Hash, String> {
    use std::io::Read as _;

    let validated = crate::fs_safety::validate_synced_path(project_root, path, false)
        .map_err(|error| format!("validate source {}: {error}", path.display()))?;
    let guard = crate::fs_safety::guard_synced_parent_chain(project_root, &validated, false)
        .map_err(|error| format!("guard source {}: {error}", path.display()))?;
    guard
        .verify()
        .map_err(|error| format!("verify source parent {}: {error}", path.display()))?;
    let before = crate::fs_safety::file_generation_no_follow(&validated)?;
    let file = crate::fs_safety::open_regular_file_no_follow(&validated)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut pending_cr = false;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        let mut start = 0usize;
        for (index, byte) in buffer[..count].iter().copied().enumerate() {
            if pending_cr {
                if byte == b'\n' {
                    hasher.update(b"\n");
                    pending_cr = false;
                    start = index + 1;
                    continue;
                }
                hasher.update(b"\r");
                pending_cr = false;
                start = index;
            }
            if byte == b'\r' {
                if start < index {
                    hasher.update(&buffer[start..index]);
                }
                pending_cr = true;
                start = index + 1;
            }
        }
        if start < count {
            hasher.update(&buffer[start..count]);
        }
    }
    if pending_cr {
        hasher.update(b"\r");
    }
    if crate::fs_safety::file_generation_no_follow(&validated)? != before {
        return Err(format!(
            "source changed while it was hashed: {}",
            path.display()
        ));
    }
    guard
        .verify()
        .map_err(|error| format!("source parent changed {}: {error}", path.display()))?;
    Ok(hasher.finalize().into())
}

fn finish_initial_comparison(
    state: &AppState,
    disk_stats: Stats,
    studio_stats: Stats,
    comparison: InitialComparison,
    staged_comparison: Option<StagedComparisonState>,
) -> Json<Value> {
    let compact_response = staged_comparison.is_some();
    if comparison.is_clean() {
        if let Some(staged_comparison) = staged_comparison {
            for generation in &staged_comparison.service_generations {
                let current = match crate::fs_safety::capture_tree_metadata(
                    state.canonical_project.as_path(),
                    &generation.service,
                ) {
                    Ok(current) => current,
                    Err(error) => {
                        return Json(json!({
                            "ok": false,
                            "error": format!("revalidate clean comparison: {error}"),
                        }));
                    }
                };
                if &current != generation {
                    return Json(json!({
                        "ok": false,
                        "error": format!(
                            "disk service {} changed before initial comparison completed; restart the scan",
                            generation.service
                        ),
                    }));
                }
            }
            for baseline in &staged_comparison.baselines {
                match crate::fs_safety::file_generation_no_follow(&baseline.path) {
                    Ok(current) if current == baseline.generation => {}
                    Ok(_) => {
                        return Json(json!({
                            "ok": false,
                            "error": format!(
                                "disk script {} changed before initial comparison completed; restart the scan",
                                baseline.path.display()
                            ),
                        }));
                    }
                    Err(error) => {
                        return Json(json!({
                            "ok": false,
                            "error": format!("revalidate clean comparison: {error}"),
                        }));
                    }
                }
            }
            for baseline in staged_comparison.baselines {
                state
                    .conflict
                    .record_sync(&baseline.path, baseline.source_hash, baseline.fs_mtime);
            }
        } else if let Err(error) =
            seed_clean_script_baselines(state.canonical_project.as_path(), state.conflict.as_ref())
        {
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

    // Both non-empty and divergent → park a pending decision and tell the
    // plugin to drive the overwrite UI.
    let choice_id = new_choice_id();
    let details = match initial_choice_details_from_comparison(&comparison) {
        Ok(details) => details,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": format!("prepare initial-choice details: {error}"),
            }));
        }
    };
    let summary = InitialChoiceSummary {
        new_files: comparison.summary.new_files,
        changed_files: comparison.summary.changed_files,
        removed_files: comparison.summary.removed_files,
    };
    let detail_count = details.len();
    let pending = PendingInitial {
        choice_id: choice_id.clone(),
        disk_stats,
        studio_stats,
        choice: None,
        details,
        summary,
        selected_disk_paths: None,
        selection: None,
    };
    {
        let mut slot = state.pending_initial.lock().unwrap();
        *slot = Some(pending);
    }
    let evt = json!({
        "type": "initial-choice-needed",
        "choiceId": choice_id,
        "diskStats": disk_stats,
        "studioStats": studio_stats,
        "detailCount": detail_count,
        "comparison": {
            "summary": summary,
        },
    });
    if let Ok(serialized) = serde_json::to_string(&evt) {
        let _ = state.events.send(serialized);
    }
    if compact_response {
        Json(json!({
            "action": "decide",
            "choiceId": choice_id,
            "diskStats": disk_stats,
            "comparison": { "summary": summary },
        }))
    } else {
        Json(json!({
            "action": "decide",
            "choiceId": choice_id,
            "diskStats": disk_stats,
            "comparison": comparison,
        }))
    }
}

fn seed_clean_script_baselines(
    root: &Path,
    conflicts: &crate::conflict::ConflictEngine,
) -> Result<usize, String> {
    let mut seeded = 0usize;
    for service in snapshot::SYNCED_SERVICES {
        let service_dir = root.join(service);
        if crate::fs_safety::metadata_no_follow(&service_dir)
            .map_err(|error| format!("inspect service {}: {error}", service_dir.display()))?
            .is_some_and(|metadata| metadata.is_dir())
        {
            seeded += seed_script_baselines_in_dir(root, &service_dir, conflicts)?;
        }
    }
    Ok(seeded)
}

fn seed_script_baselines_in_dir(
    project_root: &Path,
    dir: &Path,
    conflicts: &crate::conflict::ConflictEngine,
) -> Result<usize, String> {
    let Some(fence) = capture_synced_subtree(project_root, dir)? else {
        return Ok(0);
    };
    let mut seeded = 0usize;
    for entry in &fence.entries {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if classify_script_file(name).is_none() && !is_init_file(name) {
            continue;
        }
        let bytes = read_synced_file(project_root, &entry.path)?;
        let normalized = normalize_line_endings(&bytes).into_owned();
        conflicts.record_sync(&entry.path, hash(&normalized), fs_mtime(&entry.path));
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
    validate_bootstrap_service_roots(studio_services, false)?;
    validate_bootstrap_services(studio_services)?;
    let local_services =
        snapshot::emit_services(root).map_err(|e| format!("scan {}: {e}", root.display()))?;
    initial_snapshot_comparison_with_local(root, local_services, studio_services)
}

fn initial_service_snapshot_comparison(
    root: &Path,
    studio_service: &Value,
) -> Result<InitialComparison, String> {
    validate_bootstrap_service_roots(std::slice::from_ref(studio_service), true)?;
    validate_bootstrap_services(std::slice::from_ref(studio_service))?;
    let service = studio_service
        .get("name")
        .and_then(Value::as_str)
        .ok_or("Studio service snapshot is missing a name")?;
    if !snapshot::SYNCED_SERVICES.contains(&service) {
        return Err(format!("unsupported Studio service snapshot: {service}"));
    }
    let local_services = vec![snapshot::emit_service(root, service)
        .map_err(|error| format!("scan {}: {error}", root.join(service).display()))?];
    initial_snapshot_comparison_with_local(
        root,
        local_services,
        std::slice::from_ref(studio_service),
    )
}

fn initial_snapshot_comparison_with_local(
    root: &Path,
    mut local_services: Vec<Value>,
    studio_services: &[Value],
) -> Result<InitialComparison, String> {
    overlay_local_avoid_sync_reservations(root, &mut local_services, studio_services)?;
    let local = diff::collect_local_nodes(&local_services);
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

fn reservation_subtree_ids(nodes: &[Value]) -> HashSet<usize> {
    let mut ids = HashSet::new();
    let mut pending = nodes
        .iter()
        .rev()
        .map(|node| (node, false))
        .collect::<Vec<_>>();
    while let Some((node, children_visited)) = pending.pop() {
        if children_visited {
            let marked = node_is_avoid_sync_boundary(node)
                || node_is_avoid_sync_carrier(node)
                || node
                    .get("children")
                    .and_then(Value::as_array)
                    .is_some_and(|children| {
                        children
                            .iter()
                            .any(|child| ids.contains(&(child as *const Value as usize)))
                    });
            if marked {
                ids.insert(node as *const Value as usize);
            }
            continue;
        }
        pending.push((node, true));
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            pending.extend(children.iter().rev().map(|child| (child, false)));
        }
    }
    ids
}

/// Overlay Studio's synthetic AvoidSync markers onto the emitted disk tree.
///
/// A carrier and a live sibling can share the same Roblox name. The carrier
/// reserves the bare generated identity while the live sibling receives
/// `[1]`, but the disk scan cannot infer that reservation from decoded names
/// alone and may sort the two subtrees in the opposite order. Match markers by
/// their exact physical fragment assignment, annotate an existing disk node,
/// or insert a marker-only phantom when the ignored branch is absent. The diff
/// collector then runs the same reservation-first logical allocator for both
/// trees, suppressing ignored identities without ever filtering the live one.
fn overlay_local_avoid_sync_reservations(
    root: &Path,
    local_services: &mut [Value],
    studio_services: &[Value],
) -> Result<(), String> {
    let reservation_ids = reservation_subtree_ids(studio_services);
    if reservation_ids.is_empty() {
        return Ok(());
    }

    for studio_service in studio_services {
        if !reservation_ids.contains(&(studio_service as *const Value as usize)) {
            continue;
        }
        let Some(service_name) = studio_service.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(local_service) = local_services
            .iter_mut()
            .find(|service| service.get("name").and_then(Value::as_str) == Some(service_name))
        else {
            continue;
        };
        overlay_local_avoid_sync_children(
            &root.join(service_name),
            local_service,
            studio_service,
            &reservation_ids,
        )?;
    }
    Ok(())
}

fn overlay_local_avoid_sync_children(
    parent_dir: &Path,
    local_parent: &mut Value,
    studio_parent: &Value,
    reservation_ids: &HashSet<usize>,
) -> Result<(), String> {
    let studio_children = studio_parent
        .get("children")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if studio_children.is_empty() {
        return Ok(());
    }

    let existing = index_child_fragments(parent_dir)
        .map_err(|error| format!("scan AvoidSync parent {}: {error}", parent_dir.display()))?;
    let mut consumed_existing = HashSet::new();
    let mut planned: Vec<(&Value, Vec<(String, bool)>)> = Vec::new();
    for assignment in child_fragment_assignments(studio_children) {
        let fragment = resolve_child_assignment_fragment(
            parent_dir,
            &assignment,
            &existing,
            &mut consumed_existing,
        )?;
        let same_node = planned
            .last()
            .is_some_and(|(node, _)| std::ptr::eq(*node, assignment.node));
        if same_node {
            planned
                .last_mut()
                .expect("same-node plan has a last entry")
                .1
                .push((fragment, assignment.projection_has_children));
        } else {
            planned.push((
                assignment.node,
                vec![(fragment, assignment.projection_has_children)],
            ));
        }
    }

    let local_parent_name = local_parent
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>")
        .to_string();
    let local_children = local_parent
        .get_mut("children")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("local snapshot node {local_parent_name} has invalid children"))?;
    let mut local_by_fragment = HashMap::with_capacity(local_children.len());
    for (index, child) in local_children.iter().enumerate() {
        if let Some(fragment) = child.get("diskFragment").and_then(Value::as_str) {
            local_by_fragment.insert(fragment.to_ascii_lowercase(), index);
        }
    }

    for (studio_child, fragments) in planned {
        if !reservation_ids.contains(&(studio_child as *const Value as usize)) {
            continue;
        }
        let matching = fragments.iter().find_map(|(fragment, is_dir)| {
            local_by_fragment
                .get(&fragment.to_ascii_lowercase())
                .copied()
                .map(|index| (index, fragment.clone(), *is_dir))
        });
        let (local_index, fragment, fragment_is_dir) = if let Some(matching) = matching {
            matching
        } else if node_is_avoid_sync_boundary(studio_child)
            || node_is_avoid_sync_carrier(studio_child)
        {
            let (fragment, fragment_is_dir) = fragments
                .first()
                .cloned()
                .ok_or("AvoidSync reservation is missing a physical fragment")?;
            let mut synthetic = studio_child.clone();
            let object = synthetic
                .as_object_mut()
                .ok_or("AvoidSync reservation node must be an object")?;
            object.insert("diskFragment".into(), Value::String(fragment.clone()));
            object.insert("diskFragmentIsDir".into(), Value::Bool(fragment_is_dir));
            object.insert("children".into(), Value::Array(Vec::new()));
            let index = local_children.len();
            local_children.push(synthetic);
            local_by_fragment.insert(fragment.to_ascii_lowercase(), index);
            (index, fragment, fragment_is_dir)
        } else {
            continue;
        };

        let local_child = &mut local_children[local_index];
        if node_is_avoid_sync_boundary(studio_child) {
            local_child["avoidSync"] = Value::Bool(true);
            continue;
        }
        if node_is_avoid_sync_carrier(studio_child) {
            local_child["avoidSyncCarrier"] = Value::Bool(true);
        }
        if fragment_is_dir {
            overlay_local_avoid_sync_children(
                &parent_dir.join(fragment),
                local_child,
                studio_child,
                reservation_ids,
            )?;
        }
    }
    Ok(())
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
    // Each poll is proof the plugin's decision picker is still alive. Refresh
    // the compare session's expiry clocks so a human who takes longer than the
    // TTLs (15 min pending / 2 min completed) to answer doesn't have the
    // session pruned out from under the picker — that silently orphaned the
    // eventual choice and wedged the initial sync.
    {
        let sessions = INITIAL_COMPARE_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        if let Some(session) = sessions.get(state.canonical_project.as_ref()) {
            if let Ok(mut session) = session.try_lock() {
                session.started_at = Instant::now();
                if session.completed_at.is_some() {
                    session.completed_at = Some(Instant::now());
                }
            }
        }
    }
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
            if choice == Choice::Disk {
                if let Some(paths) = selected_disk_paths.as_ref() {
                    let grants =
                        SELECTIVE_TRANSFER_GRANTS.get_or_init(|| Mutex::new(HashMap::new()));
                    let mut grants = grants.lock().unwrap();
                    grants.retain(|_, grant| {
                        grant.created_at.elapsed() < SELECTIVE_TRANSFER_GRANT_TTL
                    });
                    grants.insert(
                        (
                            state.canonical_project.as_ref().clone(),
                            params.choice_id.clone(),
                        ),
                        SelectiveTransferGrant {
                            paths: paths.clone(),
                            created_at: Instant::now(),
                        },
                    );
                }
            }
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
                (Choice::Disk, Some(paths)) => Json(json!({
                    "choice": s,
                    "selective": true,
                    "selectedCount": paths.len(),
                }))
                .into_response(),
                _ => Json(json!({ "choice": s })).into_response(),
            };
        }

        if started.elapsed() >= Duration::from_secs(60) {
            return Json(json!({ "pending": true })).into_response();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

const INITIAL_CHOICE_DETAIL_DEFAULT_LIMIT: usize = 512;
const INITIAL_CHOICE_DETAIL_MAX_LIMIT: usize = 1024;
const INITIAL_CHOICE_DETAIL_MAX_RESPONSE: usize = 512 * 1024;
const INITIAL_SELECTION_MAX_IDS: usize = 2048;
const INITIAL_SELECTION_ID_MAX_BYTES: usize = 256;
const INITIAL_SELECTION_TTL: Duration = Duration::from_secs(5 * 60);
const INITIAL_SELECTION_REPLAY_TTL: Duration = Duration::from_secs(5 * 60);
const INITIAL_SELECTION_REPLAY_LIMIT: usize = 64;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitialChoiceDetailsParams {
    choice_id: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn initial_choice_status(State(state): State<AppState>) -> Json<Value> {
    let mut pending = state.pending_initial.lock().unwrap();
    let Some(pending) = pending.as_mut() else {
        return Json(json!({ "pending": false }));
    };
    expire_initial_selection(pending);
    Json(json!({
        "pending": true,
        "choiceId": pending.choice_id,
        "diskStats": pending.disk_stats,
        "studioStats": pending.studio_stats,
        "choice": pending.choice.map(initial_choice_name),
        "comparison": { "summary": pending.summary },
        "detailCount": pending.details.len(),
    }))
}

async fn initial_choice_details(
    State(state): State<AppState>,
    Query(params): Query<InitialChoiceDetailsParams>,
) -> InitialChoiceHttpResponse {
    if !valid_initial_choice_token(&params.choice_id) {
        return initial_choice_error("invalid choiceId");
    }
    let limit = params
        .limit
        .unwrap_or(INITIAL_CHOICE_DETAIL_DEFAULT_LIMIT)
        .clamp(1, INITIAL_CHOICE_DETAIL_MAX_LIMIT);
    let (items, total_count, start) = {
        let pending = state.pending_initial.lock().unwrap();
        let Some(pending) = pending.as_ref() else {
            return initial_choice_error("no pending decision");
        };
        if pending.choice_id != params.choice_id {
            return initial_choice_error("no pending decision");
        }
        if pending.choice.is_some() {
            return initial_choice_error("initial decision is already resolved");
        }
        let start = match params.cursor.as_deref() {
            Some(cursor) => match decode_initial_choice_cursor(cursor, &params.choice_id) {
                Ok(offset) => offset,
                Err(error) => return initial_choice_error(error),
            },
            None => 0,
        };
        if start > pending.details.len() {
            return initial_choice_error("detail cursor is out of range");
        }
        let end = start.saturating_add(limit).min(pending.details.len());
        (
            pending.details[start..end].to_vec(),
            pending.details.len(),
            start,
        )
    };

    pack_initial_choice_detail_page(&params.choice_id, total_count, start, items)
}

fn pack_initial_choice_detail_page(
    choice_id: &str,
    total_count: usize,
    start: usize,
    mut items: Vec<InitialChoiceItem>,
) -> InitialChoiceHttpResponse {
    if start < total_count {
        let envelope = json!({
            "ok": true,
            "choiceId": choice_id,
            "totalCount": total_count,
            "items": [],
            "nextCursor": encode_initial_choice_cursor(choice_id, total_count),
            "complete": false,
        });
        let envelope_bytes = match serde_json::to_vec(&envelope) {
            Ok(encoded) => encoded.len().saturating_sub(2),
            Err(error) => {
                return initial_choice_error(format!(
                    "encode initial-choice detail envelope: {error}"
                ));
            }
        };
        let mut item_bytes = 2usize;
        let mut accepted = 0usize;
        for item in &items {
            let encoded = match serde_json::to_vec(item) {
                Ok(encoded) => encoded,
                Err(error) => {
                    return initial_choice_error(format!(
                        "encode initial-choice detail item: {error}"
                    ));
                }
            };
            let separator = usize::from(accepted > 0);
            let Some(candidate) = envelope_bytes
                .checked_add(item_bytes)
                .and_then(|size| size.checked_add(separator))
                .and_then(|size| size.checked_add(encoded.len()))
            else {
                break;
            };
            if candidate > INITIAL_CHOICE_DETAIL_MAX_RESPONSE {
                break;
            }
            item_bytes += separator + encoded.len();
            accepted += 1;
        }
        if accepted == 0 {
            return initial_choice_error(
                "one initial-choice detail exceeds the response byte limit",
            );
        }
        items.truncate(accepted);
    }

    let end = start + items.len();
    let complete = end == total_count;
    let next_cursor = (!complete).then(|| encode_initial_choice_cursor(choice_id, end));
    let response = json!({
        "ok": true,
        "choiceId": choice_id,
        "totalCount": total_count,
        "items": items,
        "nextCursor": next_cursor,
        "complete": complete,
    });
    match serde_json::to_vec(&response) {
        Ok(encoded) if encoded.len() <= INITIAL_CHOICE_DETAIL_MAX_RESPONSE => {
            initial_choice_ok(response)
        }
        Ok(_) => initial_choice_error("initial-choice detail page exceeds its byte limit"),
        Err(error) => initial_choice_error(format!("encode initial-choice detail page: {error}")),
    }
}

fn encode_initial_choice_cursor(choice_id: &str, offset: usize) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("{choice_id}\0{offset}").as_bytes())
}

fn decode_initial_choice_cursor(cursor: &str, choice_id: &str) -> Result<usize, &'static str> {
    use base64::Engine as _;
    if cursor.is_empty() || cursor.len() > 4096 {
        return Err("invalid detail cursor");
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| "invalid detail cursor")?;
    let decoded = std::str::from_utf8(&decoded).map_err(|_| "invalid detail cursor")?;
    let (cursor_choice, offset) = decoded.split_once('\0').ok_or("invalid detail cursor")?;
    if cursor_choice != choice_id {
        return Err("detail cursor belongs to another choice");
    }
    offset.parse::<usize>().map_err(|_| "invalid detail cursor")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitialChoiceBody {
    choice_id: String,
    choice: String,
    #[serde(default)]
    mode: Option<String>,
}

async fn initial_choice(
    State(state): State<AppState>,
    Json(body): Json<InitialChoiceBody>,
) -> InitialChoiceHttpResponse {
    if !valid_initial_choice_token(&body.choice_id) {
        return initial_choice_error("invalid choiceId");
    }
    let choice = match body.choice.as_str() {
        "disk" if body.mode.as_deref() == Some("all") => Choice::Disk,
        "disk" => return initial_choice_error("disk choice requires mode \"all\""),
        "studio" if body.mode.is_none() => Choice::Studio,
        "cancel" if body.mode.is_none() => Choice::Cancel,
        "studio" | "cancel" => {
            return initial_choice_error("mode is only valid for a disk choice");
        }
        other => return initial_choice_error(format!("unknown choice: {other}")),
    };

    let newly_chosen = {
        let mut slot = state.pending_initial.lock().unwrap();
        let Some(pending) = slot
            .as_mut()
            .filter(|pending| pending.choice_id == body.choice_id)
        else {
            return initial_choice_error("no pending decision");
        };
        if let Some(existing) = pending.choice {
            if existing == choice
                && (choice != Choice::Disk || pending.selected_disk_paths.is_none())
            {
                false
            } else {
                return initial_choice_error("initial decision is already resolved");
            }
        } else {
            pending.choice = Some(choice);
            pending.selected_disk_paths = None;
            pending.selection = None;
            true
        }
    };

    if newly_chosen {
        finalize_initial_choice(&state, &body.choice_id, choice);
    }
    initial_choice_ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitialChoiceSelectionBody {
    #[serde(default)]
    op: Option<String>,
    choice_id: String,
    submission_id: String,
    #[serde(default)]
    chunk_index: Option<u32>,
    #[serde(default)]
    final_chunk: Option<bool>,
    #[serde(default)]
    restart: bool,
    #[serde(default)]
    ids: Vec<u32>,
}

#[derive(Clone)]
struct CompletedInitialSelection {
    receipts: Vec<InitialSelectionReceipt>,
    completed_at: Instant,
}

type CompletedInitialSelectionKey = (PathBuf, String, String);
static COMPLETED_INITIAL_SELECTIONS: OnceLock<
    Mutex<HashMap<CompletedInitialSelectionKey, CompletedInitialSelection>>,
> = OnceLock::new();

async fn initial_choice_selection(
    State(state): State<AppState>,
    Json(body): Json<InitialChoiceSelectionBody>,
) -> InitialChoiceHttpResponse {
    if !valid_initial_choice_token(&body.choice_id)
        || !valid_initial_choice_token(&body.submission_id)
    {
        return initial_choice_error("invalid choiceId or submissionId");
    }
    if body.op.as_deref() == Some("abort") {
        if body.chunk_index.is_some()
            || body.final_chunk.is_some()
            || body.restart
            || !body.ids.is_empty()
        {
            return initial_choice_error("abort cannot include chunk fields");
        }
        return abort_initial_selection(&state, &body.choice_id, &body.submission_id);
    }
    if body.op.is_some() {
        return initial_choice_error("unknown initial selection operation");
    }
    let Some(chunk_index) = body.chunk_index else {
        return initial_choice_error("selection chunk is missing chunkIndex");
    };
    let Some(final_chunk) = body.final_chunk else {
        return initial_choice_error("selection chunk is missing finalChunk");
    };
    if chunk_index == u32::MAX {
        return initial_choice_error("selection chunk index is too large");
    }
    if body.ids.is_empty() {
        return initial_choice_error("selection chunk must include at least one ID");
    }
    if body.ids.len() > INITIAL_SELECTION_MAX_IDS {
        return initial_choice_error("selection chunk exceeds 2048 IDs");
    }
    if body.restart && chunk_index != 0 {
        return initial_choice_error("restart is only valid on chunk zero");
    }
    let request = InitialSelectionReceipt {
        chunk_index,
        final_chunk,
        restart: body.restart,
        ids: body.ids,
        selected_count: 0,
        committed: false,
    };

    if let Some(replay) = replay_completed_initial_selection(
        state.canonical_project.as_path(),
        &body.choice_id,
        &body.submission_id,
        &request,
    ) {
        return match replay {
            Ok(response) => initial_choice_ok(response),
            Err(error) => initial_choice_error(error),
        };
    }

    let mut committed = false;
    let response = {
        let mut slot = state.pending_initial.lock().unwrap();
        let pending = match slot
            .as_mut()
            .filter(|pending| pending.choice_id == body.choice_id)
        {
            Some(pending) => pending,
            None => {
                drop(slot);
                return replay_completed_initial_selection(
                    state.canonical_project.as_path(),
                    &body.choice_id,
                    &body.submission_id,
                    &request,
                )
                .map_or_else(
                    || initial_choice_error("no pending decision"),
                    |replay| match replay {
                        Ok(response) => initial_choice_ok(response),
                        Err(error) => initial_choice_error(error),
                    },
                );
            }
        };
        if pending.choice.is_some() {
            drop(slot);
            return replay_completed_initial_selection(
                state.canonical_project.as_path(),
                &body.choice_id,
                &body.submission_id,
                &request,
            )
            .map_or_else(
                || initial_choice_error("initial decision is already resolved"),
                |replay| match replay {
                    Ok(response) => initial_choice_ok(response),
                    Err(error) => initial_choice_error(error),
                },
            );
        }
        expire_initial_selection(pending);
        if request
            .ids
            .iter()
            .any(|id| (*id as usize) >= pending.details.len())
        {
            return initial_choice_error("selection ID is outside the current divergence");
        }
        let mut unique = BTreeSet::new();
        if request.ids.iter().any(|id| !unique.insert(*id)) {
            return initial_choice_error("selection chunk repeats an ID");
        }

        if pending
            .selection
            .as_ref()
            .is_some_and(|selection| selection.submission_id != body.submission_id)
        {
            if request.chunk_index == 0 && request.restart {
                pending.selection = None;
            } else {
                return initial_choice_error(
                    "another uncommitted selection exists; restart chunk zero",
                );
            }
        }
        if pending.selection.is_none() {
            if request.chunk_index != 0 || !request.restart {
                return initial_choice_error("start selection with restart chunk zero");
            }
            pending.selection = Some(InitialSelectionAccumulator {
                submission_id: body.submission_id.clone(),
                next_chunk: 0,
                selected_ids: BTreeSet::new(),
                receipts: Vec::new(),
                updated_at: Instant::now(),
            });
        }

        let selection = pending
            .selection
            .as_mut()
            .expect("selection was initialized above");
        if let Some(receipt) = selection
            .receipts
            .iter()
            .find(|receipt| receipt.chunk_index == request.chunk_index)
        {
            if selection_receipt_matches(receipt, &request) {
                return initial_choice_ok(initial_selection_receipt_json(
                    &body.choice_id,
                    &body.submission_id,
                    receipt,
                ));
            }
            return initial_choice_error("selection chunk retry does not match its receipt");
        }
        if request.chunk_index != selection.next_chunk {
            return initial_choice_error(format!(
                "selection expected chunk {}, received {}",
                selection.next_chunk, request.chunk_index
            ));
        }
        if request
            .ids
            .iter()
            .any(|id| selection.selected_ids.contains(id))
        {
            return initial_choice_error("selection repeats an ID from an earlier chunk");
        }

        for id in &request.ids {
            selection.selected_ids.insert(*id);
        }
        selection.next_chunk = selection
            .next_chunk
            .checked_add(1)
            .expect("u32 chunk index cannot overflow after a valid request");
        selection.updated_at = Instant::now();
        let receipt = InitialSelectionReceipt {
            selected_count: selection.selected_ids.len(),
            committed: request.final_chunk,
            ..request
        };
        selection.receipts.push(receipt.clone());

        if receipt.committed {
            let completed_selection = selection.clone();
            commit_initial_selection_with(
                state.canonical_project.as_path(),
                &body.choice_id,
                &body.submission_id,
                pending,
                completed_selection,
                || {},
            );
            committed = true;
        }
        initial_selection_receipt_json(&body.choice_id, &body.submission_id, &receipt)
    };

    if committed {
        finalize_initial_choice(&state, &body.choice_id, Choice::Disk);
    }
    initial_choice_ok(response)
}

fn commit_initial_selection_with<F>(
    project: &Path,
    choice_id: &str,
    submission_id: &str,
    pending: &mut PendingInitial,
    selection: InitialSelectionAccumulator,
    before_publish: F,
) where
    F: FnOnce(),
{
    let selected_paths = selection
        .selected_ids
        .iter()
        .map(|id| pending.details[*id as usize].path.clone())
        .collect::<Vec<_>>();
    // Persist the exact final receipt before publishing Choice::Disk. A retry
    // can race either side of the pending-choice lock, but never observe a
    // resolved choice without an authoritative replay receipt.
    remember_completed_initial_selection(project, choice_id, submission_id, selection);
    before_publish();
    pending.selected_disk_paths = Some(selected_paths);
    pending.choice = Some(Choice::Disk);
}

fn abort_initial_selection(
    state: &AppState,
    choice_id: &str,
    submission_id: &str,
) -> InitialChoiceHttpResponse {
    let mut slot = state.pending_initial.lock().unwrap();
    let Some(pending) = slot
        .as_mut()
        .filter(|pending| pending.choice_id == choice_id)
    else {
        return initial_choice_error("no pending decision");
    };
    if pending.choice.is_some() {
        return initial_choice_error("initial decision is already resolved");
    }
    expire_initial_selection(pending);
    match pending.selection.as_ref() {
        Some(selection) if selection.submission_id != submission_id => {
            initial_choice_error("abort belongs to another selection submission")
        }
        Some(_) => {
            pending.selection = None;
            initial_choice_ok(json!({
                "ok": true,
                "choiceId": choice_id,
                "submissionId": submission_id,
                "aborted": true,
            }))
        }
        None => initial_choice_ok(json!({
            "ok": true,
            "choiceId": choice_id,
            "submissionId": submission_id,
            "aborted": false,
        })),
    }
}

fn initial_selection_receipt_json(
    choice_id: &str,
    submission_id: &str,
    receipt: &InitialSelectionReceipt,
) -> Value {
    json!({
        "ok": true,
        "choiceId": choice_id,
        "submissionId": submission_id,
        "acceptedChunk": receipt.chunk_index,
        "nextChunk": receipt.chunk_index + 1,
        "selectedCount": receipt.selected_count,
        "committed": receipt.committed,
    })
}

fn selection_receipt_matches(
    receipt: &InitialSelectionReceipt,
    request: &InitialSelectionReceipt,
) -> bool {
    receipt.chunk_index == request.chunk_index
        && receipt.final_chunk == request.final_chunk
        && receipt.restart == request.restart
        && receipt.ids == request.ids
}

fn expire_initial_selection(pending: &mut PendingInitial) {
    if pending
        .selection
        .as_ref()
        .is_some_and(|selection| selection.updated_at.elapsed() >= INITIAL_SELECTION_TTL)
    {
        pending.selection = None;
    }
}

fn replay_completed_initial_selection(
    project: &Path,
    choice_id: &str,
    submission_id: &str,
    request: &InitialSelectionReceipt,
) -> Option<Result<Value, String>> {
    let replays = COMPLETED_INITIAL_SELECTIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut replays = replays.lock().unwrap();
    prune_completed_initial_selections(&mut replays);
    let replay = replays.get(&(
        project.to_path_buf(),
        choice_id.to_string(),
        submission_id.to_string(),
    ))?;
    Some(
        match replay
            .receipts
            .iter()
            .find(|receipt| receipt.chunk_index == request.chunk_index)
        {
            Some(receipt) if selection_receipt_matches(receipt, request) => Ok(
                initial_selection_receipt_json(choice_id, submission_id, receipt),
            ),
            Some(_) => Err("selection chunk retry does not match its receipt".into()),
            None => Err("selection submission is already committed".into()),
        },
    )
}

fn remember_completed_initial_selection(
    project: &Path,
    choice_id: &str,
    submission_id: &str,
    selection: InitialSelectionAccumulator,
) {
    let replays = COMPLETED_INITIAL_SELECTIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut replays = replays.lock().unwrap();
    prune_completed_initial_selections(&mut replays);
    if replays.len() >= INITIAL_SELECTION_REPLAY_LIMIT {
        if let Some(oldest) = replays
            .iter()
            .min_by_key(|(_, replay)| replay.completed_at)
            .map(|(key, _)| key.clone())
        {
            replays.remove(&oldest);
        }
    }
    replays.insert(
        (
            project.to_path_buf(),
            choice_id.to_string(),
            submission_id.to_string(),
        ),
        CompletedInitialSelection {
            receipts: selection.receipts,
            completed_at: Instant::now(),
        },
    );
}

fn prune_completed_initial_selections(
    replays: &mut HashMap<CompletedInitialSelectionKey, CompletedInitialSelection>,
) {
    replays.retain(|_, replay| replay.completed_at.elapsed() < INITIAL_SELECTION_REPLAY_TTL);
}

fn finalize_initial_choice(state: &AppState, choice_id: &str, choice: Choice) {
    clear_completed_initial_compare_for_choice(state.canonical_project.as_path(), choice_id);
    let evt = json!({
        "type": "initial-choice-made",
        "choiceId": choice_id,
        "choice": initial_choice_name(choice),
    });
    if let Ok(serialized) = serde_json::to_string(&evt) {
        let _ = state.events.send(serialized);
    }
}

fn initial_choice_name(choice: Choice) -> &'static str {
    match choice {
        Choice::Disk => "disk",
        Choice::Studio => "studio",
        Choice::Cancel => "cancel",
    }
}

fn valid_initial_choice_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= INITIAL_SELECTION_ID_MAX_BYTES
        && !value.chars().any(char::is_control)
}

type InitialChoiceHttpResponse = (StatusCode, Json<Value>);

fn initial_choice_ok(body: Value) -> InitialChoiceHttpResponse {
    (StatusCode::OK, Json(body))
}

fn initial_choice_error(error: impl Into<String>) -> InitialChoiceHttpResponse {
    (
        StatusCode::CONFLICT,
        Json(json!({ "ok": false, "error": error.into() })),
    )
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
    #[serde(default)]
    service: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotStreamBody {
    #[serde(default)]
    plugin_protocol: Option<u64>,
    request_id: String,
    #[serde(default)]
    stream_id: Option<String>,
    phase: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    chunk_index: Option<u64>,
    #[serde(default)]
    strict: bool,
    #[serde(default)]
    avoid_sync_paths: Vec<Vec<String>>,
    #[serde(default)]
    choice_id: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SnapshotStreamPhase {
    Structure,
    Sources,
    Deletes,
}

struct PullActiveSource {
    id: u64,
    text: String,
    offset: usize,
    part_index: u64,
    total_bytes: u64,
    sha256: String,
    generation: crate::fs_safety::FileGeneration,
    path: PathBuf,
}

struct SnapshotServiceStream {
    project_root: PathBuf,
    service: String,
    phase: SnapshotStreamPhase,
    next_chunk: u64,
    records: Vec<snapshot::FlatSnapshotRecord>,
    record_offset: usize,
    source_ids: Vec<u64>,
    source_paths: HashMap<u64, PathBuf>,
    source_generations: HashMap<u64, crate::fs_safety::FileGeneration>,
    source_index: usize,
    active_source: Option<PullActiveSource>,
    deletes: Vec<Vec<String>>,
    delete_offset: usize,
    generation: crate::fs_safety::TreeGeneration,
    initial_fingerprint: Option<ExactTreeFingerprint>,
    initial_fingerprint_result:
        Option<std::sync::mpsc::Receiver<Result<ExactTreeFingerprint, String>>>,
    revalidate_result: Option<std::sync::mpsc::Receiver<Result<ExactTreeFingerprint, String>>>,
}

struct SnapshotStreamAccumulator {
    request_id: String,
    stream_id: String,
    selective_paths: Option<Vec<String>>,
    selective_choice_id: Option<String>,
    next_service: usize,
    service_stream: Option<SnapshotServiceStream>,
    prepare_result: Option<std::sync::mpsc::Receiver<Result<SnapshotServiceStream, String>>>,
    prepare_next_chunk: u64,
    last_request_hash: Option<crate::conflict::Hash>,
    last_response: Option<Value>,
    last_activity: Instant,
    completed_at: Option<Instant>,
}

static SNAPSHOT_STREAM_ACCUMULATORS: OnceLock<
    Mutex<HashMap<PathBuf, Arc<Mutex<SnapshotStreamAccumulator>>>>,
> = OnceLock::new();

type SelectiveFlatService = (
    Vec<snapshot::FlatSnapshotRecord>,
    HashMap<u64, PathBuf>,
    Vec<Vec<String>>,
);

fn selective_flat_service(
    service: &str,
    records: Vec<snapshot::FlatSnapshotRecord>,
    source_paths: HashMap<u64, PathBuf>,
    selected_paths: &[String],
) -> Result<SelectiveFlatService, String> {
    let validated = validate_flat_snapshot(&records, service, true)?;
    let local_nodes = diff::collect_local_nodes(std::slice::from_ref(&validated.service));
    let service_prefix = format!("{service}/");
    let selected = selected_paths
        .iter()
        .filter(|path| path.starts_with(&service_prefix))
        .collect::<Vec<_>>();
    let mut selected_ids = HashSet::new();
    let mut deletes = Vec::new();
    for path in selected {
        if let Some(id) = local_nodes.get(path).and_then(|node| node.stream_id) {
            selected_ids.insert(id as usize);
        } else {
            deletes.push(path.split('/').map(str::to_string).collect::<Vec<_>>());
        }
    }
    deletes.sort();
    deletes.dedup();

    let mut selected_subtree = vec![false; records.len()];
    for index in 1..records.len() {
        let parent = records[index]
            .parent_id
            .ok_or("selective disk record is disconnected")? as usize;
        selected_subtree[index] = selected_ids.contains(&index) || selected_subtree[parent];
    }
    let mut included = vec![false; records.len()];
    included[0] = true;
    for (index, selected) in selected_subtree.iter().copied().enumerate().skip(1) {
        if selected {
            included[index] = true;
        }
    }
    for mut selected in selected_ids {
        loop {
            included[selected] = true;
            let Some(parent) = records[selected].parent_id else {
                break;
            };
            selected = parent as usize;
        }
    }

    let mut remap = vec![None; records.len()];
    let mut filtered: Vec<snapshot::FlatSnapshotRecord> = Vec::new();
    let mut filtered_sources = HashMap::new();
    for (old_id, record) in records.iter().enumerate() {
        if !included[old_id] {
            continue;
        }
        let new_id = filtered.len() as u64;
        remap[old_id] = Some(new_id);
        let mut record = record.clone();
        record.id = new_id;
        record.child_count = 0;
        record.child_index = 0;
        if old_id == 0 {
            record.parent_id = None;
            record.source_included = None;
        } else {
            let old_parent = record
                .parent_id
                .ok_or("selective disk record is disconnected")?
                as usize;
            let new_parent =
                remap[old_parent].ok_or("selective disk record omitted a required ancestor")?;
            record.parent_id = Some(new_parent);
            record.child_index = filtered[new_parent as usize].child_count;
            filtered[new_parent as usize].child_count += 1;
            if ScriptClass::from_class(&record.class).is_some() {
                record.source_included = Some(selected_subtree[old_id]);
            }
        }
        if record.source_included != Some(false) {
            if let Some(path) = source_paths.get(&(old_id as u64)) {
                filtered_sources.insert(new_id, path.clone());
            }
        }
        filtered.push(record);
    }
    Ok((filtered, filtered_sources, deletes))
}

fn prepare_snapshot_service_stream(
    root: &Path,
    service: &str,
    selected_paths: Option<&[String]>,
) -> Result<SnapshotServiceStream, String> {
    let generation = crate::fs_safety::capture_tree_metadata(root, service)?;
    let disk = snapshot::emit_flat_service(root, service)
        .map_err(|error| format!("snapshot stream {service}: {error}"))?;
    if crate::fs_safety::capture_tree_metadata(root, service)? != generation {
        return Err(format!(
            "disk service {service} changed while its structure was emitted"
        ));
    }
    let (records, source_paths, deletes) = if let Some(selected_paths) = selected_paths {
        selective_flat_service(service, disk.records, disk.source_paths, selected_paths)?
    } else {
        (disk.records, disk.source_paths, Vec::new())
    };
    let validated = validate_flat_snapshot(&records, service, true)?;
    let source_ids = validated
        .script_ids
        .into_iter()
        .filter(|id| {
            records
                .get(*id as usize)
                .is_some_and(|record| record.source_included != Some(false))
        })
        .collect::<Vec<_>>();
    let mut source_generations = HashMap::with_capacity(source_ids.len());
    for id in &source_ids {
        let path = source_paths
            .get(id)
            .ok_or_else(|| format!("disk script stream ID {id} has no Source path"))?;
        source_generations.insert(*id, crate::fs_safety::file_generation_no_follow(path)?);
    }
    Ok(SnapshotServiceStream {
        project_root: root.to_path_buf(),
        service: service.to_string(),
        phase: SnapshotStreamPhase::Structure,
        next_chunk: 0,
        records,
        record_offset: 0,
        source_ids,
        source_paths,
        source_generations,
        source_index: 0,
        active_source: None,
        deletes,
        delete_offset: 0,
        generation,
        initial_fingerprint: None,
        initial_fingerprint_result: Some(spawn_exact_fingerprint(
            root.to_path_buf(),
            service.to_string(),
        )),
        revalidate_result: None,
    })
}

fn spawn_snapshot_service_prepare(
    root: PathBuf,
    service: String,
    selected_paths: Option<Vec<String>>,
) -> std::sync::mpsc::Receiver<Result<SnapshotServiceStream, String>> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = prepare_snapshot_service_stream(&root, &service, selected_paths.as_deref());
        let _ = send.send(result);
    });
    receive
}

fn snapshot_stream_expired(session: &SnapshotStreamAccumulator) -> bool {
    match session.completed_at {
        Some(completed) => completed.elapsed() >= STREAM_COMPLETED_TTL,
        None => session.last_activity.elapsed() >= STREAM_SESSION_TTL,
    }
}

fn prune_snapshot_stream_sessions(
    sessions: &mut HashMap<PathBuf, Arc<Mutex<SnapshotStreamAccumulator>>>,
) {
    sessions.retain(|_, session| {
        session
            .try_lock()
            .map(|session| !snapshot_stream_expired(&session))
            .unwrap_or(true)
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamCleanupAttempt {
    Removed,
    Retry,
    Superseded,
}

fn try_remove_expired_stream_session<T>(
    sessions: &mut HashMap<PathBuf, Arc<Mutex<T>>>,
    project: &Path,
    expected: &Arc<Mutex<T>>,
    expired: impl FnOnce(&T) -> bool,
) -> StreamCleanupAttempt {
    if !sessions
        .get(project)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
    {
        return StreamCleanupAttempt::Superseded;
    }
    let should_remove = match expected.try_lock() {
        Ok(session) => expired(&session),
        // An expiry timer can race a long request that owns the session. It
        // must retry after contention instead of silently abandoning cleanup.
        Err(_) => return StreamCleanupAttempt::Retry,
    };
    if !should_remove {
        return StreamCleanupAttempt::Retry;
    }
    sessions.remove(project);
    StreamCleanupAttempt::Removed
}

fn schedule_snapshot_stream_cleanup(
    project: PathBuf,
    session: &Arc<Mutex<SnapshotStreamAccumulator>>,
    wake_after: Duration,
) {
    let session = Arc::downgrade(session);
    tokio::spawn(async move {
        tokio::time::sleep(wake_after).await;
        loop {
            let Some(session) = session.upgrade() else {
                return;
            };
            let remaining = {
                let session = session.lock().unwrap();
                match session.completed_at {
                    Some(completed) => STREAM_COMPLETED_TTL.saturating_sub(completed.elapsed()),
                    None => STREAM_SESSION_TTL.saturating_sub(session.last_activity.elapsed()),
                }
            };
            if !remaining.is_zero() {
                drop(session);
                tokio::time::sleep(remaining).await;
                continue;
            }
            let attempt = {
                let sessions =
                    SNAPSHOT_STREAM_ACCUMULATORS.get_or_init(|| Mutex::new(HashMap::new()));
                let mut sessions = sessions.lock().unwrap();
                try_remove_expired_stream_session(
                    &mut sessions,
                    &project,
                    &session,
                    snapshot_stream_expired,
                )
            };
            match attempt {
                StreamCleanupAttempt::Removed | StreamCleanupAttempt::Superseded => return,
                StreamCleanupAttempt::Retry => {
                    tokio::time::sleep(STREAM_CLEANUP_RETRY_DELAY).await;
                }
            }
        }
    });
}

fn snapshot_stream_request_hash(
    body: &SnapshotStreamBody,
) -> Result<crate::conflict::Hash, String> {
    serde_json::to_vec(body)
        .map(|encoded| hash(&encoded))
        .map_err(|error| error.to_string())
}

fn encoded_stream_response_len(response: &Value) -> Result<usize, String> {
    serde_json::to_vec(response)
        .map(|encoded| encoded.len())
        .map_err(|error| format!("encode snapshot stream response: {error}"))
}

fn structure_stream_response(
    stream_id: &str,
    service: &str,
    chunk_index: u64,
    final_chunk: bool,
    records: &[snapshot::FlatSnapshotRecord],
) -> Value {
    json!({
        "ok": true,
        "streamId": stream_id,
        "service": service,
        "phase": "structure",
        "chunkIndex": chunk_index,
        "finalChunk": final_chunk,
        "records": records,
    })
}

fn disk_prepare_stream_response(stream_id: &str, service: &str, chunk_index: u64) -> Value {
    json!({
        "ok": true,
        "streamId": stream_id,
        "service": service,
        "phase": "diskPrepare",
        "chunkIndex": chunk_index,
        "finalChunk": false,
    })
}

fn source_stream_response(
    stream_id: &str,
    service: &str,
    chunk_index: u64,
    final_chunk: bool,
    sources: &[StreamSourcePart],
    complete: bool,
) -> Value {
    let mut response = json!({
        "ok": true,
        "streamId": stream_id,
        "service": service,
        "phase": "sources",
        "chunkIndex": chunk_index,
        "finalChunk": final_chunk,
        "sources": sources,
    });
    if complete {
        response["action"] = Value::String("complete".into());
    }
    response
}

fn delete_stream_response(
    stream_id: &str,
    service: &str,
    chunk_index: u64,
    final_chunk: bool,
    deletes: &[Vec<String>],
    complete: bool,
) -> Value {
    let deletes = deletes
        .iter()
        .map(|path| json!({ "path": path, "pathMode": "generated" }))
        .collect::<Vec<_>>();
    let mut response = json!({
        "ok": true,
        "streamId": stream_id,
        "service": service,
        "phase": "deletes",
        "chunkIndex": chunk_index,
        "finalChunk": final_chunk,
        "deletes": deletes,
    });
    if complete {
        response["action"] = Value::String("complete".into());
    }
    response
}

fn poll_initial_pull_fingerprint(stream: &mut SnapshotServiceStream) -> Result<bool, String> {
    if stream.initial_fingerprint.is_some() {
        return Ok(true);
    }
    let result = stream
        .initial_fingerprint_result
        .as_ref()
        .ok_or("snapshot service fingerprint worker is missing")?
        .try_recv();
    match result {
        Err(std::sync::mpsc::TryRecvError::Empty) => Ok(false),
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            Err("snapshot service fingerprint worker disconnected".into())
        }
        Ok(Err(error)) => Err(error),
        Ok(Ok(fingerprint)) => {
            if fingerprint.metadata != stream.generation {
                return Err(format!(
                    "disk service {} changed before Source streaming began",
                    stream.service
                ));
            }
            stream.initial_fingerprint = Some(fingerprint);
            stream.initial_fingerprint_result = None;
            Ok(true)
        }
    }
}

fn poll_revalidated_pull_fingerprint(
    root: &Path,
    stream: &mut SnapshotServiceStream,
) -> Result<bool, String> {
    if stream.revalidate_result.is_none() {
        stream.revalidate_result = Some(spawn_exact_fingerprint(
            root.to_path_buf(),
            stream.service.clone(),
        ));
        return Ok(false);
    }
    let result = stream
        .revalidate_result
        .as_ref()
        .expect("revalidation worker was initialized")
        .try_recv();
    match result {
        Err(std::sync::mpsc::TryRecvError::Empty) => Ok(false),
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            Err("snapshot revalidation worker disconnected".into())
        }
        Ok(Err(error)) => Err(error),
        Ok(Ok(current)) => {
            let initial = stream
                .initial_fingerprint
                .as_ref()
                .ok_or("snapshot initial fingerprint is missing")?;
            if &current != initial {
                return Err(format!(
                    "disk service {} changed during snapshot streaming",
                    stream.service
                ));
            }
            stream.revalidate_result = None;
            Ok(true)
        }
    }
}

fn load_pull_source(stream: &mut SnapshotServiceStream) -> Result<(), String> {
    if stream.active_source.is_some() || stream.source_index >= stream.source_ids.len() {
        return Ok(());
    }
    let id = stream.source_ids[stream.source_index];
    let path = stream
        .source_paths
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("snapshot Source ID {id} has no disk path"))?;
    let expected_generation = stream
        .source_generations
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("snapshot Source ID {id} has no file generation"))?;
    if crate::fs_safety::file_generation_no_follow(&path)? != expected_generation {
        return Err(format!(
            "disk Source {} changed before it was streamed",
            path.display()
        ));
    }
    if expected_generation.len > MAX_STREAM_SOURCE_BYTES {
        return Err(format!(
            "disk Source {} exceeds {MAX_STREAM_SOURCE_BYTES} bytes",
            path.display()
        ));
    }
    let bytes = read_synced_file(&stream.project_root, &path)?;
    if crate::fs_safety::file_generation_no_follow(&path)? != expected_generation {
        return Err(format!(
            "disk Source {} changed while it was read",
            path.display()
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("disk Source {} is not valid UTF-8", path.display()))?;
    let sha256 = hash(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    stream.active_source = Some(PullActiveSource {
        id,
        total_bytes: text.len() as u64,
        text,
        offset: 0,
        part_index: 0,
        sha256,
        generation: expected_generation,
        path,
    });
    Ok(())
}

fn pull_source_part(active: &PullActiveSource) -> StreamSourcePart {
    let mut end = active
        .offset
        .saturating_add(STREAM_SOURCE_PART_BYTES)
        .min(active.text.len());
    while end > active.offset && !active.text.is_char_boundary(end) {
        end -= 1;
    }
    StreamSourcePart {
        id: active.id,
        part_index: active.part_index,
        offset: active.offset as u64,
        total_bytes: active.total_bytes,
        data: active.text[active.offset..end].to_string(),
        final_part: end == active.text.len(),
        sha256: active.sha256.clone(),
    }
}

const STREAM_RESPONSE_PACK_TARGET: usize = STREAM_SOURCE_CHUNK_BYTES - 2048;

fn produce_structure_response(
    stream_id: &str,
    stream: &mut SnapshotServiceStream,
) -> Result<Value, String> {
    let chunk_index = stream.next_chunk;
    let mut chunk = Vec::new();
    while stream.record_offset + chunk.len() < stream.records.len()
        && chunk.len() < STREAM_STRUCTURE_CHUNK_NODES
    {
        let next = stream.records[stream.record_offset + chunk.len()].clone();
        chunk.push(next);
        let final_chunk = stream.record_offset + chunk.len() == stream.records.len();
        let candidate =
            structure_stream_response(stream_id, &stream.service, chunk_index, final_chunk, &chunk);
        if encoded_stream_response_len(&candidate)? > STREAM_RESPONSE_PACK_TARGET {
            chunk.pop();
            break;
        }
    }
    if chunk.is_empty() {
        return Err(format!(
            "one structure record for {} exceeds the encoded response limit",
            stream.service
        ));
    }
    stream.record_offset += chunk.len();
    let final_chunk = stream.record_offset == stream.records.len();
    let response =
        structure_stream_response(stream_id, &stream.service, chunk_index, final_chunk, &chunk);
    if encoded_stream_response_len(&response)? > STREAM_SOURCE_CHUNK_BYTES {
        return Err("encoded structure response exceeds 512 KiB".into());
    }
    if final_chunk {
        stream.phase = SnapshotStreamPhase::Sources;
        stream.next_chunk = 0;
    } else {
        stream.next_chunk += 1;
    }
    Ok(response)
}

fn commit_pull_source_part(
    stream: &mut SnapshotServiceStream,
    part: &StreamSourcePart,
) -> Result<(), String> {
    let active = stream
        .active_source
        .as_mut()
        .ok_or("snapshot Source part has no active file")?;
    if crate::fs_safety::file_generation_no_follow(&active.path)? != active.generation {
        return Err(format!(
            "disk Source {} changed between streamed parts",
            active.path.display()
        ));
    }
    active.offset = active
        .offset
        .checked_add(part.data.len())
        .ok_or("snapshot Source offset overflowed")?;
    active.part_index += 1;
    if part.final_part {
        if active.offset != active.text.len() {
            return Err("snapshot Source final part ended at the wrong offset".into());
        }
        stream.active_source = None;
        stream.source_index += 1;
    }
    Ok(())
}

fn produce_source_response(
    root: &Path,
    stream_id: &str,
    stream: &mut SnapshotServiceStream,
    selective: bool,
    final_service: bool,
) -> Result<(Value, bool), String> {
    let chunk_index = stream.next_chunk;
    if !poll_initial_pull_fingerprint(stream)? {
        let response =
            source_stream_response(stream_id, &stream.service, chunk_index, false, &[], false);
        stream.next_chunk += 1;
        return Ok((response, false));
    }

    let mut parts = Vec::new();
    while parts.len() < STREAM_HASH_CHUNK_NODES && stream.source_index < stream.source_ids.len() {
        load_pull_source(stream)?;
        let part = pull_source_part(
            stream
                .active_source
                .as_ref()
                .expect("source loader initialized the active Source"),
        );
        let mut candidate_parts = parts.clone();
        candidate_parts.push(part.clone());
        let candidate = source_stream_response(
            stream_id,
            &stream.service,
            chunk_index,
            false,
            &candidate_parts,
            false,
        );
        if encoded_stream_response_len(&candidate)? > STREAM_RESPONSE_PACK_TARGET {
            if parts.is_empty() {
                return Err(format!(
                    "one Source part for stream ID {} exceeds the encoded response limit",
                    part.id
                ));
            }
            break;
        }
        commit_pull_source_part(stream, &part)?;
        parts.push(part);
    }

    let sources_complete =
        stream.source_index == stream.source_ids.len() && stream.active_source.is_none();
    let revalidated = if sources_complete {
        poll_revalidated_pull_fingerprint(root, stream)?
    } else {
        false
    };
    let final_chunk = sources_complete && revalidated;
    let complete = final_chunk && !selective && final_service;
    let response = source_stream_response(
        stream_id,
        &stream.service,
        chunk_index,
        final_chunk,
        &parts,
        complete,
    );
    if encoded_stream_response_len(&response)? > STREAM_SOURCE_CHUNK_BYTES {
        return Err("encoded Source response exceeds 512 KiB".into());
    }
    if final_chunk {
        if selective {
            stream.phase = SnapshotStreamPhase::Deletes;
            stream.next_chunk = 0;
            // Deletes can span multiple HTTP turns. Fence the exact disk
            // generation again immediately before their terminal response.
            stream.revalidate_result = None;
        }
    } else {
        stream.next_chunk += 1;
    }
    Ok((response, final_chunk))
}

fn produce_delete_response(
    root: &Path,
    stream_id: &str,
    stream: &mut SnapshotServiceStream,
    final_service: bool,
) -> Result<(Value, bool), String> {
    let chunk_index = stream.next_chunk;
    let mut chunk = Vec::new();
    while stream.delete_offset + chunk.len() < stream.deletes.len()
        && chunk.len() < STREAM_STRUCTURE_CHUNK_NODES
    {
        chunk.push(stream.deletes[stream.delete_offset + chunk.len()].clone());
        let candidate = delete_stream_response(
            stream_id,
            &stream.service,
            chunk_index,
            false,
            &chunk,
            false,
        );
        if encoded_stream_response_len(&candidate)? > STREAM_RESPONSE_PACK_TARGET {
            chunk.pop();
            break;
        }
    }
    if chunk.is_empty() && stream.delete_offset < stream.deletes.len() {
        return Err(format!(
            "one delete record for {} exceeds the encoded response limit",
            stream.service
        ));
    }
    stream.delete_offset += chunk.len();
    let deletes_complete = stream.delete_offset == stream.deletes.len();
    let revalidated = if deletes_complete {
        poll_revalidated_pull_fingerprint(root, stream)?
    } else {
        false
    };
    let final_chunk = deletes_complete && revalidated;
    let response = delete_stream_response(
        stream_id,
        &stream.service,
        chunk_index,
        final_chunk,
        &chunk,
        final_chunk && final_service,
    );
    if encoded_stream_response_len(&response)? > STREAM_SOURCE_CHUNK_BYTES {
        return Err("encoded delete response exceeds 512 KiB".into());
    }
    if !final_chunk {
        stream.next_chunk += 1;
    }
    Ok((response, final_chunk))
}

fn snapshot_phase_name(phase: SnapshotStreamPhase) -> &'static str {
    match phase {
        SnapshotStreamPhase::Structure => "structure",
        SnapshotStreamPhase::Sources => "sources",
        SnapshotStreamPhase::Deletes => "deletes",
    }
}

fn restore_snapshot_selective_grant(project: &Path, session: &SnapshotStreamAccumulator) {
    let (Some(choice_id), Some(paths)) = (
        session.selective_choice_id.as_ref(),
        session.selective_paths.as_ref(),
    ) else {
        return;
    };
    SELECTIVE_TRANSFER_GRANTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(
            (project.to_path_buf(), choice_id.clone()),
            SelectiveTransferGrant {
                paths: paths.clone(),
                created_at: Instant::now(),
            },
        );
}

async fn snapshot_stream(
    State(state): State<AppState>,
    Json(body): Json<SnapshotStreamBody>,
) -> Json<Value> {
    if body.plugin_protocol != Some(crate::ws::PLUGIN_PROTOCOL_VERSION) {
        return Json(json!({
            "ok": false,
            "error": format!(
                "incompatible Studio plugin protocol; expected {}",
                crate::ws::PLUGIN_PROTOCOL_VERSION
            ),
        }));
    }
    if body.request_id.is_empty() || body.request_id.len() > 128 {
        return Json(json!({ "ok": false, "error": "invalid snapshot stream requestId" }));
    }
    let request_hash = match snapshot_stream_request_hash(&body) {
        Ok(request_hash) => request_hash,
        Err(error) => {
            return Json(
                json!({ "ok": false, "error": format!("encode snapshot request: {error}") }),
            );
        }
    };
    let project = state.canonical_project.as_ref().clone();
    let sessions = SNAPSHOT_STREAM_ACCUMULATORS.get_or_init(|| Mutex::new(HashMap::new()));

    if body.phase == "start" {
        if body.stream_id.is_some() || body.service.is_some() || body.chunk_index.is_some() {
            return Json(json!({
                "ok": false,
                "error": "snapshot stream start cannot include a cursor",
            }));
        }
        {
            let mut sessions = sessions.lock().unwrap();
            prune_snapshot_stream_sessions(&mut sessions);
            if let Some(existing) = sessions.get(&project).cloned() {
                let existing = existing.lock().unwrap();
                if existing.request_id == body.request_id
                    && existing.last_request_hash == Some(request_hash)
                {
                    if let Some(response) = existing.last_response.clone() {
                        return Json(response);
                    }
                }
            }
            if sessions.len() >= MAX_STREAM_SESSIONS && !sessions.contains_key(&project) {
                return Json(json!({
                    "ok": false,
                    "error": "too many active streamed transfer sessions",
                }));
            }
        }

        let avoid_sync_paths = match compact_avoid_sync_paths(&json!({
            "avoidSyncPaths": body.avoid_sync_paths,
        })) {
            Ok(Some(paths)) => paths,
            Ok(None) => Vec::new(),
            Err(error) => return Json(json!({ "ok": false, "error": error })),
        };
        set_avoid_sync_paths(&project, avoid_sync_paths);

        let selective_grant = if let Some(choice_id) = body.choice_id.as_ref() {
            if body.strict {
                return Json(json!({
                    "ok": false,
                    "error": "selective snapshot streams cannot enable global strict pruning",
                }));
            }
            let grants = SELECTIVE_TRANSFER_GRANTS.get_or_init(|| Mutex::new(HashMap::new()));
            let mut grants = grants.lock().unwrap();
            grants.retain(|_, grant| grant.created_at.elapsed() < SELECTIVE_TRANSFER_GRANT_TTL);
            let Some(grant) = grants.remove(&(project.clone(), choice_id.clone())) else {
                return Json(json!({
                    "ok": false,
                    "error": "selective snapshot choiceId is stale, consumed, or unauthorized",
                }));
            };
            Some((choice_id.clone(), grant))
        } else {
            None
        };
        let selected_paths = selective_grant
            .as_ref()
            .map(|(_, grant)| grant.paths.clone());
        let selective_choice_id = selective_grant
            .as_ref()
            .map(|(choice_id, _)| choice_id.clone());
        let first_service = snapshot::SYNCED_SERVICES[0];
        let stream_id = new_choice_id();
        let mut session = SnapshotStreamAccumulator {
            request_id: body.request_id.clone(),
            stream_id: stream_id.clone(),
            selective_paths: selected_paths.clone(),
            selective_choice_id,
            next_service: 0,
            service_stream: None,
            prepare_result: Some(spawn_snapshot_service_prepare(
                project.clone(),
                first_service.to_string(),
                selected_paths,
            )),
            // Chunk zero is the bounded start response below.
            prepare_next_chunk: 1,
            last_request_hash: Some(request_hash),
            last_response: None,
            last_activity: Instant::now(),
            completed_at: None,
        };
        let response = disk_prepare_stream_response(&stream_id, first_service, 0);
        session.last_response = Some(response.clone());
        let session = Arc::new(Mutex::new(session));
        {
            let mut sessions = sessions.lock().unwrap();
            prune_snapshot_stream_sessions(&mut sessions);
            sessions.insert(project.clone(), session.clone());
        }
        schedule_snapshot_stream_cleanup(project, &session, STREAM_SESSION_TTL);
        return Json(response);
    }

    let Some(stream_id) = body.stream_id.as_deref() else {
        return Json(json!({ "ok": false, "error": "snapshot cursor requires streamId" }));
    };
    if body.strict || !body.avoid_sync_paths.is_empty() || body.choice_id.is_some() {
        return Json(json!({
            "ok": false,
            "error": "snapshot continuation cannot change start options",
        }));
    }
    let session_handle = sessions.lock().unwrap().get(&project).cloned();
    let Some(session_handle) = session_handle else {
        return Json(json!({
            "ok": false,
            "error": "snapshot stream session is stale; restart the pull",
        }));
    };
    let mut session = session_handle.lock().unwrap();
    if snapshot_stream_expired(&session) {
        return Json(json!({
            "ok": false,
            "error": "snapshot stream session expired; restart the pull",
        }));
    }
    if session.stream_id != stream_id || session.request_id != body.request_id {
        return Json(json!({
            "ok": false,
            "error": "snapshot stream cursor is stale",
        }));
    }
    if session.last_request_hash == Some(request_hash) {
        if let Some(response) = session.last_response.clone() {
            return Json(response);
        }
    }
    if session.completed_at.is_some() {
        return Json(json!({ "ok": false, "error": "snapshot stream already completed" }));
    }
    let expected_service = snapshot::SYNCED_SERVICES
        .get(session.next_service)
        .copied()
        .expect("incomplete snapshot stream has a service");
    if body.service.as_deref() != Some(expected_service) {
        return Json(json!({
            "ok": false,
            "error": format!("snapshot stream expected service {expected_service}"),
        }));
    }
    if session.service_stream.is_none() {
        if body.phase != "diskPrepare" || body.chunk_index != Some(session.prepare_next_chunk) {
            return Json(json!({
                "ok": false,
                "error": format!(
                    "snapshot stream expected {expected_service} diskPrepare chunk {}",
                    session.prepare_next_chunk
                ),
            }));
        }
        let prepared: Result<Option<SnapshotServiceStream>, String> = session
            .prepare_result
            .as_ref()
            .ok_or_else(|| "snapshot diskPrepare worker is missing".to_string())
            .and_then(|receive| match receive.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err("snapshot diskPrepare worker disconnected".into())
                }
                Ok(Err(error)) => Err(error),
                Ok(Ok(stream)) => Ok(Some(stream)),
            });
        match prepared {
            Ok(None) => {
                let response = disk_prepare_stream_response(
                    stream_id,
                    expected_service,
                    session.prepare_next_chunk,
                );
                session.prepare_next_chunk += 1;
                session.last_request_hash = Some(request_hash);
                session.last_response = Some(response.clone());
                session.last_activity = Instant::now();
                return Json(response);
            }
            Ok(Some(service_stream)) => {
                session.prepare_result = None;
                session.service_stream = Some(service_stream);
                let response = match produce_structure_response(
                    stream_id,
                    session
                        .service_stream
                        .as_mut()
                        .expect("prepared snapshot service is present"),
                ) {
                    Ok(response) => response,
                    Err(error) => {
                        restore_snapshot_selective_grant(&project, &session);
                        drop(session);
                        sessions.lock().unwrap().remove(&project);
                        return Json(json!({ "ok": false, "error": error }));
                    }
                };
                session.last_request_hash = Some(request_hash);
                session.last_response = Some(response.clone());
                session.last_activity = Instant::now();
                return Json(response);
            }
            Err(error) => {
                restore_snapshot_selective_grant(&project, &session);
                drop(session);
                sessions.lock().unwrap().remove(&project);
                return Json(json!({ "ok": false, "error": error }));
            }
        }
    }
    let final_service = session.next_service + 1 == snapshot::SYNCED_SERVICES.len();
    let selective = session.selective_paths.is_some();
    let stream = session
        .service_stream
        .as_mut()
        .expect("snapshot service was prepared");
    let expected_phase = snapshot_phase_name(stream.phase);
    if body.phase != expected_phase || body.chunk_index != Some(stream.next_chunk) {
        return Json(json!({
            "ok": false,
            "error": format!(
                "snapshot stream expected {expected_service} {expected_phase} chunk {}",
                stream.next_chunk
            ),
        }));
    }

    let result = match stream.phase {
        SnapshotStreamPhase::Structure => {
            produce_structure_response(stream_id, stream).map(|response| (response, false))
        }
        SnapshotStreamPhase::Sources => {
            produce_source_response(&project, stream_id, stream, selective, final_service)
        }
        SnapshotStreamPhase::Deletes => {
            produce_delete_response(&project, stream_id, stream, final_service)
        }
    };
    let (response, phase_finished) = match result {
        Ok(result) => result,
        Err(error) => {
            restore_snapshot_selective_grant(&project, &session);
            drop(session);
            let mut sessions = sessions.lock().unwrap();
            if sessions
                .get(&project)
                .is_some_and(|current| Arc::ptr_eq(current, &session_handle))
            {
                sessions.remove(&project);
            }
            return Json(json!({ "ok": false, "error": error }));
        }
    };

    let terminal = response.get("action").and_then(Value::as_str) == Some("complete");
    let service_complete = phase_finished
        && ((!selective && expected_phase == "sources")
            || (selective && expected_phase == "deletes"));
    if service_complete && !terminal {
        session.next_service += 1;
        session.service_stream = None;
        let next_service = snapshot::SYNCED_SERVICES[session.next_service];
        session.prepare_result = Some(spawn_snapshot_service_prepare(
            project.clone(),
            next_service.to_string(),
            session.selective_paths.clone(),
        ));
        session.prepare_next_chunk = 0;
    }
    session.last_request_hash = Some(request_hash);
    session.last_response = Some(response.clone());
    session.last_activity = Instant::now();
    if terminal {
        session.completed_at = Some(Instant::now());
        schedule_snapshot_stream_cleanup(project, &session_handle, STREAM_COMPLETED_TTL);
    }
    Json(response)
}

async fn snapshot(
    State(state): State<AppState>,
    Query(params): Query<SnapshotParams>,
) -> Json<Value> {
    let project = state.canonical_project.as_path();
    let services = match params.service.as_deref() {
        Some(service) => snapshot::emit_service(project, service).map(|node| vec![node]),
        None if params.strict && params.force_prune => snapshot::SYNCED_SERVICES
            .iter()
            .map(|service| snapshot::emit_service(project, service))
            .collect(),
        None => snapshot::emit_services(project),
    };
    let services = match services {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({ "ok": false, "error": format!("snapshot: {e}") }));
        }
    };
    let bootstrap = params.service.is_none() && !params.strict && services.is_empty();
    let plugin_connected = state.active_plugin.lock().unwrap().is_some();
    Json(json!({
        "services": services,
        "bootstrap": bootstrap,
        "strict": params.strict,
        "forcePrune": params.force_prune,
        "service": params.service,
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
    let mut compacted_set = HashSet::with_capacity(ordered.len());
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
        let has_selected_ancestor = path
            .match_indices('/')
            .map(|(index, _)| &path[..index])
            .any(|ancestor| compacted_set.contains(ancestor));
        if has_selected_ancestor {
            continue;
        }
        compacted_set.insert(path.clone());
        compacted.push(path);
    }
    Ok(compacted)
}

fn shallow_snapshot_node(node: &Value) -> Value {
    let mut shallow = Map::new();
    if let Some(object) = node.as_object() {
        for (key, value) in object {
            if key != "properties" && key != "children" {
                shallow.insert(key.clone(), value.clone());
            }
        }
    }
    shallow.insert("properties".into(), Value::Object(Map::new()));
    shallow.insert("children".into(), Value::Array(Vec::new()));
    Value::Object(shallow)
}

fn build_selective_snapshot(root: &Path, paths: &[String]) -> Result<Value, String> {
    let selected_paths = compact_selected_paths(paths)?;
    let services = snapshot::emit_services(root)
        .map_err(|error| format!("selective snapshot scan {}: {error}", root.display()))?;
    let disk_nodes = diff::collect_local_snapshot_entries(&services);
    let mut ops = Vec::new();
    let mut emitted_ancestors = BTreeSet::new();

    for path in &selected_paths {
        let segments = path.split('/').map(str::to_string).collect::<Vec<_>>();
        if let Some(entry) = disk_nodes.get(path) {
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
                    "targetPath": segments[..depth].to_vec(),
                    "pathMode": "generated",
                    "diskPath": ancestor.disk_path,
                    "node": shallow_snapshot_node(ancestor.node),
                    "strict": false,
                    "forcePrune": false,
                }));
            }
            ops.push(json!({
                "op": "set",
                "path": segments[..segments.len() - 1].to_vec(),
                "targetPath": segments,
                "pathMode": "generated",
                "diskPath": entry.disk_path,
                "node": entry.node,
                "strict": true,
                "forcePrune": true,
            }));
        } else {
            // The selected path exists only in Studio. Applying the disk state
            // therefore means removing that synced instance from Studio.
            ops.push(json!({
                "op": "delete",
                "path": segments,
                "pathMode": "generated",
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

#[derive(Clone, Deserialize, Serialize)]
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
    #[serde(rename = "streamId", default)]
    stream_id: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(rename = "chunkIndex", default)]
    chunk_index: Option<u64>,
    #[serde(rename = "finalChunk", default)]
    final_chunk: bool,
    #[serde(default)]
    records: Vec<snapshot::FlatSnapshotRecord>,
    #[serde(default)]
    sources: Vec<StreamSourcePart>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamSourcePart {
    id: u64,
    part_index: u64,
    offset: u64,
    total_bytes: u64,
    data: String,
    final_part: bool,
    sha256: String,
}

// `apply_service_node` intentionally mirrors the nested Studio projection onto
// nested filesystem directories. Keep that implementation simple, but bound
// the recursive portion before it starts. This limit is also below
// serde_json's default nesting ceiling after accounting for each node's
// surrounding `children` array, so an over-deep request that reaches the
// handler receives our structured bootstrap error instead of approaching the
// Rust call-stack limit.
const MAX_BOOTSTRAP_INSTANCE_DEPTH: usize = 48;
const MAX_BOOTSTRAP_NODES: usize = 1_000_000;
const STREAM_REQUEST_BODY_BYTES: usize = 512 * 1024;
const STREAM_STRUCTURE_CHUNK_NODES: usize = 512;
const STREAM_HASH_CHUNK_NODES: usize = 64;
const STREAM_SOURCE_CHUNK_BYTES: usize = 512 * 1024;
const STREAM_SOURCE_PART_BYTES: usize = 64 * 1024;
const MAX_STREAM_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STREAM_SERVICE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STREAM_SESSION_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_STREAM_NAME_BYTES: usize = 32 * 1024;
const MAX_STREAM_CLASS_BYTES: usize = 128;
const MAX_STREAM_SERVICE_STRUCTURE_BYTES: usize = 64 * 1024 * 1024;
const MAX_STREAM_SESSION_STRUCTURE_BYTES: usize = 128 * 1024 * 1024;
const MAX_SUCCESSFUL_STREAM_BACKUPS: usize = 32;
const SUCCESSFUL_STREAM_BACKUP_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SUCCESSFUL_STREAM_BACKUP_MARKER: &str = ".rosync-successful-stream-backup.json";
const MAX_SUCCESSFUL_STREAM_BACKUP_MARKER_BYTES: u64 = 2 * 1024;
const STREAM_SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const STREAM_COMPLETED_TTL: Duration = Duration::from_secs(2 * 60);
const STREAM_CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(25);
const MAX_STREAM_SESSIONS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PushStreamPhase {
    Structure,
    DiskFence,
    Sources,
    DiskRevalidate,
}

#[derive(Clone)]
struct ReceivingSource {
    id: u64,
    next_part: u64,
    offset: u64,
    total_bytes: u64,
    sha256: crate::conflict::Hash,
    hasher: Sha256,
}

#[derive(Clone, PartialEq, Eq)]
struct ExactTreeFingerprint {
    metadata: crate::fs_safety::TreeGeneration,
    content_hash: crate::conflict::Hash,
}

#[derive(Debug)]
struct StreamCommitResult {
    applied: usize,
    backup: Option<PathBuf>,
    created: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum StreamRecoveryAction {
    RestoreBackup,
    RemoveCreatedService,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommittedStreamService {
    service: String,
    created: bool,
    backup: Option<PathBuf>,
    recovery_action: StreamRecoveryAction,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SuccessfulStreamBackupMarker {
    version: u8,
    kind: String,
    stream_id: String,
    completed_services: usize,
    transaction: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamCommitHookPoint {
    BeforeBackupRename,
    AfterBackupRename,
    BeforeStageInstall,
    AfterStageInstall,
}

#[cfg(test)]
type StreamCommitTestHook =
    Arc<dyn Fn(StreamCommitHookPoint, &Path, &Path, &Path) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
struct StreamCommitControl {
    cancelled: bool,
    committed: bool,
    #[cfg(test)]
    test_hook: Option<StreamCommitTestHook>,
    retained_backup: Option<PathBuf>,
    partial_failure: bool,
}

struct StreamCommitInput {
    state: AppState,
    service: String,
    service_node: Value,
    source_dir: tempfile::TempDir,
    initial_fingerprint: ExactTreeFingerprint,
    strict: bool,
    force_prune: bool,
    commit_control: Arc<Mutex<StreamCommitControl>>,
}

struct PushServiceStream {
    service: String,
    phase: PushStreamPhase,
    next_chunk: u64,
    records: Vec<snapshot::FlatSnapshotRecord>,
    accepted_structure_bytes: usize,
    accepted_source_bytes: u64,
    service_node: Option<Value>,
    script_ids: Vec<u64>,
    next_script: usize,
    receiving_source: Option<ReceivingSource>,
    source_dir: Option<tempfile::TempDir>,
    initial_fingerprint: Option<ExactTreeFingerprint>,
    fence_result: Option<std::sync::mpsc::Receiver<Result<ExactTreeFingerprint, String>>>,
    commit_result: Option<std::sync::mpsc::Receiver<Result<StreamCommitResult, String>>>,
    commit_control: Option<Arc<Mutex<StreamCommitControl>>>,
}

impl Drop for PushServiceStream {
    fn drop(&mut self) {
        if let Some(control) = self.commit_control.as_ref() {
            let mut control = control.lock().unwrap();
            if !control.committed {
                control.cancelled = true;
            }
        }
    }
}

struct PushStreamAccumulator {
    stream_id: String,
    strict: bool,
    force_prune: bool,
    next_service: usize,
    service_stream: PushServiceStream,
    applied: usize,
    backups: Vec<PathBuf>,
    committed_services: Vec<CommittedStreamService>,
    accepted_stream_bytes: usize,
    accepted_source_bytes: u64,
    last_request_hash: Option<crate::conflict::Hash>,
    last_response: Option<Value>,
    last_activity: Instant,
    completed_at: Option<Instant>,
}

static PUSH_STREAM_ACCUMULATORS: OnceLock<
    Mutex<HashMap<PathBuf, Arc<Mutex<PushStreamAccumulator>>>>,
> = OnceLock::new();

#[derive(Debug)]
struct ValidatedFlatSnapshot {
    service: Value,
    script_ids: Vec<u64>,
}

fn validate_stream_record_chunk_fields(
    records: &[snapshot::FlatSnapshotRecord],
) -> Result<(), String> {
    for record in records {
        if record.name.len() > MAX_STREAM_NAME_BYTES {
            return Err(format!(
                "streamed node {} name exceeds {MAX_STREAM_NAME_BYTES} bytes",
                record.id
            ));
        }
        if record.class.len() > MAX_STREAM_CLASS_BYTES {
            return Err(format!(
                "streamed node {} class exceeds {MAX_STREAM_CLASS_BYTES} bytes",
                record.id
            ));
        }
        if record
            .disk_fragment
            .as_ref()
            .is_some_and(|fragment| fragment.len() > MAX_STREAM_NAME_BYTES)
        {
            return Err(format!(
                "streamed node {} diskFragment exceeds {MAX_STREAM_NAME_BYTES} bytes",
                record.id
            ));
        }
    }
    Ok(())
}

fn encoded_stream_record_chunk_bytes(
    records: &[snapshot::FlatSnapshotRecord],
) -> Result<usize, String> {
    struct ByteCounter(usize);

    impl std::io::Write for ByteCounter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(buffer.len())
                .ok_or_else(|| std::io::Error::other("encoded byte count overflowed"))?;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, records)
        .map_err(|error| format!("encode streamed structure chunk: {error}"))?;
    Ok(counter.0)
}

fn charge_stream_structure_bytes(
    service_bytes: usize,
    session_bytes: usize,
    chunk_bytes: usize,
) -> Result<(usize, usize), String> {
    let service_bytes = service_bytes
        .checked_add(chunk_bytes)
        .ok_or("streamed service structure byte count overflowed")?;
    if service_bytes > MAX_STREAM_SERVICE_STRUCTURE_BYTES {
        return Err(format!(
            "streamed service structure exceeds {MAX_STREAM_SERVICE_STRUCTURE_BYTES} encoded bytes"
        ));
    }
    let session_bytes = session_bytes
        .checked_add(chunk_bytes)
        .ok_or("streamed session structure byte count overflowed")?;
    if session_bytes > MAX_STREAM_SESSION_STRUCTURE_BYTES {
        return Err(format!(
            "streamed session structure exceeds {MAX_STREAM_SESSION_STRUCTURE_BYTES} encoded bytes"
        ));
    }
    Ok((service_bytes, session_bytes))
}

fn charge_stream_source_bytes(
    service_bytes: u64,
    session_bytes: u64,
    declared_bytes: u64,
) -> Result<(u64, u64), String> {
    let service_bytes = service_bytes
        .checked_add(declared_bytes)
        .ok_or("streamed service Source byte count overflowed")?;
    if service_bytes > MAX_STREAM_SERVICE_SOURCE_BYTES {
        return Err(format!(
            "streamed service Sources exceed {MAX_STREAM_SERVICE_SOURCE_BYTES} declared bytes"
        ));
    }
    let session_bytes = session_bytes
        .checked_add(declared_bytes)
        .ok_or("streamed session Source byte count overflowed")?;
    if session_bytes > MAX_STREAM_SESSION_SOURCE_BYTES {
        return Err(format!(
            "streamed session Sources exceed {MAX_STREAM_SESSION_SOURCE_BYTES} declared bytes"
        ));
    }
    Ok((service_bytes, session_bytes))
}

fn validate_flat_snapshot(
    records: &[snapshot::FlatSnapshotRecord],
    expected_service: &str,
    allow_disk_identity: bool,
) -> Result<ValidatedFlatSnapshot, String> {
    if records.is_empty() {
        return Err("streamed service structure is empty".into());
    }
    if records.len() > MAX_BOOTSTRAP_NODES {
        return Err(format!(
            "streamed service contains more than the supported limit of {MAX_BOOTSTRAP_NODES} instances"
        ));
    }
    for (index, record) in records.iter().enumerate() {
        if record.id != index as u64 {
            return Err(format!(
                "streamed service IDs must be dense preorder ordinals; expected {index}, received {}",
                record.id
            ));
        }
        if record.name.contains('\0') {
            return Err(format!(
                "streamed node {index} has an invalid NUL in its name"
            ));
        }
        if !allow_disk_identity && record.source_included.is_some() {
            return Err(format!(
                "Studio structure node {index} cannot choose sourceIncluded"
            ));
        }
        if index == 0 {
            if record.parent_id.is_some()
                || record.child_index != 0
                || record.name != expected_service
                || record.class != expected_service
                || record.avoid_sync
                || record.avoid_sync_carrier
            {
                return Err(format!(
                    "streamed service root must be the matching {expected_service} service"
                ));
            }
            if record.disk_fragment.is_some() || record.disk_fragment_is_dir.is_some() {
                return Err("streamed service root cannot carry a disk fragment".into());
            }
            continue;
        }
        let Some(parent_id) = record.parent_id else {
            return Err(format!(
                "streamed node {index} is disconnected from its service"
            ));
        };
        if parent_id >= record.id {
            return Err(format!(
                "streamed node {index} must follow its parent in preorder"
            ));
        }
        if !is_scoped_class(&record.class) {
            return Err(format!(
                "streamed node {index} has unsupported projected class {}",
                record.class
            ));
        }
        if record.avoid_sync && record.avoid_sync_carrier {
            return Err(format!(
                "streamed node {index} cannot be both an AvoidSync boundary and carrier"
            ));
        }
        if record.avoid_sync && record.child_count != 0 {
            return Err(format!(
                "streamed AvoidSync boundary {index} must omit descendants"
            ));
        }
        if record.avoid_sync_carrier && record.class != "Folder" {
            return Err(format!(
                "streamed AvoidSync carrier {index} must project as a Folder"
            ));
        }
        if (record.child_count > 0 || record.class == "Folder") && !record.has_children {
            return Err(format!(
                "streamed node {index} hasChildren does not match its projected children"
            ));
        }
        match (
            allow_disk_identity,
            record.disk_fragment.as_deref(),
            record.disk_fragment_is_dir,
        ) {
            (false, None, None) => {}
            (false, _, _) => {
                return Err(format!(
                    "Studio structure node {index} cannot choose a disk fragment"
                ));
            }
            (true, Some(fragment), Some(is_dir)) => {
                crate::fs_map::validate_portable_component(fragment)?;
                let expected_is_dir = record.class == "Folder" || record.has_children;
                if is_dir != expected_is_dir {
                    return Err(format!(
                        "disk fragment shape for streamed node {index} does not match its projection"
                    ));
                }
            }
            (true, _, _) => {
                return Err(format!(
                    "disk structure node {index} is missing its exact fragment identity"
                ));
            }
        }
    }

    let mut children = vec![Vec::<usize>::new(); records.len()];
    let mut depths = vec![0usize; records.len()];
    for (index, record) in records.iter().enumerate().skip(1) {
        let parent = record.parent_id.expect("non-root parent was validated") as usize;
        let depth = depths[parent] + 1;
        if depth > snapshot::MAX_FLAT_INSTANCE_DEPTH {
            return Err(format!(
                "flat Studio tree depth exceeds the supported limit of {} instances",
                snapshot::MAX_FLAT_INSTANCE_DEPTH
            ));
        }
        depths[index] = depth;
        children[parent].push(index);
    }
    for (parent, child_ids) in children.iter_mut().enumerate() {
        child_ids.sort_by_key(|child| records[*child].child_index);
        for (expected_index, child) in child_ids.iter().copied().enumerate() {
            if records[child].child_index != expected_index as u32 {
                return Err(format!(
                    "streamed parent {parent} childIndex values must be contiguous from zero; expected {expected_index}, received {}",
                    records[child].child_index
                ));
            }
        }
        if records[parent].child_count as usize != child_ids.len() {
            return Err(format!(
                "streamed parent {parent} declared {} children but received {}",
                records[parent].child_count,
                child_ids.len()
            ));
        }
    }

    fn build_node(
        id: usize,
        records: &[snapshot::FlatSnapshotRecord],
        children: &[Vec<usize>],
    ) -> Value {
        let record = &records[id];
        let mut object = Map::new();
        object.insert("class".into(), Value::String(record.class.clone()));
        object.insert("name".into(), Value::String(record.name.clone()));
        object.insert("streamId".into(), json!(record.id));
        object.insert("hasChildren".into(), Value::Bool(record.has_children));
        object.insert("properties".into(), Value::Object(Map::new()));
        object.insert(
            "children".into(),
            Value::Array(
                children[id]
                    .iter()
                    .map(|child| build_node(*child, records, children))
                    .collect(),
            ),
        );
        if record.avoid_sync {
            object.insert("avoidSync".into(), Value::Bool(true));
        }
        if record.avoid_sync_carrier {
            object.insert("avoidSyncCarrier".into(), Value::Bool(true));
        }
        if let Some(fragment) = record.disk_fragment.as_ref() {
            object.insert("diskFragment".into(), Value::String(fragment.clone()));
        }
        if let Some(is_dir) = record.disk_fragment_is_dir {
            object.insert("diskFragmentIsDir".into(), Value::Bool(is_dir));
        }
        if let Some(source_included) = record.source_included {
            object.insert("sourceIncluded".into(), Value::Bool(source_included));
        }
        Value::Object(object)
    }

    let script_ids = records
        .iter()
        .filter(|record| {
            !record.avoid_sync
                && !record.avoid_sync_carrier
                && ScriptClass::from_class(&record.class).is_some()
        })
        .map(|record| record.id)
        .collect();
    Ok(ValidatedFlatSnapshot {
        service: build_node(0, records, &children),
        script_ids,
    })
}

fn preflight_streamed_service_fragments(service: &Value) -> Result<(), String> {
    let mut pending = vec![service];
    while let Some(node) = pending.pop() {
        let children = node
            .get("children")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let assignments = child_fragment_assignments(children);
        let mut folded = HashSet::with_capacity(assignments.len());
        for assignment in assignments {
            crate::fs_map::validate_portable_component(&assignment.fragment)?;
            if !folded.insert(assignment.fragment.to_ascii_lowercase()) {
                return Err(format!(
                    "streamed siblings collapse to the same portable fragment {:?}",
                    assignment.fragment
                ));
            }
            if assignment.action == ChildAction::Materialize
                && ScriptClass::from_class(assignment.projection_class).is_some()
                && assignment.projection_has_children
            {
                let name = assignment
                    .node
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("streamed script is missing its name")?;
                let class = ScriptClass::from_class(assignment.projection_class)
                    .expect("script class was checked");
                crate::fs_map::validate_portable_component(&portable_init_file_name(name, class))?;
            }
        }
        pending.extend(children.iter());
    }
    Ok(())
}

fn hash_tree_contents(
    project_root: &Path,
    generation: &crate::fs_safety::TreeGeneration,
) -> Result<crate::conflict::Hash, String> {
    let mut hasher = Sha256::new();
    for entry in generation.entries() {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        let relative = entry.relative.to_str().ok_or_else(|| {
            format!(
                "non-UTF-8 path cannot be fingerprinted: {}",
                entry.path.display()
            )
        })?;
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update(entry.generation.len.to_le_bytes());
        if crate::fs_safety::file_generation_no_follow(&entry.path)? != entry.generation {
            return Err(format!(
                "disk file {} changed before it could be fingerprinted",
                entry.path.display()
            ));
        }
        let bytes = read_synced_file(project_root, &entry.path)?;
        hasher.update(&bytes);
        if crate::fs_safety::file_generation_no_follow(&entry.path)? != entry.generation {
            return Err(format!(
                "disk file {} changed while it was fingerprinted",
                entry.path.display()
            ));
        }
    }
    Ok(hasher.finalize().into())
}

fn capture_exact_tree_fingerprint(
    root: &Path,
    service: &str,
) -> Result<ExactTreeFingerprint, String> {
    let metadata = crate::fs_safety::capture_tree_metadata(root, service)?;
    let content_hash = hash_tree_contents(root, &metadata)?;
    if crate::fs_safety::capture_tree_metadata(root, service)? != metadata {
        return Err(format!(
            "disk service {service} changed while its transfer fence was captured"
        ));
    }
    Ok(ExactTreeFingerprint {
        metadata,
        content_hash,
    })
}

fn relocated_tree_generation_matches(
    expected: &crate::fs_safety::TreeGeneration,
    actual: &crate::fs_safety::TreeGeneration,
) -> bool {
    expected.service == actual.service
        && expected.present == actual.present
        && expected.root_generation == actual.root_generation
        && expected.entries().len() == actual.entries().len()
        && expected
            .entries()
            .iter()
            .zip(actual.entries())
            .all(|(expected, actual)| {
                expected.relative == actual.relative
                    && expected.kind == actual.kind
                    && expected.generation == actual.generation
            })
}

fn relocated_fingerprint_matches(
    expected: &ExactTreeFingerprint,
    actual: &ExactTreeFingerprint,
) -> bool {
    expected.content_hash == actual.content_hash
        && relocated_tree_generation_matches(&expected.metadata, &actual.metadata)
}

fn copy_fenced_service_to_stage(
    project_root: &Path,
    generation: &crate::fs_safety::TreeGeneration,
    stage_root: &Path,
) -> Result<crate::conflict::Hash, String> {
    use std::io::{Read as _, Write as _};

    let stage_service = stage_root.join(&generation.service);
    std::fs::create_dir(&stage_service)
        .map_err(|error| format!("create staged service {}: {error}", stage_service.display()))?;
    for entry in generation.entries() {
        if entry.kind == crate::fs_safety::SafeEntryKind::Directory {
            ensure_descendant_directory_chain(stage_root, &stage_service.join(&entry.relative))?;
        }
    }

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    for entry in generation.entries() {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        if crate::fs_safety::file_generation_no_follow(&entry.path)? != entry.generation {
            return Err(format!(
                "disk file {} changed before staging",
                entry.path.display()
            ));
        }
        let relative = entry
            .relative
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 path cannot be staged: {}", entry.path.display()))?;
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update(entry.generation.len.to_le_bytes());
        let target = stage_service.join(&entry.relative);
        let parent = target
            .parent()
            .ok_or_else(|| format!("staged source has no parent: {}", target.display()))?;
        ensure_descendant_directory_chain(stage_root, parent)?;
        let source_guard =
            crate::fs_safety::guard_synced_parent_chain(project_root, &entry.path, false).map_err(
                |error| format!("guard staged source {}: {error}", entry.path.display()),
            )?;
        let target_guard =
            crate::fs_safety::guard_descendant_parent_chain(stage_root, &target, true)
                .map_err(|error| format!("guard staged target {}: {error}", target.display()))?;
        source_guard.verify().map_err(|error| {
            format!(
                "verify staged source parent {}: {error}",
                entry.path.display()
            )
        })?;
        target_guard.verify().map_err(|error| {
            format!("verify staged target parent {}: {error}", target.display())
        })?;
        let mut source = crate::fs_safety::open_regular_file_no_follow(&entry.path)
            .map_err(|error| format!("read {}: {error}", entry.path.display()))?;
        let mut destination = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| format!("create {}: {error}", target.display()))?;
        loop {
            let count = source
                .read(&mut buffer)
                .map_err(|error| format!("read {}: {error}", entry.path.display()))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            destination
                .write_all(&buffer[..count])
                .map_err(|error| format!("write {}: {error}", target.display()))?;
        }
        destination
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", target.display()))?;
        let permissions = crate::fs_safety::require_metadata_no_follow(&entry.path)
            .map_err(|error| format!("inspect {}: {error}", entry.path.display()))?
            .permissions();
        std::fs::set_permissions(&target, permissions)
            .map_err(|error| format!("set permissions {}: {error}", target.display()))?;
        if crate::fs_safety::file_generation_no_follow(&entry.path)? != entry.generation {
            return Err(format!(
                "disk file {} changed while staging",
                entry.path.display()
            ));
        }
        source_guard.verify().map_err(|error| {
            format!(
                "staged source parent changed {}: {error}",
                entry.path.display()
            )
        })?;
        target_guard.verify().map_err(|error| {
            format!("staged target parent changed {}: {error}", target.display())
        })?;
    }
    Ok(hasher.finalize().into())
}

fn create_stream_backup_destination(
    root: &Path,
    service: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let backup_root = ensure_descendant_directory_chain(root, &root.join(".rosync-backups"))?;
    static STREAM_BACKUP_SEQUENCE: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(1);
    let sequence = STREAM_BACKUP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let transaction = backup_root.join(format!("stream-{stamp}-{sequence}"));
    let guard = crate::fs_safety::guard_descendant_parent_chain(root, &transaction, true).map_err(
        |error| {
            format!(
                "guard backup transaction {}: {error}",
                transaction.display()
            )
        },
    )?;
    guard.verify().map_err(|error| {
        format!(
            "verify backup transaction parent {}: {error}",
            transaction.display()
        )
    })?;
    std::fs::create_dir(&transaction).map_err(|error| {
        format!(
            "create backup transaction {}: {error}",
            transaction.display()
        )
    })?;
    if let Err(error) = guard.verify() {
        let failure = format!(
            "backup transaction parent changed {}: {error}",
            transaction.display()
        );
        return Err(
            match cleanup_empty_stream_backup_transaction(root, &transaction) {
                Ok(()) => failure,
                Err(cleanup) => {
                    format!("{failure}; empty backup transaction cleanup failed: {cleanup}")
                }
            },
        );
    }
    Ok((transaction.join(service), transaction))
}

fn cleanup_empty_stream_backup_transaction(root: &Path, transaction: &Path) -> Result<(), String> {
    cleanup_empty_stream_backup_transaction_with(root, transaction, || Ok(()))
}

fn cleanup_empty_stream_backup_transaction_with(
    root: &Path,
    transaction: &Path,
    before_recheck: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let Some(metadata) = crate::fs_safety::metadata_no_follow(transaction)
        .map_err(|error| format!("inspect empty stream transaction: {error}"))?
    else {
        return Ok(());
    };
    if !metadata.is_dir() {
        return Err(format!(
            "refusing to clean non-directory stream transaction {}",
            transaction.display()
        ));
    }
    let guard = crate::fs_safety::guard_descendant_parent_chain(root, transaction, false).map_err(
        |error| {
            format!(
                "guard empty stream transaction {}: {error}",
                transaction.display()
            )
        },
    )?;
    let before =
        crate::fs_safety::directory_generation_no_follow(transaction).map_err(|error| {
            format!(
                "inspect stream transaction {}: {error}",
                transaction.display()
            )
        })?;
    if std::fs::read_dir(transaction)
        .map_err(|error| format!("scan stream transaction {}: {error}", transaction.display()))?
        .next()
        .transpose()
        .map_err(|error| format!("scan stream transaction {}: {error}", transaction.display()))?
        .is_some()
    {
        return Err(format!(
            "refusing to clean non-empty stream transaction {}",
            transaction.display()
        ));
    }
    before_recheck()?;
    if crate::fs_safety::directory_generation_no_follow(transaction).map_err(|error| {
        format!(
            "reinspect stream transaction {}: {error}",
            transaction.display()
        )
    })? != before
    {
        return Err(format!(
            "refusing to clean changed stream transaction {}",
            transaction.display()
        ));
    }
    guard.verify().map_err(|error| {
        format!(
            "stream transaction parent changed {}: {error}",
            transaction.display()
        )
    })?;
    std::fs::remove_dir(transaction).map_err(|error| {
        format!(
            "remove empty stream transaction {}: {error}",
            transaction.display()
        )
    })?;
    guard.verify().map_err(|error| {
        format!(
            "stream transaction parent changed after cleanup {}: {error}",
            transaction.display()
        )
    })
}

#[derive(Debug)]
enum PrunableStreamBackupEntry {
    File {
        path: PathBuf,
        generation: crate::fs_safety::FileGeneration,
    },
    Directory {
        path: PathBuf,
        identity: crate::fs_safety::FileIdentity,
    },
}

impl PrunableStreamBackupEntry {
    fn path(&self) -> &Path {
        match self {
            Self::File { path, .. } | Self::Directory { path, .. } => path,
        }
    }

    fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }
}

fn capture_prunable_stream_backup(
    root: &Path,
    transaction: &Path,
    discovered_generation: &crate::fs_safety::FileGeneration,
) -> Result<Vec<PrunableStreamBackupEntry>, String> {
    let backup_root = root.join(".rosync-backups");
    if transaction.parent() != Some(backup_root.as_path())
        || transaction
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| successful_stream_backup_stamp(name).is_none())
    {
        return Err(format!(
            "refusing to prune an unclassified stream backup {}",
            transaction.display()
        ));
    }
    let mut entries = Vec::new();
    let mut pending = vec![(transaction.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > crate::fs_safety::MAX_SERVICE_TREE_DEPTH {
            return Err(format!(
                "successful stream backup exceeds the safe depth limit: {}",
                transaction.display()
            ));
        }
        let directory_guard = crate::fs_safety::guard_descendant_directory_chain(root, &directory)
            .map_err(|error| {
                format!(
                    "guard successful stream backup directory {}: {error}",
                    directory.display()
                )
            })?;
        directory_guard.verify().map_err(|error| {
            format!(
                "verify successful stream backup directory {}: {error}",
                directory.display()
            )
        })?;
        let before = crate::fs_safety::directory_generation_no_follow(&directory)
            .map_err(|error| format!("inspect stream backup {}: {error}", directory.display()))?;
        if depth == 0 && &before != discovered_generation {
            return Err(format!(
                "refusing to prune stream backup replaced after discovery: {}",
                transaction.display()
            ));
        }
        let children = std::fs::read_dir(&directory)
            .map_err(|error| format!("scan stream backup {}: {error}", directory.display()))?;
        for child in children {
            let child = child
                .map_err(|error| format!("scan stream backup {}: {error}", directory.display()))?;
            let path = child.path();
            let metadata =
                crate::fs_safety::require_metadata_no_follow(&path).map_err(|error| {
                    format!("inspect stream backup entry {}: {error}", path.display())
                })?;
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else if metadata.is_file() {
                let generation = crate::fs_safety::file_generation_no_follow(&path)?;
                entries.push(PrunableStreamBackupEntry::File { path, generation });
            } else {
                return Err(format!(
                    "refusing unsupported stream backup entry {}",
                    path.display()
                ));
            }
            if entries.len() + pending.len() > MAX_BOOTSTRAP_NODES {
                return Err(format!(
                    "successful stream backup exceeds the safe entry limit of {MAX_BOOTSTRAP_NODES}"
                ));
            }
        }
        let after = crate::fs_safety::directory_generation_no_follow(&directory)
            .map_err(|error| format!("reinspect stream backup {}: {error}", directory.display()))?;
        if before != after {
            return Err(format!(
                "stream backup changed while it was scanned: {}",
                directory.display()
            ));
        }
        if depth == 0 && &after != discovered_generation {
            return Err(format!(
                "refusing to prune stream backup changed after discovery: {}",
                transaction.display()
            ));
        }
        directory_guard.verify().map_err(|error| {
            format!(
                "stream backup parent changed while scanning {}: {error}",
                directory.display()
            )
        })?;
        entries.push(PrunableStreamBackupEntry::Directory {
            path: directory,
            identity: before.identity,
        });
    }
    entries.sort_by(|left, right| {
        right
            .path()
            .components()
            .count()
            .cmp(&left.path().components().count())
            .then_with(|| right.is_file().cmp(&left.is_file()))
            .then_with(|| right.path().cmp(left.path()))
    });
    Ok(entries)
}

fn remove_successful_stream_backup(
    root: &Path,
    transaction: &Path,
    discovered_generation: &crate::fs_safety::FileGeneration,
) -> Result<(), String> {
    let backup_root = root.join(".rosync-backups");
    if transaction.parent() != Some(backup_root.as_path())
        || transaction
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| successful_stream_backup_stamp(name).is_none())
    {
        return Err(format!(
            "refusing to prune an unclassified stream backup {}",
            transaction.display()
        ));
    }
    validate_successful_stream_backup_marker(root, transaction)?;
    if crate::fs_safety::directory_generation_no_follow(transaction).map_err(|error| {
        format!(
            "reinspect discovered stream backup {}: {error}",
            transaction.display()
        )
    })? != *discovered_generation
    {
        return Err(format!(
            "refusing to prune stream backup replaced after discovery: {}",
            transaction.display()
        ));
    }
    let entries = capture_prunable_stream_backup(root, transaction, discovered_generation)?;
    for entry in entries {
        let path = entry.path().to_path_buf();
        let guard = crate::fs_safety::guard_descendant_parent_chain(root, &path, false)
            .map_err(|error| format!("guard stream backup removal {}: {error}", path.display()))?;
        guard.verify().map_err(|error| {
            format!(
                "verify stream backup removal parent {}: {error}",
                path.display()
            )
        })?;
        match entry {
            PrunableStreamBackupEntry::File { path, generation } => {
                if crate::fs_safety::file_generation_no_follow(&path)? != generation {
                    return Err(format!(
                        "refusing to prune changed stream backup file {}",
                        path.display()
                    ));
                }
                std::fs::remove_file(&path).map_err(|error| {
                    format!("remove stream backup file {}: {error}", path.display())
                })?;
            }
            PrunableStreamBackupEntry::Directory { path, identity } => {
                if crate::fs_safety::directory_generation_no_follow(&path)
                    .map_err(|error| {
                        format!(
                            "reinspect stream backup directory {}: {error}",
                            path.display()
                        )
                    })?
                    .identity
                    != identity
                {
                    return Err(format!(
                        "refusing to prune replaced stream backup directory {}",
                        path.display()
                    ));
                }
                if std::fs::read_dir(&path)
                    .map_err(|error| {
                        format!("verify empty stream backup {}: {error}", path.display())
                    })?
                    .next()
                    .transpose()
                    .map_err(|error| {
                        format!("verify empty stream backup {}: {error}", path.display())
                    })?
                    .is_some()
                {
                    return Err(format!(
                        "refusing to prune stream backup directory that gained entries: {}",
                        path.display()
                    ));
                }
                std::fs::remove_dir(&path).map_err(|error| {
                    format!("remove stream backup directory {}: {error}", path.display())
                })?;
            }
        }
        guard.verify().map_err(|error| {
            format!(
                "stream backup parent changed during removal {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn stream_backup_name_parts(name: &str, prefix: &str) -> Option<(u128, u64)> {
    if name.len() > 96 {
        return None;
    }
    let mut parts = name.strip_prefix(prefix)?.split('-');
    let stamp_text = parts.next()?;
    let sequence_text = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let stamp = stamp_text.parse::<u128>().ok()?;
    let sequence = sequence_text.parse::<u64>().ok()?;
    if sequence == 0 || stamp.to_string() != stamp_text || sequence.to_string() != sequence_text {
        return None;
    }
    Some((stamp, sequence))
}

fn successful_stream_backup_stamp(name: &str) -> Option<u128> {
    stream_backup_name_parts(name, "stream-success-").map(|(stamp, _)| stamp)
}

fn successful_stream_backup_marker(
    transaction: &Path,
    stream_id: &str,
) -> Result<SuccessfulStreamBackupMarker, String> {
    if stream_id.is_empty() || stream_id.len() > 128 {
        return Err("successful stream backup has an invalid stream ID".into());
    }
    let transaction_name = transaction
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("stream backup name is not UTF-8: {}", transaction.display()))?;
    if successful_stream_backup_stamp(transaction_name).is_none() {
        return Err(format!(
            "successful stream backup name is not canonical: {}",
            transaction.display()
        ));
    }
    Ok(SuccessfulStreamBackupMarker {
        version: 1,
        kind: "completed-stream".into(),
        stream_id: stream_id.to_string(),
        completed_services: snapshot::SYNCED_SERVICES.len(),
        transaction: transaction_name.to_string(),
    })
}

fn validate_successful_stream_backup_marker(
    root: &Path,
    transaction: &Path,
) -> Result<SuccessfulStreamBackupMarker, String> {
    let transaction_name = transaction
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("stream backup name is not UTF-8: {}", transaction.display()))?;
    if successful_stream_backup_stamp(transaction_name).is_none() {
        return Err(format!(
            "successful stream backup name is not canonical: {}",
            transaction.display()
        ));
    }
    let marker_path = transaction.join(SUCCESSFUL_STREAM_BACKUP_MARKER);
    let transaction_guard = crate::fs_safety::guard_descendant_directory_chain(root, transaction)
        .map_err(|error| {
        format!(
            "guard successful stream backup marker parent {}: {error}",
            transaction.display()
        )
    })?;
    let marker_guard = crate::fs_safety::guard_descendant_parent_chain(root, &marker_path, false)
        .map_err(|error| {
        format!(
            "guard successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    transaction_guard.verify().map_err(|error| {
        format!(
            "verify successful stream backup marker parent {}: {error}",
            transaction.display()
        )
    })?;
    marker_guard.verify().map_err(|error| {
        format!(
            "verify successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    let metadata = crate::fs_safety::require_metadata_no_follow(&marker_path).map_err(|error| {
        format!(
            "successful stream backup marker is missing or unsafe {}: {error}",
            marker_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_SUCCESSFUL_STREAM_BACKUP_MARKER_BYTES {
        return Err(format!(
            "successful stream backup marker is not a bounded regular file: {}",
            marker_path.display()
        ));
    }
    let before = crate::fs_safety::file_generation_no_follow(&marker_path)?;
    let bytes = crate::fs_safety::read_file_no_follow(&marker_path).map_err(|error| {
        format!(
            "read successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    if crate::fs_safety::file_generation_no_follow(&marker_path)? != before {
        return Err(format!(
            "successful stream backup marker changed while reading: {}",
            marker_path.display()
        ));
    }
    transaction_guard.verify().map_err(|error| {
        format!(
            "successful stream backup marker parent changed {}: {error}",
            transaction.display()
        )
    })?;
    marker_guard.verify().map_err(|error| {
        format!(
            "successful stream backup marker parent changed {}: {error}",
            marker_path.display()
        )
    })?;
    let marker: SuccessfulStreamBackupMarker = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "parse successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    if marker.version != 1
        || marker.kind != "completed-stream"
        || marker.stream_id.is_empty()
        || marker.stream_id.len() > 128
        || marker.completed_services != snapshot::SYNCED_SERVICES.len()
        || marker.transaction != transaction_name
    {
        return Err(format!(
            "successful stream backup marker has invalid provenance: {}",
            marker_path.display()
        ));
    }
    Ok(marker)
}

fn write_successful_stream_backup_marker(
    root: &Path,
    transaction: &Path,
    stream_id: &str,
) -> Result<(), String> {
    use std::io::Write as _;

    let marker = successful_stream_backup_marker(transaction, stream_id)?;
    let bytes = serde_json::to_vec(&marker)
        .map_err(|error| format!("encode successful stream backup marker: {error}"))?;
    if bytes.len() as u64 > MAX_SUCCESSFUL_STREAM_BACKUP_MARKER_BYTES {
        return Err("successful stream backup marker exceeded its byte limit".into());
    }
    let marker_path = transaction.join(SUCCESSFUL_STREAM_BACKUP_MARKER);
    let transaction_guard = crate::fs_safety::guard_descendant_directory_chain(root, transaction)
        .map_err(|error| {
        format!(
            "guard successful stream backup marker parent {}: {error}",
            transaction.display()
        )
    })?;
    let marker_guard = crate::fs_safety::guard_descendant_parent_chain(root, &marker_path, true)
        .map_err(|error| {
            format!(
                "guard successful stream backup marker {}: {error}",
                marker_path.display()
            )
        })?;
    transaction_guard.verify().map_err(|error| {
        format!(
            "verify successful stream backup marker parent {}: {error}",
            transaction.display()
        )
    })?;
    marker_guard.verify().map_err(|error| {
        format!(
            "verify successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    if crate::fs_safety::metadata_no_follow(&marker_path)
        .map_err(|error| format!("inspect successful stream backup marker target: {error}"))?
        .is_some()
    {
        return Err(format!(
            "successful stream backup marker already exists: {}",
            marker_path.display()
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_path)
        .map_err(|error| {
            format!(
                "create successful stream backup marker {}: {error}",
                marker_path.display()
            )
        })?;
    file.write_all(&bytes).map_err(|error| {
        format!(
            "write successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    file.sync_all().map_err(|error| {
        format!(
            "sync successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    drop(file);
    transaction_guard.verify().map_err(|error| {
        format!(
            "successful stream backup marker parent changed {}: {error}",
            transaction.display()
        )
    })?;
    marker_guard.verify().map_err(|error| {
        format!(
            "successful stream backup marker parent changed {}: {error}",
            marker_path.display()
        )
    })?;
    let validated = validate_successful_stream_backup_marker(root, transaction)?;
    if validated != marker {
        return Err(format!(
            "successful stream backup marker changed after creation: {}",
            marker_path.display()
        ));
    }
    Ok(())
}

fn promote_successful_stream_backup(
    root: &Path,
    transaction: &Path,
) -> Result<(PathBuf, Option<String>), String> {
    let backup_root = root.join(".rosync-backups");
    if transaction.parent() != Some(backup_root.as_path()) {
        return Err(format!(
            "stream backup is outside the project backup root: {}",
            transaction.display()
        ));
    }
    let name = transaction
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("stream backup name is not UTF-8: {}", transaction.display()))?;
    if successful_stream_backup_stamp(name).is_some() {
        return Ok((transaction.to_path_buf(), None));
    }
    let (stamp, sequence) = stream_backup_name_parts(name, "stream-").ok_or_else(|| {
        format!(
            "stream backup has an unexpected transaction name: {}",
            transaction.display()
        )
    })?;
    let promoted = backup_root.join(format!("stream-success-{stamp}-{sequence}"));
    let source_guard = crate::fs_safety::guard_descendant_parent_chain(root, transaction, false)
        .map_err(|error| format!("guard successful stream backup: {error}"))?;
    let target_guard = crate::fs_safety::guard_descendant_parent_chain(root, &promoted, true)
        .map_err(|error| format!("guard promoted stream backup: {error}"))?;
    source_guard
        .verify()
        .map_err(|error| format!("verify successful stream backup parent: {error}"))?;
    target_guard
        .verify()
        .map_err(|error| format!("verify promoted stream backup parent: {error}"))?;
    let source_generation = crate::fs_safety::directory_generation_no_follow(transaction)
        .map_err(|error| format!("inspect successful stream backup: {error}"))?;
    if crate::fs_safety::metadata_no_follow(&promoted)
        .map_err(|error| format!("inspect promoted stream backup target: {error}"))?
        .is_some()
    {
        return Err(format!(
            "promoted stream backup target already exists: {}",
            promoted.display()
        ));
    }
    source_guard
        .verify()
        .map_err(|error| format!("successful stream backup parent changed: {error}"))?;
    target_guard
        .verify()
        .map_err(|error| format!("promoted stream backup parent changed: {error}"))?;
    std::fs::rename(transaction, &promoted)
        .map_err(|error| format!("promote successful stream backup: {error}"))?;

    let warning = source_guard
        .verify()
        .err()
        .map(|error| format!("successful backup parent changed after promotion: {error}"))
        .or_else(|| {
            target_guard
                .verify()
                .err()
                .map(|error| format!("promoted backup parent changed: {error}"))
        })
        .or_else(|| {
            crate::fs_safety::directory_generation_no_follow(&promoted)
                .err()
                .map(|error| format!("reinspect promoted backup: {error}"))
        })
        .or_else(|| {
            crate::fs_safety::directory_generation_no_follow(&promoted)
                .ok()
                .filter(|generation| generation.identity != source_generation.identity)
                .map(|_| "promoted backup identity changed after rename".to_string())
        });
    Ok((promoted, warning))
}

fn prune_successful_stream_backups(root: &Path) -> Vec<String> {
    let backup_root = root.join(".rosync-backups");
    let metadata = match crate::fs_safety::metadata_no_follow(&backup_root) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Vec::new(),
        Err(error) => {
            return vec![format!(
                "inspect successful stream backup root {}: {error}",
                backup_root.display()
            )];
        }
    };
    if !metadata.is_dir() {
        return vec![format!(
            "backup root is not a physical directory: {}",
            backup_root.display()
        )];
    }
    let backup_root_guard =
        match crate::fs_safety::guard_descendant_directory_chain(root, &backup_root) {
            Ok(guard) => guard,
            Err(error) => {
                return vec![format!(
                    "guard successful stream backup root {}: {error}",
                    backup_root.display()
                )];
            }
        };
    if let Err(error) = backup_root_guard.verify() {
        return vec![format!(
            "verify successful stream backup root {}: {error}",
            backup_root.display()
        )];
    }
    let mut warnings = Vec::new();
    let mut candidates = Vec::<(u128, PathBuf, crate::fs_safety::FileGeneration)>::new();
    let children = match std::fs::read_dir(&backup_root) {
        Ok(children) => children,
        Err(error) => {
            return vec![format!(
                "scan successful stream backups {}: {error}",
                backup_root.display()
            )];
        }
    };
    for child in children {
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                warnings.push(format!("scan successful stream backup: {error}"));
                continue;
            }
        };
        let Some(name) = child.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stamp) = successful_stream_backup_stamp(&name) else {
            continue;
        };
        let path = child.path();
        if let Err(error) = validate_successful_stream_backup_marker(root, &path) {
            warnings.push(format!(
                "skip unproven successful stream backup {}: {error}",
                path.display()
            ));
            continue;
        }
        match crate::fs_safety::directory_generation_no_follow(&path) {
            Ok(generation) => candidates.push((stamp, path, generation)),
            Err(error) => warnings.push(format!(
                "inspect successful stream backup {}: {error}",
                path.display()
            )),
        }
    }
    if let Err(error) = backup_root_guard.verify() {
        warnings.push(format!(
            "successful stream backup root changed while scanning {}: {error}",
            backup_root.display()
        ));
        return warnings;
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for (index, (stamp, path, generation)) in candidates.into_iter().enumerate() {
        let expired = now.saturating_sub(stamp) > SUCCESSFUL_STREAM_BACKUP_RETENTION.as_nanos();
        if index >= MAX_SUCCESSFUL_STREAM_BACKUPS || expired {
            if let Err(error) = remove_successful_stream_backup(root, &path, &generation) {
                warnings.push(error);
            }
        }
    }
    warnings
}

fn run_stream_commit_hook(
    control: &StreamCommitControl,
    point: StreamCommitHookPoint,
    backup_service: &Path,
    live_service: &Path,
    stage_service: &Path,
) -> Result<(), String> {
    #[cfg(test)]
    if let Some(hook) = control.test_hook.as_ref() {
        return hook(point, backup_service, live_service, stage_service);
    }
    #[cfg(not(test))]
    let _ = (control, point, backup_service, live_service, stage_service);
    Ok(())
}

fn restore_stream_backup_before_install(
    root: &Path,
    service: &str,
    live_service: &Path,
    backup_service: &Path,
    backup_transaction: &Path,
    live_parent_guard: &crate::fs_safety::PathParentGuard,
    backup_parent_guard: &crate::fs_safety::PathParentGuard,
) -> Result<Option<String>, String> {
    let backup_fingerprint = capture_exact_tree_fingerprint(backup_transaction, service)
        .map_err(|error| format!("capture retained backup before rollback: {error}"))?;
    if crate::fs_safety::metadata_no_follow(live_service)
        .map_err(|error| {
            format!(
                "inspect live rollback target {}: {error}",
                live_service.display()
            )
        })?
        .is_some()
    {
        return Err(format!(
            "refusing rollback because live service target appeared: {}",
            live_service.display()
        ));
    }
    live_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because live service parent changed {}: {error}",
            live_service.display()
        )
    })?;
    backup_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because backup parent changed {}: {error}",
            backup_service.display()
        )
    })?;
    std::fs::rename(backup_service, live_service).map_err(|error| {
        format!(
            "restore backup {} -> {}: {error}",
            backup_service.display(),
            live_service.display()
        )
    })?;

    // Once the exact backup has been atomically restored, keep the live tree
    // even if a concurrent writer changes it during post-rename verification.
    // Returning it to the user is safer than attempting another destructive
    // move with no remaining backup source.
    let _ = live_parent_guard.verify();
    let backup_parent_check = backup_parent_guard.verify();
    let _ = capture_exact_tree_fingerprint(root, service)
        .map(|current| relocated_fingerprint_matches(&backup_fingerprint, &current));
    Ok(match backup_parent_check {
        Err(error) => Some(format!(
            "restored live files but refused backup transaction cleanup after its parent changed {}: {error}",
            backup_transaction.display()
        )),
        Ok(()) => cleanup_empty_stream_backup_transaction(root, backup_transaction)
            .err()
            .map(|error| {
                format!(
                    "restored live files but could not clean backup transaction {}: {error}",
                    backup_transaction.display()
                )
            }),
    })
}

struct InstalledStreamRollback<'a> {
    root: &'a Path,
    service: &'a str,
    live_service: &'a Path,
    backup_service: &'a Path,
    backup_transaction: &'a Path,
    stage_service: &'a Path,
    staged_fingerprint: &'a ExactTreeFingerprint,
    live_parent_guard: &'a crate::fs_safety::PathParentGuard,
    backup_parent_guard: &'a crate::fs_safety::PathParentGuard,
    stage_parent_guard: &'a crate::fs_safety::PathParentGuard,
}

fn restore_stream_backup_after_install(
    rollback: InstalledStreamRollback<'_>,
) -> Result<Option<String>, String> {
    let InstalledStreamRollback {
        root,
        service,
        live_service,
        backup_service,
        backup_transaction,
        stage_service,
        staged_fingerprint,
        live_parent_guard,
        backup_parent_guard,
        stage_parent_guard,
    } = rollback;
    let current_live = capture_exact_tree_fingerprint(root, service)
        .map_err(|error| format!("capture installed service before rollback: {error}"))?;
    if !relocated_fingerprint_matches(staged_fingerprint, &current_live) {
        return Err(format!(
            "refusing rollback because installed service changed: {}",
            live_service.display()
        ));
    }
    let backup_fingerprint = capture_exact_tree_fingerprint(backup_transaction, service)
        .map_err(|error| format!("capture retained backup before rollback: {error}"))?;
    if crate::fs_safety::metadata_no_follow(stage_service)
        .map_err(|error| {
            format!(
                "inspect staged rollback target {}: {error}",
                stage_service.display()
            )
        })?
        .is_some()
    {
        return Err(format!(
            "refusing rollback because staged target reappeared: {}",
            stage_service.display()
        ));
    }
    live_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because live service parent changed {}: {error}",
            live_service.display()
        )
    })?;
    backup_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because backup parent changed {}: {error}",
            backup_service.display()
        )
    })?;
    stage_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because stage parent changed {}: {error}",
            stage_service.display()
        )
    })?;
    std::fs::rename(live_service, stage_service).map_err(|error| {
        format!(
            "move installed service aside {} -> {}: {error}",
            live_service.display(),
            stage_service.display()
        )
    })?;
    if let Err(error) = std::fs::rename(backup_service, live_service) {
        let reinstall = std::fs::rename(stage_service, live_service);
        return Err(format!(
            "restore backup {} -> {}: {error}; reinstall staged service: {}",
            backup_service.display(),
            live_service.display(),
            reinstall
                .map(|_| "ok".to_string())
                .unwrap_or_else(|reinstall| reinstall.to_string())
        ));
    }

    // As above, the original disk tree is now back at the live path. Do not
    // risk a second destructive swap merely because post-rename observation
    // races another local writer.
    let _ = live_parent_guard.verify();
    let backup_parent_check = backup_parent_guard.verify();
    let _ = stage_parent_guard.verify();
    let _ = capture_exact_tree_fingerprint(root, service)
        .map(|current| relocated_fingerprint_matches(&backup_fingerprint, &current));
    Ok(match backup_parent_check {
        Err(error) => Some(format!(
            "restored live files but refused backup transaction cleanup after its parent changed {}: {error}",
            backup_transaction.display()
        )),
        Ok(()) => cleanup_empty_stream_backup_transaction(root, backup_transaction)
            .err()
            .map(|error| {
                format!(
                    "restored live files but could not clean backup transaction {}: {error}",
                    backup_transaction.display()
                )
            }),
    })
}

fn retain_stream_commit_backup(
    state: &AppState,
    control: &mut StreamCommitControl,
    service: &str,
    live_service: &Path,
    backup_transaction: Option<&Path>,
    failure: &str,
    rollback: &str,
) {
    control.partial_failure = true;
    control.retained_backup = backup_transaction.map(Path::to_path_buf);
    let event = json!({
        "type": "stream-commit-partial",
        "service": service,
        "livePath": live_service,
        "backup": backup_transaction,
        "error": failure,
        "rollbackError": rollback,
    });
    if let Ok(serialized) = serde_json::to_string(&event) {
        let _ = state.events.send(serialized);
    }
    #[cfg(not(test))]
    {
        let _ = write_log_entry(Json(json!({
            "action": "stream-commit-partial",
            "service": service,
            "livePath": live_service,
            "backup": backup_transaction,
            "error": failure,
            "rollbackError": rollback,
        })));
    }
}

fn commit_streamed_service(input: StreamCommitInput) -> Result<StreamCommitResult, String> {
    let StreamCommitInput {
        state,
        service,
        service_node,
        source_dir,
        initial_fingerprint,
        strict,
        force_prune,
        commit_control,
    } = input;
    let root = state.canonical_project.as_path();
    let created = !initial_fingerprint.metadata.present;
    crate::fs_safety::validate_service_tree_no_follow(root, &service)?;
    let stage_parent = root.parent().ok_or_else(|| {
        format!(
            "project root has no same-volume staging parent: {}",
            root.display()
        )
    })?;
    let stage_parent_metadata = crate::fs_safety::require_metadata_no_follow(stage_parent)
        .map_err(|error| format!("inspect staging parent {}: {error}", stage_parent.display()))?;
    if !stage_parent_metadata.is_dir() {
        return Err(format!(
            "same-volume staging parent is not a directory: {}",
            stage_parent.display()
        ));
    }
    let stage = tempfile::Builder::new()
        .prefix(".rosync-stage-")
        .tempdir_in(stage_parent)
        .map_err(|error| format!("create same-volume service stage: {error}"))?;
    let staged_hash =
        copy_fenced_service_to_stage(root, &initial_fingerprint.metadata, stage.path())?;
    if staged_hash != initial_fingerprint.content_hash
        || crate::fs_safety::capture_tree_metadata(root, &service)? != initial_fingerprint.metadata
    {
        return Err(format!(
            "disk service {service} changed during streamed upload; no files were replaced"
        ));
    }

    let stage_service = stage.path().join(&service);
    let stage_quiet = Mutex::new(HashMap::new());
    let stage_ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: &stage_quiet,
        force_overwrite: true,
        strict,
        force_prune,
        project_root: stage.path(),
        backup_forced_removals: false,
    };
    let mut source_provider = |node: &Value| {
        let id = node
            .get("streamId")
            .and_then(Value::as_u64)
            .ok_or("streamed script is missing its source ID")?;
        let path = source_dir.path().join(format!("{id}.source"));
        crate::fs_safety::read_file_no_follow(&path)
            .map(Some)
            .map_err(|error| format!("read staged Source {}: {error}", path.display()))
    };
    let applied = match apply_service_node_with_sources(
        stage.path(),
        &service_node,
        &stage_ctx,
        &mut source_provider,
    ) {
        Ok(applied) => applied,
        Err(error) => {
            state.conflict.forget_path(&stage_service);
            return Err(format!("apply staged service {service}: {error}"));
        }
    };

    let staged_fingerprint = capture_exact_tree_fingerprint(stage.path(), &service)?;
    let final_fingerprint = capture_exact_tree_fingerprint(root, &service)?;
    if final_fingerprint != initial_fingerprint {
        state.conflict.forget_path(&stage_service);
        return Err(format!(
            "disk service {service} changed before atomic commit; no files were replaced"
        ));
    }

    let mut commit_control = commit_control.lock().unwrap();
    if commit_control.cancelled {
        state.conflict.forget_path(&stage_service);
        return Err("streamed service commit was cancelled before disk replacement".into());
    }
    let live_service = root.join(&service);
    let mut backup_transaction = None;
    if initial_fingerprint.metadata.present {
        let (destination, transaction) = create_stream_backup_destination(root, &service)?;
        let backup_parent = transaction.as_path();
        let prepare = (|| {
            let live_parent_guard =
                crate::fs_safety::guard_synced_parent_chain(root, &live_service, false).map_err(
                    |error| format!("guard live service {}: {error}", live_service.display()),
                )?;
            let backup_parent_guard =
                crate::fs_safety::guard_descendant_parent_chain(root, &destination, true).map_err(
                    |error| {
                        format!(
                            "guard stream backup destination {}: {error}",
                            destination.display()
                        )
                    },
                )?;
            let stage_parent_guard = crate::fs_safety::guard_descendant_parent_chain(
                stage.path(),
                &stage_service,
                false,
            )
            .map_err(|error| {
                format!("guard staged service {}: {error}", stage_service.display())
            })?;
            live_parent_guard.verify().map_err(|error| {
                format!(
                    "verify live service parent {}: {error}",
                    live_service.display()
                )
            })?;
            backup_parent_guard.verify().map_err(|error| {
                format!(
                    "verify stream backup parent {}: {error}",
                    backup_parent.display()
                )
            })?;
            run_stream_commit_hook(
                &commit_control,
                StreamCommitHookPoint::BeforeBackupRename,
                &destination,
                &live_service,
                &stage_service,
            )?;
            std::fs::rename(&live_service, &destination).map_err(|error| {
                format!(
                    "move live service {} to backup {}: {error}",
                    live_service.display(),
                    destination.display()
                )
            })?;
            Ok::<_, String>((live_parent_guard, backup_parent_guard, stage_parent_guard))
        })();
        let (live_parent_guard, backup_parent_guard, stage_parent_guard) = match prepare {
            Ok(guards) => guards,
            Err(error) => {
                state.conflict.forget_path(&stage_service);
                return Err(
                    match cleanup_empty_stream_backup_transaction(root, &transaction) {
                        Ok(()) => error,
                        Err(cleanup) => {
                            format!("{error}; empty backup transaction cleanup failed: {cleanup}")
                        }
                    },
                );
            }
        };
        let mut stage_installed = false;
        let install = (|| -> Result<(), String> {
            live_parent_guard.verify().map_err(|error| {
                format!(
                    "live service parent changed during backup rename {}: {error}",
                    live_service.display()
                )
            })?;
            backup_parent_guard.verify().map_err(|error| {
                format!(
                    "stream backup parent changed during rename {}: {error}",
                    backup_parent.display()
                )
            })?;
            run_stream_commit_hook(
                &commit_control,
                StreamCommitHookPoint::AfterBackupRename,
                &destination,
                &live_service,
                &stage_service,
            )?;
            let moved_fingerprint = capture_exact_tree_fingerprint(&transaction, &service)?;
            if !relocated_fingerprint_matches(&initial_fingerprint, &moved_fingerprint) {
                return Err(
                    "the moved tree no longer matched its transfer fence after backup rename"
                        .into(),
                );
            }
            stage_parent_guard.verify().map_err(|error| {
                format!(
                    "verify staged service parent {}: {error}",
                    stage_service.display()
                )
            })?;
            live_parent_guard.verify().map_err(|error| {
                format!(
                    "verify live service parent {}: {error}",
                    live_service.display()
                )
            })?;
            run_stream_commit_hook(
                &commit_control,
                StreamCommitHookPoint::BeforeStageInstall,
                &destination,
                &live_service,
                &stage_service,
            )?;
            std::fs::rename(&stage_service, &live_service).map_err(|error| {
                format!("commit staged service {}: {error}", live_service.display())
            })?;
            stage_installed = true;
            run_stream_commit_hook(
                &commit_control,
                StreamCommitHookPoint::AfterStageInstall,
                &destination,
                &live_service,
                &stage_service,
            )?;
            stage_parent_guard.verify().map_err(|error| {
                format!(
                    "staged service parent changed during commit {}: {error}",
                    stage_service.display()
                )
            })?;
            live_parent_guard.verify().map_err(|error| {
                format!(
                    "live service parent changed during commit {}: {error}",
                    live_service.display()
                )
            })?;
            Ok(())
        })();
        if let Err(failure) = install {
            let rollback = if stage_installed {
                restore_stream_backup_after_install(InstalledStreamRollback {
                    root,
                    service: &service,
                    live_service: &live_service,
                    backup_service: &destination,
                    backup_transaction: &transaction,
                    stage_service: &stage_service,
                    staged_fingerprint: &staged_fingerprint,
                    live_parent_guard: &live_parent_guard,
                    backup_parent_guard: &backup_parent_guard,
                    stage_parent_guard: &stage_parent_guard,
                })
            } else {
                restore_stream_backup_before_install(
                    root,
                    &service,
                    &live_service,
                    &destination,
                    &transaction,
                    &live_parent_guard,
                    &backup_parent_guard,
                )
            };
            state.conflict.forget_path(&stage_service);
            return Err(match rollback {
                Ok(cleanup_warning) => format!(
                    "streamed service commit failed: {failure}; live files were restored{}",
                    cleanup_warning
                        .map(|warning| format!("; cleanup warning: {warning}"))
                        .unwrap_or_default()
                ),
                Err(rollback) => {
                    let retained = crate::fs_safety::metadata_no_follow(&transaction)
                        .ok()
                        .flatten()
                        .is_some_and(|metadata| metadata.is_dir())
                        .then_some(transaction.as_path());
                    retain_stream_commit_backup(
                        &state,
                        &mut commit_control,
                        &service,
                        &live_service,
                        retained,
                        &failure,
                        &rollback,
                    );
                    format!(
                        "streamed service commit is partial: {failure}; rollback refused or failed: {rollback}; recovery backup: {}",
                        retained
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "not retained; inspect the live service".into())
                    )
                }
            });
        }
        backup_transaction = Some(transaction);
    } else {
        let stage_parent_guard =
            crate::fs_safety::guard_descendant_parent_chain(stage.path(), &stage_service, false)
                .map_err(|error| {
                    format!("guard staged service {}: {error}", stage_service.display())
                })?;
        let live_parent_guard =
            crate::fs_safety::guard_synced_parent_chain(root, &live_service, true).map_err(
                |error| {
                    format!(
                        "guard live service target {}: {error}",
                        live_service.display()
                    )
                },
            )?;
        stage_parent_guard.verify().map_err(|error| {
            format!(
                "verify staged service parent {}: {error}",
                stage_service.display()
            )
        })?;
        live_parent_guard.verify().map_err(|error| {
            format!(
                "verify live service parent {}: {error}",
                live_service.display()
            )
        })?;
        std::fs::rename(&stage_service, &live_service).map_err(|error| {
            format!("commit staged service {}: {error}", live_service.display())
        })?;
        // There was no prior disk tree to lose. Once the staged service is
        // installed, publish the commit even if a post-rename observation
        // races another local writer.
        let _ = stage_parent_guard.verify();
        let _ = live_parent_guard.verify();
    }

    state.conflict.forget_path(&live_service);
    state
        .conflict
        .commit_fs_rename(&stage_service, &live_service);
    commit_control.committed = true;
    drop(commit_control);
    let live_ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: true,
        strict,
        force_prune,
        project_root: root,
        backup_forced_removals: true,
    };
    live_ctx.mark_quiet(&live_service);
    Ok(StreamCommitResult {
        applied,
        backup: backup_transaction,
        created,
    })
}

fn new_push_service_stream(service: &str) -> PushServiceStream {
    PushServiceStream {
        service: service.to_string(),
        phase: PushStreamPhase::Structure,
        next_chunk: 0,
        records: Vec::new(),
        accepted_structure_bytes: 0,
        accepted_source_bytes: 0,
        service_node: None,
        script_ids: Vec::new(),
        next_script: 0,
        receiving_source: None,
        source_dir: None,
        initial_fingerprint: None,
        fence_result: None,
        commit_result: None,
        commit_control: None,
    }
}

fn push_stream_expired(session: &PushStreamAccumulator) -> bool {
    match session.completed_at {
        Some(completed) => completed.elapsed() >= STREAM_COMPLETED_TTL,
        None => session.last_activity.elapsed() >= STREAM_SESSION_TTL,
    }
}

fn prune_push_stream_sessions(sessions: &mut HashMap<PathBuf, Arc<Mutex<PushStreamAccumulator>>>) {
    sessions.retain(|_, session| {
        session
            .try_lock()
            .map(|session| !push_stream_expired(&session))
            .unwrap_or(true)
    });
}

fn schedule_push_stream_cleanup(
    project: PathBuf,
    session: &Arc<Mutex<PushStreamAccumulator>>,
    wake_after: Duration,
) {
    let session = Arc::downgrade(session);
    tokio::spawn(async move {
        tokio::time::sleep(wake_after).await;
        loop {
            let Some(session) = session.upgrade() else {
                return;
            };
            let remaining = {
                let session = session.lock().unwrap();
                match session.completed_at {
                    Some(completed) => STREAM_COMPLETED_TTL.saturating_sub(completed.elapsed()),
                    None => STREAM_SESSION_TTL.saturating_sub(session.last_activity.elapsed()),
                }
            };
            if !remaining.is_zero() {
                drop(session);
                tokio::time::sleep(remaining).await;
                continue;
            }
            let attempt = {
                let sessions = PUSH_STREAM_ACCUMULATORS.get_or_init(|| Mutex::new(HashMap::new()));
                let mut sessions = sessions.lock().unwrap();
                try_remove_expired_stream_session(
                    &mut sessions,
                    &project,
                    &session,
                    push_stream_expired,
                )
            };
            match attempt {
                StreamCleanupAttempt::Removed | StreamCleanupAttempt::Superseded => return,
                StreamCleanupAttempt::Retry => {
                    tokio::time::sleep(STREAM_CLEANUP_RETRY_DELAY).await;
                }
            }
        }
    });
}

fn push_stream_request_hash(body: &PushBody) -> Result<crate::conflict::Hash, String> {
    serde_json::to_vec(body)
        .map(|encoded| hash(&encoded))
        .map_err(|error| error.to_string())
}

fn push_stream_response(
    session: &PushStreamAccumulator,
    service: &str,
    phase: &str,
    next_chunk: u64,
) -> Value {
    json!({
        "ok": true,
        "streamId": session.stream_id,
        "nextService": service,
        "phase": phase,
        "nextChunk": next_chunk,
    })
}

fn append_source_parts_atomically(
    service: &mut PushServiceStream,
    session_source_bytes: &mut u64,
    parts: &[StreamSourcePart],
    final_chunk: bool,
) -> Result<(), String> {
    use std::io::Write as _;

    let encoded =
        serde_json::to_vec(parts).map_err(|error| format!("encode streamed Sources: {error}"))?;
    if encoded.len() > STREAM_SOURCE_CHUNK_BYTES {
        return Err(format!(
            "encoded Source chunks are limited to {STREAM_SOURCE_CHUNK_BYTES} bytes"
        ));
    }
    if parts.len() > STREAM_HASH_CHUNK_NODES {
        return Err(format!(
            "Source chunks are limited to {STREAM_HASH_CHUNK_NODES} parts"
        ));
    }

    let mut next_script = service.next_script;
    let mut receiving = service.receiving_source.clone();
    let mut accepted_service_bytes = service.accepted_source_bytes;
    let mut accepted_session_bytes = *session_source_bytes;
    let mut writes = Vec::<(PathBuf, Vec<u8>)>::with_capacity(parts.len());
    let source_dir = service
        .source_dir
        .as_ref()
        .ok_or("source stream has no temporary directory")?;
    for part in parts {
        let bytes = part.data.as_bytes();
        if bytes.len() > STREAM_SOURCE_PART_BYTES {
            return Err(format!(
                "Source part {} for stream ID {} exceeds {STREAM_SOURCE_PART_BYTES} bytes",
                part.part_index, part.id
            ));
        }
        if part.total_bytes > MAX_STREAM_SOURCE_BYTES {
            return Err(format!(
                "Source for stream ID {} exceeds {MAX_STREAM_SOURCE_BYTES} bytes",
                part.id
            ));
        }
        let expected_id = service
            .script_ids
            .get(next_script)
            .copied()
            .ok_or_else(|| format!("unexpected Source for stream ID {}", part.id))?;
        if part.id != expected_id {
            return Err(format!(
                "Source stream expected script ID {expected_id}, received {}",
                part.id
            ));
        }
        let digest = parse_sha256_hex(&part.sha256)?;
        if receiving.is_none() {
            if part.part_index != 0 || part.offset != 0 {
                return Err(format!(
                    "Source for stream ID {} must begin at part 0, offset 0",
                    part.id
                ));
            }
            (accepted_service_bytes, accepted_session_bytes) = charge_stream_source_bytes(
                accepted_service_bytes,
                accepted_session_bytes,
                part.total_bytes,
            )?;
            receiving = Some(ReceivingSource {
                id: part.id,
                next_part: 0,
                offset: 0,
                total_bytes: part.total_bytes,
                sha256: digest,
                hasher: Sha256::new(),
            });
        }
        let current = receiving
            .as_mut()
            .expect("receiving source was initialized");
        if current.id != part.id
            || current.next_part != part.part_index
            || current.offset != part.offset
            || current.total_bytes != part.total_bytes
            || current.sha256 != digest
        {
            return Err(format!(
                "Source part {} for stream ID {} is stale or out of order",
                part.part_index, part.id
            ));
        }
        let new_offset = current
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or("Source byte offset overflowed")?;
        if new_offset > current.total_bytes {
            return Err(format!(
                "Source part {} for stream ID {} exceeds its declared total",
                part.part_index, part.id
            ));
        }
        if part.final_part != (new_offset == current.total_bytes) {
            return Err(format!(
                "Source part {} for stream ID {} has an inconsistent finalPart",
                part.part_index, part.id
            ));
        }
        current.hasher.update(bytes);
        current.offset = new_offset;
        current.next_part += 1;
        writes.push((
            source_dir.path().join(format!("{}.source", part.id)),
            bytes.to_vec(),
        ));
        if part.final_part {
            let actual: crate::conflict::Hash = current.hasher.clone().finalize().into();
            if actual != current.sha256 {
                return Err(format!("Source SHA-256 mismatch for stream ID {}", part.id));
            }
            receiving = None;
            next_script += 1;
        }
    }

    let complete = next_script == service.script_ids.len() && receiving.is_none();
    if final_chunk && !complete {
        return Err(format!(
            "Source stream ended after {next_script}/{} scripts",
            service.script_ids.len()
        ));
    }

    let mut original_lengths = HashMap::<PathBuf, u64>::new();
    let write_result = (|| -> Result<(), String> {
        for (path, bytes) in &writes {
            let original = std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            original_lengths.entry(path.clone()).or_insert(original);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|error| format!("stage Source {}: {error}", path.display()))?;
            file.write_all(bytes)
                .map_err(|error| format!("stage Source {}: {error}", path.display()))?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        for (written_path, original_len) in &original_lengths {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(written_path) {
                let _ = file.set_len(*original_len);
            }
        }
        return Err(error);
    }
    service.next_script = next_script;
    service.receiving_source = receiving;
    service.accepted_source_bytes = accepted_service_bytes;
    *session_source_bytes = accepted_session_bytes;
    Ok(())
}

fn spawn_exact_fingerprint(
    root: PathBuf,
    service: String,
) -> std::sync::mpsc::Receiver<Result<ExactTreeFingerprint, String>> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = send.send(capture_exact_tree_fingerprint(&root, &service));
    });
    receive
}

fn spawn_stream_commit(
    input: StreamCommitInput,
) -> std::sync::mpsc::Receiver<Result<StreamCommitResult, String>> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = commit_streamed_service(input);
        let _ = send.send(result);
    });
    receive
}

fn process_streamed_push_chunk(
    state: &AppState,
    session: &mut PushStreamAccumulator,
    body: &PushBody,
) -> Result<Value, String> {
    let service = body
        .service
        .as_deref()
        .ok_or("streamed push is missing service")?;
    let phase = body
        .phase
        .as_deref()
        .ok_or("streamed push is missing phase")?;
    let chunk_index = body
        .chunk_index
        .ok_or("streamed push is missing chunkIndex")?;
    if service != session.service_stream.service {
        return Err(format!(
            "streamed push expected service {}, received {service}",
            session.service_stream.service
        ));
    }
    if chunk_index != session.service_stream.next_chunk {
        return Err(format!(
            "streamed push {service} {phase} expected chunk {}, received {chunk_index}",
            session.service_stream.next_chunk
        ));
    }

    match session.service_stream.phase {
        PushStreamPhase::Structure => {
            if phase != "structure" || !body.sources.is_empty() {
                return Err("service structure phase accepts only flat records".into());
            }
            validate_stream_record_chunk_fields(&body.records)?;
            let chunk_bytes = encoded_stream_record_chunk_bytes(&body.records)?;
            if body.records.len() > STREAM_STRUCTURE_CHUNK_NODES {
                return Err(format!(
                    "structure chunks are limited to {STREAM_STRUCTURE_CHUNK_NODES} records"
                ));
            }
            if session
                .service_stream
                .records
                .len()
                .checked_add(body.records.len())
                .is_none_or(|count| count > MAX_BOOTSTRAP_NODES)
            {
                return Err(format!(
                    "streamed service exceeds {MAX_BOOTSTRAP_NODES} records"
                ));
            }
            for (offset, record) in body.records.iter().enumerate() {
                let expected = (session.service_stream.records.len() + offset) as u64;
                if record.id != expected {
                    return Err(format!(
                        "streamed structure IDs must be dense; expected {expected}, received {}",
                        record.id
                    ));
                }
            }
            let (service_bytes, session_bytes) = charge_stream_structure_bytes(
                session.service_stream.accepted_structure_bytes,
                session.accepted_stream_bytes,
                chunk_bytes,
            )?;
            session.service_stream.accepted_structure_bytes = service_bytes;
            session.accepted_stream_bytes = session_bytes;
            session
                .service_stream
                .records
                .extend(body.records.iter().cloned());
            session.service_stream.next_chunk += 1;
            if !body.final_chunk {
                return Ok(push_stream_response(
                    session,
                    service,
                    "structure",
                    session.service_stream.next_chunk,
                ));
            }
            let validated =
                validate_flat_snapshot(&session.service_stream.records, service, false)?;
            preflight_streamed_service_fragments(&validated.service)?;
            session.service_stream.records.clear();
            session.service_stream.records.shrink_to_fit();
            session.service_stream.service_node = Some(validated.service);
            session.service_stream.script_ids = validated.script_ids;
            session.service_stream.source_dir = Some(
                tempfile::Builder::new()
                    .prefix("rosync-push-sources-")
                    .tempdir()
                    .map_err(|error| format!("create Source staging directory: {error}"))?,
            );
            session.service_stream.fence_result = Some(spawn_exact_fingerprint(
                state.canonical_project.as_ref().clone(),
                service.to_string(),
            ));
            session.service_stream.phase = PushStreamPhase::DiskFence;
            session.service_stream.next_chunk = 0;
            Ok(push_stream_response(session, service, "diskFence", 0))
        }
        PushStreamPhase::DiskFence => {
            if phase != "diskFence"
                || !body.records.is_empty()
                || !body.sources.is_empty()
                || body.final_chunk
            {
                return Err("diskFence accepts only empty continuation ticks".into());
            }
            let result = session
                .service_stream
                .fence_result
                .as_ref()
                .ok_or("diskFence worker is missing")?
                .try_recv();
            match result {
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    session.service_stream.next_chunk += 1;
                    Ok(push_stream_response(
                        session,
                        service,
                        "diskFence",
                        session.service_stream.next_chunk,
                    ))
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err("diskFence worker disconnected".into())
                }
                Ok(Err(error)) => Err(error),
                Ok(Ok(fingerprint)) => {
                    session.service_stream.initial_fingerprint = Some(fingerprint);
                    session.service_stream.fence_result = None;
                    session.service_stream.phase = PushStreamPhase::Sources;
                    session.service_stream.next_chunk = 0;
                    Ok(push_stream_response(session, service, "sources", 0))
                }
            }
        }
        PushStreamPhase::Sources => {
            if phase != "sources" || !body.records.is_empty() {
                return Err("service Source phase accepts only Source parts".into());
            }
            append_source_parts_atomically(
                &mut session.service_stream,
                &mut session.accepted_source_bytes,
                &body.sources,
                body.final_chunk,
            )?;
            session.service_stream.next_chunk += 1;
            if !body.final_chunk {
                return Ok(push_stream_response(
                    session,
                    service,
                    "sources",
                    session.service_stream.next_chunk,
                ));
            }
            let service_node = session
                .service_stream
                .service_node
                .take()
                .ok_or("streamed service structure is missing")?;
            let source_dir = session
                .service_stream
                .source_dir
                .take()
                .ok_or("streamed Source stage is missing")?;
            let initial_fingerprint = session
                .service_stream
                .initial_fingerprint
                .take()
                .ok_or("streamed disk fence is missing")?;
            let commit_control = Arc::new(Mutex::new(StreamCommitControl::default()));
            session.service_stream.commit_result = Some(spawn_stream_commit(StreamCommitInput {
                state: state.clone(),
                service: service.to_string(),
                service_node,
                source_dir,
                initial_fingerprint,
                strict: session.strict,
                force_prune: session.force_prune,
                commit_control: commit_control.clone(),
            }));
            session.service_stream.commit_control = Some(commit_control);
            session.service_stream.phase = PushStreamPhase::DiskRevalidate;
            session.service_stream.next_chunk = 0;
            Ok(push_stream_response(session, service, "diskRevalidate", 0))
        }
        PushStreamPhase::DiskRevalidate => {
            if phase != "diskRevalidate"
                || !body.records.is_empty()
                || !body.sources.is_empty()
                || body.final_chunk
            {
                return Err("diskRevalidate accepts only empty continuation ticks".into());
            }
            let result = session
                .service_stream
                .commit_result
                .as_ref()
                .ok_or("diskRevalidate worker is missing")?
                .try_recv();
            match result {
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    session.service_stream.next_chunk += 1;
                    Ok(push_stream_response(
                        session,
                        service,
                        "diskRevalidate",
                        session.service_stream.next_chunk,
                    ))
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err("diskRevalidate worker disconnected".into())
                }
                Ok(Err(error)) => Err(error),
                Ok(Ok(result)) => {
                    session.applied += result.applied;
                    if let Some(backup) = result.backup.as_ref() {
                        session.backups.push(backup.clone());
                    }
                    session.committed_services.push(CommittedStreamService {
                        service: service.to_string(),
                        created: result.created,
                        backup: result.backup,
                        recovery_action: if result.created {
                            StreamRecoveryAction::RemoveCreatedService
                        } else {
                            StreamRecoveryAction::RestoreBackup
                        },
                    });
                    session.next_service += 1;
                    if let Some(next_service) =
                        snapshot::SYNCED_SERVICES.get(session.next_service).copied()
                    {
                        session.service_stream = new_push_service_stream(next_service);
                        Ok(push_stream_response(session, next_service, "structure", 0))
                    } else {
                        Ok(json!({
                            "ok": true,
                            "action": "complete",
                            "streamId": session.stream_id,
                            "applied": session.applied,
                            "backups": session.backups,
                            "committedServices": session.committed_services,
                        }))
                    }
                }
            }
        }
    }
}

fn finalize_successful_stream_backups(
    root: &Path,
    session: &mut PushStreamAccumulator,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if session.committed_services.len() != snapshot::SYNCED_SERVICES.len()
        || !session
            .committed_services
            .iter()
            .zip(snapshot::SYNCED_SERVICES)
            .all(|(committed, expected)| committed.service == *expected)
    {
        return vec![
            "refusing to classify backups before every synced service committed in order".into(),
        ];
    }
    let stream_id = session.stream_id.clone();
    for committed in &mut session.committed_services {
        let Some(backup) = committed.backup.clone() else {
            continue;
        };
        match promote_successful_stream_backup(root, &backup) {
            Ok((promoted, warning)) => {
                committed.backup = Some(promoted.clone());
                if let Some(warning) = warning {
                    warnings.push(warning);
                }
                if let Err(error) =
                    write_successful_stream_backup_marker(root, &promoted, &stream_id)
                {
                    warnings.push(format!(
                        "successful backup {} remains ineligible for automatic pruning: {error}",
                        promoted.display()
                    ));
                }
            }
            Err(error) => warnings.push(format!(
                "retain successful backup {} without automatic pruning: {error}",
                backup.display()
            )),
        }
    }
    session.backups = session
        .committed_services
        .iter()
        .filter_map(|committed| committed.backup.clone())
        .collect();
    warnings.extend(prune_successful_stream_backups(root));
    warnings
}

fn audit_stream_push_partial(
    state: &AppState,
    session: &PushStreamAccumulator,
    failed_service: &str,
    error: &str,
) {
    let event = json!({
        "type": "stream-push-partial",
        "streamId": session.stream_id,
        "failedService": failed_service,
        "error": error,
        "applied": session.applied,
        "backups": session.backups,
        "committedServices": session.committed_services,
        "recoveryRequired": true,
    });
    if let Ok(serialized) = serde_json::to_string(&event) {
        let _ = state.events.send(serialized);
    }
    #[cfg(not(test))]
    {
        let _ = write_log_entry(Json(json!({
            "action": "stream-push-partial",
            "streamId": session.stream_id,
            "failedService": failed_service,
            "error": error,
            "applied": session.applied,
            "backups": session.backups,
            "committedServices": session.committed_services,
            "recoveryRequired": true,
        })));
    }
}

fn audit_stream_push_complete(
    state: &AppState,
    session: &PushStreamAccumulator,
    retention_warnings: &[String],
) {
    let event = json!({
        "type": "stream-push-complete",
        "streamId": session.stream_id,
        "applied": session.applied,
        "backups": session.backups,
        "committedServices": session.committed_services,
        "backupRetentionWarnings": retention_warnings,
    });
    if let Ok(serialized) = serde_json::to_string(&event) {
        let _ = state.events.send(serialized);
    }
    #[cfg(not(test))]
    {
        let _ = write_log_entry(Json(json!({
            "action": "stream-push-complete",
            "streamId": session.stream_id,
            "applied": session.applied,
            "backups": session.backups,
            "committedServices": session.committed_services,
            "backupRetentionWarnings": retention_warnings,
        })));
    }
}

fn streamed_push(state: &AppState, body: PushBody) -> Json<Value> {
    if body.plugin_protocol != Some(crate::ws::PLUGIN_PROTOCOL_VERSION) {
        return Json(json!({
            "ok": false,
            "error": format!(
                "incompatible Studio plugin protocol; expected {}",
                crate::ws::PLUGIN_PROTOCOL_VERSION
            ),
        }));
    }
    let Some(stream_id) = body.stream_id.as_deref() else {
        return Json(json!({ "ok": false, "error": "streamed push requires streamId" }));
    };
    if stream_id.is_empty() || stream_id.len() > 128 {
        return Json(json!({ "ok": false, "error": "invalid streamed push streamId" }));
    }
    let is_start = body.service.as_deref() == snapshot::SYNCED_SERVICES.first().copied()
        && body.phase.as_deref() == Some("structure")
        && body.chunk_index == Some(0);
    let request_hash = match push_stream_request_hash(&body) {
        Ok(request_hash) => request_hash,
        Err(error) => {
            return Json(json!({ "ok": false, "error": format!("encode push chunk: {error}") }));
        }
    };
    let project = state.canonical_project.as_ref().clone();
    let sessions = PUSH_STREAM_ACCUMULATORS.get_or_init(|| Mutex::new(HashMap::new()));
    let session_handle = {
        let mut sessions = sessions.lock().unwrap();
        prune_push_stream_sessions(&mut sessions);
        match sessions.get(&project).cloned() {
            Some(session) if session.lock().unwrap().stream_id == stream_id => session,
            Some(_) if !is_start => {
                return Json(json!({
                    "ok": false,
                    "error": "streamed push session is stale; restart from structure chunk 0",
                }));
            }
            _ if !is_start => {
                return Json(json!({
                    "ok": false,
                    "error": "streamed push session is missing; restart from structure chunk 0",
                }));
            }
            _ => {
                if sessions.len() >= MAX_STREAM_SESSIONS && !sessions.contains_key(&project) {
                    return Json(json!({
                        "ok": false,
                        "error": "too many active streamed transfer sessions",
                    }));
                }
                let session = Arc::new(Mutex::new(PushStreamAccumulator {
                    stream_id: stream_id.to_string(),
                    strict: body.strict,
                    force_prune: body.force_prune || body.strict,
                    next_service: 0,
                    service_stream: new_push_service_stream(snapshot::SYNCED_SERVICES[0]),
                    applied: 0,
                    backups: Vec::new(),
                    committed_services: Vec::new(),
                    accepted_stream_bytes: 0,
                    accepted_source_bytes: 0,
                    last_request_hash: None,
                    last_response: None,
                    last_activity: Instant::now(),
                    completed_at: None,
                }));
                sessions.insert(project.clone(), session.clone());
                schedule_push_stream_cleanup(project.clone(), &session, STREAM_SESSION_TTL);
                session
            }
        }
    };

    let mut session = session_handle.lock().unwrap();
    if push_stream_expired(&session) {
        return Json(json!({
            "ok": false,
            "error": "streamed push session expired; restart from structure chunk 0",
        }));
    }
    if session.last_request_hash == Some(request_hash) {
        if let Some(response) = session.last_response.clone() {
            return Json(response);
        }
    }
    if session.completed_at.is_some() {
        return Json(json!({
            "ok": false,
            "error": "streamed push already completed",
        }));
    }
    let mut response = match process_streamed_push_chunk(state, &mut session, &body) {
        Ok(response) => response,
        Err(error) => {
            let failed_service = session.service_stream.service.clone();
            let (commit_already_finished, partial_failure, retained_backup) = session
                .service_stream
                .commit_control
                .as_ref()
                .map(|control| {
                    let mut control = control.lock().unwrap();
                    if !control.committed && !control.partial_failure {
                        control.cancelled = true;
                    }
                    (
                        control.committed,
                        control.partial_failure,
                        control.retained_backup.clone(),
                    )
                })
                .unwrap_or((false, false, None));
            if commit_already_finished {
                // The atomic rename finished before this malformed poll
                // arrived. Retain the session/result so a corrected cursor can
                // still observe the committed outcome.
                return Json(json!({ "ok": false, "error": error }));
            }
            if partial_failure {
                if let Some(backup) = retained_backup.as_ref() {
                    if !session.backups.contains(backup) {
                        session.backups.push(backup.clone());
                    }
                }
            }
            if partial_failure || !session.committed_services.is_empty() {
                let response = json!({
                    "ok": false,
                    "action": "partial",
                    "streamId": session.stream_id,
                    "error": error,
                    "failedService": failed_service,
                    "recoveryRequired": true,
                    "backups": session.backups,
                    "committedServices": session.committed_services,
                });
                session.last_request_hash = Some(request_hash);
                session.last_response = Some(response.clone());
                session.last_activity = Instant::now();
                session.completed_at = Some(Instant::now());
                audit_stream_push_partial(state, &session, &failed_service, &error);
                schedule_push_stream_cleanup(
                    project.clone(),
                    &session_handle,
                    STREAM_COMPLETED_TTL,
                );
                return Json(response);
            }
            drop(session);
            let mut sessions = sessions.lock().unwrap();
            if sessions
                .get(&project)
                .is_some_and(|current| Arc::ptr_eq(current, &session_handle))
            {
                sessions.remove(&project);
            }
            return Json(json!({ "ok": false, "error": error }));
        }
    };
    let complete = response.get("action").and_then(Value::as_str) == Some("complete");
    if complete {
        let retention_warnings =
            finalize_successful_stream_backups(state.canonical_project.as_path(), &mut session);
        if let Some(object) = response.as_object_mut() {
            object.insert("backups".into(), json!(session.backups));
            object.insert(
                "committedServices".into(),
                json!(session.committed_services),
            );
            if !retention_warnings.is_empty() {
                object.insert("backupRetentionWarnings".into(), json!(retention_warnings));
            }
        }
        audit_stream_push_complete(state, &session, &retention_warnings);
    }
    session.last_request_hash = Some(request_hash);
    session.last_response = Some(response.clone());
    session.last_activity = Instant::now();
    if complete {
        session.completed_at = Some(Instant::now());
        schedule_push_stream_cleanup(project, &session_handle, STREAM_COMPLETED_TTL);
    }
    Json(response)
}

fn validate_bootstrap_services(services: &[Value]) -> Result<(), String> {
    validate_bootstrap_services_with_limits(
        services,
        MAX_BOOTSTRAP_INSTANCE_DEPTH,
        MAX_BOOTSTRAP_NODES,
    )
}

fn validate_bootstrap_service_roots(
    services: &[Value],
    require_exactly_one: bool,
) -> Result<(), String> {
    if require_exactly_one && services.len() != 1 {
        return Err(format!(
            "protocol {} bootstrap requires exactly one synced service per request",
            crate::ws::PLUGIN_PROTOCOL_VERSION
        ));
    }
    let mut seen = HashSet::with_capacity(services.len());
    for service in services {
        let name = service
            .get("name")
            .and_then(Value::as_str)
            .ok_or("bootstrap service root is missing a string name")?;
        let class = service
            .get("class")
            .and_then(Value::as_str)
            .ok_or("bootstrap service root is missing a string class")?;
        if !snapshot::SYNCED_SERVICES.contains(&name) || class != name {
            return Err(format!(
                "bootstrap root {name:?} is not an allowed synced service"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("bootstrap repeats synced service {name}"));
        }
    }
    Ok(())
}

fn validate_full_tree_value(tree: &Value) -> Result<(), String> {
    match tree {
        Value::Array(nodes) => validate_bootstrap_services(nodes),
        node => validate_bootstrap_services(std::slice::from_ref(node)),
    }
}

fn validate_bootstrap_services_with_limits(
    services: &[Value],
    max_depth: usize,
    max_nodes: usize,
) -> Result<(), String> {
    let mut pending = services
        .iter()
        .rev()
        // The service itself is the traversal root. Match the disk emitter's
        // depth accounting by counting its direct children as depth 1.
        .map(|service| (service, 0usize))
        .collect::<Vec<_>>();
    let mut node_count = 0usize;

    while let Some((node, depth)) = pending.pop() {
        if depth > max_depth {
            return Err(format!(
                "Studio tree depth exceeds the supported limit of {max_depth} instances"
            ));
        }
        node_count = node_count
            .checked_add(1)
            .ok_or_else(|| "Studio tree node count overflowed".to_string())?;
        if node_count > max_nodes {
            return Err(format!(
                "Studio tree contains more than the supported limit of {max_nodes} instances"
            ));
        }

        let object = node
            .as_object()
            .ok_or_else(|| "Studio tree node must be an object".to_string())?;
        if object.get("name").and_then(Value::as_str).is_none() {
            return Err("Studio tree node is missing a string name".to_string());
        }
        if object.get("class").and_then(Value::as_str).is_none() {
            return Err("Studio tree node is missing a string class".to_string());
        }
        match object.get("children") {
            None | Some(Value::Null) => {}
            Some(Value::Array(children)) => {
                pending.extend(children.iter().rev().map(|child| (child, depth + 1)));
            }
            Some(_) => return Err("Studio tree node children must be an array".to_string()),
        }
    }
    Ok(())
}

async fn push(State(state): State<AppState>, Json(body): Json<PushBody>) -> Json<Value> {
    if body.stream_id.is_some() || body.phase.is_some() {
        return streamed_push(&state, body);
    }
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
    if body.bootstrap {
        if let Err(error) = validate_bootstrap_service_roots(&body.services, true)
            .and_then(|_| validate_bootstrap_services(&body.services))
        {
            return Json(json!({
                "ok": false,
                "applied": 0,
                "skipped": 0,
                "conflicts": [],
                "errors": [format!("bootstrap: {error}")],
            }));
        }
    }
    let root = state.canonical_project.as_path();
    let ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: false,
        strict: false,
        force_prune: false,
        project_root: root,
        backup_forced_removals: true,
    };
    let mut res = PushApplyResult::default();

    if body.bootstrap {
        let bootstrap_ctx = PushCtx {
            conflicts: state.conflict.as_ref(),
            push_quiet: state.push_quiet.as_ref(),
            force_overwrite: true,
            strict: body.strict,
            force_prune: body.force_prune,
            project_root: root,
            backup_forced_removals: true,
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
        project_root: root,
        backup_forced_removals: true,
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
    pub project_root: &'a Path,
    pub backup_forced_removals: bool,
}

impl<'a> PushCtx<'a> {
    fn mark_quiet(&self, path: &Path) {
        // Every production path is constructed below the already-canonical
        // project root. Keep the watcher key lexical: canonicalizing a path
        // here would follow a link/reparse point that appeared concurrently
        // and could alias an unrelated external tree.
        let canon = path
            .strip_prefix(self.project_root)
            .map(|relative| self.project_root.join(relative))
            .unwrap_or_else(|_| path.to_path_buf());
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
    let value = serde_json::to_value(op).map_err(|error| format!("serialize op: {error}"))?;
    let payload =
        crate::ws::journal_op_event(&value).ok_or_else(|| "journal op event".to_string())?;
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
    let canon = path
        .strip_prefix(state.canonical_project.as_path())
        .map(|relative| state.canonical_project.join(relative))
        .unwrap_or_else(|_| path.to_path_buf());
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

#[cfg(test)]
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

#[cfg(test)]
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
    ctx: &PushCtx<'_>,
) -> Result<(), String> {
    let from_metadata = crate::fs_safety::metadata_no_follow(from)
        .map_err(|error| format!("inspect restore source {}: {error}", from.display()))?;
    let to_metadata = crate::fs_safety::metadata_no_follow(to)
        .map_err(|error| format!("inspect retained destination {}: {error}", to.display()))?;
    if from_metadata.is_some() || to_metadata.is_none() {
        return Err(format!(
            "restore rename requires only the retained destination to exist (from={}, to={})",
            from_metadata.is_some(),
            to_metadata.is_some()
        ));
    }
    let retained_fence = capture_synced_subtree(ctx.project_root, to)?
        .ok_or_else(|| format!("retained rename destination disappeared: {}", to.display()))?;
    let from_parent = from
        .parent()
        .ok_or_else(|| format!("restore rename has no source parent: {}", from.display()))?;
    ensure_synced_directory_chain(ctx.project_root, from_parent)?;
    let to_parent = to
        .parent()
        .ok_or_else(|| format!("restore rename has no destination parent: {}", to.display()))?;
    let from_guard = crate::fs_safety::guard_synced_directory_chain(ctx.project_root, from_parent)
        .map_err(|error| {
            format!(
                "guard restore source parent {}: {error}",
                from_parent.display()
            )
        })?;
    let to_guard = crate::fs_safety::guard_synced_directory_chain(ctx.project_root, to_parent)
        .map_err(|error| {
            format!(
                "guard retained destination parent {}: {error}",
                to_parent.display()
            )
        })?;
    from_guard.verify().map_err(|error| {
        format!(
            "verify restore source parent {}: {error}",
            from_parent.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "verify retained destination parent {}: {error}",
            to_parent.display()
        )
    })?;
    std::fs::rename(to, from).map_err(|error| {
        format!(
            "restore rename {} -> {}: {error}",
            to.display(),
            from.display()
        )
    })?;
    from_guard.verify().map_err(|error| {
        format!(
            "restore source parent changed during rename {}: {error}",
            from_parent.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "retained destination parent changed during rename {}: {error}",
            to_parent.display()
        )
    })?;
    let restored_fence = capture_synced_subtree(ctx.project_root, from)?
        .ok_or_else(|| format!("restored rename source disappeared: {}", from.display()))?;
    if !relocated_subtree_matches(&retained_fence, &restored_fence) {
        return Err(format!(
            "retained subtree changed during restore rename: {}",
            from.display()
        ));
    }
    if let Some(parent) = conflict_path.parent() {
        ensure_synced_directory_chain(ctx.project_root, parent)?;
    }
    if let Err(error) = write_synced_file_atomic(conflict_path, studio_bytes, ctx) {
        let rollback = rollback_restored_rename_if_unchanged(
            ctx.project_root,
            from,
            to,
            &restored_fence,
            &from_guard,
            &to_guard,
        );
        return Err(format!(
            "install restored Studio source {}: {error}; directory rollback: {}",
            conflict_path.display(),
            rollback
                .map(|_| "ok".to_string())
                .unwrap_or_else(|rollback| rollback.to_string())
        ));
    }
    Ok(())
}

fn rollback_restored_rename_if_unchanged(
    project_root: &Path,
    from: &Path,
    to: &Path,
    restored_fence: &SafeSubtreeFence,
    from_guard: &crate::fs_safety::PathParentGuard,
    to_guard: &crate::fs_safety::PathParentGuard,
) -> Result<(), String> {
    from_guard.verify().map_err(|error| {
        format!(
            "refusing directory rollback because source parent changed {}: {error}",
            from.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "refusing directory rollback because destination parent changed {}: {error}",
            to.display()
        )
    })?;
    let current = capture_synced_subtree(project_root, from)?.ok_or_else(|| {
        format!(
            "refusing directory rollback because restored source disappeared: {}",
            from.display()
        )
    })?;
    if !relocated_subtree_matches(restored_fence, &current) {
        return Err(format!(
            "refusing directory rollback because restored source changed: {}",
            from.display()
        ));
    }
    if crate::fs_safety::metadata_no_follow(to)
        .map_err(|error| format!("inspect rollback destination {}: {error}", to.display()))?
        .is_some()
    {
        return Err(format!(
            "refusing directory rollback because destination appeared: {}",
            to.display()
        ));
    }
    from_guard.verify().map_err(|error| {
        format!(
            "refusing directory rollback because source parent changed {}: {error}",
            from.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "refusing directory rollback because destination parent changed {}: {error}",
            to.display()
        )
    })?;
    std::fs::rename(from, to).map_err(|error| {
        format!(
            "restore rollback {} -> {}: {error}",
            from.display(),
            to.display()
        )
    })?;
    from_guard.verify().map_err(|error| {
        format!(
            "source parent changed during directory rollback {}: {error}",
            from.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "destination parent changed during directory rollback {}: {error}",
            to.display()
        )
    })?;
    Ok(())
}

fn restore_fs_deleted_source(
    path: &Path,
    studio_bytes: &[u8],
    ctx: &PushCtx<'_>,
) -> Result<(), String> {
    if crate::fs_safety::metadata_no_follow(path)
        .map_err(|error| format!("inspect restored source {}: {error}", path.display()))?
        .is_some()
    {
        return Err(format!(
            "refusing to restore deleted source because {} already exists",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("restored source has no parent: {}", path.display()))?;
    ensure_synced_directory_chain(ctx.project_root, parent)?;
    write_synced_file_atomic(path, studio_bytes, ctx)
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

#[cfg(test)]
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

#[cfg(test)]
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

fn collect_tree_update_ops(
    project_root: &Path,
    path: &Path,
    out: &mut Vec<Op>,
) -> Result<(), String> {
    let fence = capture_synced_subtree(project_root, path)?
        .ok_or_else(|| format!("resolved local tree disappeared: {}", path.display()))?;
    for entry in &fence.entries {
        let is_dir = entry.kind == crate::fs_safety::SafeEntryKind::Directory;
        let content = if is_dir {
            None
        } else {
            let Some(name) = entry.path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if classify_script_file(name).is_none() && !is_init_file(name) {
                continue;
            }
            Some(read_synced_file(project_root, &entry.path)?)
        };
        out.push(Op {
            kind: OpKind::Update,
            path: entry.path.clone(),
            from: None,
            content,
            is_dir: Some(is_dir),
        });
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

fn resolve_conflict_target(project: &Path, raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    let candidate = if path.is_absolute() {
        path
    } else {
        project.join(path)
    };
    crate::fs_safety::validate_synced_path(project, &candidate, true)
        .map(|_| candidate.clone())
        .map_err(|error| format!("unsafe conflict path {}: {error}", candidate.display()))
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

    let target = match resolve_conflict_target(&state.canonical_project, &body.path) {
        Ok(target) => target,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": error,
                "path": body.path,
            }));
        }
    };
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
    let resolution_ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: true,
        strict: false,
        force_prune: false,
        project_root: state.canonical_project.as_path(),
        backup_forced_removals: false,
    };

    match decision {
        Resolved::WriteFs(bytes) => {
            if let Some(parent) = target.parent() {
                if let Err(error) =
                    ensure_synced_directory_chain(state.canonical_project.as_path(), parent)
                {
                    return Json(json!({ "ok": false, "error": error }));
                }
            }
            if let Err(error) = write_synced_file_atomic(&target, &bytes, &resolution_ctx) {
                return Json(json!({ "ok": false, "error": error }));
            }
            state
                .conflict
                .record_sync(&target, hash(&bytes), fs_mtime(&target));
            Json(json!({ "ok": true, "action": "wrote-fs", "path": body.path }))
        }
        Resolved::PushStudio {
            bytes,
            is_dir,
            rejected_studio,
        } => {
            let ops = if is_dir {
                let mut ops = Vec::new();
                if let Err(error) =
                    collect_tree_update_ops(state.canonical_project.as_path(), &target, &mut ops)
                {
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
                    is_dir: Some(false),
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
            if let Err(error) = remove_synced_subtree(&target, &resolution_ctx, None) {
                state
                    .conflict
                    .park_studio_delete(&target, bytes, fs_mtime(&target), is_dir);
                return Json(json!({ "ok": false, "error": error }));
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
                is_dir: Some(is_dir),
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
                is_dir: Some(is_dir),
            };
            let mut ops = Vec::new();
            if let Err(error) =
                collect_tree_update_ops(state.canonical_project.as_path(), &to, &mut ops)
            {
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
            if let Err(error) =
                restore_fs_deleted_source(&conflict_path, &studio_bytes, &resolution_ctx)
            {
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
            if let Err(error) = restore_fs_rename_transactional(
                &from,
                &to,
                &conflict_path,
                &studio_bytes,
                &resolution_ctx,
            ) {
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
    projection_class: &'a str,
    projection_has_children: bool,
    action: ChildAction,
}

struct AppliedChildren {
    applied: usize,
    wanted_fragments: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChildAction {
    Materialize,
    PruneCarrier,
    ReserveOnly,
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

fn exact_disk_path_from_op(
    root: &Path,
    op: &Value,
    field: &str,
) -> Result<Option<PathBuf>, String> {
    let Some(raw) = op.get(field) else {
        return Ok(None);
    };
    let array = raw
        .as_array()
        .ok_or_else(|| format!("{field} must be an array of filesystem fragments"))?;
    let mut fragments = Vec::with_capacity(array.len());
    for value in array {
        let fragment = value
            .as_str()
            .ok_or_else(|| format!("{field} entries must be strings"))?;
        if fragment.is_empty()
            || fragment == "."
            || fragment == ".."
            || fragment.contains(['/', '\\', '\0', ':'])
            || Path::new(fragment).is_absolute()
        {
            return Err(format!("unsafe {field} fragment: {fragment:?}"));
        }
        fragments.push(fragment.to_string());
    }
    let Some(service) = fragments.first() else {
        return Err(format!("{field} must include a synced service"));
    };
    if !snapshot::SYNCED_SERVICES.contains(&service.as_str()) {
        return Err(format!("{field} is outside a synced service: {service}"));
    }

    let path = fragments
        .iter()
        .fold(root.to_path_buf(), |path, fragment| path.join(fragment));
    crate::fs_safety::validate_synced_path(root, &path, true)
        .map(|_| Some(path.clone()))
        .map_err(|error| format!("unsafe {field} path {}: {error}", path.display()))
}

fn disk_fragment_matches_node(fragment: &str, node: &Value) -> bool {
    let Some(name) = node.get("name").and_then(Value::as_str) else {
        return false;
    };
    let Some(class) = node.get("class").and_then(Value::as_str) else {
        return false;
    };
    let is_dir = node
        .get("diskFragmentIsDir")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| node_projection_has_children(node) || class == "Folder");
    disk_fragment_matches_identity(fragment, name, class, is_dir)
}

fn disk_fragment_matches_identity(fragment: &str, name: &str, class: &str, is_dir: bool) -> bool {
    let (fragment_class, stem) = if is_dir {
        (None, fragment.to_string())
    } else {
        let Some((script_class, stem)) = classify_script_file(fragment) else {
            return false;
        };
        (Some(script_class.class_name()), stem)
    };
    if fragment_class.is_some_and(|fragment_class| fragment_class != class) {
        return false;
    }
    let encoded_name = parse_disambiguated(&stem)
        .map(|(base, _)| base)
        .unwrap_or(stem);
    crate::fs_map::decode_name(&encoded_name) == name
}

fn paths_refer_to_same_entry(left: &Path, right: &Path) -> bool {
    left == right || crate::fs_safety::same_physical_object_no_follow(left, right).unwrap_or(false)
}

fn validate_exact_set_target(target: &Path, node: &Value) -> Result<(), String> {
    let fragment = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("set: non-UTF-8 disk fragment {}", target.display()))?;
    let class = node
        .get("class")
        .and_then(Value::as_str)
        .ok_or("set: node missing class")?;
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .ok_or("set: node missing name")?;
    let expected_is_dir = class == "Folder" || node_projection_has_children(node);
    if node
        .get("diskFragmentIsDir")
        .and_then(Value::as_bool)
        .is_some_and(|declared| declared != expected_is_dir)
    {
        return Err(format!(
            "set: diskFragmentIsDir does not match the node representation for {fragment:?}"
        ));
    }
    if node
        .get("diskFragment")
        .and_then(Value::as_str)
        .is_some_and(|declared| declared != fragment)
        || !disk_fragment_matches_node(fragment, node)
    {
        return Err(format!(
            "set: disk fragment {fragment:?} does not match node identity"
        ));
    }
    if target.exists() {
        let existing = path_to_instance_meta(target)
            .map_err(|error| format!("set: inspect {}: {error}", target.display()))?
            .ok_or_else(|| {
                format!(
                    "set: existing target is not a synced instance: {}",
                    target.display()
                )
            })?;
        if existing.name != name || existing.class != class || existing.is_dir != expected_is_dir {
            return Err(format!(
                "set: existing target does not match {class} {name:?}: {}",
                target.display()
            ));
        }
    }
    Ok(())
}

fn apply_exact_set(
    target: PathBuf,
    transition_from: Option<PathBuf>,
    node: &Value,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    validate_exact_set_target(&target, node)?;
    let parent = target
        .parent()
        .ok_or_else(|| format!("set: no parent for {}", target.display()))?;
    let fragment = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("set: non-UTF-8 disk fragment {}", target.display()))?;

    let Some(source) = transition_from else {
        return apply_set_in_dir(parent, node, ctx, Some((fragment, false)));
    };
    if paths_refer_to_same_entry(&source, &target) {
        return apply_set_in_dir(parent, node, ctx, Some((fragment, false)));
    }

    let source_parent = source
        .parent()
        .ok_or_else(|| format!("set: no parent for {}", source.display()))?;
    if !paths_refer_to_same_entry(source_parent, parent) {
        return Err(format!(
            "set: representation transition must stay in one directory: {} -> {}",
            source.display(),
            target.display()
        ));
    }
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .ok_or("set: node missing name")?;
    let class = node
        .get("class")
        .and_then(Value::as_str)
        .ok_or("set: node missing class")?;
    let new_is_dir = class == "Folder" || node_projection_has_children(node);

    if !source.exists() {
        // A replay after a successful transition is idempotent: the old
        // representation is gone and the new one is updated in place.
        if target.exists() {
            if !existing_fragment_compatible(&target, class, new_is_dir) {
                return Err(format!(
                    "set: transition destination is not the requested instance: {}",
                    target.display()
                ));
            }
            return apply_set_in_dir(parent, node, ctx, Some((fragment, false)));
        }
        return Err(format!(
            "set: transition source does not exist: {}",
            source.display()
        ));
    }
    if target.exists() {
        return Err(format!(
            "set: transition destination already exists: {}",
            target.display()
        ));
    }

    let old = path_to_instance_meta(&source)
        .map_err(|error| format!("set: inspect {}: {error}", source.display()))?
        .ok_or_else(|| {
            format!(
                "set: transition source is not a synced instance: {}",
                source.display()
            )
        })?;
    let old_fragment = source
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("set: non-UTF-8 transition source {}", source.display()))?;
    if ScriptClass::from_class(class).is_none()
        || old.name != name
        || old.class != class
        || old.is_dir == new_is_dir
        || !disk_fragment_matches_identity(old_fragment, name, class, old.is_dir)
    {
        return Err(format!(
            "set: {} is not the opposite representation of {class} {name:?}",
            source.display()
        ));
    }

    // A representation change destroys the old physical tree. Require every
    // synced source in that tree to still match its baseline before creating
    // the replacement, so a local edit is never obscured by a parallel path.
    if !ctx.force_overwrite
        && !transition_tree_matches_baselines(&source, ctx.conflicts, ctx.project_root)?
    {
        if source.is_file() {
            let local = read_synced_file(ctx.project_root, &source)?;
            let studio = node
                .get("properties")
                .and_then(|properties| properties.get("Source"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            ctx.conflicts
                .park_studio_update(&source, local, studio, fs_mtime(&source));
        } else {
            ctx.conflicts.park_studio_delete(
                &source,
                format!("[directory retained on disk: {}]", source.display()).into_bytes(),
                fs_mtime(&source),
                true,
            );
        }
        return Ok(ApplyOutcome::Conflict(source));
    }

    let outcome = apply_set_in_dir(parent, node, ctx, Some((fragment, false)))?;
    let ApplyOutcome::Applied(applied) = outcome else {
        return Ok(outcome);
    };

    // Only remove the clean old representation after the new tree has been
    // materialized successfully. Forced startup reconciliation keeps its
    // normal recoverable backup behavior.
    remove_path_for_replace(&source, ctx)?;
    ctx.conflicts.forget_path(&source);
    ctx.mark_quiet(&source);
    ctx.mark_quiet(&target);
    Ok(ApplyOutcome::Applied(applied + 1))
}

fn apply_op(root: &Path, op: &Value, ctx: &PushCtx<'_>) -> Result<ApplyOutcome, String> {
    match op_kind(op) {
        "set" | "replace" => {
            let parent_segs = op.get("path").map(path_segments).unwrap_or_default();
            let node = op.get("node").ok_or("set: missing node")?;
            if let Some(target) = exact_disk_path_from_op(root, op, "diskPath")? {
                let transition_from = exact_disk_path_from_op(root, op, "fromDiskPath")?;
                apply_exact_set(target, transition_from, node, ctx)
            } else if op.get("fromDiskPath").is_some() {
                Err("set: fromDiskPath requires diskPath".into())
            } else {
                apply_set(root, &parent_segs, node, ctx)
            }
        }
        "delete" | "remove" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            if let Some(target) = exact_disk_path_from_op(root, op, "diskPath")? {
                apply_delete_target(target, ctx)
            } else {
                apply_delete(root, &segs, ctx)
            }
        }
        "update" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            let props = op.get("properties").cloned();
            let name = op.get("name").and_then(|v| v.as_str()).map(str::to_string);
            if let Some(target) = exact_disk_path_from_op(root, op, "diskPath")? {
                apply_update_target(target, props, ctx)
            } else {
                apply_update(root, &segs, props, name, ctx)
            }
        }
        "rename" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            let new_name = op
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("rename: missing name")?;
            let legacy_exact = exact_disk_path_from_op(root, op, "diskPath")?;
            let exact_from = exact_disk_path_from_op(root, op, "fromDiskPath")?;
            let exact_to = exact_disk_path_from_op(root, op, "toDiskPath")?;
            if let (Some(legacy), Some(from)) = (&legacy_exact, &exact_from) {
                if !paths_refer_to_same_entry(legacy, from) {
                    return Err(
                        "rename: diskPath and fromDiskPath identify different sources".into(),
                    );
                }
            }
            let exact_source = exact_from.or(legacy_exact);
            if exact_source.is_some() || exact_to.is_some() {
                let source = match exact_source {
                    Some(path) => path,
                    None => match resolve_segments_to_path(root, &segs)? {
                        Some(path) => path,
                        None => return Ok(ApplyOutcome::Applied(0)),
                    },
                };
                apply_rename_target(source, new_name, exact_to, ctx).map(ApplyOutcome::Applied)
            } else {
                apply_rename(root, &segs, new_name, ctx).map(ApplyOutcome::Applied)
            }
        }
        "move" => {
            let from_segs = op.get("from").map(path_segments).unwrap_or_default();
            let to_segs = op.get("to").map(path_segments).unwrap_or_default();
            let exact_from = exact_disk_path_from_op(root, op, "fromDiskPath")?;
            let exact_to = exact_disk_path_from_op(root, op, "toDiskPath")?;
            if exact_from.is_some() || exact_to.is_some() {
                let source = match exact_from {
                    Some(path) => path,
                    None => match resolve_segments_to_path(root, &from_segs)? {
                        Some(path) => path,
                        None => return Ok(ApplyOutcome::Applied(0)),
                    },
                };
                let new_name = to_segs.last().ok_or("move: empty 'to' path")?;
                apply_move_target(source, exact_to, new_name, ctx).map(ApplyOutcome::Applied)
            } else {
                apply_move(root, &from_segs, &to_segs, ctx).map(ApplyOutcome::Applied)
            }
        }
        "" => Err("op missing kind".to_string()),
        other => Err(format!("unknown op: {other}")),
    }
}

type StreamSourceProvider<'a> = dyn FnMut(&Value) -> Result<Option<Vec<u8>>, String> + 'a;

fn apply_service_node(root: &Path, node: &Value, ctx: &PushCtx<'_>) -> Result<usize, String> {
    let mut source_provider = |node: &Value| {
        Ok(node
            .get("properties")
            .and_then(|properties| properties.get("Source"))
            .and_then(Value::as_str)
            .map(|source| source.as_bytes().to_vec()))
    };
    apply_service_node_with_sources(root, node, ctx, &mut source_provider)
}

fn apply_service_node_with_sources(
    root: &Path,
    node: &Value,
    ctx: &PushCtx<'_>,
    source_provider: &mut StreamSourceProvider<'_>,
) -> Result<usize, String> {
    let name = node
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("service: missing name")?;
    let svc_dir = root.join(encode_name(name));
    ensure_synced_directory_chain(ctx.project_root, &svc_dir)?;
    ctx.mark_quiet(&svc_dir);
    // Materialize children of the service node.
    let mut n = 0usize;
    let children = node
        .get("children")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let child_result =
        apply_children_in_dir_with_sources(&svc_dir, children, ctx, source_provider)?;
    n += child_result.applied;
    if ctx.strict && ctx.force_prune {
        n += prune_dir_to_fragments(&svc_dir, &child_result.wanted_fragments, false, ctx)?;
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
    let mut source_provider = |node: &Value| {
        Ok(node
            .get("properties")
            .and_then(|properties| properties.get("Source"))
            .and_then(Value::as_str)
            .map(|source| source.as_bytes().to_vec()))
    };
    apply_set_in_dir_with_sources(
        parent_dir,
        node,
        ctx,
        preferred_fragment,
        &mut source_provider,
    )
}

fn apply_set_in_dir_with_sources(
    parent_dir: &Path,
    node: &Value,
    ctx: &PushCtx<'_>,
    preferred_fragment: Option<(&str, bool)>,
    source_provider: &mut StreamSourceProvider<'_>,
) -> Result<ApplyOutcome, String> {
    if node_is_avoid_sync_boundary(node) || node_is_avoid_sync_carrier(node) {
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
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let has_children = !children.is_empty();
    if class == "Folder" && !has_children {
        return Ok(ApplyOutcome::Skipped);
    }
    ensure_synced_directory_chain(ctx.project_root, parent_dir)?;

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
            None => {
                let taken = siblings_except(parent_dir, None)?;
                instance_to_path(
                    &InstanceDescriptor {
                        class,
                        name,
                        has_children,
                    },
                    &taken,
                )
            }
        },
    };

    let target = parent_dir.join(&frag.fragment);

    let sc = ScriptClass::from_class(class);
    let mut applied = 0usize;

    match (sc, has_children) {
        (Some(_), false) => {
            // Leaf script file. Normalize CRLF→LF so comparisons against FS
            // bytes and cached hashes line up regardless of checkout style.
            let raw_bytes = source_provider(node)?.unwrap_or_default();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            match apply_source_bytes(&target, &bytes, ctx)? {
                SourceWriteOutcome::Applied => applied += 1,
                SourceWriteOutcome::Skipped => {}
                SourceWriteOutcome::Conflict(path) => return Ok(ApplyOutcome::Conflict(path)),
            }
        }
        (Some(sc), true) => {
            // Script-with-children directory.
            ensure_synced_directory_chain(ctx.project_root, &target)?;
            ctx.mark_quiet(&target);
            let init_name = portable_init_file_name(name, sc);
            let preferred_init_path = target.join(&init_name);
            let init_path = if preferred_init_path.exists() {
                preferred_init_path
            } else {
                find_existing_init_source(&target, name, sc)?.unwrap_or(preferred_init_path)
            };
            let raw_bytes = source_provider(node)?.unwrap_or_default();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            match apply_source_bytes(&init_path, &bytes, ctx)? {
                SourceWriteOutcome::Applied => applied += 1,
                SourceWriteOutcome::Skipped => {}
                SourceWriteOutcome::Conflict(path) => return Ok(ApplyOutcome::Conflict(path)),
            }
            let child_result =
                apply_children_in_dir_with_sources(&target, children, ctx, source_provider)?;
            applied += child_result.applied;
            if ctx.strict && ctx.force_prune {
                applied +=
                    prune_dir_to_fragments(&target, &child_result.wanted_fragments, true, ctx)?;
            }
        }
        (None, _) => {
            // Folder (the only surviving non-script whitelisted class).
            ensure_synced_directory_chain(ctx.project_root, &target)?;
            ctx.mark_quiet(&target);
            let child_result =
                apply_children_in_dir_with_sources(&target, children, ctx, source_provider)?;
            applied += child_result.applied;
            if ctx.strict && ctx.force_prune {
                applied +=
                    prune_dir_to_fragments(&target, &child_result.wanted_fragments, false, ctx)?;
            }
            applied += 1;
        }
    }
    Ok(ApplyOutcome::Applied(applied))
}

/// Apply a complete sibling batch after indexing the existing directory once.
///
/// Bootstrap snapshots commonly contain thousands of children under one
/// service. Looking up a legacy/case-disambiguated fragment separately for
/// every child turns that workload into O(children * directory entries).
/// Reusing this index keeps each directory level linear while preserving the
/// exact-fragment-first and best-compatible legacy fallback behavior.
fn apply_children_in_dir_with_sources(
    parent_dir: &Path,
    children: &[Value],
    ctx: &PushCtx<'_>,
    source_provider: &mut StreamSourceProvider<'_>,
) -> Result<AppliedChildren, String> {
    let existing_index = index_child_fragments(parent_dir)
        .map_err(|error| format!("scan {}: {error}", parent_dir.display()))?;
    let assignments = child_fragment_assignments(children);
    let mut applied = 0usize;
    let mut wanted_fragments = HashSet::new();
    let mut consumed_existing = HashSet::new();
    for child in assignments {
        let fragment = resolve_child_assignment_fragment(
            parent_dir,
            &child,
            &existing_index,
            &mut consumed_existing,
        )?;
        wanted_fragments.insert(fragment.to_ascii_lowercase());
        if child.action == ChildAction::ReserveOnly {
            continue;
        }
        if child.action == ChildAction::PruneCarrier {
            applied +=
                prune_existing_avoid_sync_carrier(parent_dir, child.node, ctx, &fragment, false)?;
            continue;
        }
        if let ApplyOutcome::Applied(count) = apply_set_in_dir_with_sources(
            parent_dir,
            child.node,
            ctx,
            Some((&fragment, false)),
            source_provider,
        )? {
            applied += count;
        }
    }
    Ok(AppliedChildren {
        applied,
        wanted_fragments,
    })
}

/// Strict Studio-wins must prune stale sync-owned entries around an ignored
/// branch without ever creating the Studio-only carrier on disk. Descend only
/// when the carrier already has a filesystem directory; an AvoidSync boundary
/// below it is retained wholesale, while unrelated siblings are removed.
fn prune_existing_avoid_sync_carrier(
    parent_dir: &Path,
    node: &Value,
    ctx: &PushCtx<'_>,
    preferred_fragment: &str,
    fallback_by_name: bool,
) -> Result<usize, String> {
    if !ctx.strict || !ctx.force_prune {
        return Ok(0);
    }
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .ok_or("carrier: node missing name")?;
    let target = if parent_dir.join(preferred_fragment).exists() {
        parent_dir.join(preferred_fragment)
    } else if fallback_by_name {
        let Some(existing) =
            find_child_fragment_by_name(parent_dir, name).map_err(|error| error.to_string())?
        else {
            return Ok(0);
        };
        parent_dir.join(existing)
    } else {
        return Ok(0);
    };

    if !target.is_dir() {
        if target.exists() && disk_path_is_sync_owned(&target) {
            remove_path_for_replace(&target, ctx)?;
            return Ok(1);
        }
        return Ok(0);
    }

    let children = node
        .get("children")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let existing_index = index_child_fragments(&target)
        .map_err(|error| format!("scan {}: {error}", target.display()))?;
    let mut applied = 0usize;
    let mut wanted_fragments = HashSet::new();
    let mut consumed_existing = HashSet::new();
    for child in child_fragment_assignments(children) {
        let fragment = resolve_child_assignment_fragment(
            &target,
            &child,
            &existing_index,
            &mut consumed_existing,
        )?;
        wanted_fragments.insert(fragment.to_ascii_lowercase());
        if child.action == ChildAction::PruneCarrier {
            applied +=
                prune_existing_avoid_sync_carrier(&target, child.node, ctx, &fragment, false)?;
        }
    }
    applied += prune_dir_to_fragments(&target, &wanted_fragments, false, ctx)?;
    Ok(applied)
}

/// Resolve one logical snapshot child to at most one existing filesystem
/// fragment. AvoidSync reservations are processed before materialized
/// siblings, so consuming candidates here prevents an ignored branch and a
/// same-name live sibling from ever claiming the same path. Exact canonical
/// fragments always win; the legacy-name fallback is intentionally limited to
/// an undisambiguated compatible fragment so a missing ignored branch cannot
/// steal an existing live `[N]` sibling on a later bootstrap.
fn resolve_child_assignment_fragment(
    parent_dir: &Path,
    assignment: &ChildAssignment<'_>,
    existing: &ExistingChildFragmentIndex,
    consumed_existing: &mut HashSet<String>,
) -> Result<String, String> {
    let name = assignment
        .node
        .get("name")
        .and_then(Value::as_str)
        .ok_or("set: node missing name")?;
    let canonical_key = assignment.fragment.to_ascii_lowercase();
    let canonical_path = parent_dir.join(&assignment.fragment);
    if canonical_path.exists() && !consumed_existing.contains(&canonical_key) {
        consumed_existing.insert(canonical_key);
        return Ok(assignment.fragment.clone());
    }

    let may_use_legacy_name =
        assignment.action != ChildAction::Materialize || assignment.fallback_by_name;
    if may_use_legacy_name {
        if let Some(candidates) = existing.all_by_name.get(name) {
            for candidate in candidates {
                let candidate_key = candidate.to_ascii_lowercase();
                if consumed_existing.contains(&candidate_key)
                    || fragment_disambiguation_ordinal(candidate) != 0
                {
                    continue;
                }
                let candidate_path = parent_dir.join(candidate);
                let compatible = match assignment.action {
                    ChildAction::Materialize | ChildAction::ReserveOnly => {
                        if is_scoped_class(assignment.projection_class) {
                            existing_fragment_compatible(
                                &candidate_path,
                                assignment.projection_class,
                                assignment.projection_has_children,
                            )
                        } else {
                            candidate_path.is_dir()
                        }
                    }
                    ChildAction::PruneCarrier => candidate_path.is_dir(),
                };
                if compatible {
                    consumed_existing.insert(candidate_key);
                    return Ok(candidate.clone());
                }
            }
        }
    }

    // Reserving a nonexistent canonical fragment still consumes its logical
    // slot for this batch. PathFragmentAllocator already guarantees distinct
    // canonical fragments, and this makes accidental duplicate assignments
    // fail closed instead of aliasing one target.
    if !consumed_existing.insert(canonical_key) {
        return Err(format!(
            "ambiguous snapshot children resolve to the same fragment {}",
            parent_dir.join(&assignment.fragment).display()
        ));
    }
    Ok(assignment.fragment.clone())
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
    let index = crate::fs_safety::PortableDirectoryIndex::read(dir)
        .map_err(|error| format!("scan {}: {error}", dir.display()))?;
    let mut named_matches = Vec::new();
    let mut plain_match = None;
    for entry in index.entries() {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        if let Some((class, name)) = parse_init_file(&entry.fragment) {
            if class == expected_class && name == expected_name {
                named_matches.push(entry.path.clone());
            }
            continue;
        }
        if parse_plain_init_file(&entry.fragment) == Some(expected_class) {
            plain_match = Some(entry.path.clone());
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
        if !node_should_reserve_path(child) {
            continue;
        }
        if let Some(name) = child.get("name").and_then(|v| v.as_str()) {
            *name_counts.entry(name.to_string()).or_insert(0) += 1;
        }
    }

    let mut allocator = PathFragmentAllocator::new();
    let mut out = Vec::new();
    for index in diff::snapshot_sibling_order(children) {
        let child = &children[index];
        if !node_should_reserve_path(child) {
            continue;
        }
        let Some(name) = child.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(class) = child.get("class").and_then(|v| v.as_str()) else {
            continue;
        };
        let has_children = node_projection_has_children(child);
        let fragment = allocator.allocate(&InstanceDescriptor {
            class,
            name,
            has_children,
        });
        let action = if node_is_avoid_sync_boundary(child) {
            ChildAction::ReserveOnly
        } else if node_is_avoid_sync_carrier(child) {
            ChildAction::PruneCarrier
        } else {
            ChildAction::Materialize
        };
        out.push(ChildAssignment {
            node: child,
            fragment: fragment.fragment,
            fallback_by_name: name_counts.get(name).copied().unwrap_or(0) == 1,
            projection_class: class,
            projection_has_children: has_children,
            action,
        });
        // An AvoidSync marker deliberately omits its descendants, so the
        // daemon cannot know whether an ignored script currently projects as a
        // leaf file or a script-with-children directory. Reserve both portable
        // shapes; safety takes precedence over packing a same-name live sibling
        // into either bare fragment.
        if node_is_avoid_sync_boundary(child) && ScriptClass::from_class(class).is_some() {
            let alternate = allocator.allocate(&InstanceDescriptor {
                class,
                name,
                has_children: !has_children,
            });
            out.push(ChildAssignment {
                node: child,
                fragment: alternate.fragment,
                fallback_by_name: name_counts.get(name).copied().unwrap_or(0) == 1,
                projection_class: class,
                projection_has_children: !has_children,
                action: ChildAction::ReserveOnly,
            });
        }
    }
    out
}

fn node_should_reserve_path(node: &Value) -> bool {
    node_should_materialize(node)
        || node_is_avoid_sync_boundary(node)
        || node_is_avoid_sync_carrier(node)
}

fn node_projection_has_children(node: &Value) -> bool {
    node.get("children")
        .and_then(Value::as_array)
        .is_some_and(|children| !children.is_empty())
        || node.get("hasChildren").and_then(Value::as_bool) == Some(true)
}

fn node_should_materialize(node: &Value) -> bool {
    if node_is_avoid_sync_boundary(node) || node_is_avoid_sync_carrier(node) {
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

fn node_is_avoid_sync_boundary(node: &Value) -> bool {
    node.get("avoidSync").and_then(Value::as_bool) == Some(true)
}

fn node_is_avoid_sync_carrier(node: &Value) -> bool {
    node.get("avoidSyncCarrier").and_then(Value::as_bool) == Some(true)
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

fn prune_dir_to_fragments(
    dir: &Path,
    wanted_fragments: &HashSet<String>,
    keep_init_files: bool,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    let validated = crate::fs_safety::validate_synced_path(ctx.project_root, dir, true)
        .map_err(|error| format!("validate prune directory {}: {error}", dir.display()))?;
    let Some(metadata) = crate::fs_safety::metadata_no_follow(&validated)
        .map_err(|error| format!("inspect prune directory {}: {error}", dir.display()))?
    else {
        return Ok(0);
    };
    if !metadata.is_dir() {
        return Err(format!(
            "prune target is not a directory: {}",
            dir.display()
        ));
    }
    let mut removed = 0usize;
    let index = crate::fs_safety::PortableDirectoryIndex::read(&validated)
        .map_err(|error| format!("scan prune directory {}: {error}", dir.display()))?;
    for entry in index.entries() {
        let path = entry.path.clone();
        let file_name = entry.fragment.as_str();
        if file_name == META_FILE || file_name == ".DS_Store" {
            continue;
        }
        if wanted_fragments.contains(&file_name.to_ascii_lowercase()) {
            continue;
        }
        if is_init_file(file_name) {
            if keep_init_files {
                continue;
            }
            remove_path_for_replace(&path, ctx)?;
            removed += 1;
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
    let mut stack = vec![dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = stack.pop() {
        let Ok(index) = crate::fs_safety::PortableDirectoryIndex::read(&directory) else {
            return false;
        };
        for entry in index.entries() {
            visited = visited.saturating_add(1);
            if visited > crate::fs_safety::MAX_SERVICE_TREE_NODES {
                return false;
            }
            if entry.fragment == META_FILE || entry.fragment == ".DS_Store" {
                continue;
            }
            if is_init_file(&entry.fragment) {
                return true;
            }
            if entry.kind == crate::fs_safety::SafeEntryKind::File {
                if classify_script_file(&entry.fragment).is_some() {
                    return true;
                }
            } else {
                stack.push(entry.path.clone());
            }
        }
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SafeSubtreeEntry {
    path: PathBuf,
    relative: PathBuf,
    kind: crate::fs_safety::SafeEntryKind,
    file_generation: Option<crate::fs_safety::FileGeneration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SafeSubtreeFence {
    root: PathBuf,
    entries: Vec<SafeSubtreeEntry>,
}

fn relocated_subtree_matches(left: &SafeSubtreeFence, right: &SafeSubtreeFence) -> bool {
    left.entries.len() == right.entries.len()
        && left
            .entries
            .iter()
            .zip(&right.entries)
            .all(|(left, right)| {
                left.relative == right.relative
                    && left.kind == right.kind
                    && left.file_generation == right.file_generation
            })
}

#[derive(Debug)]
struct BackupReceipt {
    #[cfg_attr(not(test), allow(dead_code))]
    destination: PathBuf,
    source_fence: SafeSubtreeFence,
}

fn capture_synced_subtree(
    project_root: &Path,
    path: &Path,
) -> Result<Option<SafeSubtreeFence>, String> {
    let lexical_root = path.to_path_buf();
    let validated = crate::fs_safety::validate_synced_path(project_root, path, true)
        .map_err(|error| format!("validate synced subtree {}: {error}", path.display()))?;
    let parent_guard = crate::fs_safety::guard_synced_parent_chain(project_root, &validated, true)
        .map_err(|error| format!("guard synced subtree {}: {error}", path.display()))?;
    parent_guard
        .verify()
        .map_err(|error| format!("verify synced subtree parent {}: {error}", path.display()))?;
    let Some(root_metadata) = crate::fs_safety::metadata_no_follow(&validated)
        .map_err(|error| format!("inspect synced subtree {}: {error}", path.display()))?
    else {
        return Ok(None);
    };
    let root_kind = if root_metadata.is_dir() {
        crate::fs_safety::SafeEntryKind::Directory
    } else if root_metadata.is_file() {
        crate::fs_safety::SafeEntryKind::File
    } else {
        return Err(format!(
            "unsupported object in synced subtree: {}",
            validated.display()
        ));
    };
    let root_generation = if root_kind == crate::fs_safety::SafeEntryKind::File {
        Some(crate::fs_safety::file_generation_no_follow(&validated)?)
    } else {
        None
    };
    let mut entries = vec![SafeSubtreeEntry {
        path: lexical_root.clone(),
        relative: PathBuf::new(),
        kind: root_kind,
        file_generation: root_generation,
    }];
    let mut stack = Vec::new();
    if root_kind == crate::fs_safety::SafeEntryKind::Directory {
        stack.push((validated.clone(), PathBuf::new(), 0usize));
    }
    while let Some((directory, relative, depth)) = stack.pop() {
        if depth > crate::fs_safety::MAX_SERVICE_TREE_DEPTH {
            return Err(format!(
                "synced subtree exceeds depth {} at {}",
                crate::fs_safety::MAX_SERVICE_TREE_DEPTH,
                directory.display()
            ));
        }
        let index = crate::fs_safety::PortableDirectoryIndex::read(&directory)
            .map_err(|error| format!("scan synced subtree {}: {error}", directory.display()))?;
        for entry in index.entries() {
            if entries.len() >= crate::fs_safety::MAX_SERVICE_TREE_NODES {
                return Err(format!(
                    "synced subtree exceeds node limit {}",
                    crate::fs_safety::MAX_SERVICE_TREE_NODES
                ));
            }
            let entry_relative = relative.join(&entry.fragment);
            let file_generation = if entry.kind == crate::fs_safety::SafeEntryKind::File {
                Some(crate::fs_safety::file_generation_no_follow(&entry.path)?)
            } else {
                None
            };
            entries.push(SafeSubtreeEntry {
                path: lexical_root.join(&entry_relative),
                relative: entry_relative.clone(),
                kind: entry.kind,
                file_generation,
            });
        }
        for entry in index.entries().iter().rev() {
            if entry.kind == crate::fs_safety::SafeEntryKind::Directory {
                stack.push((
                    entry.path.clone(),
                    relative.join(&entry.fragment),
                    depth + 1,
                ));
            }
        }
    }
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    parent_guard
        .verify()
        .map_err(|error| format!("synced subtree parent changed {}: {error}", path.display()))?;
    Ok(Some(SafeSubtreeFence {
        root: lexical_root,
        entries,
    }))
}

fn ensure_descendant_directory_chain(base: &Path, target: &Path) -> Result<PathBuf, String> {
    let canonical_base = crate::fs_safety::stable_canonical_directory(base)
        .map_err(|error| format!("validate directory base {}: {error}", base.display()))?;
    let relative = target
        .strip_prefix(base)
        .or_else(|_| target.strip_prefix(&canonical_base))
        .map_err(|error| {
            format!(
                "directory target {} is outside {}: {error}",
                target.display(),
                canonical_base.display()
            )
        })?;
    let mut current = canonical_base.clone();
    for component in relative.components() {
        let std::path::Component::Normal(fragment) = component else {
            return Err(format!(
                "unsafe directory component in {}",
                target.display()
            ));
        };
        let next = current.join(fragment);
        let guard = crate::fs_safety::guard_descendant_parent_chain(&canonical_base, &next, true)
            .map_err(|error| format!("guard directory {}: {error}", next.display()))?;
        guard
            .verify()
            .map_err(|error| format!("verify directory parent {}: {error}", next.display()))?;
        match crate::fs_safety::metadata_no_follow(&next)
            .map_err(|error| format!("inspect directory {}: {error}", next.display()))?
        {
            Some(metadata) if metadata.is_dir() => {}
            Some(_) => {
                return Err(format!(
                    "directory chain contains a non-directory: {}",
                    next.display()
                ));
            }
            None => {
                std::fs::create_dir(&next)
                    .map_err(|error| format!("create directory {}: {error}", next.display()))?;
            }
        }
        guard
            .verify()
            .map_err(|error| format!("directory parent changed {}: {error}", next.display()))?;
        let metadata = crate::fs_safety::require_metadata_no_follow(&next)
            .map_err(|error| format!("verify created directory {}: {error}", next.display()))?;
        if !metadata.is_dir() {
            return Err(format!(
                "created directory changed into another object: {}",
                next.display()
            ));
        }
        current = next;
    }
    Ok(current)
}

fn ensure_synced_directory_chain(project_root: &Path, target: &Path) -> Result<PathBuf, String> {
    let validated = crate::fs_safety::validate_synced_path(project_root, target, true)
        .map_err(|error| format!("validate synced directory {}: {error}", target.display()))?;
    ensure_descendant_directory_chain(project_root, &validated)
}

fn copy_backup_path(
    project_root: &Path,
    source_fence: &SafeSubtreeFence,
    transaction: &Path,
    destination: &Path,
) -> Result<(), String> {
    use std::io::{Read as _, Write as _};

    let mut ordered = source_fence.entries.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.relative
            .components()
            .count()
            .cmp(&right.relative.components().count())
            .then_with(|| left.relative.cmp(&right.relative))
    });
    let mut buffer = [0u8; 64 * 1024];
    for entry in ordered {
        let target = if entry.relative.as_os_str().is_empty() {
            destination.to_path_buf()
        } else {
            destination.join(&entry.relative)
        };
        match entry.kind {
            crate::fs_safety::SafeEntryKind::Directory => {
                let parent = target.parent().ok_or_else(|| {
                    format!("backup directory has no parent: {}", target.display())
                })?;
                ensure_descendant_directory_chain(transaction, parent)?;
                let guard =
                    crate::fs_safety::guard_descendant_parent_chain(transaction, &target, true)
                        .map_err(|error| {
                            format!("guard backup directory {}: {error}", target.display())
                        })?;
                guard.verify().map_err(|error| {
                    format!(
                        "verify backup directory parent {}: {error}",
                        target.display()
                    )
                })?;
                std::fs::create_dir(&target).map_err(|error| {
                    format!("create backup directory {}: {error}", target.display())
                })?;
                guard.verify().map_err(|error| {
                    format!(
                        "backup directory parent changed {}: {error}",
                        target.display()
                    )
                })?;
            }
            crate::fs_safety::SafeEntryKind::File => {
                let expected = entry.file_generation.as_ref().ok_or_else(|| {
                    format!(
                        "backup file is missing a generation: {}",
                        entry.path.display()
                    )
                })?;
                let source_guard =
                    crate::fs_safety::guard_synced_parent_chain(project_root, &entry.path, false)
                        .map_err(|error| {
                        format!("guard backup source {}: {error}", entry.path.display())
                    })?;
                source_guard.verify().map_err(|error| {
                    format!(
                        "verify backup source parent {}: {error}",
                        entry.path.display()
                    )
                })?;
                if crate::fs_safety::file_generation_no_follow(&entry.path)? != *expected {
                    return Err(format!(
                        "backup source changed before copy: {}",
                        entry.path.display()
                    ));
                }
                let parent = target
                    .parent()
                    .ok_or_else(|| format!("backup file has no parent: {}", target.display()))?;
                ensure_descendant_directory_chain(transaction, parent)?;
                let target_guard =
                    crate::fs_safety::guard_descendant_parent_chain(transaction, &target, true)
                        .map_err(|error| {
                            format!("guard backup target {}: {error}", target.display())
                        })?;
                target_guard.verify().map_err(|error| {
                    format!("verify backup target parent {}: {error}", target.display())
                })?;
                let mut source = crate::fs_safety::open_regular_file_no_follow(&entry.path)
                    .map_err(|error| {
                        format!("open backup source {}: {error}", entry.path.display())
                    })?;
                let mut target_file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map_err(|error| format!("create backup file {}: {error}", target.display()))?;
                loop {
                    let count = source.read(&mut buffer).map_err(|error| {
                        format!("read backup source {}: {error}", entry.path.display())
                    })?;
                    if count == 0 {
                        break;
                    }
                    target_file.write_all(&buffer[..count]).map_err(|error| {
                        format!("write backup file {}: {error}", target.display())
                    })?;
                }
                target_file
                    .sync_all()
                    .map_err(|error| format!("sync backup file {}: {error}", target.display()))?;
                if crate::fs_safety::file_generation_no_follow(&entry.path)? != *expected {
                    return Err(format!(
                        "backup source changed during copy: {}",
                        entry.path.display()
                    ));
                }
                source_guard.verify().map_err(|error| {
                    format!(
                        "backup source parent changed {}: {error}",
                        entry.path.display()
                    )
                })?;
                target_guard.verify().map_err(|error| {
                    format!("backup target parent changed {}: {error}", target.display())
                })?;
            }
        }
    }
    Ok(())
}

fn backup_forced_removal(path: &Path, project_root: &Path) -> Result<BackupReceipt, String> {
    let source_fence = capture_synced_subtree(project_root, path)?
        .ok_or_else(|| format!("backup source disappeared: {}", path.display()))?;
    let canonical_project_root = crate::fs_safety::stable_canonical_directory(project_root)
        .map_err(|error| format!("validate backup project root: {error}"))?;
    let relative = source_fence
        .root
        .strip_prefix(project_root)
        .or_else(|_| source_fence.root.strip_prefix(&canonical_project_root))
        .map_err(|error| format!("backup path {}: {error}", path.display()))?;
    let service = relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .ok_or_else(|| format!("backup path has no synced service: {}", path.display()))?;
    if !snapshot::SYNCED_SERVICES.contains(&service) {
        return Err(format!(
            "refusing destructive write outside a synced service: {}",
            path.display()
        ));
    }

    let backup_root =
        ensure_descendant_directory_chain(project_root, &project_root.join(".rosync-backups"))?;
    static BACKUP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = BACKUP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let transaction = backup_root.join(format!("{stamp}-{sequence}"));
    let transaction_guard =
        crate::fs_safety::guard_descendant_parent_chain(project_root, &transaction, true).map_err(
            |error| {
                format!(
                    "guard backup transaction {}: {error}",
                    transaction.display()
                )
            },
        )?;
    transaction_guard.verify().map_err(|error| {
        format!(
            "verify backup transaction parent {}: {error}",
            transaction.display()
        )
    })?;
    std::fs::create_dir(&transaction).map_err(|error| {
        format!(
            "create backup transaction {}: {error}",
            transaction.display()
        )
    })?;
    transaction_guard.verify().map_err(|error| {
        format!(
            "backup transaction parent changed {}: {error}",
            transaction.display()
        )
    })?;

    let destination = transaction.join(relative);
    copy_backup_path(project_root, &source_fence, &transaction, &destination)?;
    let current = capture_synced_subtree(project_root, path)?
        .ok_or_else(|| format!("backup source disappeared during copy: {}", path.display()))?;
    if current != source_fence {
        return Err(format!(
            "backup source changed while it was copied: {}",
            path.display()
        ));
    }
    Ok(BackupReceipt {
        destination,
        source_fence,
    })
}

fn remove_synced_subtree(
    path: &Path,
    ctx: &PushCtx<'_>,
    expected: Option<&SafeSubtreeFence>,
) -> Result<bool, String> {
    let Some(current) = capture_synced_subtree(ctx.project_root, path)? else {
        return Ok(false);
    };
    if expected.is_some_and(|expected| expected != &current) {
        return Err(format!(
            "refusing to remove subtree that changed after backup: {}",
            path.display()
        ));
    }
    let mut entries = current.entries.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .relative
            .components()
            .count()
            .cmp(&left.relative.components().count())
            .then_with(|| right.relative.cmp(&left.relative))
            .then_with(|| {
                (right.kind == crate::fs_safety::SafeEntryKind::File)
                    .cmp(&(left.kind == crate::fs_safety::SafeEntryKind::File))
            })
    });
    for entry in entries {
        let guard =
            crate::fs_safety::guard_synced_parent_chain(ctx.project_root, &entry.path, false)
                .map_err(|error| format!("guard removal {}: {error}", entry.path.display()))?;
        guard
            .verify()
            .map_err(|error| format!("verify removal parent {}: {error}", entry.path.display()))?;
        ctx.mark_quiet(&entry.path);
        match entry.kind {
            crate::fs_safety::SafeEntryKind::File => {
                let expected_generation = entry.file_generation.as_ref().ok_or_else(|| {
                    format!(
                        "removal file is missing a generation: {}",
                        entry.path.display()
                    )
                })?;
                if crate::fs_safety::file_generation_no_follow(&entry.path)? != *expected_generation
                {
                    return Err(format!(
                        "refusing to remove file changed after validation: {}",
                        entry.path.display()
                    ));
                }
                std::fs::remove_file(&entry.path)
                    .map_err(|error| format!("remove file {}: {error}", entry.path.display()))?;
            }
            crate::fs_safety::SafeEntryKind::Directory => {
                let index = crate::fs_safety::PortableDirectoryIndex::read(&entry.path).map_err(
                    |error| format!("verify empty directory {}: {error}", entry.path.display()),
                )?;
                if !index.entries().is_empty() {
                    return Err(format!(
                        "refusing to remove directory that gained entries: {}",
                        entry.path.display()
                    ));
                }
                std::fs::remove_dir(&entry.path).map_err(|error| {
                    format!("remove directory {}: {error}", entry.path.display())
                })?;
            }
        }
        guard.verify().map_err(|error| {
            format!(
                "removal parent changed while deleting {}: {error}",
                entry.path.display()
            )
        })?;
    }
    ctx.mark_quiet(path);
    Ok(true)
}

fn remove_path_for_replace(path: &Path, ctx: &PushCtx<'_>) -> Result<(), String> {
    let backup = if ctx.backup_forced_removals
        && (ctx.force_overwrite || (ctx.strict && ctx.force_prune))
        && crate::fs_safety::metadata_no_follow(path)
            .map_err(|error| format!("inspect replacement target {}: {error}", path.display()))?
            .is_some()
    {
        Some(backup_forced_removal(path, ctx.project_root)?)
    } else {
        None
    };
    let expected = backup.as_ref().map(|backup| &backup.source_fence);
    remove_synced_subtree(path, ctx, expected)?;
    Ok(())
}

enum SourceWriteOutcome {
    Applied,
    Skipped,
    Conflict(PathBuf),
}

fn read_synced_file(project_root: &Path, path: &Path) -> Result<Vec<u8>, String> {
    let validated = crate::fs_safety::validate_synced_path(project_root, path, false)
        .map_err(|error| format!("validate source {}: {error}", path.display()))?;
    let guard = crate::fs_safety::guard_synced_parent_chain(project_root, &validated, false)
        .map_err(|error| format!("guard source {}: {error}", path.display()))?;
    guard
        .verify()
        .map_err(|error| format!("verify source parent {}: {error}", path.display()))?;
    let before = crate::fs_safety::file_generation_no_follow(&validated)?;
    let bytes = crate::fs_safety::read_file_no_follow(&validated)
        .map_err(|error| format!("read source {}: {error}", path.display()))?;
    let after = crate::fs_safety::file_generation_no_follow(&validated)?;
    if before != after {
        return Err(format!(
            "source changed while it was read: {}",
            path.display()
        ));
    }
    guard
        .verify()
        .map_err(|error| format!("source parent changed {}: {error}", path.display()))?;
    Ok(bytes)
}

fn write_synced_file_atomic(target: &Path, bytes: &[u8], ctx: &PushCtx<'_>) -> Result<(), String> {
    write_synced_file_atomic_with(target, bytes, ctx, || {})
}

fn write_synced_file_atomic_with<F>(
    target: &Path,
    bytes: &[u8],
    ctx: &PushCtx<'_>,
    before_commit: F,
) -> Result<(), String>
where
    F: FnOnce(),
{
    use std::io::Write as _;

    let validated = crate::fs_safety::validate_synced_path(ctx.project_root, target, true)
        .map_err(|error| format!("validate write target {}: {error}", target.display()))?;
    let parent = validated
        .parent()
        .ok_or_else(|| format!("write target has no parent: {}", validated.display()))?;
    let parent_metadata = crate::fs_safety::require_metadata_no_follow(parent)
        .map_err(|error| format!("inspect write parent {}: {error}", parent.display()))?;
    if !parent_metadata.is_dir() {
        return Err(format!(
            "write parent is not a directory: {}",
            parent.display()
        ));
    }
    let target_permissions = match crate::fs_safety::metadata_no_follow(&validated)
        .map_err(|error| format!("inspect write target {}: {error}", validated.display()))?
    {
        Some(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Some(_) => {
            return Err(format!(
                "write target is not a regular file: {}",
                validated.display()
            ));
        }
        None => None,
    };
    let guard = crate::fs_safety::guard_synced_parent_chain(ctx.project_root, &validated, true)
        .map_err(|error| format!("guard write target {}: {error}", validated.display()))?;
    guard
        .verify()
        .map_err(|error| format!("verify write parent {}: {error}", parent.display()))?;

    static WRITE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut temporary = None;
    for _ in 0..64 {
        let sequence = WRITE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".rosync-write-{}-{sequence}.tmp",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Some(permissions) = target_permissions.clone() {
                    file.set_permissions(permissions).map_err(|error| {
                        format!(
                            "set staged write permissions {}: {error}",
                            candidate.display()
                        )
                    })?;
                }
                file.write_all(bytes)
                    .and_then(|_| file.sync_all())
                    .map_err(|error| {
                        format!("write staged source {}: {error}", candidate.display())
                    })?;
                drop(file);
                temporary = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create staged source in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    let temporary = temporary
        .ok_or_else(|| format!("could not allocate staged source in {}", parent.display()))?;

    before_commit();
    if let Err(error) = guard.verify() {
        // Only clean up through the still-proven parent. If its identity
        // changed, leaving the inaccessible temp behind is safer than
        // deleting an attacker-controlled same-named external file.
        return Err(format!(
            "write parent changed before commit {}: {error}",
            parent.display()
        ));
    }
    ctx.mark_quiet(&validated);
    if let Err(error) = crate::lifecycle::replace_file_atomic(&temporary, &validated) {
        if guard.verify().is_ok() {
            let _ = std::fs::remove_file(&temporary);
        }
        return Err(format!(
            "commit staged source {}: {error}",
            validated.display()
        ));
    }
    guard.verify().map_err(|error| {
        format!(
            "write parent changed during commit {}: {error}",
            parent.display()
        )
    })?;
    let metadata = crate::fs_safety::require_metadata_no_follow(&validated)
        .map_err(|error| format!("verify written source {}: {error}", validated.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "written source is not a regular file: {}",
            validated.display()
        ));
    }
    ctx.mark_quiet(&validated);
    Ok(())
}

fn apply_source_bytes(
    target: &Path,
    bytes: &[u8],
    ctx: &PushCtx<'_>,
) -> Result<SourceWriteOutcome, String> {
    let conflicts = ctx.conflicts;
    if ctx.force_overwrite {
        write_synced_file_atomic(target, bytes, ctx)?;
        conflicts.record_sync(target, hash(bytes), fs_mtime(target));
        return Ok(SourceWriteOutcome::Applied);
    }

    let current = match crate::fs_safety::metadata_no_follow(target)
        .map_err(|error| format!("inspect source {}: {error}", target.display()))?
    {
        Some(metadata) if metadata.is_file() => Some((
            read_synced_file(ctx.project_root, target)?,
            fs_mtime(target),
        )),
        Some(_) => {
            return Err(format!("source target is not a file: {}", target.display()));
        }
        None => None,
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
            write_synced_file_atomic(target, bytes, ctx)?;
            conflicts.record_sync(target, hash(bytes), fs_mtime(target));
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
    apply_delete_target(target, ctx)
}

fn apply_delete_target(target: PathBuf, ctx: &PushCtx<'_>) -> Result<ApplyOutcome, String> {
    let Some(metadata) = crate::fs_safety::metadata_no_follow(&target)
        .map_err(|error| format!("inspect delete target {}: {error}", target.display()))?
    else {
        return Ok(ApplyOutcome::Skipped);
    };
    if metadata.is_dir() && !disk_path_is_sync_owned(&target) {
        return Ok(ApplyOutcome::Skipped);
    }
    if !ctx.force_overwrite
        && !path_tree_matches_baselines(&target, ctx.conflicts, ctx.project_root)?
    {
        let is_dir = metadata.is_dir();
        let local = if is_dir {
            format!("[directory retained on disk: {}]", target.display()).into_bytes()
        } else {
            read_synced_file(ctx.project_root, &target)?
        };
        ctx.conflicts
            .park_studio_delete(&target, local, fs_mtime(&target), is_dir);
        return Ok(ApplyOutcome::Conflict(target));
    }
    remove_synced_subtree(&target, ctx, None)?;
    ctx.conflicts.forget_path(&target);
    Ok(ApplyOutcome::Applied(1))
}

fn path_tree_matches_baselines(
    path: &Path,
    conflicts: &crate::conflict::ConflictEngine,
    project_root: &Path,
) -> Result<bool, String> {
    let Some(fence) = capture_synced_subtree(project_root, path)? else {
        return Ok(false);
    };
    for entry in &fence.entries {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        let Some(name) = entry.path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if classify_script_file(name).is_none() && !is_init_file(name) {
            continue;
        }
        let bytes = read_synced_file(project_root, &entry.path)?;
        if !conflicts.matches_baseline(&entry.path, &normalize_line_endings(&bytes)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A representation transition removes its old physical root wholesale.
/// Unlike an ordinary synced-tree conflict check, every descendant must be a
/// source Ro Sync owns; otherwise an unrelated sidecar file or empty folder
/// would be lost when the old directory is removed.
fn transition_tree_matches_baselines(
    path: &Path,
    conflicts: &crate::conflict::ConflictEngine,
    project_root: &Path,
) -> Result<bool, String> {
    let Some(fence) = capture_synced_subtree(project_root, path)? else {
        return Ok(false);
    };
    let root_is_file = fence
        .entries
        .first()
        .is_some_and(|entry| entry.kind == crate::fs_safety::SafeEntryKind::File);
    for (index, entry) in fence.entries.iter().enumerate() {
        match entry.kind {
            crate::fs_safety::SafeEntryKind::Directory => {
                if index != 0 && !disk_path_is_sync_owned(&entry.path) {
                    return Ok(false);
                }
            }
            crate::fs_safety::SafeEntryKind::File => {
                let Some(name) = entry.path.file_name().and_then(|value| value.to_str()) else {
                    return Ok(false);
                };
                if classify_script_file(name).is_none() && !is_init_file(name) {
                    return Ok(false);
                }
                let bytes = read_synced_file(project_root, &entry.path)?;
                if !conflicts.matches_baseline(&entry.path, &normalize_line_endings(&bytes)) {
                    return Ok(false);
                }
            }
        }
    }
    if root_is_file {
        let root = &fence.entries[0].path;
        let Some(name) = root.file_name().and_then(|value| value.to_str()) else {
            return Ok(false);
        };
        if classify_script_file(name).is_none() && !is_init_file(name) {
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
    let Some(target) = resolve_segments_to_path(root, segs)? else {
        return Ok(ApplyOutcome::Skipped);
    };
    apply_update_target(target, properties, ctx)
}

fn apply_update_target(
    target: PathBuf,
    properties: Option<Value>,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    let Some(metadata) = crate::fs_safety::metadata_no_follow(&target)
        .map_err(|error| format!("inspect update target {}: {error}", target.display()))?
    else {
        return Ok(ApplyOutcome::Skipped);
    };
    let Some(props) = properties.and_then(|v| v.as_object().cloned()) else {
        return Ok(ApplyOutcome::Skipped);
    };

    // Script leaf: properties.Source replaces file contents.
    if metadata.is_file() {
        if let Some(source) = props.get("Source").and_then(|v| v.as_str()) {
            let raw_bytes = source.as_bytes().to_vec();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            return match apply_source_bytes(&target, &bytes, ctx)? {
                SourceWriteOutcome::Applied => Ok(ApplyOutcome::Applied(1)),
                SourceWriteOutcome::Skipped => Ok(ApplyOutcome::Skipped),
                SourceWriteOutcome::Conflict(path) => Ok(ApplyOutcome::Conflict(path)),
            };
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
    apply_rename_target(target, new_name, None, ctx)
}

fn apply_rename_target(
    target: PathBuf,
    new_name: &str,
    exact_destination: Option<PathBuf>,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    if crate::fs_safety::metadata_no_follow(&target)
        .map_err(|error| format!("inspect rename source {}: {error}", target.display()))?
        .is_none()
    {
        return Ok(0);
    }
    let parent_dir = target
        .parent()
        .ok_or_else(|| format!("rename: no parent for {}", target.display()))?
        .to_path_buf();

    let inst = path_to_instance_meta(&target)
        .map_err(|error| format!("rename: inspect {}: {error}", target.display()))?
        .ok_or_else(|| {
            format!(
                "rename: source is not a synced instance: {}",
                target.display()
            )
        })?;
    let class = inst.class;
    let has_children = inst.is_dir;
    let script_with_children = inst.is_script_with_children;
    let current_frag = target
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let new_path = if let Some(destination) = exact_destination {
        let destination_parent = destination
            .parent()
            .ok_or_else(|| format!("rename: no parent for {}", destination.display()))?;
        if !paths_refer_to_same_entry(&parent_dir, destination_parent) {
            return Err(format!(
                "rename: destination must stay in the source directory: {}",
                destination.display()
            ));
        }
        let fragment = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                format!(
                    "rename: non-UTF-8 destination fragment {}",
                    destination.display()
                )
            })?;
        if !disk_fragment_matches_identity(fragment, new_name, &class, has_children) {
            return Err(format!(
                "rename: destination fragment {fragment:?} does not match renamed instance identity"
            ));
        }
        destination
    } else {
        let taken = siblings_except(&parent_dir, current_frag.as_deref())?;
        let new_frag = instance_to_path(
            &InstanceDescriptor {
                class: &class,
                name: new_name,
                has_children,
            },
            &taken,
        );
        parent_dir.join(&new_frag.fragment)
    };
    if crate::fs_safety::metadata_no_follow(&new_path)
        .map_err(|error| format!("inspect rename destination {}: {error}", new_path.display()))?
        .is_some()
        && !paths_refer_to_same_entry(&target, &new_path)
    {
        return Err(format!(
            "rename: destination already exists: {}",
            new_path.display()
        ));
    }
    rename_path_and_init(&target, &new_path, new_name, script_with_children, ctx)?;
    // The source bytes did not change, but conflict baselines are keyed by
    // filesystem path. Leaving them under the old name makes the next clean
    // Studio edit/delete look like an unknown post-restart divergence. Rebase
    // only after the outer + named-init rename has completed successfully.
    ctx.conflicts.forget_path(&target);
    let renamed_metadata = crate::fs_safety::require_metadata_no_follow(&new_path)
        .map_err(|error| format!("inspect renamed path {}: {error}", new_path.display()))?;
    if renamed_metadata.is_dir() {
        seed_script_baselines_in_dir(ctx.project_root, &new_path, ctx.conflicts)?;
    } else if classify_script_file(
        new_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default(),
    )
    .is_some()
    {
        let bytes = read_synced_file(ctx.project_root, &new_path)?;
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
    let index = crate::fs_safety::PortableDirectoryIndex::read(dir)
        .map_err(|error| format!("scan rename source {}: {error}", dir.display()))?;
    let mut named = Vec::new();
    for entry in index.entries() {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        if let Some((class, _)) = parse_init_file(&entry.fragment) {
            named.push((std::ffi::OsString::from(&entry.fragment), class));
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
    let new_name = portable_init_file_name(new_instance_name, class);
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
        match crate::fs_safety::metadata_no_follow(&candidate) {
            Ok(None) => return Ok(candidate),
            Ok(Some(_)) => {}
            Err(error) => {
                return Err(format!(
                    "rename: inspect temporary init path {}: {error}",
                    candidate.display()
                ));
            }
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
    let source_fence = capture_synced_subtree(ctx.project_root, target)?
        .ok_or_else(|| format!("rename source disappeared: {}", target.display()))?;
    let source_parent = target
        .parent()
        .ok_or_else(|| format!("rename source has no parent: {}", target.display()))?;
    let destination_parent = new_path
        .parent()
        .ok_or_else(|| format!("rename destination has no parent: {}", new_path.display()))?;
    let source_parent_guard =
        crate::fs_safety::guard_synced_directory_chain(ctx.project_root, source_parent).map_err(
            |error| {
                format!(
                    "guard rename source parent {}: {error}",
                    source_parent.display()
                )
            },
        )?;
    let destination_parent_guard =
        crate::fs_safety::guard_synced_directory_chain(ctx.project_root, destination_parent)
            .map_err(|error| {
                format!(
                    "guard rename destination parent {}: {error}",
                    destination_parent.display()
                )
            })?;
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
    source_parent_guard.verify().map_err(|error| {
        format!(
            "rename source parent changed before commit {}: {error}",
            source_parent.display()
        )
    })?;
    destination_parent_guard.verify().map_err(|error| {
        format!(
            "rename destination parent changed before commit {}: {error}",
            destination_parent.display()
        )
    })?;
    rename(target, new_path).map_err(|error| {
        format!(
            "rename {} → {}: {error}",
            target.display(),
            new_path.display()
        )
    })?;
    source_parent_guard.verify().map_err(|error| {
        format!(
            "rename source parent changed during commit {}: {error}",
            source_parent.display()
        )
    })?;
    destination_parent_guard.verify().map_err(|error| {
        format!(
            "rename destination parent changed during commit {}: {error}",
            destination_parent.display()
        )
    })?;
    let moved_fence = capture_synced_subtree(ctx.project_root, new_path)?
        .ok_or_else(|| format!("renamed path disappeared: {}", new_path.display()))?;
    if !relocated_subtree_matches(&source_fence, &moved_fence) {
        return Err(format!(
            "renamed subtree changed during commit: {}",
            new_path.display()
        ));
    }

    let Some(init_plan) = init_plan else {
        return Ok(());
    };
    let old_init = new_path.join(&init_plan.old_name);
    let new_init = new_path.join(&init_plan.new_name);
    let temp_init = new_path.join(temp_name.expect("init plan allocates a temporary name"));
    ctx.mark_quiet(&old_init);
    ctx.mark_quiet(&new_init);
    ctx.mark_quiet(&temp_init);
    let moved_directory_guard =
        crate::fs_safety::guard_synced_directory_chain(ctx.project_root, new_path)
            .map_err(|error| format!("guard renamed directory {}: {error}", new_path.display()))?;
    moved_directory_guard.verify().map_err(|error| {
        format!(
            "renamed directory changed before init update {}: {error}",
            new_path.display()
        )
    })?;

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
    if crate::fs_safety::metadata_no_follow(&new_init)
        .map_err(|error| format!("inspect init destination {}: {error}", new_init.display()))?
        .is_some()
    {
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
    moved_directory_guard.verify().map_err(|error| {
        format!(
            "renamed directory changed during init update {}: {error}",
            new_path.display()
        )
    })?;
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
    ensure_synced_directory_chain(ctx.project_root, &parent_dir)?;
    let inst = path_to_instance_meta(&src)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("move: unsupported source {}", src.display()))?;
    let taken = siblings_except(&parent_dir, None)?;
    let fragment = instance_to_path(
        &InstanceDescriptor {
            class: &inst.class,
            name: new_name,
            has_children: inst.is_dir,
        },
        &taken,
    );
    apply_move_target(src, Some(parent_dir.join(fragment.fragment)), new_name, ctx)
}

fn apply_move_target(
    src: PathBuf,
    exact_destination: Option<PathBuf>,
    new_name: &str,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    if crate::fs_safety::metadata_no_follow(&src)
        .map_err(|error| format!("inspect move source {}: {error}", src.display()))?
        .is_none()
    {
        return Ok(0);
    }
    let inst = path_to_instance_meta(&src)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("move: unsupported source {}", src.display()))?;
    let class = inst.class;
    let has_children = inst.is_dir;
    let script_with_children = inst.is_script_with_children;
    if let Some(destination) = exact_destination.as_ref() {
        let fragment = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("move: non-UTF-8 destination {}", destination.display()))?;
        if !disk_fragment_matches_identity(fragment, new_name, &class, has_children) {
            return Err(format!(
                "move: destination fragment {fragment:?} does not match moved instance identity"
            ));
        }
    }
    let parent_dir = exact_destination
        .as_ref()
        .and_then(|destination| destination.parent())
        .map(Path::to_path_buf)
        .or_else(|| src.parent().map(Path::to_path_buf))
        .ok_or_else(|| format!("move: no destination parent for {}", src.display()))?;
    ensure_synced_directory_chain(ctx.project_root, &parent_dir)?;
    let taken = siblings_except(&parent_dir, None)?;
    let dest = exact_destination.unwrap_or_else(|| {
        let frag = instance_to_path(
            &InstanceDescriptor {
                class: &class,
                name: new_name,
                has_children,
            },
            &taken,
        );
        parent_dir.join(frag.fragment)
    });
    if crate::fs_safety::metadata_no_follow(&dest)
        .map_err(|error| format!("inspect move destination {}: {error}", dest.display()))?
        .is_some()
        && !paths_refer_to_same_entry(&dest, &src)
    {
        return Err(format!(
            "move: destination already exists: {}",
            dest.display()
        ));
    }
    rename_path_and_init(&src, &dest, new_name, script_with_children, ctx)?;
    ctx.mark_quiet(&src);
    ctx.mark_quiet(&dest);
    ctx.conflicts.forget_path(&src);
    let destination_metadata = crate::fs_safety::require_metadata_no_follow(&dest)
        .map_err(|error| format!("inspect moved destination {}: {error}", dest.display()))?;
    if destination_metadata.is_dir() {
        seed_script_baselines_in_dir(ctx.project_root, &dest, ctx.conflicts)?;
    } else {
        let bytes = read_synced_file(ctx.project_root, &dest)?;
        let normalized = normalize_line_endings(&bytes).into_owned();
        ctx.conflicts
            .record_sync(&dest, hash(&normalized), fs_mtime(&dest));
    }
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
        match find_child_fragment_by_lookup_segment(&lookup_dir, seg).map_err(|e| e.to_string())? {
            Some(frag) => cur = lookup_dir.join(frag),
            None => {
                // Fallback: encoded segment literally (top-level services).
                let candidate = lookup_dir.join(encode_name(seg));
                if crate::fs_safety::metadata_no_follow(&candidate)
                    .map_err(|error| format!("inspect path {}: {error}", candidate.display()))?
                    .is_some()
                {
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
        let next =
            match find_child_fragment_by_lookup_segment(&p, seg).map_err(|e| e.to_string())? {
                Some(fragment) => p.join(fragment),
                None => p.join(encode_name(seg)),
            };
        if let Some(metadata) = crate::fs_safety::metadata_no_follow(&next)
            .map_err(|error| format!("inspect parent path {}: {error}", next.display()))?
        {
            if !metadata.is_dir() {
                return Err(format!(
                    "path {} is a file, not a directory (needed as parent)",
                    next.display()
                ));
            }
        }
        p = next;
    }
    Ok(p)
}

/// Scan `dir` for a child whose instance name is `name`. Returns the fragment
/// (file/dir name) if found.
fn find_child_fragment_by_name(dir: &Path, name: &str) -> std::io::Result<Option<String>> {
    Ok(index_child_fragments_by_name(dir)?
        .remove(name)
        .map(|(fragment, _priority)| fragment))
}

/// Resolve a plugin lookup segment. Plain segments retain the legacy
/// name-based behavior; generated `<Name> [N]` segments select that exact
/// filesystem ordinal. A literal Roblox name ending in the reserved grammar is
/// encoded as `%5B` on disk and therefore remains available as an exact
/// logical-name match before the generated fallback.
fn find_child_fragment_by_lookup_segment(
    dir: &Path,
    segment: &str,
) -> std::io::Result<Option<String>> {
    let index = index_child_fragments(dir)?;
    if let Some((fragment, _)) = index.best_by_name.get(segment) {
        return Ok(Some(fragment.clone()));
    }
    let Some((base, ordinal)) = parse_disambiguated(segment) else {
        return Ok(None);
    };
    Ok(index.all_by_name.get(&base).and_then(|fragments| {
        fragments
            .iter()
            .find(|fragment| fragment_disambiguation_ordinal(fragment) == ordinal)
            .cloned()
    }))
}

struct ExistingChildFragmentIndex {
    best_by_name: HashMap<String, (String, u8)>,
    all_by_name: HashMap<String, Vec<String>>,
}

/// Index existing filesystem fragments in one directory scan. The best entry
/// preserves legacy lookup behavior, while the complete list supports
/// deterministic one-to-one assignment of duplicate logical names.
fn index_child_fragments(dir: &Path) -> std::io::Result<ExistingChildFragmentIndex> {
    let Some(metadata) = crate::fs_safety::metadata_no_follow(dir)? else {
        return Ok(ExistingChildFragmentIndex {
            best_by_name: HashMap::new(),
            all_by_name: HashMap::new(),
        });
    };
    if !metadata.is_dir() {
        return Ok(ExistingChildFragmentIndex {
            best_by_name: HashMap::new(),
            all_by_name: HashMap::new(),
        });
    }
    let mut best = HashMap::new();
    let mut all = HashMap::<String, Vec<String>>::new();
    let index = crate::fs_safety::PortableDirectoryIndex::read(dir)?;
    for entry in index.entries() {
        let fstr = entry.fragment.as_str();
        if fstr == META_FILE {
            continue;
        }
        let inst = path_to_instance_meta(&entry.path)?;
        if let Some(i) = inst {
            let priority = fragment_lookup_priority(&entry.path, &i);
            all.entry(i.name.clone())
                .or_default()
                .push(fstr.to_string());
            let candidate = best
                .entry(i.name)
                .or_insert_with(|| (fstr.to_string(), priority));
            if priority > candidate.1 {
                *candidate = (fstr.to_string(), priority);
            }
        }
    }
    for fragments in all.values_mut() {
        fragments.sort_by(|left, right| {
            fragment_disambiguation_ordinal(left)
                .cmp(&fragment_disambiguation_ordinal(right))
                .then_with(|| left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase()))
                .then_with(|| left.cmp(right))
        });
    }
    Ok(ExistingChildFragmentIndex {
        best_by_name: best,
        all_by_name: all,
    })
}

fn fragment_disambiguation_ordinal(fragment: &str) -> usize {
    parse_disambiguated(fragment)
        .or_else(|| classify_script_file(fragment).and_then(|(_, stem)| parse_disambiguated(&stem)))
        .map(|(_, ordinal)| ordinal)
        .unwrap_or(0)
}

fn index_child_fragments_by_name(dir: &Path) -> std::io::Result<HashMap<String, (String, u8)>> {
    Ok(index_child_fragments(dir)?.best_by_name)
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
    let Some(metadata) = crate::fs_safety::metadata_no_follow(dir)
        .map_err(|error| format!("inspect siblings directory {}: {error}", dir.display()))?
    else {
        return Ok(out);
    };
    if !metadata.is_dir() {
        return Err(format!(
            "siblings path is not a directory: {}",
            dir.display()
        ));
    }
    let index = crate::fs_safety::PortableDirectoryIndex::read(dir)
        .map_err(|error| format!("scan siblings directory {}: {error}", dir.display()))?;
    for entry in index.entries() {
        let s = entry.fragment.as_str();
        if Some(s) == except {
            continue;
        }
        out.push(s.to_string());
    }
    Ok(out)
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
            Some(json!({
                "op": "delete",
                "path": target_lookup_segs,
                "diskPath": segs,
                "diskFragmentIsDir": op.is_dir,
            }))
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
                    // Live watcher delivery must hydrate file renames through
                    // the stable, no-follow 32-MiB reader before this
                    // translation step. Never fall back to an unbounded
                    // path-based reread after the destructive preflight delay.
                    let source = String::from_utf8_lossy(op.content.as_deref()?).to_string();
                    return Some(json!({
                        "op": "class_change",
                        "path": from_lookup_path,
                        "to": to_naming_path,
                        "fromDiskPath": from_segs_fs,
                        "toDiskPath": segs,
                        "fromDiskFragmentIsDir": op.is_dir,
                        "toDiskFragmentIsDir": op.is_dir,
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
                "fromDiskPath": from_segs_fs,
                "toDiskPath": segs,
                "fromDiskFragmentIsDir": op.is_dir,
                "toDiskFragmentIsDir": op.is_dir,
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
                        "diskPath": parent_segs_fs,
                        "diskFragmentIsDir": true,
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
                "diskPath": segs,
                "node": {
                    "class": inst.class,
                    "name": inst.name,
                    "diskFragment": fname,
                    "diskFragmentIsDir": inst.is_dir,
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
    collect_marked_tree_paths(node, parent, out, "avoidSync", true);
}

fn collect_marked_tree_paths(
    node: &Value,
    parent: &[String],
    out: &mut Vec<Vec<String>>,
    marker: &str,
    stop_at_match: bool,
) {
    if let Some(nodes) = node.as_array() {
        for child in nodes {
            collect_marked_tree_paths(child, parent, out, marker, stop_at_match);
        }
        return;
    }

    let Some(name) = node.get("name").and_then(|v| v.as_str()) else {
        if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
            for child in children {
                collect_marked_tree_paths(child, parent, out, marker, stop_at_match);
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

    if node.get(marker).and_then(Value::as_bool) == Some(true) {
        out.push(path.clone());
        if stop_at_match {
            return;
        }
    }

    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for child in children {
            collect_marked_tree_paths(child, &path, out, marker, stop_at_match);
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
    crate::fs_safety::metadata_no_follow(path)
        .ok()
        .flatten()
        .filter(|metadata| metadata.is_file())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
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

    static AVOID_SYNC_TEST_LOCK: Mutex<()> = Mutex::new(());

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
        project_root: &'a Path,
    ) -> PushCtx<'a> {
        PushCtx {
            conflicts: engine,
            push_quiet: quiet,
            force_overwrite: false,
            strict: false,
            force_prune: false,
            project_root,
            backup_forced_removals: true,
        }
    }

    fn force_harness<'a>(
        engine: &'a ConflictEngine,
        quiet: &'a Mutex<HashMap<PathBuf, Instant>>,
        project_root: &'a Path,
    ) -> PushCtx<'a> {
        PushCtx {
            conflicts: engine,
            push_quiet: quiet,
            force_overwrite: true,
            strict: false,
            force_prune: false,
            project_root,
            backup_forced_removals: true,
        }
    }

    fn strict_force_harness<'a>(
        engine: &'a ConflictEngine,
        quiet: &'a Mutex<HashMap<PathBuf, Instant>>,
        project_root: &'a Path,
    ) -> PushCtx<'a> {
        PushCtx {
            conflicts: engine,
            push_quiet: quiet,
            force_overwrite: true,
            strict: true,
            force_prune: true,
            project_root,
            backup_forced_removals: true,
        }
    }

    fn push_quiet() -> Mutex<HashMap<PathBuf, Instant>> {
        Mutex::new(HashMap::new())
    }

    fn over_deep_studio_service(name: &str) -> Value {
        let mut descendant = json!({
            "class": "ModuleScript",
            "name": "Leaf",
            "children": [],
        });
        for index in 0..MAX_BOOTSTRAP_INSTANCE_DEPTH {
            descendant = json!({
                "class": "Folder",
                "name": format!("Layer{index:02}"),
                "children": [descendant],
            });
        }
        json!({
            "class": name,
            "name": name,
            "children": [descendant],
        })
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

    fn test_choice_details(paths: &[String]) -> Vec<InitialChoiceItem> {
        paths
            .iter()
            .enumerate()
            .map(|(index, path)| InitialChoiceItem {
                id: index as u32,
                action: InitialChoiceAction::Overwrite,
                path: path.clone(),
                kind: "script".into(),
                class: None,
                local_class: Some("ModuleScript".into()),
                studio_class: Some("ModuleScript".into()),
                class_changed: false,
                source_changed: true,
            })
            .collect()
    }

    fn test_pending_initial(choice_id: &str, paths: &[String]) -> PendingInitial {
        PendingInitial {
            choice_id: choice_id.into(),
            disk_stats: Stats::default(),
            studio_stats: Stats::default(),
            choice: None,
            details: test_choice_details(paths),
            summary: InitialChoiceSummary {
                new_files: 0,
                changed_files: paths.len(),
                removed_files: 0,
            },
            selected_disk_paths: None,
            selection: None,
        }
    }

    fn streamed_push_test_body(
        stream_id: &str,
        service: &str,
        phase: &str,
        chunk_index: u64,
        final_chunk: bool,
        records: Vec<snapshot::FlatSnapshotRecord>,
        sources: Vec<StreamSourcePart>,
    ) -> PushBody {
        PushBody {
            ops: Vec::new(),
            bootstrap: true,
            strict: true,
            force_prune: true,
            services: Vec::new(),
            plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
            stream_id: Some(stream_id.to_string()),
            service: Some(service.to_string()),
            phase: Some(phase.to_string()),
            chunk_index: Some(chunk_index),
            final_chunk,
            records,
            sources,
        }
    }

    fn streamed_service_records(
        service: &str,
        script: Option<(&str, &str)>,
    ) -> Vec<snapshot::FlatSnapshotRecord> {
        let mut records = vec![snapshot::FlatSnapshotRecord {
            id: 0,
            parent_id: None,
            child_index: 0,
            child_count: u32::from(script.is_some()),
            has_children: true,
            name: service.to_string(),
            class: service.to_string(),
            avoid_sync: false,
            avoid_sync_carrier: false,
            disk_fragment: None,
            disk_fragment_is_dir: None,
            source_included: None,
        }];
        if let Some((name, class)) = script {
            records.push(snapshot::FlatSnapshotRecord {
                id: 1,
                parent_id: Some(0),
                child_index: 0,
                child_count: 0,
                has_children: false,
                name: name.to_string(),
                class: class.to_string(),
                avoid_sync: false,
                avoid_sync_carrier: false,
                disk_fragment: None,
                disk_fragment_is_dir: None,
                source_included: None,
            });
        }
        records
    }

    fn flat_chain_records(service: &str, depth: usize) -> Vec<snapshot::FlatSnapshotRecord> {
        let mut records = Vec::with_capacity(depth + 1);
        records.push(snapshot::FlatSnapshotRecord {
            id: 0,
            parent_id: None,
            child_index: 0,
            child_count: u32::from(depth > 0),
            has_children: true,
            name: service.to_string(),
            class: service.to_string(),
            avoid_sync: false,
            avoid_sync_carrier: false,
            disk_fragment: None,
            disk_fragment_is_dir: None,
            source_included: None,
        });
        for level in 1..=depth {
            let leaf = level == depth;
            records.push(snapshot::FlatSnapshotRecord {
                id: level as u64,
                parent_id: Some((level - 1) as u64),
                child_index: 0,
                child_count: u32::from(!leaf),
                has_children: !leaf,
                name: format!("Level{level:03}"),
                class: if leaf { "ModuleScript" } else { "Folder" }.into(),
                avoid_sync: false,
                avoid_sync_carrier: false,
                disk_fragment: None,
                disk_fragment_is_dir: None,
                source_included: None,
            });
        }
        records
    }

    async fn advance_streamed_push_worker(
        state: &AppState,
        stream_id: &str,
        service: &str,
        mut response: Value,
    ) -> Value {
        for _ in 0..10_000 {
            let phase = response["phase"].as_str().unwrap_or_default().to_string();
            if phase != "diskFence" && phase != "diskRevalidate" {
                return response;
            }
            let chunk_index = response["nextChunk"].as_u64().unwrap();
            response = push(
                State(state.clone()),
                Json(streamed_push_test_body(
                    stream_id,
                    service,
                    &phase,
                    chunk_index,
                    false,
                    Vec::new(),
                    Vec::new(),
                )),
            )
            .await
            .0;
            assert_ne!(response["ok"], false, "{response}");
            if response["phase"].as_str() == Some(phase.as_str()) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        panic!("streamed push worker did not complete");
    }

    async fn advance_initial_compare_prepare(
        state: &AppState,
        studio_stats: Stats,
        compare_id: &str,
        service: &str,
        mut response: Value,
    ) -> Value {
        for _ in 0..10_000 {
            if response["phase"].as_str() != Some("diskPrepare") {
                return response;
            }
            let chunk_index = response["nextChunk"].as_u64().unwrap();
            response = initial_compare(
                State(state.clone()),
                Json(InitialCompareBody {
                    studio_stats,
                    studio_snapshot: Vec::new(),
                    compare_id: Some(compare_id.to_string()),
                    service: Some(service.to_string()),
                    plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                    phase: Some("diskPrepare".into()),
                    chunk_index: Some(chunk_index),
                    final_chunk: false,
                    records: Vec::new(),
                    hashes: Vec::new(),
                }),
            )
            .await
            .0;
            assert_ne!(response["ok"], false, "{response}");
            if response["phase"].as_str() == Some("diskPrepare") {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        panic!("initial compare diskPrepare worker did not complete");
    }

    fn snapshot_start_body(
        request_id: &str,
        strict: bool,
        choice_id: Option<&str>,
    ) -> SnapshotStreamBody {
        SnapshotStreamBody {
            plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
            request_id: request_id.to_string(),
            stream_id: None,
            phase: "start".into(),
            service: None,
            chunk_index: None,
            strict,
            avoid_sync_paths: Vec::new(),
            choice_id: choice_id.map(str::to_string),
        }
    }

    fn snapshot_cursor_body(
        request_id: &str,
        stream_id: &str,
        service: &str,
        phase: &str,
        chunk_index: u64,
    ) -> SnapshotStreamBody {
        SnapshotStreamBody {
            plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
            request_id: request_id.to_string(),
            stream_id: Some(stream_id.to_string()),
            phase: phase.to_string(),
            service: Some(service.to_string()),
            chunk_index: Some(chunk_index),
            strict: false,
            avoid_sync_paths: Vec::new(),
            choice_id: None,
        }
    }

    async fn advance_snapshot_disk_prepare(
        state: &AppState,
        request_id: &str,
        mut response: Value,
    ) -> Value {
        let stream_id = response["streamId"].as_str().unwrap().to_string();
        for _ in 0..10_000 {
            if response["phase"].as_str() != Some("diskPrepare") {
                return response;
            }
            let service = response["service"].as_str().unwrap().to_string();
            let next_chunk = response["chunkIndex"].as_u64().unwrap() + 1;
            response = snapshot_stream(
                State(state.clone()),
                Json(snapshot_cursor_body(
                    request_id,
                    &stream_id,
                    &service,
                    "diskPrepare",
                    next_chunk,
                )),
            )
            .await
            .0;
            assert_ne!(response["ok"], false, "{response}");
            if response["phase"].as_str() == Some("diskPrepare") {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        panic!("snapshot diskPrepare worker did not complete");
    }

    #[derive(Default)]
    struct DrivenSnapshotStream {
        responses: Vec<Value>,
        records: HashMap<String, Vec<snapshot::FlatSnapshotRecord>>,
        sources: HashMap<(String, u64), Vec<StreamSourcePart>>,
        deletes: HashMap<String, Vec<Vec<String>>>,
    }

    async fn drive_snapshot_stream(
        state: &AppState,
        request_id: &str,
        selective: bool,
        mut response: Value,
        replay_every_request: bool,
    ) -> DrivenSnapshotStream {
        let stream_id = response["streamId"].as_str().unwrap().to_string();
        let mut result = DrivenSnapshotStream::default();
        let mut service_index = 0usize;
        loop {
            assert_ne!(response["ok"], false, "{response}");
            assert!(
                serde_json::to_vec(&response).unwrap().len() <= STREAM_SOURCE_CHUNK_BYTES,
                "stream response exceeded 512 KiB"
            );
            let service = response["service"].as_str().unwrap().to_string();
            let phase = response["phase"].as_str().unwrap().to_string();
            let chunk_index = response["chunkIndex"].as_u64().unwrap();
            let final_chunk = response["finalChunk"].as_bool().unwrap();
            let action = response.get("action").and_then(Value::as_str);
            if let Some(action) = action {
                assert_eq!(action, "complete");
                assert_eq!(service, snapshot::SYNCED_SERVICES[7]);
                assert!(final_chunk);
                assert_eq!(phase, if selective { "deletes" } else { "sources" });
            }

            match phase.as_str() {
                "diskPrepare" => {
                    assert!(!final_chunk);
                    assert!(response.get("records").is_none());
                    assert!(response.get("sources").is_none());
                    assert!(response.get("deletes").is_none());
                }
                "structure" => {
                    let chunk: Vec<snapshot::FlatSnapshotRecord> =
                        serde_json::from_value(response["records"].clone()).unwrap();
                    result
                        .records
                        .entry(service.clone())
                        .or_default()
                        .extend(chunk);
                }
                "sources" => {
                    let parts: Vec<StreamSourcePart> =
                        serde_json::from_value(response["sources"].clone()).unwrap();
                    for part in parts {
                        result
                            .sources
                            .entry((service.clone(), part.id))
                            .or_default()
                            .push(part);
                    }
                }
                "deletes" => {
                    for delete in response["deletes"].as_array().unwrap() {
                        assert_eq!(delete["pathMode"], "generated");
                        let path = delete["path"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|part| part.as_str().unwrap().to_string())
                            .collect::<Vec<_>>();
                        result
                            .deletes
                            .entry(service.clone())
                            .or_default()
                            .push(path);
                    }
                }
                other => panic!("unexpected snapshot phase {other}"),
            }
            result.responses.push(response.clone());
            if action == Some("complete") {
                break;
            }

            let (next_service, next_phase, next_chunk) = match phase.as_str() {
                "diskPrepare" => (service.clone(), "diskPrepare", chunk_index + 1),
                "structure" if final_chunk => (service.clone(), "sources", 0),
                "structure" => (service.clone(), "structure", chunk_index + 1),
                "sources" if final_chunk && selective => (service.clone(), "deletes", 0),
                "sources" if final_chunk => {
                    service_index += 1;
                    (
                        snapshot::SYNCED_SERVICES[service_index].to_string(),
                        "diskPrepare",
                        0,
                    )
                }
                "sources" => (service.clone(), "sources", chunk_index + 1),
                "deletes" if final_chunk => {
                    service_index += 1;
                    (
                        snapshot::SYNCED_SERVICES[service_index].to_string(),
                        "diskPrepare",
                        0,
                    )
                }
                "deletes" => (service.clone(), "deletes", chunk_index + 1),
                _ => unreachable!(),
            };
            let body = snapshot_cursor_body(
                request_id,
                &stream_id,
                &next_service,
                next_phase,
                next_chunk,
            );
            response = snapshot_stream(State(state.clone()), Json(body.clone()))
                .await
                .0;
            if replay_every_request {
                let replay = snapshot_stream(State(state.clone()), Json(body)).await.0;
                assert_eq!(replay, response, "snapshot exact cursor retry diverged");
            }
            if response["phase"].as_str() == Some("diskPrepare")
                || (response["phase"].as_str() == Some("sources")
                    && response["finalChunk"] == false
                    && response["sources"].as_array().is_some_and(Vec::is_empty))
                || (response["phase"].as_str() == Some("deletes")
                    && response["finalChunk"] == false
                    && response["deletes"].as_array().is_some_and(Vec::is_empty))
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        result
    }

    fn artifact_test_app(temp: &TempDir) -> Router {
        router(test_state(temp, None))
    }

    #[tokio::test]
    async fn strict_per_service_snapshot_emits_missing_service_for_pruning() {
        let project = TempDir::new("strict-missing-service-snapshot");
        let response = snapshot(
            State(test_state(&project, None)),
            Query(SnapshotParams {
                strict: true,
                force_prune: true,
                service: Some("Workspace".to_string()),
            }),
        )
        .await
        .0;

        assert_eq!(response["bootstrap"], false);
        assert_eq!(response["service"], "Workspace");
        let services = response["services"].as_array().unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0]["name"], "Workspace");
        assert!(services[0]["children"].as_array().unwrap().is_empty());
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

    fn exact_lifecycle_close_body(state: &AppState, token: &str) -> Value {
        json!({
            "token": token,
            "reason": "test close",
            "expectedBootId": state.boot_id.as_str(),
            "expectedPid": state.process_id,
            "expectedPort": state.listen_port,
            "expectedCanonicalProject": state.canonical_project.display().to_string(),
        })
    }

    #[tokio::test]
    async fn destructive_lifecycle_endpoints_reject_missing_and_replacement_identity() {
        for endpoint in ["/manager-close", "/widget-close"] {
            let project = TempDir::new("lifecycle-close-replacement");
            let state = test_state(&project, None);
            let shutdown_rx = state.shutdown_tx.subscribe();
            let app = router(state.clone());

            let (_, _, missing) = artifact_json_request(
                &app,
                Method::POST,
                endpoint,
                json!({
                    "token": "artifact-widget-token",
                    "reason": "missing identity",
                }),
            )
            .await;
            assert_eq!(missing["ok"], false);
            assert_eq!(missing["error"], "missing exact daemon close identity");
            assert!(shutdown_rx.borrow().is_none());

            for (field, changed) in [
                ("expectedBootId", json!("replacement-boot")),
                ("expectedPid", json!(state.process_id.saturating_add(1))),
                ("expectedPort", json!(state.listen_port.saturating_add(1))),
                ("expectedCanonicalProject", json!("/replacement-project")),
            ] {
                let mut replacement = exact_lifecycle_close_body(&state, "artifact-widget-token");
                replacement[field] = changed;
                let (_, _, rejected) =
                    artifact_json_request(&app, Method::POST, endpoint, replacement).await;
                assert_eq!(rejected["ok"], false);
                assert_eq!(rejected["error"], "daemon lifecycle identity changed");
                assert!(
                    shutdown_rx.borrow().is_none(),
                    "a replacement identity must not receive the old boot's shutdown"
                );
            }
        }
    }

    #[tokio::test]
    async fn manager_close_accepts_the_exact_current_identity() {
        let project = TempDir::new("lifecycle-close-exact");
        let state = test_state(&project, None);
        let shutdown_rx = state.shutdown_tx.subscribe();
        let app = router(state.clone());
        let body = exact_lifecycle_close_body(&state, "artifact-widget-token");

        let (_, _, response) =
            artifact_json_request(&app, Method::POST, "/manager-close", body).await;

        assert_eq!(response["ok"], true);
        assert_eq!(shutdown_rx.borrow().as_deref(), Some("test close"));
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
        let project = TempDir::new("resolve-conflict-target");
        std::fs::create_dir_all(project.path().join("ServerScriptService")).unwrap();
        let expected = project.path().join("ServerScriptService/Foo.luau");
        assert_eq!(
            resolve_conflict_target(project.path(), "ServerScriptService/Foo.luau").unwrap(),
            expected
        );
        assert_eq!(
            resolve_conflict_target(project.path(), expected.to_str().unwrap()).unwrap(),
            expected
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
        let ctx = harness(&engine, &quiet, d.path());

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
        let ctx = harness(&engine, &quiet, d.path());

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
        let ctx = harness(&engine, &quiet, d.path());

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
        let ctx = harness(&engine, &quiet, d.path());

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
    fn bootstrap_long_script_directory_uses_portable_plain_init() {
        let d = TempDir::new("bootstrap-long-script-directory");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, d.path());
        let name = "A".repeat(240);
        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": name,
                "class": "Script",
                "properties": { "Source": "print('root')\n" },
                "children": [{
                    "name": "Child",
                    "class": "ModuleScript",
                    "properties": { "Source": "return true\n" },
                    "children": []
                }]
            }]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();

        let directory = d.path().join("ReplicatedStorage").join(&name);
        assert!(directory.join("init.server.luau").is_file());
        assert!(!directory
            .join(format!(
                "init ({}){}",
                encode_name(&name),
                ScriptClass::Script.suffix()
            ))
            .exists());
        let metadata = path_to_instance_meta(&directory).unwrap().unwrap();
        assert_eq!(metadata.name, name);
        assert_eq!(metadata.class, "Script");
    }

    #[test]
    fn script_with_children_rename_updates_directory_and_init_atomically() {
        let d = TempDir::new("rename-script-dir-atomic");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());
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
        let ctx = harness(&engine, &quiet, d.path());
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
        let ctx = harness(&engine, &quiet, d.path());
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
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = force_harness(&engine, &quiet, d.path());

        restore_fs_rename_transactional(&from, &to, &from, b"studio edit\n", &ctx).unwrap();

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
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = force_harness(&engine, &quiet, d.path());

        restore_fs_rename_transactional(
            &from,
            &to,
            &conflict_path,
            b"return 'studio edit'\n",
            &ctx,
        )
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
    fn keep_studio_rename_rollback_refuses_a_changed_restored_tree() {
        let project = TempDir::new("resolve-rename-raced-rollback");
        let parent = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&parent).unwrap();
        let from = parent.join("Old.luau");
        let to = parent.join("New.luau");
        std::fs::write(&from, b"retained before install\n").unwrap();
        let restored_fence = capture_synced_subtree(project.path(), &from)
            .unwrap()
            .unwrap();
        let from_guard =
            crate::fs_safety::guard_synced_directory_chain(project.path(), &parent).unwrap();
        let to_guard =
            crate::fs_safety::guard_synced_directory_chain(project.path(), &parent).unwrap();

        // Model an editor/watch write landing after Studio-source installation
        // fails but before the best-effort directory rollback begins.
        std::fs::write(&from, b"new local edit must survive\n").unwrap();
        let error = rollback_restored_rename_if_unchanged(
            project.path(),
            &from,
            &to,
            &restored_fence,
            &from_guard,
            &to_guard,
        )
        .unwrap_err();

        assert!(error.contains("restored source changed"), "{error}");
        assert_eq!(
            std::fs::read(&from).unwrap(),
            b"new local edit must survive\n"
        );
        assert!(!to.exists());
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
            content: Some(b"return {}\n".to_vec()),
            is_dir: Some(false),
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
        assert_eq!(
            plugin_op["fromDiskPath"],
            serde_json::json!(["ReplicatedStorage", "Shared", "OldName.luau"])
        );
        assert_eq!(
            plugin_op["toDiskPath"],
            serde_json::json!(["ReplicatedStorage", "Shared", "NewName.luau"])
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
        collect_tree_update_ops(d.path(), &root, &mut ops).unwrap();

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
            content: Some(b"return {}\n".to_vec()),
            is_dir: Some(false),
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
    fn fs_rename_class_change_refuses_an_unhydrated_live_reread() {
        let d = TempDir::new("fs-rename-class-unhydrated");
        let from = d.path().join("Workspace").join("Controller.server.luau");
        let to = d.path().join("Workspace").join("Controller.luau");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(&to, "return 'must not be reread'\n").unwrap();
        let op = Op {
            kind: OpKind::Rename,
            path: to,
            from: Some(from),
            content: None,
            is_dir: Some(false),
        };

        assert!(fs_op_to_plugin_op(d.path(), &op).is_none());
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
            is_dir: Some(false),
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
            is_dir: Some(true),
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
            is_dir: Some(true),
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
            is_dir: Some(false),
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).unwrap();
        assert_eq!(plugin_op["op"], "delete");
        assert_eq!(
            plugin_op["path"],
            serde_json::json!(["Workspace", "Controller [1]"])
        );
        assert_eq!(
            plugin_op["diskPath"],
            serde_json::json!(["Workspace", "Controller [1].luau"])
        );
        assert_eq!(plugin_op["diskFragmentIsDir"], false);
    }

    #[test]
    fn studio_update_lookup_ordinal_targets_exact_duplicate_file() {
        let d = TempDir::new("studio-update-duplicate-lookup");
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let first = workspace.join("Controller.luau");
        let second = workspace.join("Controller [1].luau");
        std::fs::write(&first, "return 'first'\n").unwrap();
        std::fs::write(&second, "return 'second'\n").unwrap();
        let engine = ConflictEngine::new();
        engine.record_sync(&first, hash(b"return 'first'\n"), fs_mtime(&first));
        engine.record_sync(&second, hash(b"return 'second'\n"), fs_mtime(&second));
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let outcome = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "update",
                "path": ["Workspace", "Controller [1]"],
                "properties": { "Source": "return 'updated second'\n" }
            }),
            &ctx,
        )
        .unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied(1)));
        assert_eq!(std::fs::read_to_string(first).unwrap(), "return 'first'\n");
        assert_eq!(
            std::fs::read_to_string(second).unwrap(),
            "return 'updated second'\n"
        );
    }

    #[test]
    fn exact_disk_path_disambiguates_literal_suffix_from_generated_duplicate() {
        let d = TempDir::new("exact-disk-path-literal-suffix");
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let literal = workspace.join("Thing %5B1].luau");
        let duplicate = workspace.join("Thing [1].luau");
        std::fs::write(&literal, "return 'literal'\n").unwrap();
        std::fs::write(&duplicate, "return 'duplicate'\n").unwrap();
        let engine = ConflictEngine::new();
        engine.record_sync(&literal, hash(b"return 'literal'\n"), fs_mtime(&literal));
        engine.record_sync(
            &duplicate,
            hash(b"return 'duplicate'\n"),
            fs_mtime(&duplicate),
        );
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let outcome = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "update",
                "path": ["Workspace", "Thing [1]"],
                "diskPath": ["Workspace", "Thing [1].luau"],
                "properties": { "Source": "return 'updated duplicate'\n" }
            }),
            &ctx,
        )
        .unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied(1)));
        assert_eq!(
            std::fs::read_to_string(literal).unwrap(),
            "return 'literal'\n"
        );
        assert_eq!(
            std::fs::read_to_string(duplicate).unwrap(),
            "return 'updated duplicate'\n"
        );
    }

    #[test]
    fn exact_set_transitions_leaf_script_to_script_with_children() {
        let d = TempDir::new("exact-set-leaf-to-directory");
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let old_path = workspace.join("Controller.server.luau");
        std::fs::write(&old_path, "print('old')\n").unwrap();
        let engine = ConflictEngine::new();
        engine.record_sync(&old_path, hash(b"print('old')\n"), fs_mtime(&old_path));
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let outcome = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "set",
                "path": ["Workspace"],
                "fromDiskPath": ["Workspace", "Controller.server.luau"],
                "diskPath": ["Workspace", "Controller"],
                "node": {
                    "name": "Controller",
                    "class": "Script",
                    "diskFragment": "Controller",
                    "diskFragmentIsDir": true,
                    "properties": { "Source": "print('new')\n" },
                    "children": [{
                        "name": "Settings",
                        "class": "ModuleScript",
                        "properties": { "Source": "return {}\n" },
                        "children": []
                    }]
                }
            }),
            &ctx,
        )
        .unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied(_)));
        assert!(!old_path.exists());
        let new_path = workspace.join("Controller");
        assert_eq!(
            std::fs::read_to_string(new_path.join("init (Controller).server.luau")).unwrap(),
            "print('new')\n"
        );
        assert_eq!(
            std::fs::read_to_string(new_path.join("Settings.luau")).unwrap(),
            "return {}\n"
        );
        assert!(engine.matches_baseline(
            &new_path.join("init (Controller).server.luau"),
            b"print('new')\n"
        ));
    }

    #[test]
    fn exact_set_transitions_script_with_children_to_leaf_script() {
        let d = TempDir::new("exact-set-directory-to-leaf");
        let workspace = d.path().join("Workspace");
        let old_path = workspace.join("Controller");
        std::fs::create_dir_all(&old_path).unwrap();
        let init = old_path.join("init (Controller).server.luau");
        let child = old_path.join("Settings.luau");
        std::fs::write(&init, "print('old')\n").unwrap();
        std::fs::write(&child, "return {}\n").unwrap();
        let engine = ConflictEngine::new();
        engine.record_sync(&init, hash(b"print('old')\n"), fs_mtime(&init));
        engine.record_sync(&child, hash(b"return {}\n"), fs_mtime(&child));
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let outcome = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "set",
                "path": ["Workspace"],
                "fromDiskPath": ["Workspace", "Controller"],
                "diskPath": ["Workspace", "Controller.server.luau"],
                "node": {
                    "name": "Controller",
                    "class": "Script",
                    "diskFragment": "Controller.server.luau",
                    "diskFragmentIsDir": false,
                    "properties": { "Source": "print('leaf')\n" },
                    "children": []
                }
            }),
            &ctx,
        )
        .unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied(_)));
        assert!(!old_path.exists());
        let new_path = workspace.join("Controller.server.luau");
        assert_eq!(
            std::fs::read_to_string(&new_path).unwrap(),
            "print('leaf')\n"
        );
        assert!(engine.matches_baseline(&new_path, b"print('leaf')\n"));
    }

    #[test]
    fn exact_set_transition_retains_locally_edited_source_as_conflict() {
        let d = TempDir::new("exact-set-transition-conflict");
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let old_path = workspace.join("Controller.server.luau");
        std::fs::write(&old_path, "print('agreed')\n").unwrap();
        let engine = ConflictEngine::new();
        engine.record_sync(&old_path, hash(b"print('agreed')\n"), fs_mtime(&old_path));
        std::fs::write(&old_path, "print('local edit')\n").unwrap();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let outcome = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "set",
                "path": ["Workspace"],
                "fromDiskPath": ["Workspace", "Controller.server.luau"],
                "diskPath": ["Workspace", "Controller"],
                "node": {
                    "name": "Controller",
                    "class": "Script",
                    "diskFragment": "Controller",
                    "diskFragmentIsDir": true,
                    "properties": { "Source": "print('studio')\n" },
                    "children": [{
                        "name": "Settings",
                        "class": "ModuleScript",
                        "properties": { "Source": "return true\n" },
                        "children": []
                    }]
                }
            }),
            &ctx,
        )
        .unwrap();

        assert!(matches!(outcome, ApplyOutcome::Conflict(path) if path == old_path));
        assert_eq!(
            std::fs::read_to_string(&old_path).unwrap(),
            "print('local edit')\n"
        );
        assert!(!workspace.join("Controller").exists());
        assert_eq!(engine.list().len(), 1);
    }

    #[test]
    fn exact_set_transition_never_overwrites_existing_destination() {
        let d = TempDir::new("exact-set-transition-no-overwrite");
        let workspace = d.path().join("Workspace");
        let destination = workspace.join("Controller");
        std::fs::create_dir_all(&destination).unwrap();
        let old_path = workspace.join("Controller.server.luau");
        let sentinel = destination.join("keep.txt");
        std::fs::write(&old_path, "print('old')\n").unwrap();
        std::fs::write(&sentinel, "untouched").unwrap();
        let engine = ConflictEngine::new();
        engine.record_sync(&old_path, hash(b"print('old')\n"), fs_mtime(&old_path));
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let result = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "set",
                "path": ["Workspace"],
                "fromDiskPath": ["Workspace", "Controller.server.luau"],
                "diskPath": ["Workspace", "Controller"],
                "node": {
                    "name": "Controller",
                    "class": "Script",
                    "diskFragment": "Controller",
                    "diskFragmentIsDir": true,
                    "properties": { "Source": "print('new')\n" },
                    "children": [{
                        "name": "Settings",
                        "class": "ModuleScript",
                        "properties": { "Source": "return true\n" },
                        "children": []
                    }]
                }
            }),
            &ctx,
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("transition must refuse an existing destination"),
        };

        assert!(
            error.contains("destination already exists")
                || error.contains("existing target does not match")
        );
        assert_eq!(std::fs::read_to_string(old_path).unwrap(), "print('old')\n");
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "untouched");
    }

    #[cfg(unix)]
    #[test]
    fn exact_disk_path_refuses_final_symlink_target() {
        use std::os::unix::fs::symlink;

        let d = TempDir::new("exact-disk-path-final-symlink");
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = d.path().join("outside.luau");
        std::fs::write(&outside, "return 'safe'\n").unwrap();
        symlink(&outside, workspace.join("Config.luau")).unwrap();
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let error = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "update",
                "path": ["Workspace", "Config"],
                "diskPath": ["Workspace", "Config.luau"],
                "properties": { "Source": "return 'unsafe'\n" }
            }),
            &ctx,
        )
        .err()
        .expect("symlink target must be rejected");

        assert!(error.contains("symlink"));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "return 'safe'\n");
    }

    #[cfg(unix)]
    #[test]
    fn staged_source_write_aborts_when_an_intermediate_parent_is_swapped() {
        use std::os::unix::fs::symlink;

        let project = TempDir::new("write-parent-swap");
        let outside = TempDir::new("write-parent-swap-outside");
        let service = project.path().join("ReplicatedStorage");
        let parent = service.join("Parent");
        let held_parent = service.join("ParentHeld");
        let target = parent.join("Worker.luau");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(&target, "return 'local'\n").unwrap();
        let sentinel = outside.path().join("Worker.luau");
        std::fs::write(&sentinel, "return 'external'\n").unwrap();
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = force_harness(&engine, &quiet, project.path());

        let error = write_synced_file_atomic_with(&target, b"return 'unsafe'\n", &ctx, || {
            std::fs::rename(&parent, &held_parent).unwrap();
            symlink(outside.path(), &parent).unwrap();
        })
        .unwrap_err();

        assert!(error.contains("parent changed"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "return 'external'\n"
        );
        assert_eq!(
            std::fs::read_to_string(held_parent.join("Worker.luau")).unwrap(),
            "return 'local'\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn forced_backup_rejects_linked_descendant_without_touching_external_data() {
        use std::os::unix::fs::symlink;

        let project = TempDir::new("backup-linked-descendant");
        let outside = TempDir::new("backup-linked-descendant-outside");
        let owned = project.path().join("ReplicatedStorage").join("Owned");
        std::fs::create_dir_all(&owned).unwrap();
        std::fs::write(owned.join("Local.luau"), "return 'local'\n").unwrap();
        let sentinel = outside.path().join("sentinel.txt");
        std::fs::write(&sentinel, "external").unwrap();
        symlink(outside.path(), owned.join("External")).unwrap();
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, project.path());

        let error = remove_path_for_replace(&owned, &ctx).unwrap_err();

        assert!(
            error.contains("linked") || error.contains("symbolic"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "external");
        assert!(owned.exists());
    }

    #[test]
    fn forced_backup_uses_explicit_root_when_a_nested_folder_has_a_service_name() {
        let project = TempDir::new("backup-explicit-project-root");
        let source = project
            .path()
            .join("ReplicatedStorage")
            .join("Workspace")
            .join("Worker.luau");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "return true\n").unwrap();

        let receipt = backup_forced_removal(&source, project.path()).unwrap();

        let canonical_project =
            crate::fs_safety::stable_canonical_directory(project.path()).unwrap();
        assert!(receipt
            .destination
            .starts_with(canonical_project.join(".rosync-backups")));
        assert!(receipt
            .destination
            .ends_with("ReplicatedStorage/Workspace/Worker.luau"));
        assert_eq!(
            std::fs::read_to_string(&receipt.destination).unwrap(),
            "return true\n"
        );
        assert!(!project
            .path()
            .join("ReplicatedStorage")
            .join(".rosync-backups")
            .exists());
    }

    #[test]
    fn exact_disk_paths_move_only_the_selected_duplicate() {
        let d = TempDir::new("exact-move-selected-duplicate");
        let workspace = d.path().join("Workspace");
        let destination = workspace.join("Destination");
        std::fs::create_dir_all(&destination).unwrap();
        let first = workspace.join("Foo.luau");
        let second = workspace.join("Foo [1].luau");
        std::fs::write(&first, "return 'first'\n").unwrap();
        std::fs::write(&second, "return 'second'\n").unwrap();
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let outcome = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "move",
                "from": ["Workspace", "Foo [1]"],
                "to": ["Workspace", "Destination", "Foo"],
                "fromDiskPath": ["Workspace", "Foo [1].luau"],
                "toDiskPath": ["Workspace", "Destination", "Foo.luau"]
            }),
            &ctx,
        )
        .unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied(1)));
        assert_eq!(std::fs::read_to_string(first).unwrap(), "return 'first'\n");
        assert!(!second.exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("Foo.luau")).unwrap(),
            "return 'second'\n"
        );
    }

    #[test]
    fn exact_disk_paths_rename_only_the_selected_duplicate() {
        let d = TempDir::new("exact-rename-selected-duplicate");
        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let first = workspace.join("Foo.luau");
        let second = workspace.join("Foo [1].luau");
        let renamed = workspace.join("Renamed [1].luau");
        std::fs::write(&first, "return 'first'\n").unwrap();
        std::fs::write(&second, "return 'second'\n").unwrap();
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let outcome = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "rename",
                "path": ["Workspace", "Foo [1]"],
                "name": "Renamed",
                "fromDiskPath": ["Workspace", "Foo [1].luau"],
                "toDiskPath": ["Workspace", "Renamed [1].luau"]
            }),
            &ctx,
        )
        .unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied(1)));
        assert_eq!(std::fs::read_to_string(first).unwrap(), "return 'first'\n");
        assert!(!second.exists());
        assert_eq!(
            std::fs::read_to_string(renamed).unwrap(),
            "return 'second'\n"
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
            is_dir: Some(false),
        };

        let plugin_op = fs_op_to_plugin_op(d.path(), &op).unwrap();
        assert_eq!(plugin_op["op"], "set");
        assert_eq!(
            plugin_op["path"],
            serde_json::json!(["Workspace", "Rig [1]"])
        );
        assert_eq!(plugin_op["node"]["name"], "Animate");
        assert_eq!(plugin_op["node"]["class"], "LocalScript");
        assert_eq!(
            plugin_op["diskPath"],
            serde_json::json!(["Workspace", "Rig [1]", "Animate.client.luau"])
        );
        assert_eq!(plugin_op["node"]["diskFragment"], "Animate.client.luau");
        assert_eq!(plugin_op["node"]["diskFragmentIsDir"], false);
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
            is_dir: Some(false),
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
            is_dir: Some(false),
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
            is_dir: None,
        };

        assert!(fs_op_to_plugin_op(d.path(), &op).is_none());
    }

    #[test]
    fn fs_op_to_plugin_ignores_avoid_sync_tree_paths() {
        let _guard = AVOID_SYNC_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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
            is_dir: Some(false),
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
            is_dir: None,
        };

        assert!(fs_op_to_plugin_op(d.path(), &op).is_none());
    }

    #[test]
    fn bootstrap_force_overwrites_sources_without_diffing_existing_files() {
        let d = TempDir::new("bootstrap-force-overwrite-script-dir");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = force_harness(&engine, &quiet, d.path());

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
        let ctx = strict_force_harness(&engine, &quiet, d.path());

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
        let ctx = strict_force_harness(&engine, &quiet, d.path());

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
        let ctx = harness(&engine, &quiet, d.path());

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
        let ctx = harness(&engine, &quiet, d.path());
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
        let ctx = harness(&engine, &quiet, d.path());
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
        let ctx = strict_force_harness(&engine, &quiet, d.path());

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
        let ctx = strict_force_harness(&engine, &quiet, d.path());

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
    fn bootstrap_avoid_sync_carrier_is_not_materialized_and_protects_ignored_disk_branch() {
        let d = TempDir::new("bootstrap-avoid-sync-carrier");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, d.path());

        let storage = d.path().join("ReplicatedStorage");
        let ignored = storage.join("ModelCarrier").join("Ignored");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("Keep.luau"), "return 'ignored'\n").unwrap();
        std::fs::write(
            storage.join("ModelCarrier").join("Stale.server.luau"),
            "return 'remove'\n",
        )
        .unwrap();
        std::fs::write(storage.join("Stale.luau"), "return 'remove'\n").unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [
                {
                    "name": "ModelCarrier",
                    "class": "Folder",
                    "avoidSyncCarrier": true,
                    "properties": {},
                    "children": [{
                        "name": "Ignored",
                        "class": "Folder",
                        "avoidSync": true,
                        "properties": {},
                        "children": []
                    }]
                },
                {
                    "name": "AbsentCarrier",
                    "class": "Folder",
                    "avoidSyncCarrier": true,
                    "properties": {},
                    "children": [{
                        "name": "Ignored",
                        "class": "Folder",
                        "avoidSync": true,
                        "properties": {},
                        "children": []
                    }]
                }
            ]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert_eq!(
            std::fs::read_to_string(ignored.join("Keep.luau")).unwrap(),
            "return 'ignored'\n",
            "strict Studio wins must not prune through an AvoidSync carrier"
        );
        assert!(
            !storage
                .join("ModelCarrier")
                .join("Stale.server.luau")
                .exists(),
            "strict pruning must still remove unrelated synced entries inside a carrier"
        );
        assert!(
            !storage.join("AbsentCarrier").exists(),
            "a marker-only carrier must never create an on-disk folder"
        );
        assert!(
            !storage.join("Stale.luau").exists(),
            "unrelated strict-prune behavior must remain active"
        );
    }

    #[test]
    fn bootstrap_avoid_sync_carrier_reserves_duplicate_name_before_synced_sibling() {
        let d = TempDir::new("bootstrap-avoid-sync-carrier-duplicate");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, d.path());

        let storage = d.path().join("ReplicatedStorage");
        let ignored = storage.join("Shared").join("Ignored");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("Keep.luau"), "return 'ignored'\n").unwrap();

        let service = serde_json::json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [
                {
                    "name": "Shared",
                    "class": "Folder",
                    "properties": {},
                    "children": [{
                        "name": "Live",
                        "class": "ModuleScript",
                        "properties": { "Source": "return 'studio'\n" },
                        "children": []
                    }]
                },
                {
                    "name": "Shared",
                    "class": "Folder",
                    "avoidSyncCarrier": true,
                    "properties": {},
                    "children": [{
                        "name": "Ignored",
                        "class": "Folder",
                        "avoidSync": true,
                        "properties": {},
                        "children": []
                    }]
                }
            ]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert_eq!(
            std::fs::read_to_string(ignored.join("Keep.luau")).unwrap(),
            "return 'ignored'\n"
        );
        assert_eq!(
            std::fs::read_to_string(storage.join("Shared [1]").join("Live.luau")).unwrap(),
            "return 'studio'\n",
            "the ignored carrier must reserve the bare fragment"
        );
    }

    #[test]
    fn bootstrap_avoid_sync_boundary_reserves_duplicate_name_before_synced_sibling() {
        let d = TempDir::new("bootstrap-avoid-sync-boundary-duplicate");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, d.path());

        let workspace = d.path().join("Workspace");
        let ignored = workspace.join("Shared");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("Keep.luau"), "return 'ignored'\n").unwrap();

        let service = serde_json::json!({
            "name": "Workspace",
            "class": "Workspace",
            "children": [
                {
                    "name": "Shared",
                    "class": "Folder",
                    "properties": {},
                    "children": [{
                        "name": "Live",
                        "class": "ModuleScript",
                        "properties": { "Source": "return 'studio'\n" },
                        "children": []
                    }]
                },
                {
                    "name": "Shared",
                    "class": "Folder",
                    "avoidSync": true,
                    "properties": {},
                    "children": []
                }
            ]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();
        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert_eq!(
            std::fs::read_to_string(ignored.join("Keep.luau")).unwrap(),
            "return 'ignored'\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("Shared [1]").join("Live.luau")).unwrap(),
            "return 'studio'\n",
            "the ignored boundary must reserve the bare fragment"
        );
        assert!(
            !workspace.join("Shared [2]").exists(),
            "reapplying the same snapshot must not grow duplicate ordinals"
        );
    }

    #[test]
    fn bootstrap_avoid_sync_boundary_reuses_legacy_unicode_fragment_idempotently() {
        let d = TempDir::new("bootstrap-avoid-sync-legacy-unicode");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, d.path());

        let workspace = d.path().join("Workspace");
        let ignored = workspace.join("Café");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("Keep.luau"), "return 'ignored'\n").unwrap();

        let service = serde_json::json!({
            "name": "Workspace",
            "class": "Workspace",
            "children": [{
                "name": "Café",
                "class": "Folder",
                "avoidSync": true,
                "properties": {},
                "children": []
            }, {
                "name": "Café",
                "class": "Folder",
                "properties": {},
                "children": [{
                    "name": "Live",
                    "class": "ModuleScript",
                    "properties": { "Source": "return 'studio'\n" },
                    "children": []
                }]
            }]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();
        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert_eq!(
            std::fs::read_to_string(ignored.join("Keep.luau")).unwrap(),
            "return 'ignored'\n"
        );
        let encoded_live = workspace.join(format!("{} [1]", encode_name("Café")));
        assert_eq!(
            std::fs::read_to_string(encoded_live.join("Live.luau")).unwrap(),
            "return 'studio'\n"
        );
        assert!(!workspace
            .join(format!("{} [2]", encode_name("Café")))
            .exists());
    }

    #[test]
    fn bootstrap_avoid_sync_script_boundary_protects_leaf_and_directory_shapes() {
        let d = TempDir::new("bootstrap-avoid-sync-script-shapes");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, d.path());

        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(workspace.join("Shared")).unwrap();
        std::fs::write(
            workspace.join("Shared").join("init (Shared).luau"),
            "return 'ignored directory'\n",
        )
        .unwrap();
        std::fs::write(workspace.join("Leaf.luau"), "return 'ignored leaf'\n").unwrap();

        let service = serde_json::json!({
            "name": "Workspace",
            "class": "Workspace",
            "children": [
                {
                    "name": "Shared",
                    "class": "ModuleScript",
                    "avoidSync": true,
                    "properties": {},
                    "children": []
                },
                {
                    "name": "Shared",
                    "class": "Folder",
                    "properties": {},
                    "children": [{
                        "name": "Live",
                        "class": "ModuleScript",
                        "properties": { "Source": "return 'studio directory'\n" },
                        "children": []
                    }]
                },
                {
                    "name": "Leaf",
                    "class": "ModuleScript",
                    "avoidSync": true,
                    "properties": {},
                    "children": []
                },
                {
                    "name": "Leaf",
                    "class": "ModuleScript",
                    "properties": { "Source": "return 'studio leaf'\n" },
                    "children": []
                }
            ]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();
        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.join("Shared").join("init (Shared).luau")).unwrap(),
            "return 'ignored directory'\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("Shared [1]").join("Live.luau")).unwrap(),
            "return 'studio directory'\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("Leaf.luau")).unwrap(),
            "return 'ignored leaf'\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("Leaf [1].luau")).unwrap(),
            "return 'studio leaf'\n"
        );
        assert!(!workspace.join("Shared [2]").exists());
        assert!(!workspace.join("Leaf [2].luau").exists());
    }

    #[test]
    fn bootstrap_strict_prunes_only_stale_duplicate_fragments() {
        let d = TempDir::new("bootstrap-prune-stale-duplicate");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, d.path());

        let workspace = d.path().join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("Shared.luau"), "return 0\n").unwrap();
        std::fs::write(workspace.join("Shared [1].luau"), "return 0\n").unwrap();
        std::fs::write(workspace.join("Shared [2].luau"), "return 'stale'\n").unwrap();

        let service = serde_json::json!({
            "name": "Workspace",
            "class": "Workspace",
            "children": [{
                "name": "Shared",
                "class": "ModuleScript",
                "properties": { "Source": "return 1\n" },
                "children": []
            }, {
                "name": "Shared",
                "class": "ModuleScript",
                "properties": { "Source": "return 2\n" },
                "children": []
            }]
        });

        apply_service_node(d.path(), &service, &ctx).unwrap();

        assert!(workspace.join("Shared.luau").is_file());
        assert!(workspace.join("Shared [1].luau").is_file());
        assert!(
            !workspace.join("Shared [2].luau").exists(),
            "strict pruning must use exact allocated fragments, not decoded names"
        );
    }

    #[test]
    fn bootstrap_strict_prunes_missing_nested_child_under_kept_folder() {
        let d = TempDir::new("bootstrap-nested-prune");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = strict_force_harness(&engine, &quiet, d.path());

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
        let ctx = strict_force_harness(&engine, &quiet, d.path());

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
        let _guard = AVOID_SYNC_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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
    fn compact_tree_payload_keeps_only_valid_unique_avoid_sync_roots() {
        let payload = serde_json::json!({
            "version": 2,
            "avoidSyncPaths": [
                ["Workspace", "Ignored"],
                ["ReplicatedStorage", "Generated", "Vendor"],
                ["Workspace", "Ignored"]
            ]
        });
        let paths = compact_avoid_sync_paths(&payload)
            .unwrap()
            .expect("compact payload");
        assert_eq!(
            paths,
            vec![
                vec![
                    "ReplicatedStorage".to_string(),
                    "Generated".to_string(),
                    "Vendor".to_string()
                ],
                vec!["Workspace".to_string(), "Ignored".to_string()],
            ]
        );
        assert!(
            serde_json::to_vec(&payload).unwrap().len() < 256,
            "compact payload must remain independent of total DataModel size"
        );
    }

    #[test]
    fn compact_tree_payload_rejects_unscoped_or_malformed_paths() {
        for payload in [
            serde_json::json!({ "avoidSyncPaths": [["Players", "Ignored"]] }),
            serde_json::json!({ "avoidSyncPaths": [[]] }),
            serde_json::json!({ "avoidSyncPaths": [["Workspace", ""]] }),
            serde_json::json!({ "avoidSyncPaths": ["Workspace/Ignored"] }),
        ] {
            assert!(
                compact_avoid_sync_paths(&payload).is_err(),
                "payload should be rejected: {payload}"
            );
        }
        assert_eq!(
            compact_avoid_sync_paths(&serde_json::json!([])).unwrap(),
            None,
            "legacy skeletons remain supported"
        );
    }

    #[test]
    fn bootstrap_tree_budget_accepts_tens_of_thousands_of_wide_nodes() {
        const WIDTH: usize = 25_000;
        let children = (0..WIDTH)
            .map(|index| {
                json!({
                    "class": "ModuleScript",
                    "name": format!("Module{index:05}"),
                    "children": [],
                })
            })
            .collect::<Vec<_>>();
        let services = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "children": children,
        })];

        validate_bootstrap_services(&services).unwrap();
    }

    #[test]
    fn flat_stream_depth_accepts_256_and_rejects_257() {
        validate_flat_snapshot(
            &flat_chain_records("Workspace", snapshot::MAX_FLAT_INSTANCE_DEPTH),
            "Workspace",
            false,
        )
        .unwrap();
        let error = validate_flat_snapshot(
            &flat_chain_records("Workspace", snapshot::MAX_FLAT_INSTANCE_DEPTH + 1),
            "Workspace",
            false,
        )
        .unwrap_err();
        assert!(error.contains("depth exceeds"), "{error}");
    }

    #[test]
    fn bootstrap_tree_budget_counts_nodes_without_recursive_traversal() {
        let services = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "children": [
                { "class": "Folder", "name": "One", "children": [] },
                { "class": "Folder", "name": "Two", "children": [] },
                { "class": "Folder", "name": "Three", "children": [] },
            ],
        })];

        let error = validate_bootstrap_services_with_limits(&services, 8, 3).unwrap_err();
        assert!(error.contains("more than the supported limit of 3"));
    }

    #[tokio::test]
    async fn push_rejects_over_deep_bootstrap_before_touching_disk() {
        let project = TempDir::new("bootstrap-depth-budget");
        let state = test_state(&project, None);
        let body = PushBody {
            ops: Vec::new(),
            bootstrap: true,
            strict: true,
            force_prune: true,
            services: vec![over_deep_studio_service("Workspace")],
            plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
            stream_id: None,
            service: None,
            phase: None,
            chunk_index: None,
            final_chunk: false,
            records: Vec::new(),
            sources: Vec::new(),
        };

        let response = push(State(state), Json(body)).await.0;
        assert_eq!(response["ok"], false);
        assert_eq!(response["applied"], 0);
        assert!(response["errors"][0]
            .as_str()
            .unwrap()
            .contains("tree depth exceeds"));
        assert!(!project.path().join("Workspace").exists());
    }

    #[tokio::test]
    async fn streamed_push_commits_services_atomically_and_rebases_live_sources() {
        let project = TempDir::new("streamed-push-end-to-end");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Config.luau"), "return 'old'\n").unwrap();
        std::fs::write(storage.join("Stale.luau"), "return 'stale'\n").unwrap();
        let state = test_state(&project, None);
        let mut events = state.events.subscribe();
        let stream_id = "push-end-to-end";
        let source = "return 'studio'\n";
        let source_sha = hash(source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        let mut response = Value::Null;
        for (index, service) in snapshot::SYNCED_SERVICES.iter().copied().enumerate() {
            let records = streamed_service_records(
                service,
                (index == 0).then_some(("Config", "ModuleScript")),
            );
            let structure_body = streamed_push_test_body(
                stream_id,
                service,
                "structure",
                0,
                true,
                records,
                Vec::new(),
            );
            response = push(State(state.clone()), Json(structure_body.clone()))
                .await
                .0;
            assert_eq!(response["phase"], "diskFence", "{response}");
            if index == 0 {
                let replay = push(State(state.clone()), Json(structure_body)).await.0;
                assert_eq!(replay, response);
            }
            response = advance_streamed_push_worker(&state, stream_id, service, response).await;
            assert_eq!(response["phase"], "sources", "{response}");

            let sources = if index == 0 {
                vec![StreamSourcePart {
                    id: 1,
                    part_index: 0,
                    offset: 0,
                    total_bytes: source.len() as u64,
                    data: source.into(),
                    final_part: true,
                    sha256: source_sha.clone(),
                }]
            } else {
                Vec::new()
            };
            response = push(
                State(state.clone()),
                Json(streamed_push_test_body(
                    stream_id,
                    service,
                    "sources",
                    0,
                    true,
                    Vec::new(),
                    sources,
                )),
            )
            .await
            .0;
            assert_eq!(response["phase"], "diskRevalidate", "{response}");
            response = advance_streamed_push_worker(&state, stream_id, service, response).await;
            if index + 1 < snapshot::SYNCED_SERVICES.len() {
                assert_eq!(
                    response["nextService"],
                    snapshot::SYNCED_SERVICES[index + 1]
                );
                assert_eq!(response["phase"], "structure");
            }
        }
        assert_eq!(response["action"], "complete");
        assert_eq!(
            std::fs::read_to_string(storage.join("Config.luau")).unwrap(),
            source
        );
        assert!(!storage.join("Stale.luau").exists());
        assert_eq!(response["backups"].as_array().unwrap().len(), 1);
        let backup = response["backups"][0].as_str().unwrap();
        assert!(Path::new(backup)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("stream-success-"));
        let marker =
            validate_successful_stream_backup_marker(project.path(), Path::new(backup)).unwrap();
        assert_eq!(marker.stream_id, stream_id);
        assert_eq!(marker.completed_services, snapshot::SYNCED_SERVICES.len());
        assert_eq!(
            response["committedServices"][0],
            json!({
                "service": "ReplicatedStorage",
                "created": false,
                "backup": backup,
                "recoveryAction": "restoreBackup",
            })
        );
        assert_eq!(
            response["committedServices"][1],
            json!({
                "service": "ServerScriptService",
                "created": true,
                "backup": null,
                "recoveryAction": "removeCreatedService",
            })
        );
        assert_eq!(
            response["committedServices"].as_array().unwrap().len(),
            snapshot::SYNCED_SERVICES.len()
        );
        let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "stream-push-complete");
        assert_eq!(event["backups"], response["backups"]);
        assert_eq!(event["committedServices"], response["committedServices"]);

        let quiet = push_quiet();
        let ctx = PushCtx {
            conflicts: state.conflict.as_ref(),
            push_quiet: &quiet,
            force_overwrite: false,
            strict: false,
            force_prune: false,
            project_root: project.path(),
            backup_forced_removals: true,
        };
        let followup = json!({
            "name": "Config",
            "class": "ModuleScript",
            "properties": { "Source": "return 'followup'\n" },
            "children": [],
        });
        assert!(matches!(
            apply_set_in_dir(&storage, &followup, &ctx, None).unwrap(),
            ApplyOutcome::Applied(1)
        ));
        assert_eq!(
            std::fs::read_to_string(storage.join("Config.luau")).unwrap(),
            "return 'followup'\n"
        );
    }

    #[test]
    fn streamed_commit_rechecks_the_tree_after_the_live_service_is_moved() {
        let project = TempDir::new("streamed-push-post-rename-fence");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Config.luau"), "return 'old'\n").unwrap();
        let state = test_state(&project, None);
        let initial_fingerprint =
            capture_exact_tree_fingerprint(state.canonical_project.as_path(), "ReplicatedStorage")
                .unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("1.source"), "return 'studio'\n").unwrap();
        let service = json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "Config",
                "class": "ModuleScript",
                "streamId": 1,
                "children": [],
            }],
        });
        let control = Arc::new(Mutex::new(StreamCommitControl {
            test_hook: Some(Arc::new(|point, backup_service, _, _| {
                if point == StreamCommitHookPoint::AfterBackupRename {
                    std::fs::write(
                        backup_service.join("Config.luau"),
                        "return 'user edit after rename'\n",
                    )
                    .unwrap();
                }
                Ok(())
            })),
            ..StreamCommitControl::default()
        }));

        let error = commit_streamed_service(StreamCommitInput {
            state,
            service: "ReplicatedStorage".into(),
            service_node: service,
            source_dir,
            initial_fingerprint,
            strict: true,
            force_prune: true,
            commit_control: control,
        })
        .unwrap_err();

        assert!(error.contains("live files were restored"), "{error}");
        assert_eq!(
            std::fs::read_to_string(storage.join("Config.luau")).unwrap(),
            "return 'user edit after rename'\n"
        );
        let backup_root = project.path().join(".rosync-backups");
        assert_eq!(std::fs::read_dir(backup_root).unwrap().count(), 0);
    }

    #[test]
    fn streamed_commit_rolls_back_every_post_backup_failure_seam() {
        for hook_point in [
            StreamCommitHookPoint::AfterBackupRename,
            StreamCommitHookPoint::BeforeStageInstall,
            StreamCommitHookPoint::AfterStageInstall,
        ] {
            let project = TempDir::new("streamed-push-commit-seam");
            let storage = project.path().join("ReplicatedStorage");
            std::fs::create_dir_all(&storage).unwrap();
            std::fs::write(storage.join("Config.luau"), "return 'old'\n").unwrap();
            let state = test_state(&project, None);
            let initial_fingerprint = capture_exact_tree_fingerprint(
                state.canonical_project.as_path(),
                "ReplicatedStorage",
            )
            .unwrap();
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("1.source"), "return 'studio'\n").unwrap();
            let service = json!({
                "name": "ReplicatedStorage",
                "class": "ReplicatedStorage",
                "children": [{
                    "name": "Config",
                    "class": "ModuleScript",
                    "streamId": 1,
                    "children": [],
                }],
            });
            let control = Arc::new(Mutex::new(StreamCommitControl {
                test_hook: Some(Arc::new(move |point, _, _, _| {
                    if point == hook_point {
                        return Err(format!("injected failure at {point:?}"));
                    }
                    Ok(())
                })),
                ..StreamCommitControl::default()
            }));

            let error = commit_streamed_service(StreamCommitInput {
                state,
                service: "ReplicatedStorage".into(),
                service_node: service,
                source_dir,
                initial_fingerprint,
                strict: true,
                force_prune: true,
                commit_control: control.clone(),
            })
            .unwrap_err();

            assert!(error.contains("live files were restored"), "{error}");
            assert_eq!(
                std::fs::read_to_string(storage.join("Config.luau")).unwrap(),
                "return 'old'\n",
                "failed seam: {hook_point:?}"
            );
            let backup_root = project.path().join(".rosync-backups");
            assert_eq!(
                std::fs::read_dir(backup_root).unwrap().count(),
                0,
                "failed seam: {hook_point:?}"
            );
            let control = control.lock().unwrap();
            assert!(!control.committed);
            assert!(!control.partial_failure);
            assert!(control.retained_backup.is_none());
        }
    }

    #[test]
    fn streamed_commit_cleans_empty_transaction_when_pre_rename_fails() {
        let project = TempDir::new("streamed-push-pre-rename-cleanup");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Config.luau"), "return 'old'\n").unwrap();
        let state = test_state(&project, None);
        let initial_fingerprint =
            capture_exact_tree_fingerprint(state.canonical_project.as_path(), "ReplicatedStorage")
                .unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("1.source"), "return 'studio'\n").unwrap();
        let control = Arc::new(Mutex::new(StreamCommitControl {
            test_hook: Some(Arc::new(|point, _, _, _| {
                if point == StreamCommitHookPoint::BeforeBackupRename {
                    return Err("injected pre-rename failure".into());
                }
                Ok(())
            })),
            ..StreamCommitControl::default()
        }));

        let error = commit_streamed_service(StreamCommitInput {
            state,
            service: "ReplicatedStorage".into(),
            service_node: json!({
                "name": "ReplicatedStorage",
                "class": "ReplicatedStorage",
                "children": [{
                    "name": "Config",
                    "class": "ModuleScript",
                    "streamId": 1,
                    "children": [],
                }],
            }),
            source_dir,
            initial_fingerprint,
            strict: true,
            force_prune: true,
            commit_control: control,
        })
        .unwrap_err();

        assert!(error.contains("injected pre-rename failure"), "{error}");
        assert_eq!(
            std::fs::read_to_string(storage.join("Config.luau")).unwrap(),
            "return 'old'\n"
        );
        assert_eq!(
            std::fs::read_dir(project.path().join(".rosync-backups"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn streamed_commit_surfaces_orphan_cleanup_warning_after_successful_rollback() {
        let project = TempDir::new("streamed-push-rollback-cleanup-warning");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Config.luau"), "return 'old'\n").unwrap();
        let state = test_state(&project, None);
        let initial_fingerprint =
            capture_exact_tree_fingerprint(state.canonical_project.as_path(), "ReplicatedStorage")
                .unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("1.source"), "return 'studio'\n").unwrap();
        let control = Arc::new(Mutex::new(StreamCommitControl {
            test_hook: Some(Arc::new(|point, backup_service, _, _| {
                if point == StreamCommitHookPoint::AfterBackupRename {
                    std::fs::write(
                        backup_service.join("Config.luau"),
                        "return 'edit in backup'\n",
                    )
                    .unwrap();
                    std::fs::write(
                        backup_service.parent().unwrap().join("orphan.txt"),
                        "preserve me",
                    )
                    .unwrap();
                }
                Ok(())
            })),
            ..StreamCommitControl::default()
        }));

        let error = commit_streamed_service(StreamCommitInput {
            state,
            service: "ReplicatedStorage".into(),
            service_node: json!({
                "name": "ReplicatedStorage",
                "class": "ReplicatedStorage",
                "children": [{
                    "name": "Config",
                    "class": "ModuleScript",
                    "streamId": 1,
                    "children": [],
                }],
            }),
            source_dir,
            initial_fingerprint,
            strict: true,
            force_prune: true,
            commit_control: control.clone(),
        })
        .unwrap_err();

        assert!(error.contains("live files were restored"), "{error}");
        assert!(error.contains("cleanup warning"), "{error}");
        assert!(error.contains("non-empty stream transaction"), "{error}");
        assert_eq!(
            std::fs::read_to_string(storage.join("Config.luau")).unwrap(),
            "return 'edit in backup'\n"
        );
        let transaction = std::fs::read_dir(project.path().join(".rosync-backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(transaction.join("orphan.txt")).unwrap(),
            "preserve me"
        );
        let control = control.lock().unwrap();
        assert!(!control.partial_failure);
        assert!(control.retained_backup.is_none());
    }

    #[test]
    fn streamed_commit_retains_and_audits_backup_when_rollback_is_refused() {
        let project = TempDir::new("streamed-push-partial-commit");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Config.luau"), "return 'old'\n").unwrap();
        let state = test_state(&project, None);
        let mut events = state.events.subscribe();
        let initial_fingerprint =
            capture_exact_tree_fingerprint(state.canonical_project.as_path(), "ReplicatedStorage")
                .unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("1.source"), "return 'studio'\n").unwrap();
        let service = json!({
            "name": "ReplicatedStorage",
            "class": "ReplicatedStorage",
            "children": [{
                "name": "Config",
                "class": "ModuleScript",
                "streamId": 1,
                "children": [],
            }],
        });
        let control = Arc::new(Mutex::new(StreamCommitControl {
            test_hook: Some(Arc::new(|point, _, live_service, _| {
                if point == StreamCommitHookPoint::AfterStageInstall {
                    std::fs::write(
                        live_service.join("Config.luau"),
                        "return 'concurrent edit'\n",
                    )
                    .unwrap();
                    return Err("injected failure after concurrent edit".into());
                }
                Ok(())
            })),
            ..StreamCommitControl::default()
        }));

        let error = commit_streamed_service(StreamCommitInput {
            state,
            service: "ReplicatedStorage".into(),
            service_node: service,
            source_dir,
            initial_fingerprint,
            strict: true,
            force_prune: true,
            commit_control: control.clone(),
        })
        .unwrap_err();

        assert!(
            error.contains("streamed service commit is partial"),
            "{error}"
        );
        assert!(error.contains("rollback refused or failed"), "{error}");
        assert_eq!(
            std::fs::read_to_string(storage.join("Config.luau")).unwrap(),
            "return 'concurrent edit'\n"
        );
        let retained_backup = control
            .lock()
            .unwrap()
            .retained_backup
            .clone()
            .expect("partial commit must retain its recovery transaction");
        assert_eq!(
            std::fs::read_to_string(
                retained_backup
                    .join("ReplicatedStorage")
                    .join("Config.luau")
            )
            .unwrap(),
            "return 'old'\n"
        );
        let control = control.lock().unwrap();
        assert!(!control.committed);
        assert!(control.partial_failure);
        drop(control);

        let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "stream-commit-partial");
        assert_eq!(event["service"], "ReplicatedStorage");
        assert_eq!(event["backup"], json!(retained_backup));
        assert!(event["rollbackError"]
            .as_str()
            .unwrap()
            .contains("installed service changed"));
    }

    #[tokio::test]
    async fn streamed_push_retains_partial_commit_receipt_and_backup() {
        let project = TempDir::new("streamed-push-partial-receipt");
        let state = test_state(&project, None);
        let stream_id = "partial-commit-receipt";
        let service = "ReplicatedStorage";
        let retained_backup = project.path().join(".rosync-backups").join("retained");
        std::fs::create_dir_all(&retained_backup).unwrap();
        let control = Arc::new(Mutex::new(StreamCommitControl {
            retained_backup: Some(retained_backup.clone()),
            partial_failure: true,
            ..StreamCommitControl::default()
        }));
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        send.send(Err("injected partial commit".into())).unwrap();
        let mut service_stream = new_push_service_stream(service);
        service_stream.phase = PushStreamPhase::DiskRevalidate;
        service_stream.commit_result = Some(receive);
        service_stream.commit_control = Some(control);
        let session = Arc::new(Mutex::new(PushStreamAccumulator {
            stream_id: stream_id.into(),
            strict: true,
            force_prune: true,
            next_service: 0,
            service_stream,
            applied: 0,
            backups: Vec::new(),
            committed_services: Vec::new(),
            accepted_stream_bytes: 0,
            accepted_source_bytes: 0,
            last_request_hash: None,
            last_response: None,
            last_activity: Instant::now(),
            completed_at: None,
        }));
        let project_key = state.canonical_project.as_ref().clone();
        PUSH_STREAM_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .insert(project_key.clone(), session.clone());
        let body = streamed_push_test_body(
            stream_id,
            service,
            "diskRevalidate",
            0,
            false,
            Vec::new(),
            Vec::new(),
        );

        let response = streamed_push(&state, body.clone()).0;
        assert_eq!(response["ok"], false);
        assert_eq!(response["action"], "partial");
        assert_eq!(response["recoveryRequired"], true);
        assert_eq!(response["backups"], json!([retained_backup]));
        assert!(session.lock().unwrap().completed_at.is_some());
        assert!(PUSH_STREAM_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .get(&project_key)
            .is_some_and(|current| Arc::ptr_eq(current, &session)));

        assert_eq!(
            streamed_push(&state, body).0,
            response,
            "the exact terminal request must replay its partial receipt"
        );
        PUSH_STREAM_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(&project_key);
    }

    #[tokio::test]
    async fn streamed_push_reports_prior_created_service_when_later_service_fails() {
        let project = TempDir::new("streamed-push-prior-created-partial");
        let state = test_state(&project, None);
        let mut events = state.events.subscribe();
        let stream_id = "prior-created-partial";
        let first_service = "ReplicatedStorage";

        let structure = push(
            State(state.clone()),
            Json(streamed_push_test_body(
                stream_id,
                first_service,
                "structure",
                0,
                true,
                streamed_service_records(first_service, None),
                Vec::new(),
            )),
        )
        .await
        .0;
        let sources =
            advance_streamed_push_worker(&state, stream_id, first_service, structure).await;
        assert_eq!(sources["phase"], "sources");
        let revalidate = push(
            State(state.clone()),
            Json(streamed_push_test_body(
                stream_id,
                first_service,
                "sources",
                0,
                true,
                Vec::new(),
                Vec::new(),
            )),
        )
        .await
        .0;
        let next = advance_streamed_push_worker(&state, stream_id, first_service, revalidate).await;
        assert_eq!(next["nextService"], "ServerScriptService");
        assert!(project.path().join(first_service).is_dir());

        let invalid = streamed_push_test_body(
            stream_id,
            "ServerScriptService",
            "structure",
            0,
            true,
            Vec::new(),
            Vec::new(),
        );
        let response = push(State(state.clone()), Json(invalid.clone())).await.0;
        let expected_commit = json!({
            "service": first_service,
            "created": true,
            "backup": null,
            "recoveryAction": "removeCreatedService",
        });
        assert_eq!(response["ok"], false);
        assert_eq!(response["action"], "partial");
        assert_eq!(response["failedService"], "ServerScriptService");
        assert_eq!(response["recoveryRequired"], true);
        assert_eq!(response["backups"], json!([]));
        assert_eq!(response["committedServices"], json!([expected_commit]));

        assert_eq!(
            push(State(state.clone()), Json(invalid)).await.0,
            response,
            "the exact later-service failure must replay its terminal receipt"
        );
        let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
        assert_eq!(event["type"], "stream-push-partial");
        assert_eq!(event["failedService"], "ServerScriptService");
        assert_eq!(event["committedServices"], response["committedServices"]);

        PUSH_STREAM_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(state.canonical_project.as_path());
    }

    #[test]
    fn streamed_source_totals_charge_once_and_failed_chunks_do_not_charge() {
        let digest = hash(b"abcdef")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let first = StreamSourcePart {
            id: 1,
            part_index: 0,
            offset: 0,
            total_bytes: 6,
            data: "abc".into(),
            final_part: false,
            sha256: digest.clone(),
        };
        let second = StreamSourcePart {
            id: 1,
            part_index: 1,
            offset: 3,
            total_bytes: 6,
            data: "def".into(),
            final_part: true,
            sha256: digest.clone(),
        };

        let mut service = new_push_service_stream("ReplicatedStorage");
        service.script_ids = vec![1];
        let source_dir = tempfile::tempdir().unwrap();
        let source_path = source_dir.path().join("1.source");
        service.source_dir = Some(source_dir);
        let mut session_bytes = 0;
        append_source_parts_atomically(
            &mut service,
            &mut session_bytes,
            std::slice::from_ref(&first),
            false,
        )
        .unwrap();
        assert_eq!(service.accepted_source_bytes, 6);
        assert_eq!(session_bytes, 6);
        append_source_parts_atomically(
            &mut service,
            &mut session_bytes,
            std::slice::from_ref(&second),
            true,
        )
        .unwrap();
        assert_eq!(service.accepted_source_bytes, 6);
        assert_eq!(session_bytes, 6);
        assert_eq!(std::fs::read_to_string(source_path).unwrap(), "abcdef");

        let mut retry_service = new_push_service_stream("ReplicatedStorage");
        retry_service.script_ids = vec![1];
        let retry_dir = tempfile::tempdir().unwrap();
        let retry_path = retry_dir.path().join("1.source");
        retry_service.source_dir = Some(retry_dir);
        let mut retry_session_bytes = 0;
        let mut stale = second;
        stale.part_index = 2;
        let error = append_source_parts_atomically(
            &mut retry_service,
            &mut retry_session_bytes,
            &[first.clone(), stale],
            false,
        )
        .unwrap_err();
        assert!(error.contains("stale or out of order"), "{error}");
        assert_eq!(retry_service.accepted_source_bytes, 0);
        assert_eq!(retry_session_bytes, 0);
        assert!(retry_service.receiving_source.is_none());
        assert!(!retry_path.exists());

        append_source_parts_atomically(
            &mut retry_service,
            &mut retry_session_bytes,
            &[first],
            false,
        )
        .unwrap();
        assert_eq!(retry_service.accepted_source_bytes, 6);
        assert_eq!(retry_session_bytes, 6);
        assert_eq!(std::fs::read_to_string(retry_path).unwrap(), "abc");
    }

    #[test]
    fn streamed_source_aggregate_limits_reject_before_writes() {
        let digest = hash(b"ab")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let part = StreamSourcePart {
            id: 1,
            part_index: 0,
            offset: 0,
            total_bytes: 2,
            data: "a".into(),
            final_part: false,
            sha256: digest,
        };

        let mut service = new_push_service_stream("ReplicatedStorage");
        service.script_ids = vec![1];
        service.accepted_source_bytes = MAX_STREAM_SERVICE_SOURCE_BYTES - 1;
        let source_dir = tempfile::tempdir().unwrap();
        let source_path = source_dir.path().join("1.source");
        service.source_dir = Some(source_dir);
        let mut session_bytes = 0;
        let error = append_source_parts_atomically(
            &mut service,
            &mut session_bytes,
            std::slice::from_ref(&part),
            false,
        )
        .unwrap_err();
        assert!(error.contains("service Sources exceed"), "{error}");
        assert_eq!(
            service.accepted_source_bytes,
            MAX_STREAM_SERVICE_SOURCE_BYTES - 1
        );
        assert_eq!(session_bytes, 0);
        assert!(!source_path.exists());

        let mut service = new_push_service_stream("ReplicatedStorage");
        service.script_ids = vec![1];
        let source_dir = tempfile::tempdir().unwrap();
        let source_path = source_dir.path().join("1.source");
        service.source_dir = Some(source_dir);
        let mut session_bytes = MAX_STREAM_SESSION_SOURCE_BYTES - 1;
        let error =
            append_source_parts_atomically(&mut service, &mut session_bytes, &[part], false)
                .unwrap_err();
        assert!(error.contains("session Sources exceed"), "{error}");
        assert_eq!(service.accepted_source_bytes, 0);
        assert_eq!(session_bytes, MAX_STREAM_SESSION_SOURCE_BYTES - 1);
        assert!(!source_path.exists());
    }

    #[test]
    fn successful_stream_backup_retention_never_prunes_partial_transactions() {
        let project = TempDir::new("stream-backup-retention");
        let backup_root = project.path().join(".rosync-backups");
        std::fs::create_dir_all(&backup_root).unwrap();
        let partial = backup_root.join("stream-1-1");
        std::fs::create_dir_all(partial.join("ReplicatedStorage")).unwrap();
        std::fs::write(
            partial.join("ReplicatedStorage").join("Recovery.luau"),
            "return true\n",
        )
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for index in 0..(MAX_SUCCESSFUL_STREAM_BACKUPS + 2) {
            let transaction = backup_root.join(format!(
                "stream-success-{}-{}",
                now + index as u128,
                index + 1
            ));
            std::fs::create_dir_all(transaction.join("ReplicatedStorage")).unwrap();
            std::fs::write(
                transaction.join("ReplicatedStorage").join("Config.luau"),
                format!("return {index}\n"),
            )
            .unwrap();
            write_successful_stream_backup_marker(project.path(), &transaction, "retention-test")
                .unwrap();
        }

        let warnings = prune_successful_stream_backups(project.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(partial.is_dir());
        assert_eq!(
            std::fs::read_to_string(partial.join("ReplicatedStorage").join("Recovery.luau"))
                .unwrap(),
            "return true\n"
        );
        let successful = std::fs::read_dir(&backup_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("stream-success-"))
            })
            .count();
        assert_eq!(successful, MAX_SUCCESSFUL_STREAM_BACKUPS);
        let partial_generation =
            crate::fs_safety::directory_generation_no_follow(&partial).unwrap();
        assert!(
            remove_successful_stream_backup(project.path(), &partial, &partial_generation)
                .unwrap_err()
                .contains("unclassified")
        );
    }

    #[test]
    fn successful_stream_backup_retention_rejects_lookalikes_and_unproven_markers() {
        let project = TempDir::new("stream-backup-provenance");
        let backup_root = project.path().join(".rosync-backups");
        std::fs::create_dir_all(&backup_root).unwrap();
        let lookalike = backup_root.join("stream-success-1-1-extra");
        let missing = backup_root.join("stream-success-1-2");
        let malformed = backup_root.join("stream-success-1-3");
        for transaction in [&lookalike, &missing, &malformed] {
            std::fs::create_dir_all(transaction.join("ReplicatedStorage")).unwrap();
            std::fs::write(
                transaction.join("ReplicatedStorage").join("User.luau"),
                "return true\n",
            )
            .unwrap();
        }
        std::fs::write(
            lookalike.join(SUCCESSFUL_STREAM_BACKUP_MARKER),
            br#"{"version":1,"kind":"completed-stream"}"#,
        )
        .unwrap();
        std::fs::write(
            malformed.join(SUCCESSFUL_STREAM_BACKUP_MARKER),
            br#"{"version":1,"kind":"wrong","streamId":"x","completedServices":8,"transaction":"stream-success-1-3"}"#,
        )
        .unwrap();

        let warnings = prune_successful_stream_backups(project.path());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("stream-success-1-2")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("stream-success-1-3")),
            "{warnings:?}"
        );
        for transaction in [&lookalike, &missing, &malformed] {
            assert!(transaction.join("ReplicatedStorage/User.luau").is_file());
        }
    }

    #[test]
    fn successful_stream_backup_removal_rejects_candidate_replacement_after_discovery() {
        let project = TempDir::new("stream-backup-replacement");
        let backup_root = project.path().join(".rosync-backups");
        let candidate = backup_root.join("stream-success-1-1");
        std::fs::create_dir_all(candidate.join("ReplicatedStorage")).unwrap();
        std::fs::write(
            candidate.join("ReplicatedStorage").join("Original.luau"),
            "return 'original'\n",
        )
        .unwrap();
        write_successful_stream_backup_marker(project.path(), &candidate, "original-stream")
            .unwrap();
        validate_successful_stream_backup_marker(project.path(), &candidate).unwrap();
        let discovered = crate::fs_safety::directory_generation_no_follow(&candidate).unwrap();

        let parked = backup_root.join("parked-original");
        std::fs::rename(&candidate, &parked).unwrap();
        std::fs::create_dir_all(candidate.join("ReplicatedStorage")).unwrap();
        std::fs::write(
            candidate.join("ReplicatedStorage").join("Replacement.luau"),
            "return 'replacement'\n",
        )
        .unwrap();
        write_successful_stream_backup_marker(project.path(), &candidate, "replacement-stream")
            .unwrap();

        let error =
            remove_successful_stream_backup(project.path(), &candidate, &discovered).unwrap_err();
        assert!(error.contains("replaced after discovery"), "{error}");
        assert!(candidate
            .join("ReplicatedStorage")
            .join("Replacement.luau")
            .is_file());
        assert!(parked
            .join("ReplicatedStorage")
            .join("Original.luau")
            .is_file());
    }

    #[test]
    fn empty_stream_transaction_cleanup_rejects_replacement_after_scan() {
        let project = TempDir::new("empty-stream-transaction-replacement");
        let backup_root = project.path().join(".rosync-backups");
        let transaction = backup_root.join("stream-1-1");
        let parked = backup_root.join("parked-original");
        std::fs::create_dir_all(&transaction).unwrap();

        let error =
            cleanup_empty_stream_backup_transaction_with(project.path(), &transaction, || {
                std::fs::rename(&transaction, &parked)
                    .map_err(|error| format!("park transaction: {error}"))?;
                std::fs::create_dir(&transaction)
                    .map_err(|error| format!("replace transaction: {error}"))?;
                Ok(())
            })
            .unwrap_err();
        assert!(error.contains("changed stream transaction"), "{error}");
        assert!(transaction.is_dir());
        assert!(parked.is_dir());
    }

    #[tokio::test]
    async fn streamed_push_preserves_a_disk_edit_made_after_its_initial_fence() {
        let project = TempDir::new("streamed-push-after-fence-mutation");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let config = storage.join("Config.luau");
        std::fs::write(&config, "return 'before'\n").unwrap();
        let state = test_state(&project, None);
        let stream_id = "push-after-fence-mutation";
        let structure = push(
            State(state.clone()),
            Json(streamed_push_test_body(
                stream_id,
                "ReplicatedStorage",
                "structure",
                0,
                true,
                streamed_service_records("ReplicatedStorage", Some(("Config", "ModuleScript"))),
                Vec::new(),
            )),
        )
        .await
        .0;
        let sources =
            advance_streamed_push_worker(&state, stream_id, "ReplicatedStorage", structure).await;
        assert_eq!(sources["phase"], "sources");
        std::fs::write(&config, "return 'user edit after fence'\n").unwrap();
        let studio_source = "return 'studio'\n";
        let digest = hash(studio_source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let response = push(
            State(state.clone()),
            Json(streamed_push_test_body(
                stream_id,
                "ReplicatedStorage",
                "sources",
                0,
                true,
                Vec::new(),
                vec![StreamSourcePart {
                    id: 1,
                    part_index: 0,
                    offset: 0,
                    total_bytes: studio_source.len() as u64,
                    data: studio_source.into(),
                    final_part: true,
                    sha256: digest,
                }],
            )),
        )
        .await
        .0;
        assert_eq!(response["phase"], "diskRevalidate");

        let mut failure = None;
        for chunk in 0_u64..10_000 {
            let response = push(
                State(state.clone()),
                Json(streamed_push_test_body(
                    stream_id,
                    "ReplicatedStorage",
                    "diskRevalidate",
                    chunk,
                    false,
                    Vec::new(),
                    Vec::new(),
                )),
            )
            .await
            .0;
            if response["ok"] == false {
                failure = Some(response);
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let failure = failure.expect("disk mutation should fail the streamed commit");
        assert!(failure["error"].as_str().unwrap().contains("changed"));
        assert_eq!(
            std::fs::read_to_string(config).unwrap(),
            "return 'user edit after fence'\n"
        );
    }

    #[tokio::test]
    async fn snapshot_stream_replays_exactly_bounds_responses_and_completes_all_services() {
        let project = TempDir::new("snapshot-stream-full");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let source = "\u{0001}".repeat(140_000);
        std::fs::write(storage.join("Large.luau"), source.as_bytes()).unwrap();
        let state = test_state(&project, None);
        let request_id = "snapshot-full-e2e";
        let start_body = snapshot_start_body(request_id, true, None);
        let started = snapshot_stream(State(state.clone()), Json(start_body.clone()))
            .await
            .0;
        let replay = snapshot_stream(State(state.clone()), Json(start_body))
            .await
            .0;
        assert_eq!(replay, started);
        assert_eq!(started["phase"], "diskPrepare");
        assert_eq!(started["chunkIndex"], 0);
        assert_eq!(started["finalChunk"], false);

        let driven = drive_snapshot_stream(&state, request_id, false, started, true).await;
        assert_eq!(driven.records.len(), snapshot::SYNCED_SERVICES.len());
        for service in snapshot::SYNCED_SERVICES {
            let records = driven.records.get(*service).unwrap();
            assert_eq!(records[0].name, *service);
            assert_eq!(records[0].class, *service);
        }
        let mut parts = driven
            .sources
            .values()
            .find(|parts| !parts.is_empty())
            .unwrap()
            .clone();
        parts.sort_by_key(|part| part.part_index);
        assert!(parts.len() >= 3, "escaped Source should be segmented");
        let mut rebuilt = String::new();
        for (index, part) in parts.iter().enumerate() {
            assert_eq!(part.part_index, index as u64);
            assert_eq!(part.offset, rebuilt.len() as u64);
            assert!(part.data.len() <= STREAM_SOURCE_PART_BYTES);
            assert_eq!(part.total_bytes, source.len() as u64);
            assert_eq!(part.final_part, index + 1 == parts.len());
            rebuilt.push_str(&part.data);
        }
        assert_eq!(rebuilt, source);
        let expected_hash = hash(source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(parts.iter().all(|part| part.sha256 == expected_hash));
        assert_eq!(
            driven
                .responses
                .iter()
                .filter(|response| response.get("action").is_some())
                .count(),
            1
        );
        SNAPSHOT_STREAM_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(state.canonical_project.as_path());
    }

    #[tokio::test]
    async fn snapshot_stream_preserves_a_file_changed_after_structure() {
        let project = TempDir::new("snapshot-stream-structure-mutation");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let source_path = storage.join("Config.luau");
        std::fs::write(&source_path, "return 'before'\n").unwrap();
        let state = test_state(&project, None);
        let request_id = "snapshot-structure-mutation";
        let started = snapshot_stream(
            State(state.clone()),
            Json(snapshot_start_body(request_id, true, None)),
        )
        .await
        .0;
        let mut response = advance_snapshot_disk_prepare(&state, request_id, started).await;
        let stream_id = response["streamId"].as_str().unwrap().to_string();
        while response["phase"] == "structure" && response["finalChunk"] == false {
            response = snapshot_stream(
                State(state.clone()),
                Json(snapshot_cursor_body(
                    request_id,
                    &stream_id,
                    "ReplicatedStorage",
                    "structure",
                    response["chunkIndex"].as_u64().unwrap() + 1,
                )),
            )
            .await
            .0;
        }
        assert_eq!(response["phase"], "structure");
        assert_eq!(response["finalChunk"], true);
        std::fs::write(&source_path, "return 'user edit'\n").unwrap();

        let mut chunk = 0;
        let error = loop {
            let response = snapshot_stream(
                State(state.clone()),
                Json(snapshot_cursor_body(
                    request_id,
                    &stream_id,
                    "ReplicatedStorage",
                    "sources",
                    chunk,
                )),
            )
            .await
            .0;
            if response["ok"] == false {
                break response;
            }
            assert_eq!(response["phase"], "sources");
            assert!(response["sources"].as_array().unwrap().is_empty());
            chunk += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        assert!(error["error"].as_str().unwrap().contains("changed"));
        assert_eq!(
            std::fs::read_to_string(source_path).unwrap(),
            "return 'user edit'\n"
        );
    }

    #[tokio::test]
    async fn snapshot_stream_detects_mutation_between_source_parts() {
        let project = TempDir::new("snapshot-stream-source-part-mutation");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let source_path = storage.join("Large.luau");
        std::fs::write(&source_path, "\u{0001}".repeat(140_000)).unwrap();
        let state = test_state(&project, None);
        let request_id = "snapshot-source-part-mutation";
        let started = snapshot_stream(
            State(state.clone()),
            Json(snapshot_start_body(request_id, true, None)),
        )
        .await
        .0;
        let structure = advance_snapshot_disk_prepare(&state, request_id, started).await;
        assert_eq!(structure["phase"], "structure");
        assert_eq!(structure["finalChunk"], true);
        let stream_id = structure["streamId"].as_str().unwrap().to_string();

        let mut chunk = 0;
        let part_response = loop {
            let response = snapshot_stream(
                State(state.clone()),
                Json(snapshot_cursor_body(
                    request_id,
                    &stream_id,
                    "ReplicatedStorage",
                    "sources",
                    chunk,
                )),
            )
            .await
            .0;
            assert_ne!(response["ok"], false, "{response}");
            if !response["sources"].as_array().unwrap().is_empty() {
                break response;
            }
            chunk += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        assert_eq!(part_response["finalChunk"], false);
        assert!(!part_response["sources"][0]["finalPart"].as_bool().unwrap());
        std::fs::write(&source_path, "return 'changed between parts'\n").unwrap();
        let error = snapshot_stream(
            State(state),
            Json(snapshot_cursor_body(
                request_id,
                &stream_id,
                "ReplicatedStorage",
                "sources",
                part_response["chunkIndex"].as_u64().unwrap() + 1,
            )),
        )
        .await
        .0;
        assert_eq!(error["ok"], false);
        assert!(error["error"].as_str().unwrap().contains("changed"));
        assert_eq!(
            std::fs::read_to_string(source_path).unwrap(),
            "return 'changed between parts'\n"
        );
    }

    #[tokio::test]
    async fn snapshot_stream_rejects_a_bad_cursor_without_poisoning_the_session() {
        let project = TempDir::new("snapshot-stream-bad-cursor");
        let state = test_state(&project, None);
        let request_id = "snapshot-bad-cursor";
        let started = snapshot_stream(
            State(state.clone()),
            Json(snapshot_start_body(request_id, true, None)),
        )
        .await
        .0;
        let stream_id = started["streamId"].as_str().unwrap().to_string();
        let malformed = snapshot_stream(
            State(state.clone()),
            Json(snapshot_cursor_body(
                request_id,
                &stream_id,
                "ReplicatedStorage",
                "diskPrepare",
                999,
            )),
        )
        .await
        .0;
        assert_eq!(malformed["ok"], false);
        assert!(malformed["error"].as_str().unwrap().contains("chunk 1"));
        let corrected = snapshot_stream(
            State(state.clone()),
            Json(snapshot_cursor_body(
                request_id,
                &stream_id,
                "ReplicatedStorage",
                "diskPrepare",
                1,
            )),
        )
        .await
        .0;
        assert_ne!(corrected["ok"], false, "{corrected}");
        assert!(matches!(
            corrected["phase"].as_str(),
            Some("diskPrepare" | "structure")
        ));
        SNAPSHOT_STREAM_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(state.canonical_project.as_path());
    }

    #[tokio::test]
    async fn snapshot_stream_reports_a_source_deleted_after_structure() {
        let project = TempDir::new("snapshot-stream-source-delete");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let source_path = storage.join("Gone.luau");
        std::fs::write(&source_path, "return true\n").unwrap();
        let state = test_state(&project, None);
        let request_id = "snapshot-source-delete";
        let started = snapshot_stream(
            State(state.clone()),
            Json(snapshot_start_body(request_id, true, None)),
        )
        .await
        .0;
        let structure = advance_snapshot_disk_prepare(&state, request_id, started).await;
        assert_eq!(structure["phase"], "structure");
        assert_eq!(structure["finalChunk"], true);
        let stream_id = structure["streamId"].as_str().unwrap().to_string();
        std::fs::remove_file(&source_path).unwrap();

        let mut chunk = 0;
        let error = loop {
            let response = snapshot_stream(
                State(state.clone()),
                Json(snapshot_cursor_body(
                    request_id,
                    &stream_id,
                    "ReplicatedStorage",
                    "sources",
                    chunk,
                )),
            )
            .await
            .0;
            if response["ok"] == false {
                break response;
            }
            chunk += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        assert!(
            error["error"].as_str().unwrap().contains("does not exist")
                || error["error"].as_str().unwrap().contains("changed"),
            "{error}"
        );
        assert!(!source_path.exists());
    }

    #[tokio::test]
    async fn selective_snapshot_grant_is_one_use_and_keeps_encoded_slashes_atomic() {
        let project = TempDir::new("snapshot-stream-selective-grant");
        let storage = project.path().join("ReplicatedStorage");
        let parent = storage.join("Parent");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join("init (Parent).luau"), "return 'parent'\n").unwrap();
        std::fs::write(parent.join("Child.luau"), "return 'child'\n").unwrap();
        std::fs::write(storage.join("Slash%2FName.luau"), "return 'slash'\n").unwrap();
        let state = test_state(&project, None);
        let choice_id = "selective-slash-choice";
        SELECTIVE_TRANSFER_GRANTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .insert(
                (
                    state.canonical_project.as_ref().clone(),
                    choice_id.to_string(),
                ),
                SelectiveTransferGrant {
                    paths: vec![
                        "ReplicatedStorage/Gone%2FName".into(),
                        "ReplicatedStorage/Parent/Child".into(),
                        "ReplicatedStorage/Slash%2FName".into(),
                    ],
                    created_at: Instant::now(),
                },
            );
        let request_id = "snapshot-selective-slash";
        let start_body = snapshot_start_body(request_id, false, Some(choice_id));
        let started = snapshot_stream(State(state.clone()), Json(start_body.clone()))
            .await
            .0;
        let replay = snapshot_stream(State(state.clone()), Json(start_body))
            .await
            .0;
        assert_eq!(replay, started);

        let second_start = snapshot_stream(
            State(state.clone()),
            Json(snapshot_start_body(
                "snapshot-selective-reuse",
                false,
                Some(choice_id),
            )),
        )
        .await
        .0;
        assert_eq!(second_start["ok"], false);
        assert!(second_start["error"]
            .as_str()
            .unwrap()
            .contains("stale, consumed, or unauthorized"));

        let driven = drive_snapshot_stream(&state, request_id, true, started, true).await;
        let records = driven.records.get("ReplicatedStorage").unwrap();
        let parent_record = records
            .iter()
            .find(|record| record.name == "Parent")
            .unwrap();
        let child_record = records
            .iter()
            .find(|record| record.name == "Child")
            .unwrap();
        let slash_record = records
            .iter()
            .find(|record| record.name == "Slash/Name")
            .unwrap();
        assert_eq!(parent_record.source_included, Some(false));
        assert_eq!(child_record.source_included, Some(true));
        assert_eq!(slash_record.source_included, Some(true));
        let replicated_source_ids = driven
            .sources
            .keys()
            .filter(|(service, _)| service == "ReplicatedStorage")
            .map(|(_, id)| *id)
            .collect::<HashSet<_>>();
        assert_eq!(replicated_source_ids.len(), 2);
        assert!(replicated_source_ids.contains(&child_record.id));
        assert!(replicated_source_ids.contains(&slash_record.id));
        assert!(!replicated_source_ids.contains(&parent_record.id));
        assert_eq!(
            driven.deletes["ReplicatedStorage"],
            vec![vec![
                "ReplicatedStorage".to_string(),
                "Gone%2FName".to_string(),
            ]]
        );
        assert!(!driven.deletes["ReplicatedStorage"]
            .iter()
            .flatten()
            .any(|segment| segment == "Gone" || segment == "Name"));

        let reused = snapshot_stream(
            State(state.clone()),
            Json(snapshot_start_body(
                "snapshot-selective-reuse-after-complete",
                false,
                Some(choice_id),
            )),
        )
        .await
        .0;
        assert_eq!(reused["ok"], false);
        SNAPSHOT_STREAM_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(state.canonical_project.as_path());
        SELECTIVE_TRANSFER_GRANTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(&(
                state.canonical_project.as_ref().clone(),
                choice_id.to_string(),
            ));
    }

    #[tokio::test]
    async fn initial_compare_rejects_over_deep_snapshot_before_diff_collection() {
        let project = TempDir::new("initial-compare-depth-budget");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Local.luau"), "return true\n").unwrap();
        let state = test_state(&project, None);
        let body = InitialCompareBody {
            studio_stats: Stats {
                script_count: 1,
                instance_count: (MAX_BOOTSTRAP_INSTANCE_DEPTH + 2) as u32,
            },
            studio_snapshot: vec![over_deep_studio_service("Workspace")],
            compare_id: None,
            service: None,
            plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
            phase: None,
            chunk_index: None,
            final_chunk: false,
            records: Vec::new(),
            hashes: Vec::new(),
        };

        let response = initial_compare(State(state), Json(body)).await.0;
        assert_eq!(response["ok"], false);
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("snapshot compare: Studio tree depth exceeds"));
    }

    #[tokio::test]
    async fn initial_compare_requests_sources_only_when_both_sides_have_data() {
        let project = TempDir::new("initial-compare-stats-first");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Disk.luau"), "return true\n").unwrap();
        let state = test_state(&project, None);
        let body = InitialCompareBody {
            studio_stats: Stats {
                script_count: 10_000,
                instance_count: 25_000,
            },
            studio_snapshot: Vec::new(),
            compare_id: None,
            service: None,
            plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
            phase: None,
            chunk_index: None,
            final_chunk: false,
            records: Vec::new(),
            hashes: Vec::new(),
        };

        let response = initial_compare(State(state), Json(body)).await.0;
        assert_eq!(response["action"], "compare");
        assert_eq!(response["diskStats"]["scriptCount"], 1);
        assert!(response["compareId"].as_str().is_some());
        assert_eq!(
            response["services"].as_array().unwrap().len(),
            snapshot::SYNCED_SERVICES.len()
        );
        assert_eq!(
            response["nextService"],
            Value::String(snapshot::SYNCED_SERVICES[0].to_string())
        );
        assert!(response.get("error").is_none());
    }

    #[test]
    fn initial_compare_retry_hash_is_stable_and_source_sensitive() {
        let first = json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [{
                "class": "ModuleScript",
                "name": "Config",
                "properties": { "Source": "return true\n" },
                "children": [],
            }],
        });
        let same = first.clone();
        let mut changed = first.clone();
        changed["children"][0]["properties"]["Source"] = Value::String("return false\n".into());

        assert_eq!(
            initial_compare_request_hash(&first).unwrap(),
            initial_compare_request_hash(&same).unwrap()
        );
        assert_ne!(
            initial_compare_request_hash(&first).unwrap(),
            initial_compare_request_hash(&changed).unwrap()
        );
    }

    #[test]
    fn stream_cleanup_retries_contention_then_removes_the_expired_session() {
        let project = PathBuf::from("cleanup-contention-project");
        let session = Arc::new(Mutex::new(true));
        let mut sessions = HashMap::from([(project.clone(), session.clone())]);
        let busy = session.lock().unwrap();

        assert_eq!(
            try_remove_expired_stream_session(&mut sessions, &project, &session, |expired| {
                *expired
            },),
            StreamCleanupAttempt::Retry
        );
        assert!(sessions.contains_key(&project));

        drop(busy);
        assert_eq!(
            try_remove_expired_stream_session(&mut sessions, &project, &session, |expired| {
                *expired
            },),
            StreamCleanupAttempt::Removed
        );
        assert!(!sessions.contains_key(&project));
    }

    #[test]
    fn initial_compare_prunes_expired_abandoned_and_completed_metadata() {
        let abandoned_project = TempDir::new("initial-compare-abandoned-expiry");
        let completed_project = TempDir::new("initial-compare-completed-expiry");
        let active_project = TempDir::new("initial-compare-active-expiry");
        let make_session = |started_at, completed_at, response| {
            Arc::new(Mutex::new(InitialCompareAccumulator {
                compare_id: new_choice_id(),
                disk_stats: Stats::default(),
                studio_stats: Stats::default(),
                next_service: 0,
                comparison: InitialComparison::default(),
                staged_baselines: Vec::new(),
                staged_service_generations: Vec::new(),
                service_stream: None,
                last_service: None,
                last_request_hash: None,
                last_response: response,
                pending_choice_id: None,
                accepted_stream_bytes: 0,
                started_at,
                completed_at,
            }))
        };
        let abandoned = make_session(
            Instant::now() - INITIAL_COMPARE_SESSION_TTL - Duration::from_secs(1),
            None,
            None,
        );
        let completed = make_session(
            Instant::now(),
            Some(Instant::now() - INITIAL_COMPARE_COMPLETED_TTL - Duration::from_secs(1)),
            Some(json!({
                "action": "decide",
                "comparison": { "changedFiles": ["potentially-large-metadata"] },
            })),
        );
        let active = make_session(Instant::now(), None, None);

        let sessions = INITIAL_COMPARE_ACCUMULATORS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut sessions = sessions.lock().unwrap();
        sessions.insert(abandoned_project.path().to_path_buf(), abandoned.clone());
        sessions.insert(completed_project.path().to_path_buf(), completed.clone());
        sessions.insert(active_project.path().to_path_buf(), active.clone());

        prune_initial_compare_sessions(&mut sessions);

        assert!(!sessions
            .get(abandoned_project.path())
            .is_some_and(|session| Arc::ptr_eq(session, &abandoned)));
        assert!(!sessions
            .get(completed_project.path())
            .is_some_and(|session| Arc::ptr_eq(session, &completed)));
        assert!(sessions
            .get(active_project.path())
            .is_some_and(|session| Arc::ptr_eq(session, &active)));
        sessions.remove(active_project.path());
    }

    #[tokio::test]
    async fn initial_compare_streams_services_and_replays_exact_retries() {
        let project = TempDir::new("initial-compare-service-stream");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Config.luau"), "return 'disk'\n").unwrap();
        let state = test_state(&project, None);
        let studio_stats = Stats {
            script_count: 1,
            instance_count: 1,
        };

        let started = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: None,
                service: None,
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        let compare_id = started["compareId"].as_str().unwrap().to_string();
        let mut final_response = None;

        for (index, service) in snapshot::SYNCED_SERVICES.iter().enumerate() {
            let children = if *service == "ReplicatedStorage" {
                vec![json!({
                    "class": "ModuleScript",
                    "name": "Config",
                    "properties": { "Source": "return 'studio'\n" },
                    "children": [],
                })]
            } else {
                Vec::new()
            };
            let service_node = json!({
                "class": service,
                "name": service,
                "properties": {},
                "children": children,
            });
            let request = || InitialCompareBody {
                studio_stats,
                studio_snapshot: vec![service_node.clone()],
                compare_id: Some(compare_id.clone()),
                service: Some((*service).to_string()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            };
            let response = initial_compare(State(state.clone()), Json(request()))
                .await
                .0;
            let replay = initial_compare(State(state.clone()), Json(request()))
                .await
                .0;
            assert_eq!(replay, response, "exact service retry must be idempotent");

            if index + 1 < snapshot::SYNCED_SERVICES.len() {
                assert_eq!(response["action"], "compare");
                assert_eq!(
                    response["nextService"],
                    Value::String(snapshot::SYNCED_SERVICES[index + 1].to_string())
                );
            } else {
                final_response = Some(response);
            }
        }

        let final_response = final_response.unwrap();
        assert_eq!(final_response["action"], "decide");
        assert_eq!(final_response["comparison"]["summary"]["changedFiles"], 1);
        assert_eq!(
            final_response["comparison"]["changedFiles"][0]["path"],
            "ReplicatedStorage/Config"
        );
    }

    #[tokio::test]
    async fn initial_compare_streams_flat_structure_hashes_and_compact_final_response() {
        let project = TempDir::new("initial-compare-flat-stream");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Config.luau"), "return 'disk'\r\n").unwrap();
        let state = test_state(&project, None);
        let studio_stats = Stats {
            script_count: 1,
            instance_count: 1,
        };
        let started = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: None,
                service: None,
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        let compare_id = started["compareId"].as_str().unwrap().to_string();

        let mut final_response = Value::Null;
        for service in snapshot::SYNCED_SERVICES {
            let has_script = *service == "ReplicatedStorage";
            let mut records = vec![snapshot::FlatSnapshotRecord {
                id: 0,
                parent_id: None,
                child_index: 0,
                child_count: usize::from(has_script) as u32,
                has_children: true,
                name: (*service).to_string(),
                class: (*service).to_string(),
                avoid_sync: false,
                avoid_sync_carrier: false,
                disk_fragment: None,
                disk_fragment_is_dir: None,
                source_included: None,
            }];
            if has_script {
                records.push(snapshot::FlatSnapshotRecord {
                    id: 1,
                    parent_id: Some(0),
                    child_index: 0,
                    child_count: 0,
                    has_children: false,
                    name: "Config".into(),
                    class: "ModuleScript".into(),
                    avoid_sync: false,
                    avoid_sync_carrier: false,
                    disk_fragment: None,
                    disk_fragment_is_dir: None,
                    source_included: None,
                });
            }
            let structure_body = || InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id.clone()),
                service: Some((*service).to_string()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("structure".into()),
                chunk_index: Some(0),
                final_chunk: true,
                records: records.clone(),
                hashes: Vec::new(),
            };
            let structure = initial_compare(State(state.clone()), Json(structure_body()))
                .await
                .0;
            let structure_retry = initial_compare(State(state.clone()), Json(structure_body()))
                .await
                .0;
            assert_eq!(structure_retry, structure);
            assert_eq!(structure["phase"], "diskPrepare");
            let structure = advance_initial_compare_prepare(
                &state,
                studio_stats,
                &compare_id,
                service,
                structure,
            )
            .await;
            assert_eq!(structure["phase"], "hashes");
            assert_eq!(structure["nextChunk"], 0);

            let hashes = if has_script {
                let digest = hash(b"return 'studio'\n")
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                vec![StreamSourceHash {
                    id: 1,
                    sha256: digest,
                }]
            } else {
                Vec::new()
            };
            let hash_body = || InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id.clone()),
                service: Some((*service).to_string()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("hashes".into()),
                chunk_index: Some(0),
                final_chunk: true,
                records: Vec::new(),
                hashes: hashes.clone(),
            };
            final_response = initial_compare(State(state.clone()), Json(hash_body()))
                .await
                .0;
            let retry = initial_compare(State(state.clone()), Json(hash_body()))
                .await
                .0;
            assert_eq!(retry, final_response);
        }

        assert_eq!(final_response["action"], "decide");
        assert_eq!(final_response["comparison"]["summary"]["changedFiles"], 1);
        assert!(final_response["comparison"].get("changedFiles").is_none());
        let status = initial_choice_status(State(state)).await.0;
        assert_eq!(status["detailCount"], 1);
        assert_eq!(status["comparison"]["summary"]["changedFiles"], 1);
        assert!(status["comparison"].get("changedFiles").is_none());
    }

    #[tokio::test]
    #[ignore = "manual 25k-file final-structure latency benchmark"]
    async fn benchmark_initial_compare_final_structure_with_twenty_five_thousand_files() {
        const WIDTH: usize = 25_000;
        let project = TempDir::new("initial-compare-wide-benchmark");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let mut records = Vec::with_capacity(WIDTH + 1);
        records.push(snapshot::FlatSnapshotRecord {
            id: 0,
            parent_id: None,
            child_index: 0,
            child_count: WIDTH as u32,
            has_children: true,
            name: "ReplicatedStorage".into(),
            class: "ReplicatedStorage".into(),
            avoid_sync: false,
            avoid_sync_carrier: false,
            disk_fragment: None,
            disk_fragment_is_dir: None,
            source_included: None,
        });
        for index in 0..WIDTH {
            let name = format!("Item{index:05}");
            std::fs::write(storage.join(format!("{name}.luau")), "").unwrap();
            records.push(snapshot::FlatSnapshotRecord {
                id: (index + 1) as u64,
                parent_id: Some(0),
                child_index: index as u32,
                child_count: 0,
                has_children: false,
                name,
                class: "ModuleScript".into(),
                avoid_sync: false,
                avoid_sync_carrier: false,
                disk_fragment: None,
                disk_fragment_is_dir: None,
                source_included: None,
            });
        }
        let state = test_state(&project, None);
        let studio_stats = Stats {
            script_count: WIDTH as u32,
            instance_count: WIDTH as u32,
        };
        let started = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: None,
                service: None,
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        let compare_id = started["compareId"].as_str().unwrap().to_string();
        let chunks = records
            .chunks(STREAM_STRUCTURE_CHUNK_NODES)
            .map(<[snapshot::FlatSnapshotRecord]>::to_vec)
            .collect::<Vec<_>>();
        for (chunk_index, chunk) in chunks[..chunks.len() - 1].iter().enumerate() {
            let response = initial_compare(
                State(state.clone()),
                Json(InitialCompareBody {
                    studio_stats,
                    studio_snapshot: Vec::new(),
                    compare_id: Some(compare_id.clone()),
                    service: Some("ReplicatedStorage".into()),
                    plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                    phase: Some("structure".into()),
                    chunk_index: Some(chunk_index as u64),
                    final_chunk: false,
                    records: chunk.clone(),
                    hashes: Vec::new(),
                }),
            )
            .await
            .0;
            assert_eq!(
                response["nextChunk"],
                Value::from((chunk_index + 1) as u64),
                "{response}"
            );
        }
        let began = Instant::now();
        let response = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id.clone()),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("structure".into()),
                chunk_index: Some((chunks.len() - 1) as u64),
                final_chunk: true,
                records: chunks.last().unwrap().clone(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        let elapsed = began.elapsed();
        eprintln!(
            "25k final-structure handler latency: {:.3}s",
            elapsed.as_secs_f64()
        );
        assert_eq!(response["phase"], "diskPrepare", "{response}");
        let worker_began = Instant::now();
        let response = advance_initial_compare_prepare(
            &state,
            studio_stats,
            &compare_id,
            "ReplicatedStorage",
            response,
        )
        .await;
        eprintln!(
            "25k background diskPrepare latency: {:.3}s",
            worker_began.elapsed().as_secs_f64()
        );
        assert_eq!(response["phase"], "hashes", "{response}");

        let pull_start_began = Instant::now();
        let pull_started = snapshot_stream(
            State(state.clone()),
            Json(snapshot_start_body("snapshot-wide-benchmark", true, None)),
        )
        .await
        .0;
        eprintln!(
            "25k snapshot start handler latency: {:.3}s",
            pull_start_began.elapsed().as_secs_f64()
        );
        assert_eq!(pull_started["phase"], "diskPrepare", "{pull_started}");
        let pull_worker_began = Instant::now();
        let pull_structure =
            advance_snapshot_disk_prepare(&state, "snapshot-wide-benchmark", pull_started).await;
        eprintln!(
            "25k snapshot background diskPrepare latency: {:.3}s",
            pull_worker_began.elapsed().as_secs_f64()
        );
        assert_eq!(pull_structure["phase"], "structure", "{pull_structure}");
        assert!(serde_json::to_vec(&pull_structure).unwrap().len() <= STREAM_SOURCE_CHUNK_BYTES);
        SNAPSHOT_STREAM_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(state.canonical_project.as_path());
    }

    #[tokio::test]
    async fn streamed_compare_disk_prepare_failure_restores_structure_cursor() {
        let project = TempDir::new("initial-compare-disk-prepare-cursor");
        std::fs::create_dir_all(project.path().join("ReplicatedStorage")).unwrap();
        let state = test_state(&project, None);
        let studio_stats = Stats {
            script_count: 1,
            instance_count: 1,
        };
        let started = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: None,
                service: None,
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        let compare_id = started["compareId"].as_str().unwrap().to_string();
        let records =
            streamed_service_records("ReplicatedStorage", Some(("Config", "ModuleScript")));

        let first = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id.clone()),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("structure".into()),
                chunk_index: Some(0),
                final_chunk: false,
                records: vec![records[0].clone()],
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        assert_eq!(first["nextChunk"], 1);

        let mut invalid_child = records[1].clone();
        invalid_child.parent_id = None;
        let mut response = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id.clone()),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("structure".into()),
                chunk_index: Some(1),
                final_chunk: true,
                records: vec![invalid_child],
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        assert_eq!(response["phase"], "diskPrepare", "{response}");

        for _ in 0..1_000 {
            let cursor = response["nextChunk"].as_u64().unwrap();
            response = initial_compare(
                State(state.clone()),
                Json(InitialCompareBody {
                    studio_stats,
                    studio_snapshot: Vec::new(),
                    compare_id: Some(compare_id.clone()),
                    service: Some("ReplicatedStorage".into()),
                    plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                    phase: Some("diskPrepare".into()),
                    chunk_index: Some(cursor),
                    final_chunk: false,
                    records: Vec::new(),
                    hashes: Vec::new(),
                }),
            )
            .await
            .0;
            if response["ok"] == false {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(response["ok"], false, "{response}");

        let corrected = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("structure".into()),
                chunk_index: Some(1),
                final_chunk: true,
                records: vec![records[1].clone()],
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        assert_eq!(corrected["phase"], "diskPrepare", "{corrected}");
        assert_eq!(corrected["nextChunk"], 0, "{corrected}");

        INITIAL_COMPARE_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(state.canonical_project.as_path());
    }

    #[tokio::test]
    async fn streamed_compare_rejects_hash_chunk_atomically_and_accepts_correction() {
        let project = TempDir::new("initial-compare-atomic-hashes");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("A.luau"), "return 'a'\n").unwrap();
        std::fs::write(storage.join("B.luau"), "return 'b'\n").unwrap();
        let state = test_state(&project, None);
        let studio_stats = Stats {
            script_count: 2,
            instance_count: 2,
        };
        let started = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: None,
                service: None,
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        let compare_id = started["compareId"].as_str().unwrap().to_string();
        let records = vec![
            snapshot::FlatSnapshotRecord {
                id: 0,
                parent_id: None,
                child_index: 0,
                child_count: 2,
                has_children: true,
                name: "ReplicatedStorage".into(),
                class: "ReplicatedStorage".into(),
                avoid_sync: false,
                avoid_sync_carrier: false,
                disk_fragment: None,
                disk_fragment_is_dir: None,
                source_included: None,
            },
            snapshot::FlatSnapshotRecord {
                id: 1,
                parent_id: Some(0),
                child_index: 0,
                child_count: 0,
                has_children: false,
                name: "A".into(),
                class: "ModuleScript".into(),
                avoid_sync: false,
                avoid_sync_carrier: false,
                disk_fragment: None,
                disk_fragment_is_dir: None,
                source_included: None,
            },
            snapshot::FlatSnapshotRecord {
                id: 2,
                parent_id: Some(0),
                child_index: 1,
                child_count: 0,
                has_children: false,
                name: "B".into(),
                class: "ModuleScript".into(),
                avoid_sync: false,
                avoid_sync_carrier: false,
                disk_fragment: None,
                disk_fragment_is_dir: None,
                source_included: None,
            },
        ];
        let structure = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id.clone()),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("structure".into()),
                chunk_index: Some(0),
                final_chunk: true,
                records,
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        assert_eq!(structure["phase"], "diskPrepare");
        let structure = advance_initial_compare_prepare(
            &state,
            studio_stats,
            &compare_id,
            "ReplicatedStorage",
            structure,
        )
        .await;
        assert_eq!(structure["phase"], "hashes");

        let digest_a = hash(b"return 'a'\n")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let digest_b = hash(b"return 'b'\n")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let invalid = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id.clone()),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("hashes".into()),
                chunk_index: Some(0),
                final_chunk: true,
                records: Vec::new(),
                hashes: vec![
                    StreamSourceHash {
                        id: 1,
                        sha256: digest_a.clone(),
                    },
                    StreamSourceHash {
                        id: 2,
                        sha256: "not-a-digest".into(),
                    },
                ],
            }),
        )
        .await
        .0;
        assert_eq!(invalid["ok"], false);

        let corrected = initial_compare(
            State(state),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: Some(compare_id),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: Some("hashes".into()),
                chunk_index: Some(0),
                final_chunk: true,
                records: Vec::new(),
                hashes: vec![
                    StreamSourceHash {
                        id: 1,
                        sha256: digest_a,
                    },
                    StreamSourceHash {
                        id: 2,
                        sha256: digest_b,
                    },
                ],
            }),
        )
        .await
        .0;
        assert_eq!(corrected["action"], "compare");
        assert_eq!(corrected["nextService"], "ServerScriptService");
        assert_eq!(corrected["phase"], "structure");
        assert_eq!(corrected["nextChunk"], 0);
    }

    #[tokio::test]
    async fn clean_streamed_compare_revalidates_an_earlier_service_at_final_completion() {
        let project = TempDir::new("initial-compare-final-generation-fence");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let config = storage.join("Config.luau");
        std::fs::write(&config, "return 'same'\n").unwrap();
        let state = test_state(&project, None);
        let studio_stats = Stats {
            script_count: 1,
            instance_count: 1,
        };
        let started = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: Vec::new(),
                compare_id: None,
                service: None,
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        let compare_id = started["compareId"].as_str().unwrap().to_string();
        let digest = hash(b"return 'same'\n")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut final_response = Value::Null;

        for (service_index, service) in snapshot::SYNCED_SERVICES.iter().enumerate() {
            let structure = initial_compare(
                State(state.clone()),
                Json(InitialCompareBody {
                    studio_stats,
                    studio_snapshot: Vec::new(),
                    compare_id: Some(compare_id.clone()),
                    service: Some((*service).to_string()),
                    plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                    phase: Some("structure".into()),
                    chunk_index: Some(0),
                    final_chunk: true,
                    records: streamed_service_records(
                        service,
                        (service_index == 0).then_some(("Config", "ModuleScript")),
                    ),
                    hashes: Vec::new(),
                }),
            )
            .await
            .0;
            let ready = advance_initial_compare_prepare(
                &state,
                studio_stats,
                &compare_id,
                service,
                structure,
            )
            .await;
            assert_eq!(ready["phase"], "hashes");
            final_response = initial_compare(
                State(state.clone()),
                Json(InitialCompareBody {
                    studio_stats,
                    studio_snapshot: Vec::new(),
                    compare_id: Some(compare_id.clone()),
                    service: Some((*service).to_string()),
                    plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                    phase: Some("hashes".into()),
                    chunk_index: Some(0),
                    final_chunk: true,
                    records: Vec::new(),
                    hashes: if service_index == 0 {
                        vec![StreamSourceHash {
                            id: 1,
                            sha256: digest.clone(),
                        }]
                    } else {
                        Vec::new()
                    },
                }),
            )
            .await
            .0;
            if service_index == 0 {
                std::fs::write(&config, "return 'changed after early hash'\n").unwrap();
            }
        }

        assert_eq!(final_response["ok"], false, "{final_response}");
        assert!(final_response["error"]
            .as_str()
            .unwrap()
            .contains("changed before initial comparison completed"));
        assert_eq!(
            std::fs::read_to_string(config).unwrap(),
            "return 'changed after early hash'\n"
        );
    }

    #[test]
    fn final_structure_preparation_does_not_read_or_hash_disk_sources() {
        let project = TempDir::new("initial-compare-metadata-only-structure");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        for index in 0..2_048 {
            std::fs::write(
                storage.join(format!("Module{index:04}.luau")),
                format!("return {index}\n"),
            )
            .unwrap();
        }
        let disk = snapshot::emit_flat_service(project.path(), "ReplicatedStorage").unwrap();
        let mut studio_records = disk.records;
        for record in &mut studio_records {
            record.disk_fragment = None;
            record.disk_fragment_is_dir = None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                storage.join("Module0000.luau"),
                std::fs::Permissions::from_mode(0o0),
            )
            .unwrap();
        }
        let studio = validate_flat_snapshot(&studio_records, "ReplicatedStorage", false).unwrap();
        let prepared = prepare_streamed_initial_service_comparison(project.path(), studio).unwrap();
        assert_eq!(prepared.local_source_paths_by_path.len(), 2_048);
        assert_eq!(prepared.expected_hash_ids.len(), 2_048);
        assert!(prepared
            .local_nodes
            .values()
            .filter(|node| node.kind == diff::DiffKind::Script)
            .all(|node| node.source_hash == Some(hash(b""))));
    }

    #[test]
    fn streamed_clean_finish_commits_staged_baselines_without_source_rereads() {
        let project = TempDir::new("initial-compare-staged-baselines");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let source_path = storage.join("Config.luau");
        let source = b"return 'agreed'\n";
        std::fs::write(&source_path, source).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o0)).unwrap();
        }
        let state = test_state(&project, None);
        let response = finish_initial_comparison(
            &state,
            Stats {
                script_count: 1,
                instance_count: 1,
            },
            Stats {
                script_count: 1,
                instance_count: 1,
            },
            InitialComparison::default(),
            Some(StagedComparisonState {
                baselines: vec![StagedScriptBaseline {
                    generation: crate::fs_safety::file_generation_no_follow(&source_path).unwrap(),
                    path: source_path.clone(),
                    source_hash: hash(source),
                    fs_mtime: 123,
                }],
                service_generations: vec![crate::fs_safety::capture_tree_metadata(
                    project.path(),
                    "ReplicatedStorage",
                )
                .unwrap()],
            }),
        )
        .0;
        assert_eq!(response["action"], "in-sync");
        assert!(state.conflict.matches_baseline(&source_path, source));
    }

    #[test]
    fn normalized_file_hash_handles_crlf_split_at_reader_boundary() {
        let project = TempDir::new("streaming-normalized-hash-boundary");
        let service = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&service).unwrap();
        let path = service.join("Boundary.luau");
        let mut source = vec![b'a'; 64 * 1024 - 1];
        source.extend_from_slice(b"\r\nnext\rline\r\n");
        std::fs::write(&path, &source).unwrap();
        assert_eq!(
            normalized_file_hash(project.path(), &path).unwrap(),
            hash(normalize_line_endings(&source).as_ref())
        );
    }

    #[test]
    fn flat_snapshot_validation_rejects_gapped_child_indices_and_bad_shapes() {
        let root = snapshot::FlatSnapshotRecord {
            id: 0,
            parent_id: None,
            child_index: 0,
            child_count: 1,
            has_children: true,
            name: "Workspace".into(),
            class: "Workspace".into(),
            avoid_sync: false,
            avoid_sync_carrier: false,
            disk_fragment: None,
            disk_fragment_is_dir: None,
            source_included: None,
        };
        let mut child = snapshot::FlatSnapshotRecord {
            id: 1,
            parent_id: Some(0),
            child_index: 5,
            child_count: 0,
            has_children: false,
            name: "Main".into(),
            class: "ModuleScript".into(),
            avoid_sync: false,
            avoid_sync_carrier: false,
            disk_fragment: None,
            disk_fragment_is_dir: None,
            source_included: None,
        };
        let error =
            validate_flat_snapshot(&[root.clone(), child.clone()], "Workspace", false).unwrap_err();
        assert!(error.contains("contiguous from zero"));

        child.child_index = 0;
        child.parent_id = Some(1);
        let error = validate_flat_snapshot(&[root, child], "Workspace", false).unwrap_err();
        assert!(error.contains("follow its parent"));
    }

    #[test]
    fn streamed_structure_budgets_reject_repeated_wide_names_before_clone() {
        let root = streamed_service_records("ReplicatedStorage", None)
            .pop()
            .unwrap();
        let wide_child = snapshot::FlatSnapshotRecord {
            id: 1,
            parent_id: Some(0),
            child_index: 0,
            child_count: 0,
            has_children: false,
            name: "n".repeat(MAX_STREAM_NAME_BYTES),
            class: "ModuleScript".into(),
            avoid_sync: false,
            avoid_sync_carrier: false,
            disk_fragment: None,
            disk_fragment_is_dir: None,
            source_included: None,
        };
        let chunk_bytes =
            encoded_stream_record_chunk_bytes(std::slice::from_ref(&wide_child)).unwrap();
        assert!(chunk_bytes < STREAM_REQUEST_BODY_BYTES);

        let mut service_bytes = 0;
        let mut session_bytes = 0;
        let mut accepted_chunks = 0;
        loop {
            match charge_stream_structure_bytes(service_bytes, session_bytes, chunk_bytes) {
                Ok((next_service, next_session)) => {
                    service_bytes = next_service;
                    session_bytes = next_session;
                    accepted_chunks += 1;
                }
                Err(error) => {
                    assert!(error.contains("service structure exceeds"), "{error}");
                    break;
                }
            }
        }
        assert!(
            accepted_chunks > 1_000,
            "the adversarial case must represent many repeated wide-name chunks"
        );
        assert!(service_bytes <= MAX_STREAM_SERVICE_STRUCTURE_BYTES);
        assert_eq!(session_bytes, service_bytes);

        let project = TempDir::new("streamed-push-wide-name-budget");
        let state = test_state(&project, None);
        let mut service_stream = new_push_service_stream("ReplicatedStorage");
        service_stream.records.push(root.clone());
        service_stream.accepted_structure_bytes =
            MAX_STREAM_SERVICE_STRUCTURE_BYTES - chunk_bytes + 1;
        let service_counter_before = service_stream.accepted_structure_bytes;
        let mut push_session = PushStreamAccumulator {
            stream_id: "wide-name-service-budget".into(),
            strict: true,
            force_prune: true,
            next_service: 0,
            service_stream,
            applied: 0,
            backups: Vec::new(),
            committed_services: Vec::new(),
            accepted_stream_bytes: 0,
            accepted_source_bytes: 0,
            last_request_hash: None,
            last_response: None,
            last_activity: Instant::now(),
            completed_at: None,
        };
        let error = process_streamed_push_chunk(
            &state,
            &mut push_session,
            &streamed_push_test_body(
                "wide-name-service-budget",
                "ReplicatedStorage",
                "structure",
                0,
                false,
                vec![wide_child.clone()],
                Vec::new(),
            ),
        )
        .unwrap_err();
        assert!(error.contains("service structure exceeds"), "{error}");
        assert_eq!(push_session.service_stream.records, vec![root.clone()]);
        assert_eq!(
            push_session.service_stream.accepted_structure_bytes,
            service_counter_before
        );
        assert_eq!(push_session.accepted_stream_bytes, 0);

        let compare_handle = Arc::new(Mutex::new(InitialCompareAccumulator {
            compare_id: "wide-name-session-budget".into(),
            disk_stats: Stats::default(),
            studio_stats: Stats::default(),
            next_service: 0,
            comparison: InitialComparison::default(),
            staged_baselines: Vec::new(),
            staged_service_generations: Vec::new(),
            service_stream: Some(InitialCompareServiceStream {
                service: "ReplicatedStorage".into(),
                phase: InitialCompareStreamPhase::Structure,
                next_chunk: 0,
                records: vec![root.clone()],
                accepted_structure_bytes: 0,
                final_structure_len: 0,
                final_structure_bytes: 0,
                final_structure_chunk: 0,
                prepare_result: None,
                local_nodes: None,
                local_source_paths_by_path: HashMap::new(),
                studio_nodes: None,
                studio_paths_by_id: HashMap::new(),
                expected_hash_ids: HashSet::new(),
                received_hash_ids: HashSet::new(),
            }),
            last_service: None,
            last_request_hash: None,
            last_response: None,
            pending_choice_id: None,
            accepted_stream_bytes: MAX_STREAM_SESSION_STRUCTURE_BYTES - chunk_bytes + 1,
            started_at: Instant::now(),
            completed_at: None,
        }));
        let compare_body = InitialCompareBody {
            studio_stats: Stats::default(),
            studio_snapshot: Vec::new(),
            compare_id: Some("wide-name-session-budget".into()),
            service: Some("ReplicatedStorage".into()),
            plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
            phase: Some("structure".into()),
            chunk_index: Some(0),
            final_chunk: false,
            records: vec![wide_child],
            hashes: Vec::new(),
        };
        let mut compare_session = compare_handle.lock().unwrap();
        let session_counter_before = compare_session.accepted_stream_bytes;
        let error = process_streamed_initial_compare_chunk(
            &state,
            &mut compare_session,
            &compare_body,
            "ReplicatedStorage",
            state.canonical_project.as_path(),
            &compare_handle,
        )
        .unwrap_err();
        assert!(error.contains("session structure exceeds"), "{error}");
        let stream = compare_session.service_stream.as_ref().unwrap();
        assert_eq!(stream.records, vec![root]);
        assert_eq!(stream.accepted_structure_bytes, 0);
        assert_eq!(
            compare_session.accepted_stream_bytes,
            session_counter_before
        );
    }

    #[test]
    fn streamed_structure_field_limits_reject_before_accounting() {
        let base = snapshot::FlatSnapshotRecord {
            id: 1,
            parent_id: Some(0),
            child_index: 0,
            child_count: 0,
            has_children: false,
            name: "Config".into(),
            class: "ModuleScript".into(),
            avoid_sync: false,
            avoid_sync_carrier: false,
            disk_fragment: None,
            disk_fragment_is_dir: None,
            source_included: None,
        };

        let mut oversized_name = base.clone();
        oversized_name.name = "n".repeat(MAX_STREAM_NAME_BYTES + 1);
        assert!(validate_stream_record_chunk_fields(&[oversized_name])
            .unwrap_err()
            .contains("name exceeds"));

        let mut oversized_class = base.clone();
        oversized_class.class = "c".repeat(MAX_STREAM_CLASS_BYTES + 1);
        assert!(validate_stream_record_chunk_fields(&[oversized_class])
            .unwrap_err()
            .contains("class exceeds"));

        let mut oversized_fragment = base;
        oversized_fragment.disk_fragment = Some("f".repeat(MAX_STREAM_NAME_BYTES + 1));
        assert!(validate_stream_record_chunk_fields(&[oversized_fragment])
            .unwrap_err()
            .contains("diskFragment exceeds"));
    }

    #[tokio::test]
    async fn initial_compare_rejects_out_of_order_and_superseded_sessions() {
        let project = TempDir::new("initial-compare-service-order");
        let storage = project.path().join("ReplicatedStorage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("Config.luau"), "return true\n").unwrap();
        let state = test_state(&project, None);
        let studio_stats = Stats {
            script_count: 1,
            instance_count: 1,
        };
        let start = |state: AppState| async move {
            initial_compare(
                State(state),
                Json(InitialCompareBody {
                    studio_stats,
                    studio_snapshot: Vec::new(),
                    compare_id: None,
                    service: None,
                    plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                    phase: None,
                    chunk_index: None,
                    final_chunk: false,
                    records: Vec::new(),
                    hashes: Vec::new(),
                }),
            )
            .await
            .0
        };

        let first = start(state.clone()).await;
        let first_id = first["compareId"].as_str().unwrap().to_string();
        let workspace = json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [],
        });
        let out_of_order = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: vec![workspace],
                compare_id: Some(first_id.clone()),
                service: Some("Workspace".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        assert_eq!(out_of_order["ok"], false);
        assert!(out_of_order["error"]
            .as_str()
            .unwrap()
            .contains("expected service ReplicatedStorage"));

        let second = start(state.clone()).await;
        let second_id = second["compareId"].as_str().unwrap().to_string();
        assert_ne!(first_id, second_id);
        let replicated_storage = json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [{
                "class": "ModuleScript",
                "name": "Config",
                "properties": { "Source": "return true\n" },
                "children": [],
            }],
        });
        let stale = initial_compare(
            State(state.clone()),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: vec![replicated_storage.clone()],
                compare_id: Some(first_id),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        assert_eq!(stale["ok"], false);
        assert!(stale["error"]
            .as_str()
            .unwrap()
            .contains("compareId is stale"));

        let accepted = initial_compare(
            State(state),
            Json(InitialCompareBody {
                studio_stats,
                studio_snapshot: vec![replicated_storage],
                compare_id: Some(second_id),
                service: Some("ReplicatedStorage".into()),
                plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
                phase: None,
                chunk_index: None,
                final_chunk: false,
                records: Vec::new(),
                hashes: Vec::new(),
            }),
        )
        .await
        .0;
        assert_eq!(accepted["action"], "compare");
        assert_eq!(accepted["nextService"], "ServerScriptService");
    }

    #[tokio::test]
    async fn legacy_tree_post_rejects_over_deep_skeleton_before_path_collection() {
        let project = TempDir::new("legacy-tree-depth-budget");
        let state = test_state(&project, None);
        let bytes = serde_json::to_vec(&over_deep_studio_service("Workspace")).unwrap();

        let response = tree_post(State(state), Bytes::from(bytes)).await.0;
        assert_eq!(response["ok"], false);
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("tree: Studio tree depth exceeds"));
    }

    #[test]
    fn wide_bootstrap_materializes_thousands_of_siblings_in_one_batch() {
        const WIDTH: usize = 2_000;
        let d = TempDir::new("wide-bootstrap");
        let engine = ConflictEngine::new();
        let quiet = push_quiet();
        let ctx = force_harness(&engine, &quiet, d.path());
        let children = (0..WIDTH)
            .map(|index| {
                json!({
                    "class": "ModuleScript",
                    "name": format!("Module{index:05}"),
                    "properties": { "Source": format!("return {index}\n") },
                    "children": [],
                })
            })
            .collect::<Vec<_>>();
        let service = json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": children,
        });

        assert_eq!(apply_service_node(d.path(), &service, &ctx).unwrap(), WIDTH);
        let materialized = std::fs::read_dir(d.path().join("ReplicatedStorage"))
            .unwrap()
            .count();
        assert_eq!(materialized, WIDTH);
        assert_eq!(
            std::fs::read_to_string(d.path().join("ReplicatedStorage").join("Module01999.luau"))
                .unwrap(),
            "return 1999\n"
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
    async fn initial_choice_status_is_bounded_summary_only() {
        let project = TempDir::new("initial-choice-replay");
        let state = test_state(&project, None);
        let paths = vec!["ReplicatedStorage/Config".into()];
        let mut pending = test_pending_initial("choice-replay", &paths);
        pending.disk_stats = Stats {
            script_count: 2,
            instance_count: 3,
        };
        pending.studio_stats = Stats {
            script_count: 4,
            instance_count: 5,
        };
        *state.pending_initial.lock().unwrap() = Some(pending);

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
        assert_eq!(body["detailCount"], 1);
        assert_eq!(body["comparison"]["summary"]["changedFiles"], 1);
        assert!(body["comparison"].get("changedFiles").is_none());
        assert!(body.get("selectedPaths").is_none());
    }

    #[tokio::test]
    async fn initial_choice_pages_twenty_five_thousand_stable_ids_under_byte_cap() {
        const WIDTH: usize = 25_000;
        let project = TempDir::new("initial-choice-details-25k");
        let state = test_state(&project, None);
        let suffix = "LongWindowsCompatibleSegment".repeat(20);
        let paths = (0..WIDTH)
            .map(|index| format!("ReplicatedStorage/Wide/Module{index:05}/{suffix}"))
            .collect::<Vec<_>>();
        *state.pending_initial.lock().unwrap() =
            Some(test_pending_initial("choice-details-25k", &paths));

        let status = initial_choice_status(State(state.clone())).await.0;
        let status_bytes = serde_json::to_vec(&status).unwrap();
        assert!(status_bytes.len() < 2048);
        assert_eq!(status["detailCount"], WIDTH);
        assert!(!String::from_utf8(status_bytes)
            .unwrap()
            .contains("Module00000"));

        let mut cursor = None;
        let mut expected_id = 0usize;
        let mut pages = 0usize;
        loop {
            let (_, Json(page)) = initial_choice_details(
                State(state.clone()),
                Query(InitialChoiceDetailsParams {
                    choice_id: "choice-details-25k".into(),
                    cursor,
                    limit: Some(INITIAL_CHOICE_DETAIL_MAX_LIMIT),
                }),
            )
            .await;
            let encoded = serde_json::to_vec(&page).unwrap();
            assert!(
                encoded.len() <= INITIAL_CHOICE_DETAIL_MAX_RESPONSE,
                "detail page was {} bytes",
                encoded.len()
            );
            assert_eq!(page["ok"], true);
            assert_eq!(page["totalCount"], WIDTH);
            let items = page["items"].as_array().unwrap();
            assert!(!items.is_empty());
            for item in items {
                assert_eq!(item["id"], expected_id);
                assert_eq!(item["path"], paths[expected_id]);
                expected_id += 1;
            }
            pages += 1;
            if page["complete"] == true {
                assert!(page["nextCursor"].is_null());
                break;
            }
            cursor = Some(page["nextCursor"].as_str().unwrap().to_string());
        }
        assert_eq!(expected_id, WIDTH);
        assert!(pages > WIDTH.div_ceil(INITIAL_CHOICE_DETAIL_MAX_LIMIT));

        let (foreign_status, Json(foreign)) = initial_choice_details(
            State(state),
            Query(InitialChoiceDetailsParams {
                choice_id: "choice-details-25k".into(),
                cursor: Some(encode_initial_choice_cursor("another-choice", 1)),
                limit: None,
            }),
        )
        .await;
        assert_eq!(foreign_status, StatusCode::CONFLICT);
        assert_eq!(foreign["ok"], false);
        assert!(foreign["error"]
            .as_str()
            .unwrap()
            .contains("another choice"));
    }

    #[test]
    fn final_selection_receipt_is_replayable_before_choice_publication() {
        let project = PathBuf::from("selection-linearization-project");
        let choice_id = "choice-linearization";
        let submission_id = "submission-linearization";
        let request = InitialSelectionReceipt {
            chunk_index: 0,
            final_chunk: true,
            restart: true,
            ids: vec![0],
            selected_count: 0,
            committed: false,
        };
        let committed_receipt = InitialSelectionReceipt {
            selected_count: 1,
            committed: true,
            ..request.clone()
        };
        let selection = InitialSelectionAccumulator {
            submission_id: submission_id.into(),
            next_chunk: 1,
            selected_ids: BTreeSet::from([0]),
            receipts: vec![committed_receipt],
            updated_at: Instant::now(),
        };
        let mut pending =
            test_pending_initial(choice_id, &["ReplicatedStorage/Config".to_string()]);
        let replayed_before_publish = std::cell::Cell::new(false);

        commit_initial_selection_with(
            &project,
            choice_id,
            submission_id,
            &mut pending,
            selection,
            || {
                let replay = replay_completed_initial_selection(
                    &project,
                    choice_id,
                    submission_id,
                    &request,
                )
                .expect("final receipt must exist before choice publication")
                .expect("exact final retry must replay");
                assert_eq!(replay["committed"], true);
                replayed_before_publish.set(true);
            },
        );

        assert!(replayed_before_publish.get());
        assert_eq!(pending.choice, Some(Choice::Disk));
        COMPLETED_INITIAL_SELECTIONS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(&(project, choice_id.to_string(), submission_id.to_string()));
    }

    #[tokio::test]
    async fn initial_choice_selection_replays_exact_chunks_and_commits_ids_only() {
        let project = TempDir::new("initial-choice-selection-replay");
        let state = test_state(&project, None);
        let choice_id = "choice-selection-replay";
        let submission_id = "submission-selection-replay";
        let paths = (0..5_000)
            .map(|index| format!("ReplicatedStorage/Module{index:05}"))
            .collect::<Vec<_>>();
        *state.pending_initial.lock().unwrap() = Some(test_pending_initial(choice_id, &paths));

        let first_ids = (0..INITIAL_SELECTION_MAX_IDS as u32).collect::<Vec<_>>();
        let first_request = || InitialChoiceSelectionBody {
            op: None,
            choice_id: choice_id.into(),
            submission_id: submission_id.into(),
            chunk_index: Some(0),
            final_chunk: Some(false),
            restart: true,
            ids: first_ids.clone(),
        };
        let (status, Json(first)) =
            initial_choice_selection(State(state.clone()), Json(first_request())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["acceptedChunk"], 0);
        assert_eq!(first["nextChunk"], 1);
        assert_eq!(first["selectedCount"], INITIAL_SELECTION_MAX_IDS);
        assert_eq!(first["committed"], false);

        let (status, Json(retry)) =
            initial_choice_selection(State(state.clone()), Json(first_request())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(retry, first, "exact chunk retry must replay its receipt");

        let (status, Json(mismatch)) = initial_choice_selection(
            State(state.clone()),
            Json(InitialChoiceSelectionBody {
                op: None,
                choice_id: choice_id.into(),
                submission_id: submission_id.into(),
                chunk_index: Some(0),
                final_chunk: Some(false),
                restart: true,
                ids: vec![0],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(mismatch["error"]
            .as_str()
            .unwrap()
            .contains("does not match"));

        let (status, Json(out_of_order)) = initial_choice_selection(
            State(state.clone()),
            Json(InitialChoiceSelectionBody {
                op: None,
                choice_id: choice_id.into(),
                submission_id: submission_id.into(),
                chunk_index: Some(2),
                final_chunk: Some(false),
                restart: false,
                ids: vec![4_000],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(out_of_order["error"]
            .as_str()
            .unwrap()
            .contains("expected chunk 1"));

        for (ids, expected_error) in [
            (vec![2_048, 2_048], "repeats an ID"),
            (vec![5_000], "outside the current divergence"),
            (vec![1, 2_048], "earlier chunk"),
        ] {
            let (status, Json(rejected)) = initial_choice_selection(
                State(state.clone()),
                Json(InitialChoiceSelectionBody {
                    op: None,
                    choice_id: choice_id.into(),
                    submission_id: submission_id.into(),
                    chunk_index: Some(1),
                    final_chunk: Some(false),
                    restart: false,
                    ids,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert!(
                rejected["error"].as_str().unwrap().contains(expected_error),
                "{rejected}"
            );
        }

        let final_ids = (INITIAL_SELECTION_MAX_IDS as u32..3_000).collect::<Vec<_>>();
        let final_request = || InitialChoiceSelectionBody {
            op: None,
            choice_id: choice_id.into(),
            submission_id: submission_id.into(),
            chunk_index: Some(1),
            final_chunk: Some(true),
            restart: false,
            ids: final_ids.clone(),
        };
        let (status, Json(final_receipt)) =
            initial_choice_selection(State(state.clone()), Json(final_request())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(final_receipt["selectedCount"], 3_000);
        assert_eq!(final_receipt["committed"], true);
        {
            let pending = state.pending_initial.lock().unwrap();
            let pending = pending.as_ref().unwrap();
            assert_eq!(pending.choice, Some(Choice::Disk));
            assert_eq!(pending.selected_disk_paths.as_ref().unwrap().len(), 3_000);
            assert_eq!(
                pending.selected_disk_paths.as_ref().unwrap()[2_999],
                "ReplicatedStorage/Module02999"
            );
        }

        let decision = initial_decision(
            State(state.clone()),
            Query(InitialDecisionParams {
                choice_id: choice_id.into(),
            }),
        )
        .await
        .into_response();
        let bytes = to_bytes(decision.into_body(), 1024).await.unwrap();
        let decision: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decision["choice"], "disk");
        assert_eq!(decision["selectedCount"], 3_000);

        let (status, Json(replayed_after_consume)) =
            initial_choice_selection(State(state.clone()), Json(final_request())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replayed_after_consume, final_receipt);

        let (status, Json(stale)) = initial_choice_selection(
            State(state.clone()),
            Json(InitialChoiceSelectionBody {
                op: None,
                choice_id: "stale-choice".into(),
                submission_id: submission_id.into(),
                chunk_index: Some(0),
                final_chunk: Some(true),
                restart: true,
                ids: vec![0],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(stale["ok"], false);

        SELECTIVE_TRANSFER_GRANTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(&(
                state.canonical_project.as_ref().clone(),
                choice_id.to_string(),
            ));
    }

    #[tokio::test]
    async fn initial_choice_selection_restart_abort_and_ttl_are_atomic() {
        let project = TempDir::new("initial-choice-selection-lifecycle");
        let state = test_state(&project, None);
        let choice_id = "choice-selection-lifecycle";
        let paths = (0..10)
            .map(|index| format!("Workspace/Item{index:02}"))
            .collect::<Vec<_>>();
        *state.pending_initial.lock().unwrap() = Some(test_pending_initial(choice_id, &paths));

        let chunk = |submission_id: &str, restart, ids| InitialChoiceSelectionBody {
            op: None,
            choice_id: choice_id.into(),
            submission_id: submission_id.into(),
            chunk_index: Some(0),
            final_chunk: Some(false),
            restart,
            ids,
        };
        let (status, _) = initial_choice_selection(
            State(state.clone()),
            Json(chunk("submission-a", true, vec![0, 1])),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, Json(blocked)) = initial_choice_selection(
            State(state.clone()),
            Json(chunk("submission-b", false, vec![2])),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(blocked["error"].as_str().unwrap().contains("restart"));

        let (status, Json(restarted)) = initial_choice_selection(
            State(state.clone()),
            Json(chunk("submission-b", true, vec![2])),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(restarted["selectedCount"], 1);
        {
            let pending = state.pending_initial.lock().unwrap();
            let selection = pending.as_ref().unwrap().selection.as_ref().unwrap();
            assert_eq!(selection.submission_id, "submission-b");
            assert_eq!(selection.selected_ids, BTreeSet::from([2]));
        }

        let abort = || InitialChoiceSelectionBody {
            op: Some("abort".into()),
            choice_id: choice_id.into(),
            submission_id: "submission-b".into(),
            chunk_index: None,
            final_chunk: None,
            restart: false,
            ids: Vec::new(),
        };
        let (status, Json(aborted)) =
            initial_choice_selection(State(state.clone()), Json(abort())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(aborted["aborted"], true);
        let (status, Json(replayed_abort)) =
            initial_choice_selection(State(state.clone()), Json(abort())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replayed_abort["aborted"], false);

        {
            let mut pending = state.pending_initial.lock().unwrap();
            pending.as_mut().unwrap().selection = Some(InitialSelectionAccumulator {
                submission_id: "expired-submission".into(),
                next_chunk: 1,
                selected_ids: BTreeSet::from([0]),
                receipts: Vec::new(),
                updated_at: Instant::now() - INITIAL_SELECTION_TTL - Duration::from_secs(1),
            });
        }
        let _ = initial_choice_status(State(state.clone())).await;
        assert!(state
            .pending_initial
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .selection
            .is_none());

        let (status, Json(fresh)) =
            initial_choice_selection(State(state), Json(chunk("submission-c", true, vec![3])))
                .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fresh["selectedCount"], 1);

        let key = (
            project.path().to_path_buf(),
            "expired-choice-receipt".to_string(),
            "expired-submission-receipt".to_string(),
        );
        let replays = COMPLETED_INITIAL_SELECTIONS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut replays = replays.lock().unwrap();
        replays.insert(
            key.clone(),
            CompletedInitialSelection {
                receipts: Vec::new(),
                completed_at: Instant::now()
                    - INITIAL_SELECTION_REPLAY_TTL
                    - Duration::from_secs(1),
            },
        );
        prune_completed_initial_selections(&mut replays);
        assert!(!replays.contains_key(&key));
    }

    #[test]
    fn initial_choice_event_broadcasts_only_summary_metadata() {
        let project = TempDir::new("initial-choice-compact-event");
        let state = test_state(&project, None);
        let mut events = state.events.subscribe();
        let response = finish_initial_comparison(
            &state,
            Stats::default(),
            Stats::default(),
            InitialComparison {
                summary: InitialComparisonSummary {
                    new_files: 0,
                    changed_files: 1,
                    removed_files: 0,
                },
                new_files: Vec::new(),
                changed_files: vec![diff::ChangedItem {
                    path: "ReplicatedStorage/Config".into(),
                    kind: diff::DiffKind::Script,
                    local_class: "ModuleScript".into(),
                    studio_class: "ModuleScript".into(),
                    class_changed: false,
                    source_changed: true,
                }],
                removed_files: Vec::new(),
            },
            None,
        )
        .0;
        assert_eq!(
            response["comparison"]["changedFiles"][0]["path"],
            "ReplicatedStorage/Config"
        );
        let event: Value = serde_json::from_str(&events.try_recv().expect("choice event")).unwrap();
        assert_eq!(event["comparison"]["summary"]["changedFiles"], 1);
        assert!(event["comparison"].get("changedFiles").is_none());
    }

    #[tokio::test]
    async fn initial_decision_keeps_twenty_five_thousand_selected_paths_out_of_http() {
        let project = TempDir::new("initial-decision-compact-selection");
        let state = test_state(&project, None);
        let choice_id = "compact-25k-selection".to_string();
        let selected = (0..25_000)
            .map(|index| format!("ReplicatedStorage/Module{index:05}"))
            .collect::<Vec<_>>();
        let mut pending = test_pending_initial(&choice_id, &selected);
        pending.choice = Some(Choice::Disk);
        pending.selected_disk_paths = Some(selected);
        *state.pending_initial.lock().unwrap() = Some(pending);

        let response = initial_decision(
            State(state.clone()),
            Query(InitialDecisionParams {
                choice_id: choice_id.clone(),
            }),
        )
        .await
        .into_response();
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(
            bytes.len() < 256,
            "decision response was {} bytes",
            bytes.len()
        );
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["choice"], "disk");
        assert_eq!(body["selective"], true);
        assert_eq!(body["selectedCount"], 25_000);
        assert!(body.get("paths").is_none());

        let grant = SELECTIVE_TRANSFER_GRANTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(&(state.canonical_project.as_ref().clone(), choice_id))
            .unwrap();
        assert_eq!(grant.paths.len(), 25_000);

        let full_choice_id = "compact-full-selection".to_string();
        let mut pending = test_pending_initial(&full_choice_id, &[]);
        pending.choice = Some(Choice::Disk);
        *state.pending_initial.lock().unwrap() = Some(pending);
        let response = initial_decision(
            State(state),
            Query(InitialDecisionParams {
                choice_id: full_choice_id,
            }),
        )
        .await
        .into_response();
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body, json!({ "choice": "disk" }));
    }

    #[tokio::test]
    async fn protocol_five_stream_routes_reject_over_512k_before_deserialization() {
        let project = TempDir::new("protocol-five-stream-body-limit");
        let app = router(test_state(&project, None));
        for route in ["/initial-compare", "/push"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(route)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(vec![b'x'; STREAM_REQUEST_BODY_BYTES + 1]))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{route}");
        }
    }

    #[tokio::test]
    async fn initial_choice_routes_enforce_16k_64k_and_409_contracts() {
        let project = TempDir::new("initial-choice-route-limits");
        let state = test_state(&project, None);
        let paths = vec!["ReplicatedStorage/Config".into()];
        *state.pending_initial.lock().unwrap() =
            Some(test_pending_initial("choice-route-limits", &paths));

        let oversized_choice = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/initial-choice")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(vec![b'x'; 16 * 1024 + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(oversized_choice.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let oversized_selection = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/initial-choice/selection")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(vec![b'x'; 64 * 1024 + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(oversized_selection.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let path_authority = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/initial-choice")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "choiceId": "choice-route-limits",
                            "choice": "disk",
                            "mode": "all",
                            "paths": ["ReplicatedStorage/Config"],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(path_authority.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let stale_details = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/initial-choice/details?choiceId=stale-choice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale_details.status(), StatusCode::CONFLICT);
        let stale_body = to_bytes(stale_details.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&stale_body).unwrap()["ok"],
            false
        );

        let all_body = json!({
            "choiceId": "choice-route-limits",
            "choice": "disk",
            "mode": "all",
        })
        .to_string();
        assert!(
            all_body.len() < 128,
            "full choice must remain constant-size"
        );
        let full_choice = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/initial-choice")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(all_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(full_choice.status(), StatusCode::OK);
        {
            let pending = state.pending_initial.lock().unwrap();
            let pending = pending.as_ref().unwrap();
            assert_eq!(pending.choice, Some(Choice::Disk));
            assert!(pending.selected_disk_paths.is_none());
        }

        let resolved_details = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/initial-choice/details?choiceId=choice-route-limits")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resolved_details.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn initial_choice_releases_the_matching_completed_compare_response() {
        let project = TempDir::new("initial-choice-releases-compare");
        let state = test_state(&project, None);
        let choice_id = "choice-release".to_string();
        let paths = vec!["ReplicatedStorage/Config".into()];
        *state.pending_initial.lock().unwrap() = Some(test_pending_initial(&choice_id, &paths));
        let session = Arc::new(Mutex::new(InitialCompareAccumulator {
            compare_id: new_choice_id(),
            disk_stats: Stats::default(),
            studio_stats: Stats::default(),
            next_service: snapshot::SYNCED_SERVICES.len(),
            comparison: InitialComparison::default(),
            staged_baselines: Vec::new(),
            staged_service_generations: Vec::new(),
            service_stream: None,
            last_service: Some("Lighting".into()),
            last_request_hash: None,
            last_response: Some(json!({
                "action": "decide",
                "choiceId": choice_id,
                "comparison": { "changedFiles": ["potentially-large-metadata"] },
            })),
            pending_choice_id: Some(choice_id.clone()),
            accepted_stream_bytes: 0,
            started_at: Instant::now(),
            completed_at: Some(Instant::now()),
        }));
        INITIAL_COMPARE_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .insert(state.canonical_project.as_ref().clone(), session.clone());

        let (_, Json(response)) = initial_choice(
            State(state.clone()),
            Json(InitialChoiceBody {
                choice_id,
                choice: "studio".into(),
                mode: None,
            }),
        )
        .await;

        assert_eq!(response["ok"], true);
        assert!(!INITIAL_COMPARE_ACCUMULATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .get(state.canonical_project.as_path())
            .is_some_and(|current| Arc::ptr_eq(current, &session)));
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
        assert_eq!(parent["pathMode"], "generated");
        assert_eq!(
            parent["targetPath"],
            json!(["ReplicatedStorage", "Feature"])
        );
        assert_eq!(parent["diskPath"], json!(["ReplicatedStorage", "Feature"]));
        assert_eq!(parent["node"]["children"], json!([]));
        assert_eq!(parent["node"]["properties"], json!({}));
        assert_eq!(chosen["node"]["name"], "Chosen");
        assert_eq!(chosen["pathMode"], "generated");
        assert_eq!(
            chosen["targetPath"],
            json!(["ReplicatedStorage", "Feature", "Chosen"])
        );
        assert_eq!(
            chosen["diskPath"],
            json!(["ReplicatedStorage", "Feature", "Chosen.luau"])
        );
        assert_eq!(chosen["node"]["properties"]["Source"], "return 'chosen'\n");
        assert_eq!(chosen["forcePrune"], true);
        assert_eq!(removed["path"], json!(["StarterGui", "StudioOnly"]));
        assert_eq!(removed["pathMode"], "generated");
        assert!(
            removed.get("diskPath").is_none(),
            "a Studio-only deletion has no physical filesystem identity"
        );
        assert!(
            !payload.to_string().contains("untouched"),
            "an unselected sibling source must not cross into Studio"
        );
    }

    #[test]
    fn selective_snapshot_keeps_exact_disk_ancestry_for_duplicate_and_literal_names() {
        let d = TempDir::new("initial-selective-exact-disk-path");
        let workspace = d.path().join("Workspace");
        let first_parent = workspace.join("Parent");
        let second_parent = workspace.join("Parent [1]");
        std::fs::create_dir_all(&first_parent).unwrap();
        std::fs::create_dir_all(&second_parent).unwrap();
        std::fs::write(first_parent.join("Other.server.luau"), "print('first')\n").unwrap();
        std::fs::write(
            second_parent.join("Name %5B1%5D.server.luau"),
            "print('literal')\n",
        )
        .unwrap();

        let payload =
            build_selective_snapshot(d.path(), &["Workspace/Parent/Name %5B1%5D".into()]).unwrap();
        let ops = payload["ops"].as_array().unwrap();
        assert_eq!(
            ops.len(),
            2,
            "one exact parent shell and one exact set: {payload}"
        );

        let parent = ops.iter().find(|op| op["op"] == "ensure").unwrap();
        assert_eq!(parent["path"], json!(["Workspace"]));
        assert_eq!(parent["pathMode"], "generated");
        assert_eq!(parent["targetPath"], json!(["Workspace", "Parent"]));
        assert_eq!(parent["node"]["name"], "Parent");
        assert_eq!(parent["diskPath"], json!(["Workspace", "Parent [1]"]));

        let selected = ops.iter().find(|op| op["op"] == "set").unwrap();
        assert_eq!(
            selected["path"],
            json!(["Workspace", "Parent"]),
            "the logical fallback keeps generated duplicate-segment semantics"
        );
        assert_eq!(selected["pathMode"], "generated");
        assert_eq!(
            selected["targetPath"],
            json!(["Workspace", "Parent", "Name %5B1%5D"])
        );
        assert_eq!(selected["node"]["name"], "Name [1]");
        assert_eq!(
            selected["diskPath"],
            json!(["Workspace", "Parent [1]", "Name %5B1%5D.server.luau"]),
            "the exact path must preserve both duplicate-parent and encoded literal fragments"
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
    fn compact_selected_paths_handles_tens_of_thousands_of_wide_siblings() {
        const WIDTH: usize = 20_000;
        let paths = (0..WIDTH)
            .map(|index| format!("Workspace/Item{index:05}"))
            .collect::<Vec<_>>();
        let compacted = compact_selected_paths(&paths).unwrap();
        assert_eq!(compacted.len(), WIDTH);
        assert_eq!(compacted.first().unwrap(), "Workspace/Item00000");
        assert_eq!(compacted.last().unwrap(), "Workspace/Item19999");
    }

    #[test]
    fn initial_choice_details_are_path_sorted_with_dense_stable_ids() {
        let comparison = InitialComparison {
            summary: InitialComparisonSummary {
                new_files: 1,
                changed_files: 1,
                removed_files: 1,
            },
            new_files: vec![diff::DiffItem {
                path: "Workspace/Zed".into(),
                class: "Folder".into(),
                kind: diff::DiffKind::Folder,
            }],
            changed_files: vec![diff::ChangedItem {
                path: "ReplicatedStorage/Config".into(),
                kind: diff::DiffKind::Script,
                local_class: "ModuleScript".into(),
                studio_class: "ModuleScript".into(),
                class_changed: false,
                source_changed: true,
            }],
            removed_files: vec![diff::DiffItem {
                path: "StarterGui/Old".into(),
                class: "LocalScript".into(),
                kind: diff::DiffKind::Script,
            }],
        };
        let details = initial_choice_details_from_comparison(&comparison).unwrap();
        assert_eq!(
            details
                .iter()
                .map(|item| (item.id, item.path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (0, "ReplicatedStorage/Config"),
                (1, "StarterGui/Old"),
                (2, "Workspace/Zed"),
            ]
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

    #[test]
    fn initial_snapshot_comparison_follows_avoid_sync_carrier_paths() {
        let d = TempDir::new("initial-avoid-sync-carrier");
        let ignored = d
            .path()
            .join("Workspace")
            .join("ModelCarrier")
            .join("Ignored");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(ignored.join("LocalOnly.server.luau"), "print('local')\n").unwrap();

        let studio = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [{
                "class": "Folder",
                "name": "ModelCarrier",
                "properties": {},
                "avoidSyncCarrier": true,
                "children": [{
                    "class": "Folder",
                    "name": "Ignored",
                    "properties": {},
                    "avoidSync": true,
                    "children": []
                }]
            }]
        })];

        let report = initial_snapshot_comparison(d.path(), &studio).unwrap();
        assert!(
            report.is_clean(),
            "carrier identity must be ignored while its nested AvoidSync path filters disk"
        );

        std::fs::write(
            d.path()
                .join("Workspace")
                .join("ModelCarrier")
                .join("Stale.server.luau"),
            "print('stale')\n",
        )
        .unwrap();
        let report = initial_snapshot_comparison(d.path(), &studio).unwrap();
        assert_eq!(
            report
                .new_files
                .iter()
                .map(|item| item.path.as_str())
                .collect::<Vec<_>>(),
            vec!["Workspace/ModelCarrier/Stale"],
            "only the carrier identity and ignored subtree are suppressed; unrelated disk scripts remain divergent"
        );
    }

    #[test]
    fn initial_snapshot_comparison_keeps_live_duplicate_after_avoid_sync_carrier() {
        let d = TempDir::new("initial-avoid-sync-carrier-duplicate");
        let ignored = d.path().join("Workspace").join("Shared").join("ZIgnored");
        let live = d.path().join("Workspace").join("Shared [1]");
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(ignored.join("LocalOnly.server.luau"), "print('ignored')\n").unwrap();
        std::fs::write(live.join("Alpha.luau"), "return 'live'\n").unwrap();

        let studio = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [{
                "class": "Model",
                "name": "Shared",
                "properties": {},
                "children": [{
                    "class": "ModuleScript",
                    "name": "Alpha",
                    "properties": { "Source": "return 'live'\r\n" },
                    "children": []
                }]
            }, {
                "class": "Folder",
                "name": "Shared",
                "properties": {},
                "avoidSyncCarrier": true,
                "children": [{
                    "class": "Folder",
                    "name": "ZIgnored",
                    "properties": {},
                    "avoidSync": true,
                    "children": []
                }]
            }]
        })];

        let report = initial_snapshot_comparison(d.path(), &studio).unwrap();
        assert!(
            report.is_clean(),
            "the physical bare carrier must reserve Shared while the live Shared [1] subtree remains comparable: {report:?}"
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
