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
    classify_script_file, encode_name, init_path_describes_parent, instance_to_path,
    is_empty_plain_folder, is_init_file, legacy_reserved_init_leaf_migration_message,
    logical_names_equivalent, normalize_line_endings, parse_disambiguated, parse_init_file,
    parse_plain_init_file, path_is_parent_init_source, path_to_instance_meta,
    portable_init_file_name, script_with_children_source, InstanceDescriptor,
    PathFragmentAllocator, PathInstance, ScriptClass, META_FILE,
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

    // Protocol 6 streams large payloads through route-specific bounded chunks.
    // Keep every unclassified localhost route well below whole-place size.
    const MAX_BODY: usize = 4 * 1024 * 1024;

    const ARTIFACT_CONTROL_BODY: usize = 4 * 1024;
    const ARTIFACT_CHUNK_BODY: usize = 768 * 1024;
    const PROJECT_INIT_BODY: usize = 16 * 1024;
    // Protocol-6 bootstrap records are deliberately bounded. Keep malformed
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
    #[serde(rename = "buildCommit")]
    build_commit: &'static str,
    #[serde(rename = "buildDirty")]
    build_dirty: bool,
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
    #[serde(rename = "hashChunkNodes")]
    hash_chunk_nodes: usize,
    #[serde(rename = "sourceChunkNodes")]
    source_chunk_nodes: usize,
    #[serde(
        rename = "initialChoiceDefault",
        skip_serializing_if = "Option::is_none"
    )]
    initial_choice_default: Option<String>,
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
        build_commit: env!("ROSYNC_BUILD_COMMIT"),
        build_dirty: env!("ROSYNC_BUILD_DIRTY") == "true",
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
        hash_chunk_nodes: STREAM_COMPARE_HASH_CHUNK_NODES,
        source_chunk_nodes: STREAM_SOURCE_PART_CHUNK_NODES,
        initial_choice_default: state.initial_choice_default.read().unwrap().clone(),
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
    // Studio reports the place title for both names it sends, so ask the
    // universe for the experience title before naming the project. Best-effort
    // and bounded: a failed lookup just leaves Studio's own names in place.
    let experience_name = match serde_json::from_slice::<ProjectInitBody>(&body) {
        Ok(parsed) => crate::project_init::resolve_experience_name(&parsed.game_id).await,
        Err(_) => None,
    };
    project_init_inner(&state, &body, experience_name)
}

