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

#[test]
fn initial_compare_marks_projection_migrations_terminal() {
    let response = initial_compare_error_value(
            "streamed snapshot compare ReplicatedStorage",
            "legacy leaf script ReplicatedStorage/Misc/init (Notice).luau uses the reserved init-marker filename grammar; rename it to ReplicatedStorage/Misc/%69nit (Notice).luau before syncing",
        );
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], PROJECTION_MIGRATION_REQUIRED_CODE);
    assert_eq!(response["retryable"], false);
    assert!(response["error"]
        .as_str()
        .is_some_and(|error| error.contains("%69nit (Notice).luau")));
}

#[test]
fn initial_compare_keeps_transient_errors_retryable_by_omission() {
    let response =
        initial_compare_error_value("streamed snapshot compare Workspace", "disk changed");
    assert_eq!(response["ok"], false);
    assert_eq!(
        response["error"],
        "streamed snapshot compare Workspace: disk changed"
    );
    assert!(response.get("code").is_none());
    assert!(response.get("retryable").is_none());
}

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
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
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
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
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
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
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
        initial_choice_default: Arc::new(RwLock::new(None)),
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
        service_generations: Vec::new(),
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
        choice_id: Some(stream_id.to_string()),
        service: Some(service.to_string()),
        phase: Some(phase.to_string()),
        chunk_index: Some(chunk_index),
        final_chunk,
        records,
        sources,
    }
}

fn authorize_studio_push_test(state: &AppState, choice_id: &str) {
    let service_generations =
        capture_initial_service_generations(state.canonical_project.as_path()).unwrap();
    STUDIO_TRANSFER_GRANTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(
            (
                state.canonical_project.as_ref().clone(),
                choice_id.to_string(),
            ),
            StudioTransferGrant {
                service_generations,
                created_at: Instant::now(),
            },
        );
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
        match response["phase"].as_str() {
            Some("identities") if response["finalChunk"] == true => {
                assert_eq!(response["nextPhase"], "hashes", "{response}");
                assert_eq!(response["nextChunk"], 0, "{response}");
                response["phase"] = Value::String("hashes".into());
                return response;
            }
            Some("diskPrepare" | "identities") => {}
            _ => return response,
        }
        let chunk_index = response["nextChunk"].as_u64().unwrap();
        let phase = response["phase"].as_str().unwrap().to_string();
        let request = || InitialCompareBody {
            studio_stats,
            studio_snapshot: Vec::new(),
            compare_id: Some(compare_id.to_string()),
            service: Some(service.to_string()),
            plugin_protocol: Some(crate::ws::PLUGIN_PROTOCOL_VERSION),
            phase: Some(phase.clone()),
            chunk_index: Some(chunk_index),
            final_chunk: false,
            records: Vec::new(),
            hashes: Vec::new(),
        };
        response = initial_compare(State(state.clone()), Json(request()))
            .await
            .0;
        let replay = initial_compare(State(state.clone()), Json(request()))
            .await
            .0;
        assert_eq!(
            replay, response,
            "initial compare exact cursor retry diverged"
        );
        assert_ne!(response["ok"], false, "{response}");
        assert!(
            serde_json::to_vec(&response).unwrap().len() <= STREAM_SOURCE_CHUNK_BYTES,
            "initial compare response exceeded 512 KiB"
        );
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

    let (_, _, response) = artifact_json_request(&app, Method::POST, "/manager-close", body).await;

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
    assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(hello["buildCommit"], env!("ROSYNC_BUILD_COMMIT"));
    assert_eq!(
        hello["buildDirty"],
        Value::Bool(env!("ROSYNC_BUILD_DIRTY") == "true")
    );
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

    let created = project_init_inner(&state, &serde_json::to_vec(&request).unwrap(), None).0;
    assert_eq!(created["ok"], true);
    assert_eq!(created["status"], "created");
    assert_eq!(created["directoryName"], "race-stars");
    assert_eq!(created["name"], "Race Stars - Main Place");
    let created_path = PathBuf::from(created["project"].as_str().unwrap());
    assert_eq!(created_path.parent(), Some(canonical_projects.as_path()));
    assert!(created_path
        .join(crate::project_config::CONFIG_FILE)
        .is_file());

    let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
    assert_eq!(event["type"], "project-init");
    assert_eq!(event["status"], "created");
    assert_eq!(event["name"], "Race Stars - Main Place");
    assert_eq!(event["metadata"]["gameId"], "123");

    let existing = project_init_inner(&state, &serde_json::to_vec(&request).unwrap(), None).0;
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
        artifact_json_request(&app, Method::GET, &format!("/artifacts/{id}"), Value::Null).await;
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
        artifact_json_request(&app, Method::GET, &format!("/artifacts/{id}"), Value::Null).await;
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
fn delete_prunes_the_directory_it_empties() {
    // Regression: deleting the last script under `MarketService/Products/`
    // used to leave `Products/` behind. The scanner ignores empty plain
    // directories, so MarketService stayed physically directory-form while
    // projecting zero children — a state the comparison protocol rejects
    // and no later Studio delete can clear (an empty dir is not
    // sync-owned), wedging every reconnect.
    let d = TempDir::new("prune-emptied");
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());

    let market = d.path().join("ServerScriptService/Server/MarketService");
    let products = market.join("Products");
    std::fs::create_dir_all(&products).unwrap();
    std::fs::write(market.join("init (MarketService).luau"), b"-- market").unwrap();
    let buy = products.join("Buy.luau");
    std::fs::write(&buy, b"-- buy").unwrap();
    // Without an agreed baseline the delete parks as a conflict and the
    // pruning below is never reached.
    engine.record_sync(&buy, hash(b"-- buy"), 1);

    let out = apply_delete(
        d.path(),
        &[
            "ServerScriptService".into(),
            "Server".into(),
            "MarketService".into(),
            "Products".into(),
            "Buy".into(),
        ],
        &ctx,
    )
    .unwrap();

    assert!(matches!(out, ApplyOutcome::Applied(_)));
    assert!(!products.exists(), "emptied Products/ must be pruned");
    assert!(
        market.join("init (MarketService).luau").exists(),
        "pruning must stop at the first non-empty ancestor"
    );
    assert!(market.exists());
}

