use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[derive(Clone, Debug)]
struct ScriptedReply {
    delay: Duration,
    fields: Value,
    disconnect_before_response: bool,
}

impl ScriptedReply {
    fn ok(value: Value) -> Self {
        Self {
            delay: Duration::ZERO,
            fields: json!({ "ok": true, "value": value }),
            disconnect_before_response: false,
        }
    }

    fn error(error: Value, job_status: &str) -> Self {
        Self {
            delay: Duration::ZERO,
            fields: json!({
                "ok": false,
                "error": error,
                "jobStatus": job_status,
            }),
            disconnect_before_response: false,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn disconnected(mut self) -> Self {
        self.disconnect_before_response = true;
        self
    }
}

#[derive(Clone, Debug)]
struct RequestRecord {
    op: String,
    args: Value,
}

#[derive(Debug)]
struct FakeState {
    project: String,
    replies: Mutex<HashMap<String, VecDeque<ScriptedReply>>>,
    requests: Mutex<Vec<RequestRecord>>,
    hello_requests: AtomicUsize,
    websocket_connections: AtomicUsize,
}

struct FakeDaemon {
    port: u16,
    state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDaemon {
    async fn start(project: &Path, replies: Vec<(&str, ScriptedReply)>) -> Self {
        let mut by_operation: HashMap<String, VecDeque<ScriptedReply>> = HashMap::new();
        for (operation, reply) in replies {
            by_operation
                .entry(operation.to_owned())
                .or_default()
                .push_back(reply);
        }

        let project = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
        let state = Arc::new(FakeState {
            project: project.display().to_string(),
            replies: Mutex::new(by_operation),
            requests: Mutex::new(Vec::new()),
            hello_requests: AtomicUsize::new(0),
            websocket_connections: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/hello", get(fake_hello))
            .route("/ws", get(fake_websocket_upgrade))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake daemon");
        let port = listener.local_addr().expect("fake daemon address").port();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve fake daemon");
        });

        Self { port, state, task }
    }

    fn requests(&self) -> Vec<RequestRecord> {
        self.state.requests.lock().expect("request log").clone()
    }

    fn hello_requests(&self) -> usize {
        self.state.hello_requests.load(Ordering::SeqCst)
    }

    fn websocket_connections(&self) -> usize {
        self.state.websocket_connections.load(Ordering::SeqCst)
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fake_hello(State(state): State<Arc<FakeState>>) -> Json<Value> {
    state.hello_requests.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "ok": true,
        "project": state.project,
        "protocol": 2,
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
            // RemoteSession sends a hello first but deliberately does not wait for
            // an acknowledgement, matching the real daemon's CLI connection.
            continue;
        }

        let request_id = frame
            .get("request_id")
            .and_then(Value::as_u64)
            .expect("request id");
        let operation = frame
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
                op: operation.clone(),
                args,
            });

        let reply = {
            let mut replies = state.replies.lock().expect("scripted replies");
            replies.get_mut(&operation).and_then(VecDeque::pop_front)
        }
        .unwrap_or_else(|| {
            ScriptedReply::error(
                Value::String(format!("unexpected fake-daemon operation: {operation}")),
                "fixture-error",
            )
        });

        if !reply.delay.is_zero() {
            tokio::time::sleep(reply.delay).await;
        }
        if reply.disconnect_before_response {
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
        let mut response = match reply.fields {
            Value::Object(fields) => fields,
            other => panic!("scripted response fields must be an object, got {other}"),
        };
        response.insert("type".into(), Value::String("response".into()));
        response.insert("request_id".into(), Value::from(request_id));
        if socket
            .send(Message::Text(Value::Object(response).to_string()))
            .await
            .is_err()
        {
            return;
        }
    }
}

struct CliFixture {
    _project: TempDir,
    project: PathBuf,
    script: PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        let project = tempfile::tempdir().expect("temporary project");
        let script = project.path().join("main.server.luau");
        std::fs::write(&script, "return { ok = true }\n").expect("write playscript");
        Self {
            project: project.path().to_path_buf(),
            script,
            _project: project,
        }
    }
}

async fn run_cli(fixture: &CliFixture, daemon: &FakeDaemon, extra_args: &[&str]) -> Output {
    let binary = env!("CARGO_BIN_EXE_rosync").to_owned();
    let project = fixture.project.clone();
    let script = fixture.script.clone();
    let port = daemon.port.to_string();
    let extra_args = extra_args
        .iter()
        .map(|argument| argument.to_string())
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        Command::new(binary)
            .args(["playtest", "run", "--project"])
            .arg(project)
            .args(["--port", &port, "--script"])
            .arg(script)
            .args(extra_args)
            .output()
            .expect("run rosync CLI")
    })
    .await
    .expect("join CLI process")
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit status; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn ndjson_lines(output: &Output) -> Vec<Value> {
    let stdout = std::str::from_utf8(&output.stdout).expect("stdout UTF-8");
    assert!(stdout.ends_with('\n'), "NDJSON must end with a newline");
    stdout
        .lines()
        .enumerate()
        .map(|(index, line)| {
            assert!(!line.is_empty(), "NDJSON line {} is empty", index + 1);
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("NDJSON line {} is invalid: {error}: {line}", index + 1)
            })
        })
        .collect()
}