fn project_init_inner(
    state: &AppState,
    body: &[u8],
    experience_name: Option<String>,
) -> Json<Value> {
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
            game_name: experience_name.unwrap_or(body.game_name),
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

// A disk snapshot taken ahead of need. The initial compare walks and hashes
// each service's disk tree only after the plugin finishes streaming that
// service's Studio structure, which serializes disk IO behind Studio-side
// streaming. The walk depends only on the service name, so the session spawns
// one walker per service at creation and the prep step reuses the result when
// the tree generation still matches (falling back to a fresh walk otherwise).
struct PrewarmedDiskService {
    generation: crate::fs_safety::TreeGeneration,
    disk: snapshot::FlatDiskService,
}

struct InitialCompareAccumulator {
    compare_id: String,
    disk_stats: Stats,
    studio_stats: Stats,
    disk_prewarm: HashMap<String, std::sync::mpsc::Receiver<Result<PrewarmedDiskService, String>>>,
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

#[derive(Clone)]
struct StudioTransferGrant {
    service_generations: Vec<crate::fs_safety::TreeGeneration>,
    /// Generated comparison paths that may be overwritten by the bounded
    /// initial Studio delta endpoint. An empty set means the grant is valid
    /// only for the existing full streamed push.
    delta_source_paths: std::collections::HashSet<String>,
    created_at: Instant,
}

type StudioTransferGrantKey = (PathBuf, String);
static STUDIO_TRANSFER_GRANTS: OnceLock<
    Mutex<HashMap<StudioTransferGrantKey, StudioTransferGrant>>,
> = OnceLock::new();
const STUDIO_TRANSFER_GRANT_TTL: Duration = Duration::from_secs(5 * 60);

enum InitialCompareStreamPhase {
    Structure,
    DiskPrepare,
    Identities,
    Hashes,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamDiskIdentity {
    id: u64,
    disk_fragment: String,
    disk_fragment_is_dir: bool,
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
    identities: Vec<StreamDiskIdentity>,
    identity_offset: usize,
    identity_complete: bool,
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

pub(crate) const PROJECTION_MIGRATION_REQUIRED_CODE: &str = "WATCHER_PROJECTION_MIGRATION_REQUIRED";

fn initial_compare_error_value(prefix: &str, error: &str) -> Value {
    let message = if prefix.is_empty() {
        error.to_string()
    } else {
        format!("{prefix}: {error}")
    };
    if error.starts_with("legacy leaf script ")
        && error.contains("uses the reserved init-marker filename grammar")
    {
        return json!({
            "ok": false,
            "error": message,
            "code": PROJECTION_MIGRATION_REQUIRED_CODE,
            "retryable": false,
        });
    }
    json!({
        "ok": false,
        "error": message,
    })
}

/// Run heavy synchronous handler work on the blocking pool instead of an
/// async worker thread, while retaining the runtime context needed by cleanup
/// timers spawned from the synchronous body.
async fn run_handler_blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let _guard = handle.enter();
        f()
    })
    .await
    .map_err(|error| format!("blocking handler worker failed: {error}"))
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
        // Chunk processing (validation, disk hashing, bounded worker waits)
        // is synchronous; keep it off the async worker threads.
        return match run_handler_blocking(move || initial_compare_service_chunk(&state, body)).await
        {
            Ok(response) => response,
            Err(error) => Json(json!({ "ok": false, "error": error })),
        };
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
        // Start every service's disk walk now, in parallel, so it overlaps
        // the plugin's Studio-side structure streaming instead of running
        // serially after it. Consumed (and generation-checked) in
        // prepare_streamed_initial_service_comparison.
        let mut disk_prewarm = HashMap::new();
        for service in snapshot::SYNCED_SERVICES {
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let root = project.clone();
            let service_name = (*service).to_string();
            std::thread::spawn(move || {
                let result = (|| {
                    let generation = crate::fs_safety::capture_tree_metadata(&root, &service_name)?;
                    let disk =
                        snapshot::emit_flat_service(&root, &service_name).map_err(|error| {
                            format!("scan {}: {error}", root.join(&service_name).display())
                        })?;
                    if crate::fs_safety::capture_tree_metadata(&root, &service_name)? != generation
                    {
                        return Err(format!(
                            "disk service {service_name} changed during prewarm; rescan required"
                        ));
                    }
                    Ok(PrewarmedDiskService { generation, disk })
                })();
                let _ = send.send(result);
            });
            disk_prewarm.insert((*service).to_string(), receive);
        }
        let session = Arc::new(Mutex::new(InitialCompareAccumulator {
            compare_id: compare_id.clone(),
            disk_stats,
            studio_stats: body.studio_stats,
            disk_prewarm,
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

    // Protocol 6 is mandatory above. Retain the monolithic shape only for
    // early bounded-stream clients that already supplied it; current clients are
    // always instructed to use bounded service/chunk streaming.
    match run_handler_blocking(move || {
        match initial_snapshot_comparison(state.canonical_project.as_path(), &body.studio_snapshot)
        {
            Ok(comparison) => {
                finish_initial_comparison(&state, disk_stats, body.studio_stats, comparison, None)
            }
            Err(error) => Json(initial_compare_error_value("snapshot compare", &error)),
        }
    })
    .await
    {
        Ok(response) => response,
        Err(error) => Json(json!({ "ok": false, "error": error })),
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
                return Json(initial_compare_error_value(
                    &format!("streamed snapshot compare {service}"),
                    &error,
                ));
            }
        }
    } else {
        let service_comparison = match initial_service_snapshot_comparison(
            state.canonical_project.as_path(),
            &body.studio_snapshot[0],
        ) {
            Ok(comparison) => comparison,
            Err(error) => {
                return Json(initial_compare_error_value(
                    &format!("snapshot compare {service}"),
                    &error,
                ));
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
    // Detach this service's prewarmed disk walk before the stream borrow
    // below; it is consumed exactly once, by the final structure chunk.
    let disk_prewarm = if phase == "structure" && body.final_chunk {
        session.disk_prewarm.remove(service)
    } else {
        None
    };
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
            identities: Vec::new(),
            identity_offset: 0,
            identity_complete: false,
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
                    prepare_streamed_initial_service_comparison(&root, validated, disk_prewarm)
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
            // Bounded server-side wait: one request absorbs several plugin
            // polling ticks (each otherwise a full HTTP round trip + client
            // delay). This handler runs on the blocking pool, so a short
            // blocking wait cannot stall an async worker thread.
            let result = stream
                .prepare_result
                .as_ref()
                .ok_or("diskPrepare worker is missing")?
                .recv_timeout(STREAM_WORKER_POLL_BUDGET);
            match result {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
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
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
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
                    stream.identities = prepared.identities;
                    stream.identity_offset = 0;
                    stream.identity_complete = prepared.identity_complete;
                    stream.expected_hash_ids = prepared.expected_hash_ids;
                    let identity_count = stream.identities.len();
                    session
                        .staged_service_generations
                        .push(prepared.service_generation);
                    stream.phase = InitialCompareStreamPhase::Identities;
                    stream.next_chunk = 0;
                    session.started_at = Instant::now();
                    Ok(json!({
                        "action": "compare",
                        "compareId": session.compare_id,
                        "nextService": service,
                        "phase": "identities",
                        "nextChunk": 0,
                        "identityCount": identity_count,
                    }))
                }
            }
        }
        InitialCompareStreamPhase::Identities => {
            if phase != "identities"
                || !body.records.is_empty()
                || !body.hashes.is_empty()
                || !body.studio_snapshot.is_empty()
                || body.final_chunk
            {
                return Err("identity chunks accept only empty continuation ticks".into());
            }
            let response =
                produce_initial_compare_identity_response(&session.compare_id, service, stream)?;
            session.started_at = Instant::now();
            Ok(response)
        }
        InitialCompareStreamPhase::Hashes => {
            if phase != "hashes" {
                return Err("service hash phase cannot return to an earlier phase".into());
            }
            if !body.records.is_empty() || !body.studio_snapshot.is_empty() {
                return Err("hash chunks may contain only script hashes".into());
            }
            if body.hashes.len() > STREAM_COMPARE_HASH_CHUNK_NODES {
                return Err(format!(
                    "hash chunks are limited to {STREAM_COMPARE_HASH_CHUNK_NODES} scripts"
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
            let mut local_baselines = prepare_staged_script_baselines(project, &validated_hashes)?;
            let mut prepared_hashes = Vec::with_capacity(validated_hashes.len());
            for ((id, path, studio_hash, _), local_baseline) in
                validated_hashes.into_iter().zip(local_baselines.iter_mut())
            {
                prepared_hashes.push((id, path, studio_hash, local_baseline.take()));
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
            if report.is_clean() && !stream.identity_complete {
                return Err(
                    "clean comparison is missing exact daemon-authored disk identities".into(),
                );
            }
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
    identities: Vec<StreamDiskIdentity>,
    identity_complete: bool,
    expected_hash_ids: HashSet<u64>,
}

fn prepare_streamed_initial_service_comparison(
    root: &Path,
    studio: ValidatedFlatSnapshot,
    disk_prewarm: Option<std::sync::mpsc::Receiver<Result<PrewarmedDiskService, String>>>,
) -> Result<PreparedStreamedComparison, String> {
    let service = studio
        .service
        .get("name")
        .and_then(Value::as_str)
        .ok_or("flat Studio service is missing its name")?
        .to_string();
    reject_legacy_reserved_init_leafs(root, std::slice::from_ref(&service))?;
    // Prefer the walk prewarmed at session creation, but only while its exact
    // tree generation remains current. Drift falls back to capture/walk/verify.
    let mut prewarmed = None;
    if let Some(receiver) = disk_prewarm {
        if let Ok(Ok(candidate)) = receiver.recv() {
            if crate::fs_safety::capture_tree_metadata(root, &service)? == candidate.generation {
                prewarmed = Some(candidate);
            }
        }
    }
    let (service_generation, disk) = match prewarmed {
        Some(candidate) => (candidate.generation, candidate.disk),
        None => {
            let generation = crate::fs_safety::capture_tree_metadata(root, &service)?;
            let disk = snapshot::emit_flat_service(root, &service)
                .map_err(|error| format!("scan {}: {error}", root.join(&service).display()))?;
            if crate::fs_safety::capture_tree_metadata(root, &service)? != generation {
                return Err(format!(
                    "disk service {service} changed during initial comparison; restart the scan"
                ));
            }
            (generation, disk)
        }
    };
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
    let local_snapshot_entries = diff::collect_local_snapshot_entries(&local_services);
    let mut identities = Vec::with_capacity(studio_nodes.len());
    let mut identity_complete = true;
    for (path, studio_node) in &studio_nodes {
        let Some(id) = studio_node.stream_id else {
            identity_complete = false;
            continue;
        };
        let Some(local_node) = local_nodes.get(path) else {
            identity_complete = false;
            continue;
        };
        if local_node.class != studio_node.class || local_node.kind != studio_node.kind {
            identity_complete = false;
            continue;
        }
        let Some(entry) = local_snapshot_entries.get(path) else {
            identity_complete = false;
            continue;
        };
        let Some(fragment) = entry.disk_path.last() else {
            identity_complete = false;
            continue;
        };
        let Some(is_dir) = entry.node.get("diskFragmentIsDir").and_then(Value::as_bool) else {
            identity_complete = false;
            continue;
        };
        identities.push(StreamDiskIdentity {
            id,
            disk_fragment: fragment.clone(),
            disk_fragment_is_dir: is_dir,
        });
    }
    identities.sort_by_key(|identity| identity.id);
    identity_complete &= identities.len() == studio_nodes.len();
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
        identities,
        identity_complete,
        expected_hash_ids,
    })
}

fn initial_compare_identity_response(
    compare_id: &str,
    service: &str,
    chunk_index: u64,
    final_chunk: bool,
    next_chunk: u64,
    identity_count: usize,
    identities: &[StreamDiskIdentity],
) -> Value {
    let mut response = json!({
        "action": "compare",
        "compareId": compare_id,
        "nextService": service,
        "phase": "identities",
        "chunkIndex": chunk_index,
        "finalChunk": final_chunk,
        "nextChunk": next_chunk,
        "identityCount": identity_count,
        "identities": identities,
    });
    if final_chunk {
        response["nextPhase"] = Value::String("hashes".into());
    }
    response
}

fn produce_initial_compare_identity_response(
    compare_id: &str,
    service: &str,
    stream: &mut InitialCompareServiceStream,
) -> Result<Value, String> {
    let chunk_index = stream.next_chunk;
    let identity_count = stream.identities.len();
    let mut chunk = Vec::new();
    while stream.identity_offset + chunk.len() < stream.identities.len()
        && chunk.len() < STREAM_STRUCTURE_CHUNK_NODES
    {
        chunk.push(stream.identities[stream.identity_offset + chunk.len()].clone());
        let final_chunk = stream.identity_offset + chunk.len() == stream.identities.len();
        let candidate = initial_compare_identity_response(
            compare_id,
            service,
            chunk_index,
            final_chunk,
            if final_chunk { 0 } else { chunk_index + 1 },
            identity_count,
            &chunk,
        );
        if encoded_stream_response_len(&candidate)? > STREAM_RESPONSE_PACK_TARGET {
            chunk.pop();
            break;
        }
    }
    if chunk.is_empty() && stream.identity_offset < stream.identities.len() {
        return Err(format!(
            "one disk identity for {service} exceeds the encoded response limit"
        ));
    }
    stream.identity_offset += chunk.len();
    let final_chunk = stream.identity_offset == stream.identities.len();
    let next_chunk = if final_chunk { 0 } else { chunk_index + 1 };
    let response = initial_compare_identity_response(
        compare_id,
        service,
        chunk_index,
        final_chunk,
        next_chunk,
        identity_count,
        &chunk,
    );
    if encoded_stream_response_len(&response)? > STREAM_SOURCE_CHUNK_BYTES {
        return Err("encoded disk identity response exceeds 512 KiB".into());
    }
    if final_chunk {
        stream.phase = InitialCompareStreamPhase::Hashes;
        stream.next_chunk = 0;
    } else {
        stream.next_chunk += 1;
    }
    Ok(response)
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

fn prepare_one_staged_script_baseline(
    source_path: &Path,
    validation: &mut crate::fs_safety::SyncedPathValidationCache,
) -> Result<StagedScriptBaseline, String> {
    let generation = crate::fs_safety::file_generation_no_follow(source_path)?;
    let source_hash = normalized_file_hash_cached(source_path, validation)?;
    if crate::fs_safety::file_generation_no_follow(source_path)? != generation {
        return Err(format!(
            "disk script {} changed while it was hashed; restart the comparison",
            source_path.display()
        ));
    }
    Ok(StagedScriptBaseline {
        fs_mtime: fs_mtime(source_path),
        path: source_path.to_path_buf(),
        source_hash,
        generation,
    })
}

/// Hash every disk script referenced by one compare hash chunk.
///
/// Each worker owns one batch-scoped `SyncedPathValidationCache` so a chunk
/// validates stable ancestors once instead of once per file, and independent
/// per-file hashing is spread across up to `available_parallelism` scoped
/// threads. Returned baselines are positionally aligned with
/// `validated_hashes`; entries without a disk source stay `None`.
fn prepare_staged_script_baselines(
    project: &Path,
    validated_hashes: &[(u64, String, crate::conflict::Hash, Option<PathBuf>)],
) -> Result<Vec<Option<StagedScriptBaseline>>, String> {
    let jobs: Vec<(usize, &Path)> = validated_hashes
        .iter()
        .enumerate()
        .filter_map(|(index, (_, _, _, source_path))| {
            source_path.as_deref().map(|path| (index, path))
        })
        .collect();
    let mut baselines: Vec<Option<StagedScriptBaseline>> = std::iter::repeat_with(|| None)
        .take(validated_hashes.len())
        .collect();
    if jobs.is_empty() {
        return Ok(baselines);
    }
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(jobs.len());
    if workers <= 1 {
        let mut validation = crate::fs_safety::SyncedPathValidationCache::new(project)
            .map_err(|error| format!("validate compare batch: {error}"))?;
        for (index, path) in jobs {
            baselines[index] = Some(prepare_one_staged_script_baseline(path, &mut validation)?);
        }
        return Ok(baselines);
    }
    let chunk_len = jobs.len().div_ceil(workers);
    let results = std::thread::scope(|scope| {
        let handles = jobs
            .chunks(chunk_len)
            .map(|chunk| {
                scope.spawn(move || {
                    let mut validation = crate::fs_safety::SyncedPathValidationCache::new(project)
                        .map_err(|error| format!("validate compare batch: {error}"))?;
                    let mut out = Vec::with_capacity(chunk.len());
                    for (index, path) in chunk {
                        out.push((
                            *index,
                            prepare_one_staged_script_baseline(path, &mut validation)?,
                        ));
                    }
                    Ok::<_, String>(out)
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| "compare hash worker panicked".to_string())?
            })
            .collect::<Result<Vec<_>, String>>()
    })?;
    for chunk in results {
        for (index, baseline) in chunk {
            baselines[index] = Some(baseline);
        }
    }
    Ok(baselines)
}

/// In-memory cache of normalized (CRLF-folded) content hashes keyed by the
/// validated canonical path. Every lookup re-stats the file and returns the
/// cached digest only when the full [`FileGeneration`] (length, mtime ns,
/// physical identity) still matches, so a stale entry can never satisfy a
/// lookup — it merely costs one rehash. Entries are additionally invalidated
/// on watcher events and on daemon writes.
static CONTENT_HASH_CACHE: OnceLock<
    Mutex<HashMap<PathBuf, (crate::fs_safety::FileGeneration, crate::conflict::Hash)>>,
> = OnceLock::new();
const CONTENT_HASH_CACHE_MAX_ENTRIES: usize = 65_536;

fn content_hash_cache(
) -> &'static Mutex<HashMap<PathBuf, (crate::fs_safety::FileGeneration, crate::conflict::Hash)>> {
    CONTENT_HASH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lookup_cached_content_hash(
    validated: &Path,
    generation: &crate::fs_safety::FileGeneration,
) -> Option<crate::conflict::Hash> {
    let cache = content_hash_cache().lock().unwrap();
    cache
        .get(validated)
        .filter(|(cached_generation, _)| cached_generation == generation)
        .map(|(_, hash)| *hash)
}

fn store_cached_content_hash(
    validated: PathBuf,
    generation: crate::fs_safety::FileGeneration,
    hash: crate::conflict::Hash,
) {
    let mut cache = content_hash_cache().lock().unwrap();
    if cache.len() >= CONTENT_HASH_CACHE_MAX_ENTRIES && !cache.contains_key(&validated) {
        cache.clear();
    }
    cache.insert(validated, (generation, hash));
}

/// Drop any cached content hash for `path`. Called for daemon writes and
/// watcher events; correctness never depends on this (every hit re-checks the
/// file generation), it only keeps the map small and current.
pub(crate) fn invalidate_cached_content_hash(path: &Path) {
    if let Some(cache) = CONTENT_HASH_CACHE.get() {
        cache.lock().unwrap().remove(path);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn normalized_file_hash(project_root: &Path, path: &Path) -> Result<crate::conflict::Hash, String> {
    let mut validation = crate::fs_safety::SyncedPathValidationCache::new(project_root)
        .map_err(|error| format!("validate source {}: {error}", path.display()))?;
    normalized_file_hash_cached(path, &mut validation)
}

/// Batch variant of [`normalized_file_hash`] which reuses one
/// [`SyncedPathValidationCache`] across many files (avoiding a fresh
/// canonicalize + ancestor re-scan per file) and consults the content-hash
/// cache before re-reading unchanged sources.
fn normalized_file_hash_cached(
    path: &Path,
    validation: &mut crate::fs_safety::SyncedPathValidationCache,
) -> Result<crate::conflict::Hash, String> {
    use std::io::Read as _;

    let validated = validation
        .validate(path, false)
        .map_err(|error| format!("validate source {}: {error}", path.display()))?;
    let guard = crate::fs_safety::guard_synced_parent_chain_cached(validation, &validated, false)
        .map_err(|error| format!("guard source {}: {error}", path.display()))?;
    guard
        .verify()
        .map_err(|error| format!("verify source parent {}: {error}", path.display()))?;
    let before = crate::fs_safety::file_generation_no_follow(&validated)?;
    if let Some(cached) = lookup_cached_content_hash(&validated, &before) {
        guard
            .verify()
            .map_err(|error| format!("source parent changed {}: {error}", path.display()))?;
        return Ok(cached);
    }
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
    let digest: crate::conflict::Hash = hasher.finalize().into();
    store_cached_content_hash(validated, before, digest);
    Ok(digest)
}

fn capture_initial_service_generations(
    project: &Path,
) -> Result<Vec<crate::fs_safety::TreeGeneration>, String> {
    snapshot::SYNCED_SERVICES
        .iter()
        .map(|service| crate::fs_safety::capture_tree_metadata(project, service))
        .collect()
}

fn revalidate_initial_service_generations(
    project: &Path,
    expected: &[crate::fs_safety::TreeGeneration],
) -> Result<(), String> {
    if expected.len() != snapshot::SYNCED_SERVICES.len() {
        return Err("initial comparison disk fence is incomplete; restart the scan".into());
    }
    for (index, generation) in expected.iter().enumerate() {
        let expected_service = snapshot::SYNCED_SERVICES[index];
        if generation.service != expected_service {
            return Err(format!(
                "initial comparison disk fence expected {expected_service}, found {}",
                generation.service
            ));
        }
        let current = crate::fs_safety::capture_tree_metadata(project, expected_service)?;
        if current != *generation {
            return Err(format!(
                "disk service {expected_service} changed after the initial comparison; restart the scan"
            ));
        }
    }
    Ok(())
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
        if let Some(staged_comparison) = staged_comparison.as_ref() {
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
            for baseline in &staged_comparison.baselines {
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
    let service_generations = match staged_comparison {
        Some(staged) => staged.service_generations,
        None => match capture_initial_service_generations(state.canonical_project.as_path()) {
            Ok(generations) => generations,
            Err(error) => {
                return Json(json!({
                    "ok": false,
                    "error": format!("capture initial-choice disk fence: {error}"),
                }));
            }
        },
    };
    let pending = PendingInitial {
        choice_id: choice_id.clone(),
        disk_stats,
        studio_stats,
        choice: None,
        service_generations,
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
    reject_legacy_reserved_init_leafs(
        root,
        &snapshot::SYNCED_SERVICES
            .iter()
            .map(|service| service.to_string())
            .collect::<Vec<_>>(),
    )?;
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
    reject_legacy_reserved_init_leafs(root, &[service.to_string()])?;
    let local_services = vec![snapshot::emit_service(root, service)
        .map_err(|error| format!("scan {}: {error}", root.join(service).display()))?];
    initial_snapshot_comparison_with_local(
        root,
        local_services,
        std::slice::from_ref(studio_service),
    )
}

pub(crate) fn reject_legacy_reserved_init_leafs(
    root: &Path,
    services: &[String],
) -> Result<(), String> {
    for service in services {
        let service_path = root.join(service);
        let Some(metadata) = crate::fs_safety::metadata_no_follow(&service_path)
            .map_err(|error| format!("inspect service {}: {error}", service_path.display()))?
        else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let mut stack = vec![(service_path, 0usize)];
        let mut visited = 0usize;
        while let Some((directory, depth)) = stack.pop() {
            if depth > crate::fs_safety::MAX_SERVICE_TREE_DEPTH {
                return Err(format!(
                    "legacy projection scan exceeds maximum depth at {}",
                    directory.display()
                ));
            }
            let index = crate::fs_safety::PortableDirectoryIndex::read(&directory)
                .map_err(|error| format!("scan {}: {error}", directory.display()))?;
            for entry in index.entries() {
                visited = visited.saturating_add(1);
                if visited > crate::fs_safety::MAX_SERVICE_TREE_NODES {
                    return Err(format!(
                        "legacy projection scan exceeds node limit in {service}"
                    ));
                }
                if entry.kind == crate::fs_safety::SafeEntryKind::Directory {
                    stack.push((entry.path.clone(), depth + 1));
                    continue;
                }
                let Some(message) = legacy_reserved_init_leaf_migration_message(root, &entry.path)
                    .map_err(|error| format!("inspect {}: {error}", entry.path.display()))?
                else {
                    continue;
                };
                return Err(message);
            }
        }
    }
    Ok(())
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
                Some(p) if p.choice_id == params.choice_id => p.choice.map(|choice| {
                    (
                        choice,
                        p.selected_disk_paths.clone(),
                        p.service_generations.clone(),
                        p.details.clone(),
                    )
                }),
                _ => {
                    return Json(json!({
                        "choice": "stale",
                        "error": "unknown choiceId",
                    }))
                    .into_response();
                }
            }
        };

        if let Some((choice, selected_disk_paths, service_generations, details)) = decision {
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
            } else if choice == Choice::Studio {
                let grants = STUDIO_TRANSFER_GRANTS.get_or_init(|| Mutex::new(HashMap::new()));
                let mut grants = grants.lock().unwrap();
                grants.retain(|_, grant| grant.created_at.elapsed() < STUDIO_TRANSFER_GRANT_TTL);
                grants.insert(
                    (
                        state.canonical_project.as_ref().clone(),
                        params.choice_id.clone(),
                    ),
                    StudioTransferGrant {
                        service_generations,
                        delta_source_paths: details
                            .into_iter()
                            .filter(|item| {
                                item.action == InitialChoiceAction::Overwrite
                                    && item.kind == "script"
                                    && !item.class_changed
                                    && item.source_changed
                            })
                            .map(|item| item.path)
                            .collect(),
                        created_at: Instant::now(),
                    },
                );
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

    if choice == Choice::Studio {
        let service_generations = {
            let slot = state.pending_initial.lock().unwrap();
            let Some(pending) = slot
                .as_ref()
                .filter(|pending| pending.choice_id == body.choice_id)
            else {
                return initial_choice_error("no pending decision");
            };
            if pending.choice.is_some() {
                return initial_choice_error("initial decision is already resolved");
            }
            pending.service_generations.clone()
        };
        if let Err(error) = revalidate_initial_service_generations(
            state.canonical_project.as_path(),
            &service_generations,
        ) {
            {
                let mut slot = state.pending_initial.lock().unwrap();
                if slot.as_ref().is_some_and(|pending| {
                    pending.choice_id == body.choice_id && pending.choice.is_none()
                }) {
                    *slot = None;
                }
            }
            clear_completed_initial_compare_for_choice(
                state.canonical_project.as_path(),
                &body.choice_id,
            );
            let event = json!({
                "type": "initial-choice-stale",
                "choiceId": body.choice_id,
                "error": error,
            });
            if let Ok(serialized) = serde_json::to_string(&event) {
                let _ = state.events.send(serialized);
            }
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "stale": true,
                    "error": error,
                })),
            );
        }
    }

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
        persist_initial_choice_default(&state, choice);
    }
    initial_choice_ok(json!({ "ok": true }))
}

/// Record a full studio/disk decision in `ro-sync.json` so future compares
/// can auto-answer. Cancel means "ask me again" and clears nothing; the
/// selective-disk path never reaches here (one-off pulls are not a default).
fn persist_initial_choice_default(state: &AppState, choice: Choice) {
    let value = match choice {
        Choice::Studio => "studio",
        Choice::Disk => "disk",
        Choice::Cancel => return,
    };
    {
        let mut slot = state.initial_choice_default.write().unwrap();
        if slot.as_deref() == Some(value) {
            return;
        }
        *slot = Some(value.to_string());
    }
    let root = state.canonical_project.as_ref().clone();
    match crate::project_config::read_from_disk(&root) {
        Ok(Some(mut cfg)) => {
            cfg.initial_choice_default = Some(value.to_string());
            if let Err(error) = crate::project_config::write(&root, &cfg) {
                eprintln!("failed to persist initialChoiceDefault: {error}");
            }
        }
        Ok(None) => {}
        Err(error) => eprintln!("failed to reread project config: {error}"),
    }
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
        .recv_timeout(STREAM_WORKER_POLL_BUDGET);
    match result {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(false),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
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
        .recv_timeout(STREAM_WORKER_POLL_BUDGET);
    match result {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(false),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
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
    // Incremental size accounting: measure the envelope once, then charge
    // each record its own encoded length plus one separator byte. This is a
    // conservative overestimate of the real encoding (the envelope is
    // measured with the longer `finalChunk: false` variant and the first
    // array element has no separator), so nothing the old re-encode-per-
    // element loop would have rejected is accepted; the exact final check
    // below is unchanged.
    let envelope = encoded_stream_response_len(&structure_stream_response(
        stream_id,
        &stream.service,
        chunk_index,
        false,
        &[],
    ))?;
    let mut encoded = envelope;
    let mut count = 0usize;
    while stream.record_offset + count < stream.records.len()
        && count < STREAM_STRUCTURE_CHUNK_NODES
    {
        let record = &stream.records[stream.record_offset + count];
        let record_bytes = serde_json::to_vec(record)
            .map_err(|error| format!("encode structure record: {error}"))?
            .len();
        let candidate = encoded
            .checked_add(record_bytes)
            .and_then(|total| total.checked_add(1))
            .ok_or("encoded structure response size overflowed")?;
        if candidate > STREAM_RESPONSE_PACK_TARGET {
            break;
        }
        encoded = candidate;
        count += 1;
    }
    if count == 0 {
        return Err(format!(
            "one structure record for {} exceeds the encoded response limit",
            stream.service
        ));
    }
    let chunk = &stream.records[stream.record_offset..stream.record_offset + count];
    let final_chunk = stream.record_offset + count == stream.records.len();
    let response =
        structure_stream_response(stream_id, &stream.service, chunk_index, final_chunk, chunk);
    stream.record_offset += count;
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

    // Incremental size accounting mirroring produce_structure_response: the
    // envelope is measured once and each part charges its encoded length plus
    // one separator byte, a conservative overestimate of the real encoding.
    let envelope = encoded_stream_response_len(&source_stream_response(
        stream_id,
        &stream.service,
        chunk_index,
        false,
        &[],
        false,
    ))?;
    let mut encoded = envelope;
    let mut parts = Vec::new();
    while parts.len() < STREAM_SOURCE_PART_CHUNK_NODES
        && stream.source_index < stream.source_ids.len()
    {
        load_pull_source(stream)?;
        let part = pull_source_part(
            stream
                .active_source
                .as_ref()
                .expect("source loader initialized the active Source"),
        );
        let part_bytes = serde_json::to_vec(&part)
            .map_err(|error| format!("encode Source part: {error}"))?
            .len();
        let candidate = encoded
            .checked_add(part_bytes)
            .and_then(|total| total.checked_add(1))
            .ok_or("encoded Source response size overflowed")?;
        if candidate > STREAM_RESPONSE_PACK_TARGET {
            if parts.is_empty() {
                return Err(format!(
                    "one Source part for stream ID {} exceeds the encoded response limit",
                    part.id
                ));
            }
            break;
        }
        encoded = candidate;
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
    // Incremental size accounting mirroring produce_structure_response: the
    // envelope is measured once and each delete record charges its encoded
    // length plus one separator byte (a conservative overestimate).
    let envelope = encoded_stream_response_len(&delete_stream_response(
        stream_id,
        &stream.service,
        chunk_index,
        false,
        &[],
        false,
    ))?;
    let mut encoded = envelope;
    let mut count = 0usize;
    while stream.delete_offset + count < stream.deletes.len()
        && count < STREAM_STRUCTURE_CHUNK_NODES
    {
        let path = &stream.deletes[stream.delete_offset + count];
        let record_bytes = serde_json::to_vec(&json!({ "path": path, "pathMode": "generated" }))
            .map_err(|error| format!("encode delete record: {error}"))?
            .len();
        let candidate = encoded
            .checked_add(record_bytes)
            .and_then(|total| total.checked_add(1))
            .ok_or("encoded delete response size overflowed")?;
        if candidate > STREAM_RESPONSE_PACK_TARGET {
            break;
        }
        encoded = candidate;
        count += 1;
    }
    if count == 0 && stream.delete_offset < stream.deletes.len() {
        return Err(format!(
            "one delete record for {} exceeds the encoded response limit",
            stream.service
        ));
    }
    let chunk = stream.deletes[stream.delete_offset..stream.delete_offset + count].to_vec();
    stream.delete_offset += count;
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
    // Snapshot streaming continuations do synchronous disk reads, packing,
    // and bounded worker waits; keep them off the async worker threads.
    match run_handler_blocking(move || snapshot_stream_blocking(&state, body)).await {
        Ok(response) => response,
        Err(error) => Json(json!({ "ok": false, "error": error })),
    }
}

fn snapshot_stream_blocking(state: &AppState, body: SnapshotStreamBody) -> Json<Value> {
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
    #[serde(rename = "choiceId", default)]
    choice_id: Option<String>,
    /// Apply the comparison-authorized changed-Source set without exporting
    /// every watched service again.
    #[serde(rename = "initialDelta", default)]
    initial_delta: bool,
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
const STREAM_COMPARE_HASH_CHUNK_NODES: usize = 512;
const STREAM_SOURCE_PART_CHUNK_NODES: usize = 512;
// One request absorbs several worker polling ticks. These handlers execute on
// the blocking pool, so this wait does not stall an async runtime worker.
const STREAM_WORKER_POLL_BUDGET: Duration = Duration::from_millis(200);
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExactTreeFingerprint {
    metadata: crate::fs_safety::TreeGeneration,
    content_hash: crate::conflict::Hash,
}

#[derive(Debug)]
struct PreparedStreamBaseline {
    path: PathBuf,
    source_hash: crate::conflict::Hash,
    fs_mtime: u64,
}

#[derive(Debug)]
struct StreamCommitResult {
    applied: usize,
    backup: Option<PathBuf>,
    created: bool,
    installed_fingerprint: ExactTreeFingerprint,
    baselines: Vec<PreparedStreamBaseline>,
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
    #[serde(skip)]
    installed_fingerprint: ExactTreeFingerprint,
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
    rollback_state: AppState,
    stream_id: String,
    choice_id: Option<String>,
    expected_service_generations: Vec<crate::fs_safety::TreeGeneration>,
    strict: bool,
    force_prune: bool,
    next_service: usize,
    service_stream: PushServiceStream,
    applied: usize,
    backups: Vec<PathBuf>,
    committed_services: Vec<CommittedStreamService>,
    prepared_baselines: Vec<PreparedStreamBaseline>,
    conflict_checkpoint: Option<crate::conflict::ConflictCheckpoint>,
    accepted_stream_bytes: usize,
    accepted_source_bytes: u64,
    last_request_hash: Option<crate::conflict::Hash>,
    last_response: Option<Value>,
    last_activity: Instant,
    completed_at: Option<Instant>,
}

impl Drop for PushStreamAccumulator {
    fn drop(&mut self) {
        if self.conflict_checkpoint.is_none() {
            return;
        }

        // Cancel a worker that has not crossed its commit fence. If it already
        // committed while the session was being evicted, recover its receipt
        // before rolling back the generation.
        let (current_committed, current_partial_failure, current_retained_backup) = self
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
        let mut receipt_error = None;
        if current_committed || current_partial_failure {
            if let Some(receiver) = self.service_stream.commit_result.take() {
                match receiver.recv_timeout(Duration::from_secs(5)) {
                    Ok(Ok(result)) => {
                        let service = self.service_stream.service.clone();
                        retain_stream_commit_result(self, &service, result);
                    }
                    Ok(Err(error)) => receipt_error = Some(error),
                    Err(error) => {
                        receipt_error = Some(format!("recover committed service receipt: {error}"));
                    }
                }
            } else {
                receipt_error = Some("committed service receipt is missing".into());
            }
        }

        let state = self.rollback_state.clone();
        let mut report = rollback_stream_generation(&state, self);
        if let Some(backup) = current_retained_backup.as_ref() {
            if !self.backups.contains(backup) {
                self.backups.push(backup.clone());
            }
        }
        if let Some(error) = receipt_error {
            report.errors.push(error);
        }
        let recovery_required = current_partial_failure
            || current_retained_backup.is_some()
            || !self.committed_services.is_empty()
            || !report.errors.is_empty();
        let event = json!({
            "type": "stream-push-abandoned",
            "streamId": self.stream_id,
            "rolledBackServices": report.rolled_back_services,
            "rollbackWarnings": report.warnings,
            "rollbackErrors": report.errors,
            "partialFailure": current_partial_failure,
            "backups": self.backups,
            "recoveryRequired": recovery_required,
        });
        if let Ok(serialized) = serde_json::to_string(&event) {
            let _ = state.events.send(serialized);
        }
    }
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
    // One batch-scoped validation cache for the whole tree walk; every reuse
    // is still fenced by the per-directory generation check inside the cache.
    let mut validation = crate::fs_safety::SyncedPathValidationCache::new(project_root)
        .map_err(|error| format!("validate fingerprint root: {error}"))?;
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
        let bytes = read_synced_file_cached(&entry.path, &mut validation)?;
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
    let mut source_validation = crate::fs_safety::SyncedPathValidationCache::new(project_root)
        .map_err(|error| format!("validate staging source root: {error}"))?;
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
        let source_guard = crate::fs_safety::guard_synced_parent_chain_cached(
            &mut source_validation,
            &entry.path,
            false,
        )
        .map_err(|error| format!("guard staged source {}: {error}", entry.path.display()))?;
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
        // No per-file fsync here: the stage is a scratch tree whose crash
        // semantics come from the atomic rename at commit; the whole stage is
        // flushed once below.
        drop(destination);
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
    // One flush for the whole staged service replaces the removed per-file
    // fsyncs. Best effort: rename atomicity, the staging transaction, and the
    // metadata fences provide the crash/consistency guarantees.
    sync_directory_best_effort(&stage_service);
    Ok(hasher.finalize().into())
}

/// Best-effort directory flush used after batch writes replaced their
/// per-file `sync_all` calls. Opening a directory for read is Unix-only;
/// elsewhere (and on failure) this is a no-op because correctness comes from
/// atomic renames and generation fences, not from durability of scratch trees.
fn sync_directory_best_effort(directory: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = std::fs::File::open(directory) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = directory;
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

include!("http_apply.rs");

#[cfg(test)]
#[path = "http_tests.rs"]
mod http_tests;