#[test]
fn prune_stops_at_service_root_and_foreign_entries() {
    let d = TempDir::new("prune-bounds");
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());

    // Deleting the only script directly under a service must leave the
    // service directory itself in place.
    let service = d.path().join("ServerScriptService");
    std::fs::create_dir_all(&service).unwrap();
    let solo = service.join("Solo.luau");
    std::fs::write(&solo, b"-- solo").unwrap();
    engine.record_sync(&solo, hash(b"-- solo"), 1);
    let out = apply_delete(
        d.path(),
        &["ServerScriptService".into(), "Solo".into()],
        &ctx,
    )
    .unwrap();
    // Assert the delete actually applied. Without this the pruning bounds
    // below hold vacuously, because a parked conflict removes nothing.
    assert!(matches!(out, ApplyOutcome::Applied(_)));
    assert!(!solo.exists());
    assert!(service.exists(), "service root must survive an empty tree");

    // A directory still holding a file this sync does not own is not
    // pruned, even though that file never projects.
    let keep = service.join("Nested");
    std::fs::create_dir_all(&keep).unwrap();
    let gone = keep.join("Gone.luau");
    std::fs::write(&gone, b"-- gone").unwrap();
    std::fs::write(keep.join("notes.md"), b"mine").unwrap();
    engine.record_sync(&gone, hash(b"-- gone"), 1);
    let out = apply_delete(
        d.path(),
        &["ServerScriptService".into(), "Nested".into(), "Gone".into()],
        &ctx,
    )
    .unwrap();
    assert!(matches!(out, ApplyOutcome::Applied(_)));
    assert!(!gone.exists());
    assert!(
        keep.join("notes.md").exists(),
        "foreign files are never removed"
    );
    assert!(keep.exists(), "a non-empty directory is never pruned");
}

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
fn script_with_children_rename_ignores_mismatched_named_init_leaf() {
    let d = TempDir::new("rename-script-dir-with-init-leaf");
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());
    let parent = d.path().join("ReplicatedStorage");
    let old_path = parent.join("Old");
    let new_path = parent.join("New");
    std::fs::create_dir_all(&old_path).unwrap();
    std::fs::write(old_path.join("init (Old).luau"), "return 'parent'\n").unwrap();
    std::fs::write(
        old_path.join("init (Notifications).luau"),
        "return 'literal child'\n",
    )
    .unwrap();

    rename_path_and_init(&old_path, &new_path, "New", true, &ctx).unwrap();

    assert_eq!(
        std::fs::read_to_string(new_path.join("init (New).luau")).unwrap(),
        "return 'parent'\n"
    );
    assert_eq!(
        std::fs::read_to_string(new_path.join("init (Notifications).luau")).unwrap(),
        "return 'literal child'\n"
    );
}

#[test]
fn equivalent_legacy_parent_marker_is_migrated_by_atomic_parent_rename() {
    let d = TempDir::new("rename-equivalent-legacy-init");
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());
    let parent = d.path().join("ReplicatedStorage");
    let old_path = parent.join("notifications");
    let new_path = parent.join("Alerts");
    let old_marker = old_path.join("INIT (Notifications).luau");
    std::fs::create_dir_all(&old_path).unwrap();
    std::fs::write(&old_marker, "return 42\n").unwrap();

    rename_path_and_init(&old_path, &new_path, "Alerts", true, &ctx).unwrap();

    assert!(!old_path.exists());
    assert!(!new_path.join("INIT (Notifications).luau").exists());
    assert_eq!(
        std::fs::read_to_string(new_path.join("init (Alerts).luau")).unwrap(),
        "return 42\n"
    );
}

fn assert_script_rename_checkpoint_rolls_back(
    tag: &str,
    failure: RenamePathAndInitCheckpoint,
    expected_rename_calls: usize,
    expected_init_status: &str,
) {
    let d = TempDir::new(tag);
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());
    let parent = d.path().join("ReplicatedStorage");
    let old_path = parent.join("Old");
    let new_path = parent.join("New");
    let old_init = old_path.join("init (Old).luau");
    std::fs::create_dir_all(&old_path).unwrap();
    std::fs::write(&old_init, "return 'preserved'\n").unwrap();
    std::fs::write(old_path.join("Child.luau"), "return 'child'\n").unwrap();

    let mut rename_calls = 0usize;
    let error = rename_path_and_init_with_checkpoints(
        &old_path,
        &new_path,
        "New",
        true,
        &ctx,
        |from, to| {
            rename_calls += 1;
            std::fs::rename(from, to)
        },
        |checkpoint| {
            if checkpoint == failure {
                Err(format!("injected checkpoint failure: {checkpoint:?}"))
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();

    assert!(error.contains("injected checkpoint failure"), "{error}");
    assert!(
        error.contains(&format!("init rollback: {expected_init_status}")),
        "{error}"
    );
    assert!(error.contains("outer rollback: ok"), "{error}");
    assert_eq!(rename_calls, expected_rename_calls);
    assert!(old_path.is_dir());
    assert!(!new_path.exists());
    assert_eq!(
        std::fs::read_to_string(&old_init).unwrap(),
        "return 'preserved'\n"
    );
    assert_eq!(
        std::fs::read_to_string(old_path.join("Child.luau")).unwrap(),
        "return 'child'\n"
    );
    assert!(std::fs::read_dir(&old_path).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".rosync-init-rename-")));
}

#[test]
fn script_with_children_rename_rolls_back_after_post_outer_check_failure() {
    assert_script_rename_checkpoint_rolls_back(
        "rename-post-outer-check-rollback",
        RenamePathAndInitCheckpoint::PostOuterSourceParentVerify,
        2,
        "not needed",
    );
}

#[test]
fn script_with_children_rename_rolls_back_after_destination_inspection_failure() {
    assert_script_rename_checkpoint_rolls_back(
        "rename-destination-inspection-rollback",
        RenamePathAndInitCheckpoint::DestinationMetadataInspect,
        4,
        "ok",
    );
}

#[test]
fn script_with_children_rename_rolls_back_after_final_verify_failure() {
    assert_script_rename_checkpoint_rolls_back(
        "rename-final-verify-rollback",
        RenamePathAndInitCheckpoint::FinalMovedDirectoryVerify,
        5,
        "ok",
    );
}

