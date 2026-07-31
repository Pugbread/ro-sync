use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[derive(Clone, Debug)]
struct RequestRecord {
    op: String,
    args: Value,
}

#[derive(Debug)]
struct FakeState {
    project: String,
    requests: Mutex<Vec<RequestRecord>>,
    resolve_posts: Mutex<Vec<Value>>,
    decision_posts: Mutex<Vec<Value>>,
    websocket_connections: AtomicUsize,
}

struct FakeDaemon {
    port: u16,
    state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDaemon {
    async fn start(project: &Path) -> Self {
        let project = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
        let state = Arc::new(FakeState {
            project: project.display().to_string(),
            requests: Mutex::new(Vec::new()),
            resolve_posts: Mutex::new(Vec::new()),
            decision_posts: Mutex::new(Vec::new()),
            websocket_connections: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/hello", get(fake_hello))
            .route("/ws", get(fake_websocket_upgrade))
            .route("/resolve", post(fake_resolve))
            .route(
                "/initial-choice",
                get(fake_initial_choice).post(fake_decision),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind command-audit fake daemon");
        let port = listener.local_addr().expect("fake daemon address").port();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve command-audit fake daemon");
        });
        Self { port, state, task }
    }

    fn requests(&self) -> Vec<RequestRecord> {
        self.state.requests.lock().expect("request log").clone()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fake_hello(State(state): State<Arc<FakeState>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "project": state.project,
        "pluginProtocol": 6,
        "pluginConnected": true,
    }))
}

async fn fake_websocket_upgrade(
    State(state): State<Arc<FakeState>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    state.websocket_connections.fetch_add(1, Ordering::SeqCst);
    upgrade.on_upgrade(move |socket| fake_websocket(socket, state))
}

async fn fake_websocket(mut socket: WebSocket, state: Arc<FakeState>) {
    while let Some(Ok(message)) = socket.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if frame.get("type").and_then(Value::as_str) != Some("request") {
            continue;
        }
        let request_id = frame
            .get("request_id")
            .and_then(Value::as_u64)
            .expect("request id");
        let op = frame
            .get("op")
            .and_then(Value::as_str)
            .expect("request operation")
            .to_owned();
        let args = frame.get("args").cloned().unwrap_or(Value::Null);
        state
            .requests
            .lock()
            .expect("request log")
            .push(RequestRecord {
                op: op.clone(),
                args,
            });

        let value = match op.as_str() {
            "tree" => json!({
                "class": "DataModel",
                "name": "game",
                "children": (0..512)
                    .map(|index| json!({
                        "class": "Folder",
                        "name": format!("Node{index:04}"),
                        "children": [],
                    }))
                    .collect::<Vec<_>>(),
            }),
            "capture_authorize" => json!({
                "authorized": true,
                "providerUnsupported": false,
            }),
            "playtest_start" => json!({ "id": "fixture-job", "status": "starting" }),
            "playtest_wait" | "playtest_contexts" => json!({ "contexts": [] }),
            "select_set" => json!({ "count": 1 }),
            "new" => json!({ "path": "Workspace/Audit", "class": "Folder" }),
            "mv" => json!({ "path": "Workspace/Audit", "parent": "ReplicatedStorage" }),
            _ => json!({ "fixture": true }),
        };
        let response = json!({
            "type": "response",
            "request_id": request_id,
            "ok": true,
            "value": value,
        });
        if socket
            .send(Message::Text(response.to_string()))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn fake_resolve(State(state): State<Arc<FakeState>>, Json(body): Json<Value>) -> Json<Value> {
    state
        .resolve_posts
        .lock()
        .expect("resolve posts")
        .push(body.clone());
    Json(json!({
        "ok": true,
        "action": "fixture-resolved",
        "path": body.get("path"),
    }))
}

async fn fake_initial_choice() -> Json<Value> {
    Json(json!({
        "pending": true,
        "choiceId": "fixture-choice",
    }))
}

async fn fake_decision(
    State(state): State<Arc<FakeState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state
        .decision_posts
        .lock()
        .expect("decision posts")
        .push(body);
    Json(json!({ "ok": true }))
}

struct CliFixture {
    _project: TempDir,
    project: PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        let project = tempfile::tempdir().expect("temporary command-audit project");
        Self {
            project: project.path().to_path_buf(),
            _project: project,
        }
    }
}

fn run_cli(fixture: &CliFixture, daemon: &FakeDaemon, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rosync"));
    command.args(args);
    command.args(["--project"]).arg(&fixture.project).args([
        "--port",
        &daemon.port.to_string(),
        "--raw",
    ]);
    command.output().expect("run command-audit CLI")
}

fn assert_success(command: &str, output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{command} failed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_json_subset(actual: &Value, expected: &Value, context: &str) {
    match expected {
        Value::Object(expected_fields) => {
            let actual_fields = actual
                .as_object()
                .unwrap_or_else(|| panic!("{context}: expected object, got {actual}"));
            for (key, expected_value) in expected_fields {
                let actual_value = actual_fields
                    .get(key)
                    .unwrap_or_else(|| panic!("{context}: missing required field {key:?}"));
                assert_json_subset(actual_value, expected_value, &format!("{context}.{key}"));
            }
        }
        _ => assert_eq!(actual, expected, "{context}: wrong required value"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_cli_commands_route_to_a_fake_daemon_without_touching_studio() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(&fixture.project).await;
    let cases: &[(&str, &[&str], &str)] = &[
        (
            "capture authorize",
            &["capture", "authorize"],
            "capture_authorize",
        ),
        (
            "playtest start",
            &["playtest", "start", "--mode", "play"],
            "playtest_start",
        ),
        (
            "playtest wait",
            &["playtest", "wait", "--minimum", "1", "--timeout", "1"],
            "playtest_wait",
        ),
        (
            "playtest exec",
            &[
                "playtest",
                "exec",
                "--context",
                "server",
                "--source",
                "return 1",
                "--timeout",
                "1",
            ],
            "playtest_request",
        ),
        (
            "playtest logs",
            &["playtest", "logs", "--context", "server", "--timeout", "1"],
            "playtest_request",
        ),
        (
            "playtest ui",
            &[
                "playtest",
                "ui",
                "--context",
                "client:1",
                "--limit",
                "10",
                "--timeout",
                "1",
            ],
            "playtest_request",
        ),
        (
            "playtest input",
            &[
                "playtest",
                "input",
                "--context",
                "client:1",
                "--actions",
                r#"[{"type":"key","key":"Space","down":true}]"#,
                "--timeout",
                "1",
            ],
            "playtest_request",
        ),
        (
            "playtest request",
            &[
                "playtest",
                "request",
                "--context",
                "server",
                "--op",
                "fixture",
                "--args",
                "{}",
                "--timeout",
                "1",
            ],
            "playtest_request",
        ),
        ("playtest stop", &["playtest", "stop"], "playtest_stop"),
        (
            "set",
            &[
                "set",
                "--path",
                "Workspace/Audit",
                "--prop",
                "Name",
                "--value",
                r#""Renamed""#,
            ],
            "set",
        ),
        (
            "new",
            &[
                "new",
                "--path",
                "Workspace",
                "--class",
                "Folder",
                "--name",
                "Audit",
            ],
            "new",
        ),
        ("rm", &["rm", "--path", "Workspace/Audit"], "rm"),
        (
            "mv",
            &[
                "mv",
                "--from",
                "Workspace/Audit",
                "--to",
                "ReplicatedStorage",
                "--force",
            ],
            "mv",
        ),
        (
            "attr set",
            &[
                "attr",
                "set",
                "--path",
                "Workspace/Audit",
                "--name",
                "Enabled",
                "--value",
                "true",
            ],
            "set_attr",
        ),
        (
            "attr rm",
            &[
                "attr",
                "rm",
                "--path",
                "Workspace/Audit",
                "--name",
                "Enabled",
            ],
            "rm_attr",
        ),
        (
            "tag add",
            &[
                "tag",
                "add",
                "--path",
                "Workspace/Audit",
                "--tag",
                "Fixture",
            ],
            "add_tag",
        ),
        (
            "tag rm",
            &["tag", "rm", "--path", "Workspace/Audit", "--tag", "Fixture"],
            "rm_tag",
        ),
        ("open", &["open", "Workspace/Audit"], "select_set"),
        ("eval", &["eval", "--source", "return 1"], "eval"),
        ("save", &["save"], "save"),
        ("waypoint", &["waypoint", "--name", "fixture"], "waypoint"),
        ("undo", &["undo"], "undo"),
        ("redo", &["redo"], "redo"),
        (
            "select set",
            &["select", "set", "--paths", r#"["Workspace/Audit"]"#],
            "select_set",
        ),
    ];

    for (command, args, expected_op) in cases {
        let before = daemon.requests().len();
        let output = run_cli(&fixture, &daemon, args);
        assert_success(command, &output);
        let requests = daemon.requests();
        assert_eq!(
            requests.len(),
            before + 1,
            "{command} must issue exactly one fake-daemon request"
        );
        assert_eq!(
            requests[before].op, *expected_op,
            "{command} routed wrong op"
        );
        let expected_args = match *command {
            "capture authorize" | "playtest stop" | "save" | "undo" | "redo" => json!({}),
            "playtest start" => json!({
                "mode": "play",
                "players": 1,
                "testArgs": {},
            }),
            "playtest wait" => json!({
                "minimum": 1,
                "timeout": 1.0,
            }),
            "playtest exec" => json!({
                "context": "server",
                "op": "exec",
                "args": {
                    "source": "return 1",
                    "identity": "game",
                    "timeout": 1.0,
                },
                "timeout": 1.0,
            }),
            "playtest logs" => json!({
                "context": "server",
                "op": "logs",
                "args": {
                    "sinceSeq": 0,
                    "limit": 200,
                },
                "timeout": 1.0,
            }),
            "playtest ui" => json!({
                "context": "client:1",
                "op": "ui_tree",
                "args": { "limit": 10 },
                "timeout": 1.0,
            }),
            "playtest input" => json!({
                "context": "client:1",
                "op": "input",
                "args": {
                    "actions": [{ "type": "key", "key": "Space", "down": true }],
                },
                "timeout": 1.0,
            }),
            "playtest request" => json!({
                "context": "server",
                "op": "fixture",
                "args": {},
                "timeout": 1.0,
            }),
            "set" => json!({
                "path": "Workspace/Audit",
                "prop": "Name",
                "value": "Renamed",
            }),
            "new" => json!({
                "parent": "Workspace",
                "class": "Folder",
                "name": "Audit",
            }),
            "rm" => json!({ "path": "Workspace/Audit" }),
            "mv" => json!({
                "from": "Workspace/Audit",
                "to": "ReplicatedStorage",
                "force": true,
            }),
            "attr set" => json!({
                "path": "Workspace/Audit",
                "name": "Enabled",
                "value": true,
            }),
            "attr rm" => json!({
                "path": "Workspace/Audit",
                "name": "Enabled",
            }),
            "tag add" | "tag rm" => json!({
                "path": "Workspace/Audit",
                "tag": "Fixture",
            }),
            "open" => json!({ "paths": ["Workspace/Audit"] }),
            "eval" => json!({ "source": "return 1" }),
            "waypoint" => json!({ "name": "fixture" }),
            "select set" => json!({ "paths": ["Workspace/Audit"] }),
            other => panic!("missing required request-field assertion for {other}"),
        };
        assert_json_subset(
            &requests[before].args,
            &expected_args,
            &format!("{command} args"),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflict_and_initial_decision_mutations_use_only_fixture_http_state() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(&fixture.project).await;

    let resolve = run_cli(
        &fixture,
        &daemon,
        &[
            "resolve",
            "--path",
            "ReplicatedStorage/Audit.luau",
            "--disk",
        ],
    );
    assert_success("resolve", &resolve);
    assert_eq!(
        daemon
            .state
            .resolve_posts
            .lock()
            .expect("resolve posts")
            .as_slice(),
        &[json!({
            "path": "ReplicatedStorage/Audit.luau",
            "choice": "disk",
        })],
    );

    let decision = run_cli(&fixture, &daemon, &["decision", "--disk"]);
    assert_success("decision --disk", &decision);
    assert_eq!(
        daemon
            .state
            .decision_posts
            .lock()
            .expect("decision posts")
            .as_slice(),
        &[json!({
            "choiceId": "fixture-choice",
            "choice": "disk",
        })],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playtest_capture_builds_the_expected_request_before_artifact_validation() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(&fixture.project).await;
    let output_path = fixture.project.join("playtest.png");
    let output = run_cli(
        &fixture,
        &daemon,
        &[
            "playtest",
            "capture",
            "--context",
            "client:1",
            "--region",
            "1,2,320,180",
            "--output-size",
            "640x360",
            "--ui",
            "none",
            "--output",
            output_path.to_str().unwrap(),
            "--timeout",
            "3",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "the fixture deliberately omits artifact metadata; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("omitted artifact metadata"),
        "capture must reach artifact validation: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let requests = daemon.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].op, "playtest_capture");
    assert_json_subset(
        &requests[0].args,
        &json!({
            "context": "client:1",
            "options": {
                "ui": "none",
                "resample": "default",
                "position": { "x": 1, "y": 2 },
                "captureSize": { "x": 320, "y": 180 },
                "outputSize": { "x": 640, "y": 360 },
            },
            "filename": "playtest.png",
        }),
        "playtest capture args",
    );
    assert!(
        requests[0].args["timeout"]
            .as_f64()
            .is_some_and(|value| value > 0.0),
        "playtest capture must forward a positive remaining timeout"
    );
    assert!(
        !output_path.exists(),
        "invalid fixture artifacts must not create output files"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_uses_a_bounded_connection_pool_for_a_wide_tree() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(&fixture.project).await;
    let output_path = fixture.project.join("snapshot.json");
    let started = Instant::now();
    let output = run_cli(
        &fixture,
        &daemon,
        &["snapshot", "--output", output_path.to_str().unwrap()],
    );
    assert_success("snapshot", &output);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "512-node fake snapshot exceeded the bounded regression budget"
    );
    let connections = daemon.state.websocket_connections.load(Ordering::SeqCst);
    assert!(
        connections <= 17,
        "snapshot must use one tree connection plus at most 16 inspection workers, got {connections}"
    );
    let requests = daemon.requests();
    assert_eq!(requests.len(), 514, "one tree plus 513 node inspections");
    assert_eq!(requests[0].op, "tree");
    assert!(requests[1..].iter().all(|request| request.op == "get"));
    let snapshot: Value =
        serde_json::from_slice(&std::fs::read(output_path).expect("read snapshot"))
            .expect("parse snapshot");
    assert_eq!(snapshot["schema"], "ro-sync.snapshot.v1");
}