fn terminal_poll(kind: &str, value: Value, job_status: &str) -> Value {
    json!({
        "events": [{
            "seq": 1,
            "type": "event",
            "t": 0.1,
            "context": "server",
            "data": { "phase": "fixture" },
        }],
        "nextSeq": 1,
        "hasMore": false,
        "outcome": {
            "kind": kind,
            "elapsed": 0.2,
            "value": value,
            "jobStatus": job_status,
        },
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_success_is_exit_zero_and_every_output_line_is_valid_ndjson() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-success", "jobStatus": "running" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(terminal_poll(
                    "success",
                    json!({ "laps": [1, 2, 3] }),
                    "completed",
                )),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--raw"]).await;
    assert_exit(&output, 0);
    let lines = ndjson_lines(&output);
    assert_eq!(lines.len(), 3, "started, event, and result are expected");
    assert_eq!(lines[0]["type"], "started");
    assert_eq!(lines[0]["jobId"], "job-success");
    assert_eq!(lines[1]["type"], "event");
    assert_eq!(lines[2]["type"], "result");
    assert_eq!(lines[2]["ok"], true);
    assert_eq!(lines[2]["value"], json!({ "laps": [1, 2, 3] }));
    assert_eq!(lines[2]["jobStatus"], "completed");
    assert!(
        daemon.hello_requests() >= 1,
        "the black-box run should discover its fake daemon through /hello",
    );

    let operations = daemon
        .requests()
        .into_iter()
        .map(|request| request.op)
        .collect::<Vec<_>>();
    assert_eq!(operations, ["playtest_run_start", "playtest_run_poll"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_job_status_recovers_a_poll_failure_without_cancelling_the_run() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-recovered" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::error("transient poll failure".into(), "running"),
            ),
            (
                "playtest_status",
                ScriptedReply::ok(json!({
                    "active": true,
                    "job": {"id": "job-recovered", "status": "running"}
                })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(terminal_poll("success", json!("recovered"), "stopped")),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--quiet", "--raw"]).await;
    assert_exit(&output, 0);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["value"], "recovered");
    let operations = daemon
        .requests()
        .into_iter()
        .map(|request| request.op)
        .collect::<Vec<_>>();
    assert_eq!(
        operations,
        [
            "playtest_run_start",
            "playtest_run_poll",
            "playtest_status",
            "playtest_run_poll"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_status_uses_one_bounded_verification_window_before_cleanup() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-unverified" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::error("transient poll failure".into(), "unavailable"),
            ),
        ],
    )
    .await;

    let started = Instant::now();
    let output = run_cli(&fixture, &daemon, &["--timeout", "20", "--quiet", "--raw"]).await;
    let elapsed = started.elapsed();

    assert_exit(&output, 4);
    assert!(
        elapsed >= Duration::from_secs(4) && elapsed < Duration::from_secs(10),
        "transport verification should be bounded near five seconds, got {elapsed:?}"
    );
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["type"], "aborted");
    assert_eq!(terminal["exitCode"], 4);
    assert!(terminal["reason"]
        .as_str()
        .expect("abort reason")
        .contains("remained unverified"));

    let operations = daemon
        .requests()
        .into_iter()
        .map(|request| request.op)
        .collect::<Vec<_>>();
    let status_count = operations
        .iter()
        .filter(|operation| operation.as_str() == "playtest_status")
        .count();
    assert!(
        status_count >= 2,
        "status should be retried during the grace window"
    );
    assert_eq!(
        operations.last().map(String::as_str),
        Some("playtest_run_cancel")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_start_response_retries_with_the_same_client_run_id() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(Value::Null).disconnected(),
            ),
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({
                    "jobId": "job-idempotent",
                    "clientRunId": "replayed-by-plugin",
                    "reused": true
                })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(terminal_poll("success", json!(true), "stopped")),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--quiet", "--raw"]).await;
    assert_exit(&output, 0);
    let requests = daemon.requests();
    let starts = requests
        .iter()
        .filter(|request| request.op == "playtest_run_start")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    let first = starts[0].args["clientRunId"]
        .as_str()
        .expect("first clientRunId");
    let second = starts[1].args["clientRunId"]
        .as_str()
        .expect("second clientRunId");
    assert_eq!(first, second);
    assert_eq!(first.len(), 32);
    assert!(!requests
        .iter()
        .any(|request| request.op == "playtest_run_cancel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrecoverable_start_loss_tombstones_the_same_key_and_exits_five() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(Value::Null).disconnected(),
            ),
            (
                "playtest_run_start",
                ScriptedReply::ok(Value::Null).disconnected(),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::ok(json!({
                    "cancelledBeforeStart": true,
                    "jobStatus": "notStarted"
                })),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--raw"]).await;
    assert_exit(&output, 5);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["kind"], "bootFailure");
    assert_eq!(terminal["jobStatus"], "notStarted");
    let requests = daemon.requests();
    let client_run_ids = requests
        .iter()
        .filter_map(|request| request.args["clientRunId"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(client_run_ids.len(), 3);
    assert!(client_run_ids.windows(2).all(|ids| ids[0] == ids[1]));
    let cancel = requests.last().expect("cleanup request");
    assert_eq!(cancel.op, "playtest_run_cancel");
    assert!(cancel.args.get("jobId").is_none());
    assert_eq!(cancel.args["force"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_completion_that_beat_lost_start_cleanup_still_wins() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(Value::Null).disconnected(),
            ),
            (
                "playtest_run_start",
                ScriptedReply::ok(Value::Null).disconnected(),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::ok(json!({
                    "job": {"id": "job-finished-first", "status": "stopped"},
                    "outcome": {
                        "kind": "success",
                        "ok": true,
                        "value": {"won": "before-cleanup"},
                        "jobStatus": "stopped"
                    }
                })),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--raw"]).await;
    assert_exit(&output, 0);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["kind"], "success");
    assert_eq!(terminal["value"], json!({"won": "before-cleanup"}));
    assert_eq!(terminal["jobStatus"], "stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_failure_exits_two_and_reports_traceback() {
    let fixture = CliFixture::new();
    let mut failure = terminal_poll("failure", Value::Null, "failed");
    failure["outcome"]["error"] = Value::String("fixture exploded".into());
    failure["outcome"]["traceback"] = Value::String("stack traceback: fixture:1".into());
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-failure" })),
            ),
            ("playtest_run_poll", ScriptedReply::ok(failure)),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--raw"]).await;
    assert_exit(&output, 2);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["type"], "result");
    assert_eq!(terminal["ok"], false);
    assert_eq!(terminal["kind"], "failure");
    assert_eq!(terminal["exitCode"], 2);
    assert_eq!(terminal["error"], "fixture exploded");
    assert_eq!(terminal["traceback"], "stack traceback: fixture:1");
    assert_eq!(terminal["jobStatus"], "failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_failure_reports_the_final_job_status() {
    let fixture = CliFixture::new();
    let mut failure = terminal_poll("failure", Value::Null, "failed");
    failure["outcome"]["error"] = Value::String("fixture exploded".into());
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-human-failure" })),
            ),
            ("playtest_run_poll", ScriptedReply::ok(failure)),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &[]).await;
    assert_exit(&output, 2);
    let stdout = std::str::from_utf8(&output.stdout).expect("stdout UTF-8");
    assert!(
        stdout.contains("fixture exploded (job: failed)"),
        "{stdout}"
    );
    assert!(
        output.stderr.is_empty(),
        "playtest terminal output belongs on stdout"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_failure_cannot_be_reported_as_script_success() {
    let fixture = CliFixture::new();
    let mut terminal = terminal_poll("success", json!({"answer": 42}), "running");
    terminal["outcome"]["stopError"] = Value::String("Studio refused to leave the playtest".into());
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({"jobId": "job-stop-failed"})),
            ),
            ("playtest_run_poll", ScriptedReply::ok(terminal)),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--quiet", "--raw"]).await;
    assert_exit(&output, 4);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["type"], "aborted");
    assert_eq!(terminal["jobStatus"], "running");
    assert!(terminal["reason"]
        .as_str()
        .expect("teardown reason")
        .contains("Studio refused to leave"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wall_clock_timeout_cancels_the_job_and_exits_three() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-timeout" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(json!({ "events": [], "hasMore": false }))
                    .delayed(Duration::from_millis(150)),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::ok(json!({ "jobStatus": "stopped" })),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--timeout", "0.05", "--raw"]).await;
    assert_exit(&output, 3);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["kind"], "timeout");
    assert_eq!(terminal["exitCode"], 3);
    assert_eq!(terminal["jobStatus"], "stopped");

    let requests = daemon.requests();
    assert_eq!(
        requests.last().expect("cancel request").op,
        "playtest_run_cancel"
    );
    assert_eq!(requests.last().unwrap().args["reason"], "timeout");
    assert_eq!(requests.last().unwrap().args["force"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_cancel_error_preserves_teardown_failure_and_exits_four() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-timeout-stop-error" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(json!({ "frames": [], "hasMore": false }))
                    .delayed(Duration::from_millis(150)),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::error(
                    "playtest run cancellation could not confirm teardown: LeaveTest failed".into(),
                    "running",
                ),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--timeout", "0.05", "--raw"]).await;
    assert_exit(&output, 4);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["type"], "aborted");
    assert_eq!(terminal["jobStatus"], "running");
    assert!(terminal["reason"]
        .as_str()
        .expect("teardown diagnostic")
        .contains("could not confirm teardown"));
    let operations = daemon
        .requests()
        .into_iter()
        .map(|request| request.op)
        .collect::<Vec<_>>();
    assert_eq!(
        operations,
        [
            "playtest_run_start",
            "playtest_run_poll",
            "playtest_run_cancel"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresponsive_timeout_cleanup_has_one_bounded_grace_period() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-timeout-bounded" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(json!({ "frames": [], "hasMore": false }))
                    .delayed(Duration::from_millis(150)),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::ok(json!({ "jobStatus": "stopped" }))
                    .delayed(Duration::from_secs(10)),
            ),
        ],
    )
    .await;

    let started = Instant::now();
    let output = run_cli(&fixture, &daemon, &["--timeout", "0.05", "--raw"]).await;
    let elapsed = started.elapsed();
    assert_exit(&output, 4);
    assert!(
        elapsed < Duration::from_secs(5),
        "cleanup restarted or exceeded its one grace budget: {elapsed:?}"
    );
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["type"], "aborted");
    assert!(terminal["reason"]
        .as_str()
        .expect("cleanup timeout diagnostic")
        .contains("cancel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_that_won_at_the_deadline_is_not_overwritten_by_timeout() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({"jobId": "job-deadline-race"})),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(json!({"frames": [], "hasMore": false}))
                    .delayed(Duration::from_millis(150)),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::ok(json!({
                    "job": {"id": "job-deadline-race", "status": "stopped"},
                    "outcome": {
                        "outcome": "success",
                        "kind": "result",
                        "ok": true,
                        "elapsed": 0.049,
                        "value": {"won": "return"},
                        "jobStatus": "stopped"
                    }
                })),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--timeout", "0.05", "--raw"]).await;
    assert_exit(&output, 0);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["kind"], "success");
    assert_eq!(terminal["value"], json!({"won": "return"}));
    assert_eq!(terminal["jobStatus"], "stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_open_timeout_terminalizes_but_does_not_force_stop_studio() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-timeout-autopsy" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(json!({ "frames": [], "hasMore": false }))
                    .delayed(Duration::from_millis(150)),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::ok(json!({
                    "job": {"id": "job-timeout-autopsy", "status": "running"},
                    "run": {"keepOpen": true, "status": "timeout"}
                })),
            ),
        ],
    )
    .await;

    let output = run_cli(
        &fixture,
        &daemon,
        &["--timeout", "0.05", "--keep-open", "--raw"],
    )
    .await;
    assert_exit(&output, 3);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["kind"], "timeout");
    assert_eq!(terminal["jobStatus"], "running");
    assert_eq!(terminal["keptOpen"], true);
    assert_eq!(terminal["jobId"], "job-timeout-autopsy");

    let requests = daemon.requests();
    let cancel = requests.last().expect("cancel request");
    assert_eq!(cancel.op, "playtest_run_cancel");
    assert_eq!(cancel.args["reason"], "timeout");
    assert_eq!(cancel.args["force"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_abort_fetches_job_status_before_cancel_and_exits_four() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-aborted" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::error(
                    json!({
                        "code": "playtest_run_gone",
                        "message": "runtime stream disappeared",
                    }),
                    "unavailable",
                ),
            ),
            (
                "playtest_status",
                ScriptedReply::ok(json!({ "jobStatus": "completed" })),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::ok(json!({ "jobStatus": "completed" })),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--raw"]).await;
    assert_exit(&output, 4);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["type"], "aborted");
    assert_eq!(terminal["exitCode"], 4);
    assert_eq!(terminal["jobStatus"], "completed");
    assert!(terminal["reason"]
        .as_str()
        .expect("abort reason")
        .contains("runtime stream disappeared"));

    let operations = daemon
        .requests()
        .into_iter()
        .map(|request| request.op)
        .collect::<Vec<_>>();
    assert_eq!(
        operations,
        [
            "playtest_run_start",
            "playtest_run_poll",
            "playtest_status",
            "playtest_run_cancel",
        ],
        "status must be fetched before cleanup so an empty response is never mistaken for success",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_heartbeat_does_not_abort_while_job_status_is_still_active() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({"jobId": "job-heartbeat"})),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(json!({
                    "frames": [],
                    "hasMore": false,
                    "heartbeats": {"server": {"ageSeconds": 7.0}}
                })),
            ),
            (
                "playtest_status",
                ScriptedReply::ok(json!({
                    "active": true,
                    "job": {"id": "job-heartbeat", "status": "running"}
                })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(terminal_poll("success", json!("alive"), "stopped")),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--quiet", "--raw"]).await;
    assert_exit(&output, 0);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["value"], "alive");

    let operations = daemon
        .requests()
        .into_iter()
        .map(|request| request.op)
        .collect::<Vec<_>>();
    assert_eq!(
        operations,
        [
            "playtest_run_start",
            "playtest_run_poll",
            "playtest_status",
            "playtest_run_poll"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_failure_exits_five_with_the_final_job_status() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![(
            "playtest_run_start",
            ScriptedReply::error(
                json!({
                    "code": "playtest_boot_failed",
                    "message": "server context never became ready",
                }),
                "failed",
            ),
        )],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--raw"]).await;
    assert_exit(&output, 5);
    let lines = ndjson_lines(&output);
    assert_eq!(lines.len(), 1, "a failed start has no progress frames");
    assert_eq!(lines[0]["type"], "result");
    assert_eq!(lines[0]["kind"], "bootFailure");
    assert_eq!(lines[0]["exitCode"], 5);
    assert_eq!(lines[0]["jobStatus"], "failed");
    assert!(lines[0]["error"]
        .as_str()
        .expect("boot error")
        .contains("server context never became ready"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_start_success_is_cleaned_up_by_client_key_before_exit_five() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobStatus": "starting" })),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::ok(json!({
                    "cancelled": true,
                    "jobStatus": "stopped",
                    "outcome": {
                        "kind": "aborted",
                        "error": "start response omitted its job id",
                        "jobStatus": "stopped"
                    }
                })),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--raw"]).await;
    assert_exit(&output, 5);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["kind"], "bootFailure");
    assert_eq!(terminal["jobStatus"], "stopped");
    let requests = daemon.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].op, "playtest_run_cancel");
    assert_eq!(
        requests[0].args["clientRunId"],
        requests[1].args["clientRunId"]
    );
    assert!(requests[1].args.get("jobId").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_start_cleanup_failure_exits_four_instead_of_hiding_an_orphan() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobStatus": "starting" })),
            ),
            (
                "playtest_run_cancel",
                ScriptedReply::error(
                    "playtest start cleanup could not confirm teardown".into(),
                    "running",
                ),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--raw"]).await;
    assert_exit(&output, 4);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["type"], "aborted");
    assert_eq!(terminal["jobStatus"], "running");
    assert!(terminal["reason"]
        .as_str()
        .expect("cleanup failure")
        .contains("could not confirm teardown"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_suppresses_started_and_event_frames_but_keeps_the_result() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({ "jobId": "job-quiet" })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(terminal_poll("success", json!("done"), "completed")),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--quiet", "--raw"]).await;
    assert_exit(&output, 0);
    let lines = ndjson_lines(&output);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["type"], "result");
    assert_eq!(lines[0]["value"], "done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_open_reports_a_reusable_job_id_without_cancelling_it() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(
        &fixture.project,
        vec![
            (
                "playtest_run_start",
                ScriptedReply::ok(json!({
                    "job": {"id": "job-autopsy", "status": "running"},
                    "run": {"jobId": "job-autopsy", "keepOpen": true}
                })),
            ),
            (
                "playtest_run_poll",
                ScriptedReply::ok(terminal_poll("success", json!("inspect me"), "running")),
            ),
        ],
    )
    .await;

    let output = run_cli(&fixture, &daemon, &["--keep-open", "--quiet", "--raw"]).await;
    assert_exit(&output, 0);
    let terminal = ndjson_lines(&output).pop().expect("terminal line");
    assert_eq!(terminal["type"], "result");
    assert_eq!(terminal["keptOpen"], true);
    assert_eq!(terminal["jobId"], "job-autopsy");

    let operations = daemon
        .requests()
        .into_iter()
        .map(|request| request.op)
        .collect::<Vec<_>>();
    assert_eq!(operations, ["playtest_run_start", "playtest_run_poll"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_args_json_fails_before_opening_websocket_or_sending_a_request() {
    let fixture = CliFixture::new();
    let daemon = FakeDaemon::start(&fixture.project, Vec::new()).await;

    let output = run_cli(&fixture, &daemon, &["--args", "{"]).await;
    assert_exit(&output, 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid JSON"));
    assert_eq!(
        daemon.hello_requests(),
        0,
        "JSON preflight must remain offline"
    );
    assert_eq!(daemon.websocket_connections(), 0);
    assert!(daemon.requests().is_empty());
}