#[test]
fn rename_rollback_accepts_a_destination_that_is_the_same_physical_entry() {
    let d = TempDir::new("rename-same-entry-destination");
    let current = d.path().join("Current.luau");
    let alias = d.path().join("Alias.luau");
    let occupied = d.path().join("Occupied.luau");
    std::fs::write(&current, "return true\n").unwrap();
    std::fs::hard_link(&current, &alias).unwrap();
    std::fs::write(&occupied, "return false\n").unwrap();

    assert!(paths_refer_to_same_entry(&alias, &current));
    assert!(rollback_destination_is_available(&alias, &current).is_ok());
    assert!(rollback_destination_is_available(&occupied, &current).is_err());
}

#[test]
fn case_only_script_rename_rolls_back_after_final_verify_failure() {
    let d = TempDir::new("rename-case-only-rollback");
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());
    let parent = d.path().join("ReplicatedStorage");
    let old_path = parent.join("Controller");
    let new_path = parent.join("controller");
    let old_init = old_path.join("init (Controller).luau");
    std::fs::create_dir_all(&old_path).unwrap();
    std::fs::write(&old_init, "return 'preserved'\n").unwrap();

    let error = rename_path_and_init_with_checkpoints(
        &old_path,
        &new_path,
        "controller",
        true,
        &ctx,
        |from, to| std::fs::rename(from, to),
        |checkpoint| {
            if checkpoint == RenamePathAndInitCheckpoint::FinalMovedDirectoryVerify {
                Err("injected case-only final verification failure".to_string())
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();

    assert!(error.contains("init rollback: ok"), "{error}");
    assert!(error.contains("outer rollback: ok"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&old_init).unwrap(),
        "return 'preserved'\n"
    );
}

#[test]
fn script_with_children_rename_refuses_rollback_of_a_changed_relocated_tree() {
    let d = TempDir::new("rename-changed-tree-refusal");
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());
    let parent = d.path().join("ReplicatedStorage");
    let old_path = parent.join("Old");
    let new_path = parent.join("New");
    std::fs::create_dir_all(&old_path).unwrap();
    std::fs::write(old_path.join("init (Old).luau"), "return 'preserved'\n").unwrap();

    let error = rename_path_and_init_with_checkpoints(
        &old_path,
        &new_path,
        "New",
        true,
        &ctx,
        |from, to| std::fs::rename(from, to),
        |checkpoint| {
            if checkpoint == RenamePathAndInitCheckpoint::PostOuterSourceParentVerify {
                std::fs::write(new_path.join("Concurrent.luau"), "return 'new edit'\n").unwrap();
                Err("injected post-outer failure after concurrent edit".to_string())
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();

    assert!(error.contains("init rollback: refused"), "{error}");
    assert!(error.contains("outer rollback: refused"), "{error}");
    assert!(!old_path.exists());
    assert_eq!(
        std::fs::read_to_string(new_path.join("Concurrent.luau")).unwrap(),
        "return 'new edit'\n"
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

    let deleted = apply_delete(d.path(), &["ServerStorage".into(), "New".into()], &ctx).unwrap();
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
    let error = rename_path_and_init_with(&old_path, &new_path, "New", true, &ctx, |from, to| {
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

    assert!(error.contains("init rollback: ok"), "{error}");
    assert!(error.contains("outer rollback: ok"), "{error}");
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

    restore_fs_rename_transactional(&from, &to, &conflict_path, b"return 'studio edit'\n", &ctx)
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
    let to_guard = crate::fs_safety::guard_synced_directory_chain(project.path(), &parent).unwrap();

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
    assert_eq!(source_restore["type"], "op");
    assert!(source_restore["seq"].is_number());
    assert_eq!(source_restore["op"]["op"], "update");
    assert_eq!(source_restore["op"]["path"], json!(["Workspace", "New"]));
    assert_eq!(
        source_restore["op"]["properties"]["Source"],
        "studio edit\n"
    );

    let reverse: Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
    assert_eq!(reverse["type"], "op");
    assert!(reverse["seq"].is_number());
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
fn fs_mismatched_legacy_init_update_is_rejected_until_canonicalized() {
    let d = TempDir::new("fs-legacy-init-leaf-op");
    let misc = d.path().join("ReplicatedStorage").join("Misc");
    std::fs::create_dir_all(&misc).unwrap();
    std::fs::write(misc.join("init (Misc).luau"), "return 'parent'\n").unwrap();
    let literal = misc.join("init (Notifications).luau");
    let source = "return 'literal child'\n";
    std::fs::write(&literal, source).unwrap();
    let op = Op {
        kind: OpKind::Update,
        path: literal,
        from: None,
        content: Some(source.as_bytes().to_vec()),
        is_dir: Some(false),
    };

    assert!(
        fs_op_to_plugin_op(d.path(), &op).is_none(),
        "a raw reserved leaf must not cross the live transport"
    );
}

#[test]
fn fs_delete_of_parent_init_reprojects_parent_as_folder() {
    let d = TempDir::new("fs-parent-init-delete");
    let misc = d.path().join("ReplicatedStorage").join("Misc");
    std::fs::create_dir_all(&misc).unwrap();
    let parent_source = misc.join("init (Misc).luau");
    let literal = misc.join("init (Notifications).luau");
    std::fs::write(&parent_source, "return 'parent'\n").unwrap();
    std::fs::write(&literal, "return 'literal child'\n").unwrap();
    std::fs::remove_file(&parent_source).unwrap();
    let op = Op {
        kind: OpKind::Delete,
        path: parent_source,
        from: None,
        content: None,
        is_dir: Some(false),
    };

    let plugin_op = fs_op_to_plugin_op(d.path(), &op).expect("parent projection op");

    assert_eq!(plugin_op["op"], "set");
    assert_eq!(plugin_op["path"], serde_json::json!(["ReplicatedStorage"]));
    assert_eq!(
        plugin_op["diskPath"],
        serde_json::json!(["ReplicatedStorage", "Misc"])
    );
    assert_eq!(plugin_op["node"]["class"], "Folder");
    assert_eq!(plugin_op["node"]["name"], "Misc");
}

#[test]
fn fs_rename_of_parent_init_to_canonical_reserved_leaf_expands_to_two_wire_ops() {
    let d = TempDir::new("fs-parent-init-rename-leaf");
    let misc = d.path().join("ReplicatedStorage").join("Misc");
    std::fs::create_dir_all(&misc).unwrap();
    let from = misc.join("init (Misc).luau");
    let to = misc.join("%69nit (Notifications).luau");
    let source = "return 'renamed source'\n";
    std::fs::write(&from, source).unwrap();
    std::fs::rename(&from, &to).unwrap();
    let op = Op {
        kind: OpKind::Rename,
        path: to,
        from: Some(from),
        content: Some(source.as_bytes().to_vec()),
        is_dir: Some(false),
    };

    let plugin_op = fs_op_to_plugin_op(d.path(), &op).expect("carrier transition batch");

    assert_eq!(plugin_op["op"], "batch");
    let ops = plugin_op["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["op"], "set");
    assert_eq!(ops[0]["node"]["class"], "Folder");
    assert_eq!(ops[0]["node"]["name"], "Misc");
    assert_eq!(ops[1]["op"], "set");
    assert_eq!(ops[1]["node"]["name"], "init (Notifications)");
    assert_eq!(ops[1]["node"]["class"], "ModuleScript");
    assert_eq!(ops[1]["node"]["properties"]["Source"], source);

    let event = serde_json::json!({ "type": "op", "op": op }).to_string();
    let wire_ops = event_to_plugin_ops(d.path(), &event);
    assert_eq!(wire_ops.len(), 2);
    assert!(wire_ops.iter().all(|wire_op| wire_op["op"] != "batch"));
}

#[test]
fn fs_rename_of_parent_init_to_raw_reserved_leaf_is_rejected() {
    let d = TempDir::new("fs-parent-init-rename-legacy-leaf");
    let misc = d.path().join("ReplicatedStorage").join("Misc");
    std::fs::create_dir_all(&misc).unwrap();
    let from = misc.join("init (Misc).luau");
    let to = misc.join("init (Notifications).luau");
    let source = "return 'renamed source'\n";
    std::fs::write(&from, source).unwrap();
    std::fs::rename(&from, &to).unwrap();
    let op = Op {
        kind: OpKind::Rename,
        path: to,
        from: Some(from),
        content: Some(source.as_bytes().to_vec()),
        is_dir: Some(false),
    };

    assert!(fs_op_to_plugin_op(d.path(), &op).is_none());
    let event = serde_json::json!({ "type": "op", "op": op }).to_string();
    assert!(event_to_plugin_ops(d.path(), &event).is_empty());
}

#[test]
fn sequenced_journal_event_preserves_plugin_shaped_batch_operations() {
    let event = serde_json::json!({
        "type": "op",
        "seq": 42,
        "op": {
            "op": "batch",
            "ops": [
                { "op": "delete", "path": ["Workspace", "Old"] },
                { "op": "set", "path": ["Workspace", "New"], "node": { "class": "Folder", "name": "New" } }
            ]
        }
    })
    .to_string();

    let ops = event_to_plugin_ops(Path::new("C:/unused"), &event);
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["op"], "delete");
    assert_eq!(ops[1]["op"], "set");
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
fn exact_update_targets_script_with_children_init_source() {
    let d = TempDir::new("studio-update-script-directory");
    let controller = d.path().join("Workspace").join("Controller");
    std::fs::create_dir_all(&controller).unwrap();
    let init = controller.join("init (Controller).luau");
    let child = controller.join("Child.luau");
    std::fs::write(&init, "return 'old root'\n").unwrap();
    std::fs::write(&child, "return 'child'\n").unwrap();
    let engine = ConflictEngine::new();
    engine.record_sync(&init, hash(b"return 'old root'\n"), fs_mtime(&init));
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());

    let outcome = apply_op(
        d.path(),
        &serde_json::json!({
            "op": "update",
            "path": ["Workspace", "Controller"],
            "diskPath": ["Workspace", "Controller"],
            "properties": { "Source": "return 'updated root'\r\n" }
        }),
        &ctx,
    )
    .unwrap();

    assert!(matches!(outcome, ApplyOutcome::Applied(1)));
    assert_eq!(
        std::fs::read_to_string(init).unwrap(),
        "return 'updated root'\n"
    );
    assert_eq!(std::fs::read_to_string(child).unwrap(), "return 'child'\n");
}

#[test]
fn exact_update_reuses_legacy_unicode_named_init_source() {
    let d = TempDir::new("studio-update-legacy-unicode-init");
    let controller = d.path().join("Workspace").join("É");
    std::fs::create_dir_all(&controller).unwrap();
    let legacy_init = controller.join("init (É).luau");
    std::fs::write(&legacy_init, "return 'old'\n").unwrap();
    let engine = ConflictEngine::new();
    engine.record_sync(
        &legacy_init,
        hash(b"return 'old'\n"),
        fs_mtime(&legacy_init),
    );
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());

    let outcome = apply_op(
        d.path(),
        &serde_json::json!({
            "op": "update",
            "path": ["Workspace", "É"],
            "diskPath": ["Workspace", "É", "init (%C3%89).luau"],
            "properties": { "Source": "return 'updated'\n" }
        }),
        &ctx,
    )
    .unwrap();

    assert!(matches!(outcome, ApplyOutcome::Applied(1)));
    assert_eq!(
        std::fs::read_to_string(&legacy_init).unwrap(),
        "return 'updated'\n"
    );
    assert!(
        !controller.join("init (%C3%89).luau").exists(),
        "update must reuse the unique legacy marker"
    );
}

#[test]
fn exact_updates_reuse_case_and_normalization_equivalent_parent_markers() {
    for (tag, directory_fragment, studio_name, marker_fragment, requested_fragment) in [
        (
            "case",
            "notifications",
            "notifications",
            "INIT (Notifications).luau",
            "init (notifications).luau",
        ),
        (
            "normalization",
            "%C3%89",
            "\u{00c9}",
            "init (E\u{0301}).luau",
            "init (%C3%89).luau",
        ),
    ] {
        let d = TempDir::new(tag);
        let directory = d.path().join("Workspace").join(directory_fragment);
        std::fs::create_dir_all(&directory).unwrap();
        let legacy_marker = directory.join(marker_fragment);
        std::fs::write(&legacy_marker, "return 'old'\n").unwrap();
        let engine = ConflictEngine::new();
        engine.record_sync(
            &legacy_marker,
            hash(b"return 'old'\n"),
            fs_mtime(&legacy_marker),
        );
        let quiet = push_quiet();
        let ctx = harness(&engine, &quiet, d.path());

        let outcome = apply_op(
            d.path(),
            &serde_json::json!({
                "op": "update",
                "path": ["Workspace", studio_name],
                "diskPath": ["Workspace", directory_fragment, requested_fragment],
                "properties": { "Source": "return 'updated'\n" }
            }),
            &ctx,
        )
        .unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied(1)), "{tag}");
        assert_eq!(
            std::fs::read_to_string(&legacy_marker).unwrap(),
            "return 'updated'\n",
            "{tag}"
        );
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            1,
            "{tag}: update must not create a second marker"
        );
    }
}

#[test]
fn exact_update_reuses_plain_init_source() {
    let d = TempDir::new("studio-update-plain-init");
    let controller = d.path().join("Workspace").join("Controller");
    std::fs::create_dir_all(&controller).unwrap();
    let plain_init = controller.join("init.lua");
    std::fs::write(&plain_init, "return 'old'\n").unwrap();
    let engine = ConflictEngine::new();
    engine.record_sync(&plain_init, hash(b"return 'old'\n"), fs_mtime(&plain_init));
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());

    let outcome = apply_op(
        d.path(),
        &serde_json::json!({
            "op": "update",
            "path": ["Workspace", "Controller"],
            "diskPath": ["Workspace", "Controller", "init (Controller).luau"],
            "properties": { "Source": "return 'updated'\n" }
        }),
        &ctx,
    )
    .unwrap();

    assert!(matches!(outcome, ApplyOutcome::Applied(1)));
    assert_eq!(
        std::fs::read_to_string(&plain_init).unwrap(),
        "return 'updated'\n"
    );
    assert!(
        !controller.join("init (Controller).luau").exists(),
        "update must not create a second init marker"
    );
}

#[test]
fn source_update_to_plain_folder_is_an_error_not_a_skip() {
    let d = TempDir::new("studio-update-folder-source");
    let folder = d.path().join("Workspace").join("Misc");
    std::fs::create_dir_all(&folder).unwrap();
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());
    let mut result = PushApplyResult::default();

    apply_ops_into(
        d.path(),
        &[serde_json::json!({
            "op": "update",
            "path": ["Workspace", "Misc"],
            "diskPath": ["Workspace", "Misc"],
            "properties": { "Source": "return 'must not disappear'\n" }
        })],
        &ctx,
        &mut result,
    );

    assert_eq!(result.applied, 0);
    assert_eq!(result.skipped, 0);
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].contains("not a script-with-children directory"));
}

