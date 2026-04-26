// WebSocket transport for plugin ↔ daemon realtime traffic.
//
// Replaces the HTTP long-poll (`/poll`) + SSE (`/events`) pair with a single
// persistent connection. Everything non-realtime (/hello, /snapshot, /push as
// bootstrap, /initial-compare, /initial-decision, /resolve, etc.) still goes
// over HTTP.
//
// Wire framing: serde-tagged JSON over `Message::Text`.
//   ClientMsg (tag "type", lowercase):
//     {"type":"hello","clientId":"<string>"}
//     {"type":"push","ops":[<plugin-shape op>, ...]}
//     {"type":"ping"}   // server replies with pong
//     {"type":"pong"}   // reply to server ping (no-op)
//
//   ServerMsg (tag "type", kebab-case):
//     {"type":"op","op":<plugin-shape op>}
//     {"type":"ping"}            // 10-second heartbeat
//     {"type":"pong"}            // reply to client ping
//     {"type":"lagged"}          // broadcast overflow; close follows
//     {"type":"push-result", ok, applied, skipped, conflicts, errors}
//     {"type":"error","error":"..."}
//   Pre-existing event strings from `AppState::events` are passed through
//   unchanged (shapes like `{"type":"conflict",...}`, `{"type":"config-changed",...}`,
//   `{"type":"initial-choice-needed",...}`, etc.) except messages with
//   type=="op", which are suppressed because the watch_tx path already
//   delivered them in plugin shape.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use crate::http::{apply_push_ops, fs_op_to_plugin_op};
use crate::AppState;

/// Routing table for in-flight request/response pairs. When a CLI client sends
/// a `{type:"request",request_id,...}` frame, its outbound mpsc sender is
/// stashed here under `request_id`. Whichever connection later responds has
/// its `{type:"response",request_id,...}` routed back to that sender.
pub type PendingRoutes = Arc<Mutex<HashMap<u64, UnboundedSender<Message>>>>;