#[test]
fn source_update_to_missing_exact_path_is_an_error_not_a_skip() {
    let d = TempDir::new("studio-update-missing-source");
    std::fs::create_dir_all(d.path().join("Workspace")).unwrap();
    let engine = ConflictEngine::new();
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());
    let mut result = PushApplyResult::default();

    apply_ops_into(
        d.path(),
        &[serde_json::json!({
            "op": "update",
            "path": ["Workspace", "Missing"],
            "diskPath": ["Workspace", "Missing.luau"],
            "properties": { "Source": "return 'must not disappear'\n" }
        })],
        &ctx,
        &mut result,
    );

    assert_eq!(result.applied, 0);
    assert_eq!(result.skipped, 0);
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].contains("Source target does not exist"));
}

#[test]
fn idempotent_source_update_is_accepted_without_counting_as_skipped() {
    let d = TempDir::new("studio-update-idempotent-source");
    let workspace = d.path().join("Workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = workspace.join("Config.luau");
    std::fs::write(&script, "return 'same'\n").unwrap();
    let engine = ConflictEngine::new();
    engine.record_sync(&script, hash(b"return 'same'\n"), fs_mtime(&script));
    let quiet = push_quiet();
    let ctx = harness(&engine, &quiet, d.path());
    let mut result = PushApplyResult::default();

    apply_ops_into(
        d.path(),
        &[serde_json::json!({
            "op": "update",
            "path": ["Workspace", "Config"],
            "diskPath": ["Workspace", "Config.luau"],
            "properties": { "Source": "return 'same'\r\n" }
        })],
        &ctx,
        &mut result,
    );

    assert_eq!(result.applied, 0);
    assert_eq!(result.skipped, 0);
    assert!(result.errors.is_empty());
    assert!(result.conflicts.is_empty());
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

    let canonical_project = crate::fs_safety::stable_canonical_directory(project.path()).unwrap();
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

    let outcome = apply_delete(d.path(), &["Workspace".into(), "StudioOnly".into()], &ctx).unwrap();

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
        std::fs::read_to_string(d.path().join("Workspace/SellNPC/Animate.client.luau")).unwrap(),
        "animate\n"
    );
    assert_eq!(
        std::fs::read_to_string(stable_duplicate_target.join("DialogueDemo.client.luau")).unwrap(),
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
        choice_id: None,
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
    authorize_studio_push_test(&state, stream_id);
    let source = "return 'studio'\n";
    let source_sha = hash(source.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let mut response = Value::Null;
    for (index, service) in snapshot::SYNCED_SERVICES.iter().copied().enumerate() {
        let records =
            streamed_service_records(service, (index == 0).then_some(("Config", "ModuleScript")));
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
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
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
        rollback_state: state.clone(),
        stream_id: stream_id.into(),
        choice_id: Some(stream_id.into()),
        expected_service_generations: Vec::new(),
        strict: true,
        force_prune: true,
        next_service: 0,
        service_stream,
        applied: 0,
        backups: Vec::new(),
        committed_services: Vec::new(),
        prepared_baselines: Vec::new(),
        conflict_checkpoint: Some(state.conflict.checkpoint()),
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
async fn streamed_push_rolls_back_prior_service_and_baseline_when_later_service_fails() {
    let project = TempDir::new("streamed-push-generation-rollback");
    let state = test_state(&project, None);
    let mut events = state.events.subscribe();
    let stream_id = "generation-rollback";
    let first_service = "ReplicatedStorage";
    let replicated = project.path().join(first_service);
    std::fs::create_dir_all(&replicated).unwrap();
    let config = replicated.join("Config.luau");
    let original = b"return 'original'\n";
    let replacement = "return 'studio generation'\n";
    std::fs::write(&config, original).unwrap();
    state
        .conflict
        .record_sync(&config, hash(original), fs_mtime(&config));
    authorize_studio_push_test(&state, stream_id);
    let replacement_sha = hash(replacement.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let structure = push(
        State(state.clone()),
        Json(streamed_push_test_body(
            stream_id,
            first_service,
            "structure",
            0,
            true,
            streamed_service_records(first_service, Some(("Config", "ModuleScript"))),
            Vec::new(),
        )),
    )
    .await
    .0;
    let sources = advance_streamed_push_worker(&state, stream_id, first_service, structure).await;
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
            vec![StreamSourcePart {
                id: 1,
                part_index: 0,
                offset: 0,
                total_bytes: replacement.len() as u64,
                data: replacement.into(),
                final_part: true,
                sha256: replacement_sha,
            }],
        )),
    )
    .await
    .0;
    let next = advance_streamed_push_worker(&state, stream_id, first_service, revalidate).await;
    assert_eq!(next["nextService"], "ServerScriptService");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), replacement);
    assert!(
        state.conflict.matches_baseline(&config, original),
        "a per-service commit must not publish the generation baseline early"
    );

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
    assert_eq!(response["ok"], false);
    assert_eq!(response["action"], "rolled-back");
    assert_eq!(response["failedService"], "ServerScriptService");
    assert_eq!(response["recoveryRequired"], false);
    assert_eq!(response["backups"], json!([]));
    assert_eq!(response["committedServices"], json!([]));
    assert_eq!(response["rolledBackServices"], json!([first_service]));
    assert_eq!(std::fs::read(&config).unwrap(), original);
    assert!(
        state.conflict.matches_baseline(&config, original),
        "generation rollback must restore the pre-generation baseline"
    );
    assert!(!state
        .conflict
        .matches_baseline(&config, replacement.as_bytes()));

    assert_eq!(
        push(State(state.clone()), Json(invalid)).await.0,
        response,
        "the exact later-service failure must replay its terminal receipt"
    );
    let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
    assert_eq!(event["type"], "stream-push-rolled-back");
    assert_eq!(event["failedService"], "ServerScriptService");
    assert_eq!(event["rolledBackServices"], response["rolledBackServices"]);

    PUSH_STREAM_ACCUMULATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .remove(state.canonical_project.as_path());
}

#[tokio::test]
async fn abandoned_streamed_push_rolls_back_committed_services() {
    let project = TempDir::new("streamed-push-abandoned-rollback");
    let state = test_state(&project, None);
    let mut events = state.events.subscribe();
    let stream_id = "abandoned-generation";
    let first_service = "ReplicatedStorage";
    authorize_studio_push_test(&state, stream_id);

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
    let _sources = advance_streamed_push_worker(&state, stream_id, first_service, structure).await;
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

    let removed = PUSH_STREAM_ACCUMULATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .remove(state.canonical_project.as_path())
        .expect("active streamed generation");
    drop(removed);

    assert!(
        !project.path().join(first_service).exists(),
        "abandoning a generation must remove its newly-created service"
    );
    let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
    assert_eq!(event["type"], "stream-push-abandoned");
    assert_eq!(event["rolledBackServices"], json!([first_service]));
    assert_eq!(event["recoveryRequired"], false);
}

#[test]
fn abandoned_streamed_push_reports_partial_current_service_recovery() {
    let project = TempDir::new("streamed-push-abandoned-partial");
    let state = test_state(&project, None);
    let mut events = state.events.subscribe();
    let retained_backup = project.path().join(".rosync-backups").join("retained");
    std::fs::create_dir_all(&retained_backup).unwrap();
    let control = Arc::new(Mutex::new(StreamCommitControl {
        retained_backup: Some(retained_backup.clone()),
        partial_failure: true,
        ..StreamCommitControl::default()
    }));
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    send.send(Err("injected partial commit".into())).unwrap();
    let mut service_stream = new_push_service_stream("ReplicatedStorage");
    service_stream.phase = PushStreamPhase::DiskRevalidate;
    service_stream.commit_result = Some(receive);
    service_stream.commit_control = Some(control);

    let session = PushStreamAccumulator {
        rollback_state: state.clone(),
        stream_id: "abandoned-partial".into(),
        choice_id: Some("abandoned-partial".into()),
        expected_service_generations: Vec::new(),
        strict: true,
        force_prune: true,
        next_service: 0,
        service_stream,
        applied: 0,
        backups: Vec::new(),
        committed_services: Vec::new(),
        prepared_baselines: Vec::new(),
        conflict_checkpoint: Some(state.conflict.checkpoint()),
        accepted_stream_bytes: 0,
        accepted_source_bytes: 0,
        last_request_hash: None,
        last_response: None,
        last_activity: Instant::now(),
        completed_at: None,
    };
    drop(session);

    let event: Value = serde_json::from_str(&events.try_recv().unwrap()).unwrap();
    assert_eq!(event["type"], "stream-push-abandoned");
    assert_eq!(event["partialFailure"], true);
    assert_eq!(event["backups"], json!([retained_backup]));
    assert_eq!(event["recoveryRequired"], true);
    assert!(event["rollbackErrors"][0]
        .as_str()
        .unwrap()
        .contains("injected partial commit"));
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
    let error = append_source_parts_atomically(&mut service, &mut session_bytes, &[part], false)
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
        std::fs::read_to_string(partial.join("ReplicatedStorage").join("Recovery.luau")).unwrap(),
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
    let partial_generation = crate::fs_safety::directory_generation_no_follow(&partial).unwrap();
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
    write_successful_stream_backup_marker(project.path(), &candidate, "original-stream").unwrap();
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

    let error = cleanup_empty_stream_backup_transaction_with(project.path(), &transaction, || {
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
    authorize_studio_push_test(&state, stream_id);
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
            disk_prewarm: HashMap::new(),
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
        let structure =
            advance_initial_compare_prepare(&state, studio_stats, &compare_id, service, structure)
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
    std::fs::write(
        project.path().join("ReplicatedStorage/DiskOnly.luau"),
        "return true\n",
    )
    .unwrap();
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
    let records = streamed_service_records("ReplicatedStorage", Some(("Config", "ModuleScript")));

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
        let ready =
            advance_initial_compare_prepare(&state, studio_stats, &compare_id, service, structure)
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
    let prepared =
        prepare_streamed_initial_service_comparison(project.path(), studio, None).unwrap();
    assert_eq!(prepared.local_source_paths_by_path.len(), 2_048);
    assert_eq!(prepared.expected_hash_ids.len(), 2_048);
    assert!(prepared
        .local_nodes
        .values()
        .filter(|node| node.kind == diff::DiffKind::Script)
        .all(|node| node.source_hash == Some(hash(b""))));
}

#[test]
fn streamed_initial_compare_receipts_preserve_exact_duplicate_fragments() {
    let project = TempDir::new("initial-compare-exact-duplicate-identities");
    let storage = project.path().join("ReplicatedStorage");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("Same.luau"), "return 'same'\n").unwrap();
    std::fs::write(storage.join("Same [1].luau"), "return 'same'\n").unwrap();

    let disk = snapshot::emit_flat_service(project.path(), "ReplicatedStorage").unwrap();
    let expected = disk
        .records
        .iter()
        .skip(1)
        .map(|record| {
            (
                record.id,
                (
                    record.disk_fragment.clone().unwrap(),
                    record.disk_fragment_is_dir.unwrap(),
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(expected.len(), 2);

    let mut studio_records = disk.records;
    for record in &mut studio_records {
        record.disk_fragment = None;
        record.disk_fragment_is_dir = None;
    }
    let studio = validate_flat_snapshot(&studio_records, "ReplicatedStorage", false).unwrap();
    let prepared =
        prepare_streamed_initial_service_comparison(project.path(), studio, None).unwrap();

    assert!(prepared.identity_complete);
    assert_eq!(prepared.identities.len(), expected.len());
    for identity in prepared.identities {
        let (fragment, is_dir) = expected.get(&identity.id).unwrap();
        assert_eq!(&identity.disk_fragment, fragment);
        assert_eq!(identity.disk_fragment_is_dir, *is_dir);
    }
    assert!(expected
        .values()
        .any(|(fragment, _)| fragment == "Same.luau"));
    assert!(expected
        .values()
        .any(|(fragment, _)| fragment == "Same [1].luau"));
}

#[test]
fn initial_compare_identity_responses_are_count_and_byte_bounded() {
    let wide_fragment = "x".repeat(MAX_STREAM_NAME_BYTES - 3);
    let identities = (1..=40)
        .map(|id| StreamDiskIdentity {
            id,
            disk_fragment: format!("{id:02}-{wide_fragment}"),
            disk_fragment_is_dir: false,
        })
        .collect::<Vec<_>>();
    let mut stream = InitialCompareServiceStream {
        service: "ReplicatedStorage".into(),
        phase: InitialCompareStreamPhase::Identities,
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
        identities,
        identity_offset: 0,
        identity_complete: true,
        expected_hash_ids: HashSet::new(),
        received_hash_ids: HashSet::new(),
    };
    let mut responses = 0u64;
    let mut received = 0usize;
    loop {
        let response =
            produce_initial_compare_identity_response("compare", "ReplicatedStorage", &mut stream)
                .unwrap();
        assert_eq!(response["chunkIndex"], responses);
        assert_eq!(response["identityCount"], 40);
        assert!(
            serde_json::to_vec(&response).unwrap().len() <= STREAM_SOURCE_CHUNK_BYTES,
            "{response}"
        );
        received += response["identities"].as_array().unwrap().len();
        responses += 1;
        if response["finalChunk"] == true {
            assert_eq!(response["nextPhase"], "hashes");
            assert_eq!(response["nextChunk"], 0);
            break;
        }
        assert_eq!(response["nextChunk"], responses);
    }
    assert!(responses > 1);
    assert_eq!(received, 40);
    assert!(matches!(stream.phase, InitialCompareStreamPhase::Hashes));
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
    let chunk_bytes = encoded_stream_record_chunk_bytes(std::slice::from_ref(&wide_child)).unwrap();
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
    service_stream.accepted_structure_bytes = MAX_STREAM_SERVICE_STRUCTURE_BYTES - chunk_bytes + 1;
    let service_counter_before = service_stream.accepted_structure_bytes;
    let mut push_session = PushStreamAccumulator {
        rollback_state: state.clone(),
        stream_id: "wide-name-service-budget".into(),
        choice_id: Some("wide-name-service-budget".into()),
        expected_service_generations: Vec::new(),
        strict: true,
        force_prune: true,
        next_service: 0,
        service_stream,
        applied: 0,
        backups: Vec::new(),
        committed_services: Vec::new(),
        prepared_baselines: Vec::new(),
        conflict_checkpoint: Some(state.conflict.checkpoint()),
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
        disk_prewarm: HashMap::new(),
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
            identities: Vec::new(),
            identity_offset: 0,
            identity_complete: false,
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
fn initial_snapshot_compare_requires_reserved_leaf_filename_migration() {
    let d = TempDir::new("initial-legacy-reserved-leaf");
    let misc = d.path().join("ReplicatedStorage").join("Misc");
    std::fs::create_dir_all(&misc).unwrap();
    std::fs::write(misc.join("init (Notifications).luau"), "return 'literal'\n").unwrap();
    let studio = vec![json!({
        "class": "ReplicatedStorage",
        "name": "ReplicatedStorage",
        "properties": {},
        "children": [{
            "class": "Folder",
            "name": "Misc",
            "properties": {},
            "children": [{
                "class": "ModuleScript",
                "name": "init (Notifications)",
                "properties": { "Source": "return 'literal'\n" },
                "children": []
            }]
        }]
    })];

    let error = initial_snapshot_comparison(d.path(), &studio).unwrap_err();

    let portable_error = error.replace('\\', "/");
    assert!(portable_error.contains("reserved init-marker filename grammar"));
    assert!(portable_error.contains("ReplicatedStorage/Misc/init (Notifications).luau"));
    assert!(portable_error.contains("ReplicatedStorage/Misc/%69nit (Notifications).luau"));
}

#[test]
fn legacy_reserved_leaf_gate_covers_case_ordinals_and_all_script_classes() {
    for (file_name, canonical_name) in [
        ("INIT.luau", "%49NIT.luau"),
        ("Init (Other).lua", "%49nit (Other).luau"),
        ("INIT (Other).server.luau", "%49NIT (Other).server.luau"),
        ("init [3].server.lua", "%69nit.server.luau"),
        ("INIT (Other).client.luau", "%49NIT (Other).client.luau"),
        ("init (Other) [2].client.lua", "%69nit (Other).client.luau"),
    ] {
        let d = TempDir::new("legacy-reserved-variant");
        let misc = d.path().join("ReplicatedStorage").join("Misc");
        std::fs::create_dir_all(&misc).unwrap();
        std::fs::write(misc.join(file_name), "return true\n").unwrap();

        let error =
            reject_legacy_reserved_init_leafs(d.path(), &["ReplicatedStorage".into()]).unwrap_err();

        assert!(error.contains("reserved init-marker filename grammar"));
        assert!(error.contains(file_name), "{file_name}: {error}");
        assert!(error.contains(canonical_name), "{file_name}: {error}");
    }
}

#[test]
fn initial_snapshot_accepts_case_and_normalization_equivalent_parent_markers() {
    for (tag, directory_fragment, marker_fragment, studio_name) in [
        (
            "case-equivalent-parent-marker",
            "notifications",
            "INIT (Notifications).luau",
            "notifications",
        ),
        (
            "normalization-equivalent-parent-marker",
            "%C3%89",
            "init (E\u{0301}).luau",
            "\u{00c9}",
        ),
    ] {
        let d = TempDir::new(tag);
        let directory = d.path().join("ReplicatedStorage").join(directory_fragment);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(marker_fragment), "return true\n").unwrap();
        let studio = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [{
                "class": "ModuleScript",
                "name": studio_name,
                "properties": { "Source": "return true\n" },
                "children": []
            }]
        })];

        let comparison = initial_snapshot_comparison(d.path(), &studio).unwrap();
        assert!(
            comparison.is_clean(),
            "{tag}: equivalent parent marker must not be prescribed as a leaf: {comparison:?}"
        );
    }
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
    let mut pending = test_pending_initial(choice_id, &["ReplicatedStorage/Config".to_string()]);
    let replayed_before_publish = std::cell::Cell::new(false);

    commit_initial_selection_with(
        &project,
        choice_id,
        submission_id,
        &mut pending,
        selection,
        || {
            let replay =
                replay_completed_initial_selection(&project, choice_id, submission_id, &request)
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
        initial_choice_selection(State(state), Json(chunk("submission-c", true, vec![3]))).await;
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
            completed_at: Instant::now() - INITIAL_SELECTION_REPLAY_TTL - Duration::from_secs(1),
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
    let mut pending = test_pending_initial(&choice_id, &paths);
    pending.service_generations =
        capture_initial_service_generations(state.canonical_project.as_path()).unwrap();
    *state.pending_initial.lock().unwrap() = Some(pending);
    let session = Arc::new(Mutex::new(InitialCompareAccumulator {
        compare_id: new_choice_id(),
        disk_stats: Stats::default(),
        studio_stats: Stats::default(),
        disk_prewarm: HashMap::new(),
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

#[tokio::test]
async fn studio_choice_turns_stale_when_disk_changes_after_compare() {
    let project = TempDir::new("initial-studio-choice-disk-race");
    let state = test_state(&project, None);
    let storage = project.path().join("ReplicatedStorage");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("Config.luau"), "return 'disk'\n").unwrap();
    let choice_id = "choice-disk-race";
    let mut pending = test_pending_initial(choice_id, &["ReplicatedStorage/Config".to_string()]);
    pending.service_generations =
        capture_initial_service_generations(state.canonical_project.as_path()).unwrap();
    *state.pending_initial.lock().unwrap() = Some(pending);

    let gift = storage.join("Client").join("GiftController.luau");
    std::fs::create_dir_all(gift.parent().unwrap()).unwrap();
    std::fs::write(&gift, "return 'newer disk work'\n").unwrap();

    let (status, Json(response)) = initial_choice(
        State(state.clone()),
        Json(InitialChoiceBody {
            choice_id: choice_id.into(),
            choice: "studio".into(),
            mode: None,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(response["ok"], false);
    assert_eq!(response["stale"], true);
    assert!(response["error"]
        .as_str()
        .unwrap()
        .contains("changed after the initial comparison"));
    assert_eq!(
        std::fs::read_to_string(&gift).unwrap(),
        "return 'newer disk work'\n"
    );
    assert!(state.pending_initial.lock().unwrap().is_none());
    assert!(!project.path().join(".rosync-backups").exists());
}

#[tokio::test]
async fn strict_push_revalidates_choice_generation_before_starting_stream() {
    let project = TempDir::new("strict-push-choice-fence");
    let state = test_state(&project, None);
    let storage = project.path().join("ReplicatedStorage");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("Config.luau"), "return 'disk'\n").unwrap();
    let choice_id = "strict-push-choice-fence";
    let mut pending = test_pending_initial(choice_id, &["ReplicatedStorage/Config".to_string()]);
    pending.service_generations =
        capture_initial_service_generations(state.canonical_project.as_path()).unwrap();
    *state.pending_initial.lock().unwrap() = Some(pending);

    let (status, _) = initial_choice(
        State(state.clone()),
        Json(InitialChoiceBody {
            choice_id: choice_id.into(),
            choice: "studio".into(),
            mode: None,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let _ = initial_decision(
        State(state.clone()),
        Query(InitialDecisionParams {
            choice_id: choice_id.into(),
        }),
    )
    .await
    .into_response();

    let gift = storage.join("Client").join("GiftController.luau");
    std::fs::create_dir_all(gift.parent().unwrap()).unwrap();
    std::fs::write(&gift, "return 'created after choice'\n").unwrap();
    let response = push(
        State(state.clone()),
        Json(streamed_push_test_body(
            choice_id,
            "ReplicatedStorage",
            "structure",
            0,
            true,
            streamed_service_records("ReplicatedStorage", None),
            Vec::new(),
        )),
    )
    .await
    .0;

    assert_eq!(response["ok"], false);
    assert_eq!(response["stale"], true);
    assert!(response["error"]
        .as_str()
        .unwrap()
        .contains("changed after the initial comparison"));
    assert_eq!(
        std::fs::read_to_string(&gift).unwrap(),
        "return 'created after choice'\n"
    );
    assert!(!PUSH_STREAM_ACCUMULATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .contains_key(state.canonical_project.as_path()));
    assert!(!project.path().join(".rosync-backups").exists());
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