/// Broadcast envelope for a client-originated request. Every connection's
/// send-loop subscribes to `AppState::request_tx`; each one forwards the
/// request to its peer except for the originator (skipped via `origin`).
#[derive(Clone, Debug)]
pub struct RequestEnvelope {
    pub origin: u64,
    pub request_id: u64,
    pub op: String,
    pub args: Value,
}

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMsg {
    Hello {
        #[serde(rename = "clientId", default)]
        #[allow(dead_code)]
        client_id: Option<String>,
    },
    Push {
        #[serde(default)]
        ops: Vec<Value>,
    },
    Ping,
    Pong,
    Request {
        request_id: u64,
        op: String,
        #[serde(default)]
        args: Value,
    },
    Response {
        request_id: u64,
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        value: Value,
        #[serde(default)]
        error: Option<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerMsg {
    Op {
        op: Value,
    },
    Ping,
    Pong,
    Lagged,
    PushResult {
        ok: bool,
        applied: usize,
        skipped: usize,
        conflicts: Vec<String>,
        errors: Vec<String>,
    },
    Error {
        error: String,
    },
    Request {
        request_id: u64,
        op: String,
        args: Value,
    },
    Response {
        request_id: u64,
        ok: bool,
        value: Value,
        error: Option<String>,
    },
}

pub async fn ws_upgrade(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(socket: WebSocket, state: AppState) {
    let (sender, receiver) = socket.split();
    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);

    // Subscribe to the broadcasts up-front (before spawning) so any message
    // published between connect and the send-task's first poll is buffered for
    // this receiver rather than dropped.
    let watch_rx = state.watch_tx.subscribe();
    let events_rx = state.events.subscribe();
    let request_rx = state.request_tx.subscribe();

    // mpsc funnels recv-side replies (pong, push-result, error, and response
    // frames that land on this connection's route) through the same SplitSink
    // the send-task owns; avoids an Arc<Mutex<_>> around it.
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    let recv_task = tokio::spawn(recv_loop(receiver, state.clone(), out_tx.clone(), conn_id));
    let send_task = tokio::spawn(send_loop(
        sender,
        state.clone(),
        out_rx,
        watch_rx,
        events_rx,
        request_rx,
        conn_id,
    ));

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    // On disconnect, purge any pending routes that pointed at this connection's
    // out_tx so routes to dead senders don't leak. The check is "sender is
    // closed", which the mpsc flags automatically once the receiver is dropped.
    let mut routes = state.pending_routes.lock().unwrap();
    routes.retain(|_, sink| !sink.is_closed());
}

async fn recv_loop(
    mut receiver: futures::stream::SplitStream<WebSocket>,
    state: AppState,
    out_tx: tokio::sync::mpsc::UnboundedSender<Message>,
    conn_id: u64,
) {
    while let Some(frame) = receiver.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(_) => break,
        };
        match frame {
            Message::Text(txt) => match serde_json::from_str::<ClientMsg>(&txt) {
                Ok(ClientMsg::Hello { .. }) => {}
                Ok(ClientMsg::Pong) => {}
                Ok(ClientMsg::Ping) => {
                    let _ = send_server_msg(&out_tx, &ServerMsg::Pong);
                }
                Ok(ClientMsg::Push { ops }) => {
                    let res = apply_push_ops(&state, &ops);
                    let _ = send_server_msg(
                        &out_tx,
                        &ServerMsg::PushResult {
                            ok: res.errors.is_empty(),
                            applied: res.applied,
                            skipped: res.skipped,
                            conflicts: res.conflicts,
                            errors: res.errors,
                        },
                    );
                }
                Ok(ClientMsg::Request {
                    request_id,
                    op,
                    args,
                }) => {
                    // Stash the route so whoever responds later can find us.
                    {
                        let mut routes = state.pending_routes.lock().unwrap();
                        routes.insert(request_id, out_tx.clone());
                    }
                    // Broadcast to every other connection's send-loop.
                    let _ = state.request_tx.send(RequestEnvelope {
                        origin: conn_id,
                        request_id,
                        op,
                        args,
                    });
                }
                Ok(ClientMsg::Response {
                    request_id,
                    ok,
                    value,
                    error,
                }) => {
                    let sink = {
                        let mut routes = state.pending_routes.lock().unwrap();
                        routes.remove(&request_id)
                    };
                    if let Some(sink) = sink {
                        let msg = ServerMsg::Response {
                            request_id,
                            ok,
                            value,
                            error,
                        };
                        if let Ok(s) = serde_json::to_string(&msg) {
                            let _ = sink.send(Message::Text(s));
                        }
                    }
                }
                Err(e) => {
                    let _ = send_server_msg(
                        &out_tx,
                        &ServerMsg::Error {
                            error: format!("bad message: {e}"),
                        },
                    );
                }
            },
            Message::Ping(p) => {
                let _ = out_tx.send(Message::Pong(p));
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

async fn send_loop(
    mut sender: futures::stream::SplitSink<WebSocket, Message>,
    state: AppState,
    mut out_rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
    mut watch_rx: broadcast::Receiver<crate::watch::Op>,
    mut events_rx: broadcast::Receiver<String>,
    mut request_rx: broadcast::Receiver<RequestEnvelope>,
    conn_id: u64,
) {
    let mut ping_interval = tokio::time::interval(Duration::from_secs(10));
    // Skip the immediate first tick so we don't blast a ping at connect time.
    ping_interval.tick().await;

    loop {
        tokio::select! {
            outgoing = out_rx.recv() => {
                let Some(msg) = outgoing else { break };
                if sender.send(msg).await.is_err() { break; }
            }
            req_res = request_rx.recv() => {
                match req_res {
                    Ok(env) => {
                        if env.origin == conn_id { continue; }
                        let msg = ServerMsg::Request {
                            request_id: env.request_id,
                            op: env.op,
                            args: env.args,
                        };
                        if !send_ws_msg(&mut sender, &msg).await { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
            watch_res = watch_rx.recv() => {
                match watch_res {
                    Ok(op) => {
                        // Translate using the same shape `/poll` produces.
                        // Watcher paths are canonical — strip the canonical
                        // project root so `/private/tmp` / `/tmp` round-trip
                        // cleanly on macOS.
                        let root = state.canonical_project.as_path();
                        if let Some(po) = fs_op_to_plugin_op(root, &op) {
                            if !send_ws_msg(&mut sender, &ServerMsg::Op { op: po }).await {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = send_ws_msg(&mut sender, &ServerMsg::Lagged).await;
                        let _ = sender.send(Message::Close(None)).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            ev_res = events_rx.recv() => {
                match ev_res {
                    Ok(s) => {
                        // state.events already produces `{"type":"op",...}` for
                        // FS ops (the conflict-filtered copy that SSE used to
                        // send). The watch_tx branch above already emitted a
                        // plugin-shape op for the same event, so drop the dup.
                        if has_type(&s, "op") { continue; }
                        if sender.send(Message::Text(s)).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = send_ws_msg(&mut sender, &ServerMsg::Lagged).await;
                        let _ = sender.send(Message::Close(None)).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = ping_interval.tick() => {
                if !send_ws_msg(&mut sender, &ServerMsg::Ping).await {
                    break;
                }
            }
        }
    }
}

fn send_server_msg(
    out_tx: &tokio::sync::mpsc::UnboundedSender<Message>,
    msg: &ServerMsg,
) -> Result<(), ()> {
    let s = serde_json::to_string(msg).map_err(|_| ())?;
    out_tx.send(Message::Text(s)).map_err(|_| ())
}

async fn send_ws_msg(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    msg: &ServerMsg,
) -> bool {
    let Ok(s) = serde_json::to_string(msg) else {
        return true;
    };
    sender.send(Message::Text(s)).await.is_ok()
}

/// Cheap, parse-only probe for the top-level `"type"` field of a JSON object
/// string. Avoids a full deserialize for the event filter hot path.
fn has_type(s: &str, kind: &str) -> bool {
    serde_json::from_str::<Value>(s)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|n| n == kind))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict::ConflictEngine;
    use crate::watch::{Op, OpKind};
    #[allow(unused_imports)]
    use futures::{SinkExt as _, StreamExt as _};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;
    use tokio::sync::broadcast;
    use tokio_tungstenite::tungstenite;

    struct TestHarness {
        addr: SocketAddr,
        state: AppState,
        _tmp: tempfile::TempDir,
    }

    async fn start_server() -> TestHarness {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let canonical = std::fs::canonicalize(&project).unwrap();
        let (watch_tx, _) = broadcast::channel::<Op>(64);
        let (events_tx, _) = broadcast::channel::<String>(64);
        let (request_tx, _) = broadcast::channel::<RequestEnvelope>(64);

        let state = AppState {
            project: Arc::new(project),
            canonical_project: Arc::new(canonical),
            events: events_tx,
            conflict: Arc::new(ConflictEngine::new()),
            watch_tx,
            project_name: Arc::new(RwLock::new("test".into())),
            game_id: Arc::new(RwLock::new(None)),
            place_ids: Arc::new(RwLock::new(Vec::new())),
            pending_initial: Arc::new(Mutex::new(None)),
            push_quiet: Arc::new(Mutex::new(HashMap::<PathBuf, std::time::Instant>::new())),
            request_tx,
            pending_routes: Arc::new(Mutex::new(HashMap::new())),
        };

        let app = crate::http::router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        TestHarness {
            addr,
            state,
            _tmp: tmp,
        }
    }

    async fn recv_until_type(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        kind: &str,
        limit: Duration,
    ) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let msg = match tokio::time::timeout(remaining, ws.next()).await {
                Ok(Some(Ok(m))) => m,
                _ => return None,
            };
            if let tungstenite::Message::Text(t) = msg {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v.get("type").and_then(|x| x.as_str()) == Some(kind) {
                        return Some(v);
                    }
                }
            }
        }
    }

    /// End-to-end test of the request/response multiplex. Two WS clients
    /// connect to the same daemon: a "fake plugin" (which responds to any
    /// request it sees) and a "fake CLI" (which sends a `get` request). The
    /// daemon must forward the request from the CLI socket to the plugin
    /// socket, then route the plugin's response back to the CLI socket.
    #[tokio::test]
    async fn request_response_multiplex_routes_through_daemon() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);

        // Plugin connects first so it's subscribed when the CLI request lands.
        let (mut plugin, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        plugin
            .send(tungstenite::Message::Text(
                r#"{"type":"hello","clientId":"plugin"}"#.into(),
            ))
            .await
            .unwrap();

        // Wait until the plugin's send-loop has subscribed to request_tx.
        for _ in 0..50 {
            if h.state.request_tx.receiver_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(h.state.request_tx.receiver_count() >= 1);

        // CLI connects next.
        let (mut cli, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        cli.send(tungstenite::Message::Text(
            r#"{"type":"hello","clientId":"cli"}"#.into(),
        ))
        .await
        .unwrap();

        for _ in 0..50 {
            if h.state.request_tx.receiver_count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(h.state.request_tx.receiver_count() >= 2);

        // CLI sends a request.
        cli.send(tungstenite::Message::Text(
            r#"{"type":"request","request_id":42,"op":"get","args":{"path":"Workspace"}}"#.into(),
        ))
        .await
        .unwrap();

        // Plugin should receive the forwarded request.
        let forwarded = recv_until_type(&mut plugin, "request", Duration::from_secs(3))
            .await
            .expect("plugin should see forwarded request");
        assert_eq!(forwarded["request_id"], 42);
        assert_eq!(forwarded["op"], "get");
        assert_eq!(forwarded["args"]["path"], "Workspace");

        // Plugin replies.
        plugin
            .send(tungstenite::Message::Text(
                r#"{"type":"response","request_id":42,"ok":true,"value":{"class":"Workspace","name":"Workspace"},"error":null}"#.into(),
            ))
            .await
            .unwrap();

        // CLI should receive the routed response.
        let got = recv_until_type(&mut cli, "response", Duration::from_secs(3))
            .await
            .expect("CLI should receive routed response");
        assert_eq!(got["request_id"], 42);
        assert_eq!(got["ok"], true);
        assert_eq!(got["value"]["class"], "Workspace");

        // CLI should NOT see its own outgoing request echoed back (origin skip).
        let echoed = tokio::time::timeout(Duration::from_millis(200), async {
            while let Some(Ok(m)) = cli.next().await {
                if let tungstenite::Message::Text(t) = m {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    if v.get("type").and_then(|x| x.as_str()) == Some("request") {
                        return Some(v);
                    }
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert!(
            echoed.is_none(),
            "CLI must not receive its own request back"
        );
    }

    /// End-to-end test using the `remote::request` client against a real
    /// daemon-shaped server, with a fake plugin client that echoes a canned
    /// response. Proves the CLI's reader threads the right request_id
    /// through the daemon multiplexer.
    #[tokio::test]
    async fn remote_request_round_trips_through_daemon() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);

        // Fake plugin.
        let (mut plugin, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        plugin
            .send(tungstenite::Message::Text(
                r#"{"type":"hello","clientId":"plugin"}"#.into(),
            ))
            .await
            .unwrap();

        for _ in 0..50 {
            if h.state.request_tx.receiver_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Drive plugin from a spawned task: loop, see request frames, reply
        // with `value = {"got": args.path}`.
        let plugin_task = tokio::spawn(async move {
            while let Some(msg) = plugin.next().await {
                let Ok(tungstenite::Message::Text(t)) = msg else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&t) else {
                    continue;
                };
                if v.get("type").and_then(|x| x.as_str()) != Some("request") {
                    continue;
                }
                let rid = v.get("request_id").and_then(|x| x.as_u64()).unwrap();
                let path = v
                    .get("args")
                    .and_then(|a| a.get("path"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let resp = serde_json::json!({
                    "type": "response",
                    "request_id": rid,
                    "ok": true,
                    "value": { "got": path },
                    "error": null,
                });
                plugin
                    .send(tungstenite::Message::Text(resp.to_string()))
                    .await
                    .unwrap();
            }
        });

        let resp = crate::remote::request(
            h.addr.port(),
            "get",
            serde_json::json!({ "path": "Workspace/Baseplate" }),
        )
        .await
        .expect("remote::request");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"]["got"], "Workspace/Baseplate");

        plugin_task.abort();
    }

    #[tokio::test]
    async fn ws_forwards_watch_op_and_applies_push() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        ws.send(tungstenite::Message::Text(
            r#"{"type":"hello","clientId":"test-1"}"#.into(),
        ))
        .await
        .unwrap();

        // `connect_async` returns as soon as the HTTP 101 upgrade completes,
        // but axum's `on_upgrade` callback (which subscribes to the
        // broadcasts) runs independently. Wait until we observe a subscriber
        // so the `watch_tx.send` below isn't dropped on the floor.
        for _ in 0..50 {
            if h.state.watch_tx.receiver_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(h.state.watch_tx.receiver_count() >= 1);

        // Materialize a service dir + leaf script on disk, then inject the
        // corresponding watcher op so fs_op_to_plugin_op has a real file to
        // classify.
        let svc_dir = h.state.project.join("Workspace");
        std::fs::create_dir_all(&svc_dir).unwrap();
        let script_path = svc_dir.join("Hello.server.luau");
        std::fs::write(&script_path, b"print('hi')\n").unwrap();

        h.state
            .watch_tx
            .send(Op {
                kind: OpKind::Add,
                path: std::fs::canonicalize(&script_path).unwrap(),
                from: None,
                content: Some(b"print('hi')\n".to_vec()),
            })
            .unwrap();

        let got = recv_until_type(&mut ws, "op", Duration::from_secs(5))
            .await
            .expect("should receive op frame");
        assert_eq!(got["type"], "op");
        // Plugin-shape set op with path segments.
        assert_eq!(got["op"]["op"], "set");

        // Push a synced (Folder) instance via the WS channel. Post-scope-down,
        // the daemon only round-trips Folder + Script/LocalScript/ModuleScript,
        // so a Folder is the minimal non-script proof that `set` ops still
        // land on disk. No `.meta.json` should ever appear — property sync is
        // ripped out.
        let push = serde_json::json!({
            "type": "push",
            "ops": [
                {
                    "op": "set",
                    "path": ["Workspace"],
                    "node": {
                        "name": "Bin",
                        "class": "Folder",
                        "properties": {},
                        "children": []
                    }
                }
            ]
        });
        ws.send(tungstenite::Message::Text(push.to_string()))
            .await
            .unwrap();

        let res = recv_until_type(&mut ws, "push-result", Duration::from_secs(5))
            .await
            .expect("should receive push-result");
        assert_eq!(res["ok"], true);
        assert!(res["applied"].as_u64().unwrap() >= 1);
        let bin_dir = svc_dir.join("Bin");
        assert!(bin_dir.is_dir(), "Bin folder should be on disk");
        assert!(
            !bin_dir.join(".meta.json").exists(),
            ".meta.json must not be emitted for a Folder (property sync is ripped out)"
        );
    }
}
