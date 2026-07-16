// WebSocket transport for plugin ↔ daemon realtime traffic.
//
// Replaces the HTTP long-poll (`/poll`) + SSE (`/events`) pair with a single
// persistent connection. Everything non-realtime (/hello, /snapshot, /push as
// bootstrap, /initial-compare, /initial-decision, /resolve, etc.) still goes
// over HTTP.
//
// Wire framing: serde-tagged JSON over `Message::Text`.
//   ClientMsg (tag "type", lowercase):
//     {"type":"hello","clientId":"<string>","role":"plugin","protocol":2}
//     {"type":"push","ops":[<plugin-shape op>, ...]}
//     {"type":"ping"}   // server replies with pong
//     {"type":"pong"}   // reply to server ping (no-op)
//
//   ServerMsg (tag "type", kebab-case):
//     {"type":"op","op":<plugin-shape op>}
//     {"type":"ping"}            // 10-second heartbeat
//     {"type":"pong"}            // reply to client ping
//     {"type":"shutdown","reason":"..."} // daemon/plugin session is closing
//     {"type":"lagged"}          // broadcast overflow; close follows
//     {"type":"push-result", ok, applied, skipped, conflicts, errors}
//     {"type":"error","error":"..."}
//   Pre-existing event strings from `AppState::events` are passed through
//   unchanged (shapes like `{"type":"conflict",...}`, `{"type":"config-changed",...}`,
//   `{"type":"initial-choice-needed",...}`, etc.). Conflict-filtered
//   `type=="op"` events are translated to plugin shape here.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{hash_map::Entry, HashMap};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use crate::http::{apply_push_ops, event_to_plugin_op, is_authorized_widget_browser_request};
use crate::AppState;

pub(crate) const PLUGIN_PROTOCOL_VERSION: u64 = 2;

/// Routing table for in-flight request/response pairs, keyed by a daemon-owned
/// correlation id. Client-provided ids are retained only for translating the
/// final response back to the originating socket; they can never overwrite a
/// route belonging to another connection.
pub type PendingRoutes = Arc<Mutex<HashMap<u64, PendingRoute>>>;

pub struct PendingRoute {
    client_request_id: u64,
    origin_conn_id: u64,
    sink: UnboundedSender<Message>,
}

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
static NEXT_ROUTE_ID: AtomicU64 = AtomicU64::new(1);
static PLUGIN_CAPABILITY: OnceLock<String> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum PeerKind {
    Unidentified = 0,
    Plugin = 1,
    Client = 2,
    Watch = 3,
}

impl PeerKind {
    fn load(value: &AtomicU8) -> Self {
        match value.load(Ordering::Acquire) {
            1 => Self::Plugin,
            2 => Self::Client,
            3 => Self::Watch,
            _ => Self::Unidentified,
        }
    }

    fn store(self, value: &AtomicU8) {
        value.store(self as u8, Ordering::Release);
    }
}

/// Capability presented by the native Studio plugin during its WS hello.
/// Production runs host one daemon per process, so this CSPRNG value is both
/// process- and daemon-scoped. It is disclosed through `/hello`, which browser
/// callers can read only after widget-token CORS authorization.
pub(crate) fn plugin_capability() -> &'static str {
    PLUGIN_CAPABILITY
        .get_or_init(|| {
            crate::artifact::random_hex(32)
                .expect("operating-system randomness is required for plugin authentication")
        })
        .as_str()
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMsg {
    Hello {
        #[serde(rename = "clientId", default)]
        #[allow(dead_code)]
        client_id: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        protocol: Option<u64>,
        #[serde(rename = "pluginCapability", alias = "capability", default)]
        plugin_capability: Option<String>,
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
        error: Option<Value>,
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
    Shutdown {
        reason: String,
    },
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
        error: Option<Value>,
    },
}

pub async fn ws_upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Response {
    // Every browser socket, including custom-scheme and opaque `null`
    // webviews, must carry the widget owner capability in its query string.
    // Native CLI and Roblox Studio clients send no Origin and remain allowed.
    if let Some(origin) = headers.get(header::ORIGIN) {
        if !is_authorized_widget_browser_request(
            origin,
            &uri,
            state.widget_owned,
            state.widget_owner_token.as_ref().as_deref(),
        ) {
            return (
                StatusCode::FORBIDDEN,
                "browser WebSocket requires an authorized widget origin and capability",
            )
                .into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(socket: WebSocket, state: AppState) {
    let (sender, receiver) = socket.split();
    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);

    // Subscribe to the broadcasts up-front (before spawning) so any message
    // published between connect and the send-task's first poll is buffered for
    // this receiver rather than dropped.
    let events_rx = state.events.subscribe();
    let request_rx = state.request_tx.subscribe();

    // mpsc funnels recv-side replies (pong, push-result, error, and response
    // frames that land on this connection's route) through the same SplitSink
    // the send-task owns; avoids an Arc<Mutex<_>> around it.
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let peer_kind = Arc::new(AtomicU8::new(PeerKind::Unidentified as u8));

    let mut recv_task = tokio::spawn(recv_loop(
        receiver,
        state.clone(),
        out_tx.clone(),
        conn_id,
        peer_kind.clone(),
    ));
    let mut send_task = tokio::spawn(send_loop(
        sender,
        state.clone(),
        out_rx,
        events_rx,
        request_rx,
        conn_id,
        peer_kind,
    ));

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }

    // On disconnect, purge any pending routes that pointed at this connection's
    // out_tx so routes to dead senders don't leak. The check is "sender is
    // closed", which the mpsc flags automatically once the receiver is dropped.
    let mut routes = state.pending_routes.lock().unwrap();
    routes.retain(|_, route| route.origin_conn_id != conn_id && !route.sink.is_closed());
    let disconnected_plugin = {
        let mut active = state.active_plugin.lock().unwrap();
        if *active == Some(conn_id) {
            *active = None;
            true
        } else {
            false
        }
    };
    if disconnected_plugin {
        publish_plugin_state(&state, false);
    }
}

async fn recv_loop(
    mut receiver: futures::stream::SplitStream<WebSocket>,
    state: AppState,
    out_tx: tokio::sync::mpsc::UnboundedSender<Message>,
    conn_id: u64,
    shared_peer_kind: Arc<AtomicU8>,
) {
    let mut rejecting = false;
    let mut peer_kind = PeerKind::Unidentified;
    while let Some(frame) = receiver.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(_) => break,
        };
        match frame {
            Message::Text(txt) => match serde_json::from_str::<ClientMsg>(&txt) {
                _ if rejecting => {}
                Ok(ClientMsg::Hello {
                    client_id,
                    role,
                    protocol,
                    plugin_capability: presented_capability,
                }) => {
                    if peer_kind != PeerKind::Unidentified {
                        let _ = send_server_msg(
                            &out_tx,
                            &ServerMsg::Shutdown {
                                reason: "WebSocket hello may only be sent once".into(),
                            },
                        );
                        let _ = out_tx.send(Message::Close(None));
                        rejecting = true;
                        continue;
                    }
                    let classified_peer = classify_peer(role.as_deref(), client_id.as_deref());
                    let Some(classified_peer) = classified_peer else {
                        let _ = send_server_msg(
                            &out_tx,
                            &ServerMsg::Shutdown {
                                reason: format!(
                                    "unrecognized WebSocket role {:?}",
                                    role.as_deref().unwrap_or("missing")
                                ),
                            },
                        );
                        let _ = out_tx.send(Message::Close(None));
                        rejecting = true;
                        continue;
                    };
                    let plugin_peer = classified_peer == PeerKind::Plugin;
                    if protocol != Some(PLUGIN_PROTOCOL_VERSION) {
                        let got = protocol
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "missing".to_string());
                        let role_name = role.as_deref().unwrap_or("client");
                        let reinstall = if plugin_peer {
                            ". Reinstall the Studio plugin."
                        } else {
                            ""
                        };
                        let _ = send_server_msg(
                            &out_tx,
                            &ServerMsg::Shutdown {
                                reason: format!(
                                    "incompatible Ro Sync {role_name} protocol {got}; expected {}{reinstall}",
                                    PLUGIN_PROTOCOL_VERSION,
                                ),
                            },
                        );
                        let _ = out_tx.send(Message::Close(None));
                        rejecting = true;
                        continue;
                    }
                    if plugin_peer {
                        if !constant_time_capability_matches(presented_capability.as_deref()) {
                            let _ = send_server_msg(
                                &out_tx,
                                &ServerMsg::Shutdown {
                                    reason: "invalid or missing Studio plugin capability; reconnect or reinstall the Studio plugin"
                                        .into(),
                                },
                            );
                            let _ = out_tx.send(Message::Close(None));
                            rejecting = true;
                            continue;
                        }
                        let (already_active, became_active) = {
                            let mut active = state.active_plugin.lock().unwrap();
                            match *active {
                                Some(existing) if existing != conn_id => (true, false),
                                Some(_) => {
                                    PeerKind::Plugin.store(&shared_peer_kind);
                                    (false, false)
                                }
                                _ => {
                                    *active = Some(conn_id);
                                    PeerKind::Plugin.store(&shared_peer_kind);
                                    (false, true)
                                }
                            }
                        };
                        if already_active {
                            let _ = send_server_msg(
                                &out_tx,
                                &ServerMsg::Shutdown {
                                    reason: "another Roblox Studio plugin is already connected"
                                        .into(),
                                },
                            );
                            let _ = out_tx.send(Message::Close(None));
                            rejecting = true;
                            continue;
                        }
                        if became_active {
                            publish_plugin_state(&state, true);
                        }
                        peer_kind = PeerKind::Plugin;
                    } else {
                        peer_kind = classified_peer;
                        classified_peer.store(&shared_peer_kind);
                    }
                }
                Ok(ClientMsg::Pong) => {}
                Ok(ClientMsg::Ping) => {
                    let _ = send_server_msg(&out_tx, &ServerMsg::Pong);
                }
                Ok(ClientMsg::Push { ops }) => {
                    if peer_kind != PeerKind::Plugin {
                        let _ = send_server_msg(
                            &out_tx,
                            &ServerMsg::Error {
                                error: "push requires an authenticated plugin hello".into(),
                            },
                        );
                        continue;
                    }
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
                    if peer_kind != PeerKind::Client {
                        let _ = send_server_msg(
                            &out_tx,
                            &ServerMsg::Error {
                                error: "request requires a cli or agent hello".into(),
                            },
                        );
                        continue;
                    }
                    // Stash the route so whoever responds later can find us.
                    let daemon_request_id = register_pending_route(
                        &state.pending_routes,
                        request_id,
                        conn_id,
                        out_tx.clone(),
                    );
                    // Broadcast to every other connection's send-loop.
                    let _ = state.request_tx.send(RequestEnvelope {
                        origin: conn_id,
                        request_id: daemon_request_id,
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
                    if peer_kind != PeerKind::Plugin {
                        let _ = send_server_msg(
                            &out_tx,
                            &ServerMsg::Error {
                                error: "response requires an authenticated plugin hello".into(),
                            },
                        );
                        continue;
                    }
                    let sink = {
                        let mut routes = state.pending_routes.lock().unwrap();
                        routes.remove(&request_id)
                    };
                    if let Some(route) = sink {
                        let msg = ServerMsg::Response {
                            request_id: route.client_request_id,
                            ok,
                            value,
                            error,
                        };
                        if let Ok(s) = serde_json::to_string(&msg) {
                            let _ = route.sink.send(Message::Text(s));
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
    mut events_rx: broadcast::Receiver<String>,
    mut request_rx: broadcast::Receiver<RequestEnvelope>,
    conn_id: u64,
    peer_kind: Arc<AtomicU8>,
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
                        if env.origin == conn_id
                            || PeerKind::load(&peer_kind) != PeerKind::Plugin
                            || *state.active_plugin.lock().unwrap() != Some(conn_id)
                        {
                            continue;
                        }
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
            ev_res = events_rx.recv() => {
                match ev_res {
                    Ok(s) => {
                        if let Some(op) = event_to_plugin_op(state.canonical_project.as_path(), &s) {
                            if PeerKind::load(&peer_kind) != PeerKind::Plugin
                                || *state.active_plugin.lock().unwrap() != Some(conn_id)
                            {
                                continue;
                            }
                            if !send_ws_msg(&mut sender, &ServerMsg::Op { op }).await {
                                break;
                            }
                            continue;
                        }
                        let is_shutdown = has_type(&s, "shutdown");
                        if !is_shutdown && PeerKind::load(&peer_kind) != PeerKind::Watch {
                            continue;
                        }
                        if sender.send(Message::Text(s)).await.is_err() { break; }
                        if is_shutdown {
                            let _ = sender.send(Message::Close(None)).await;
                            break;
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
            _ = ping_interval.tick() => {
                if !send_ws_msg(&mut sender, &ServerMsg::Ping).await {
                    break;
                }
            }
        }
    }
}

fn register_pending_route(
    routes: &PendingRoutes,
    client_request_id: u64,
    origin_conn_id: u64,
    sink: UnboundedSender<Message>,
) -> u64 {
    let mut routes = routes.lock().unwrap();
    loop {
        let candidate = NEXT_ROUTE_ID.fetch_add(1, Ordering::Relaxed);
        if candidate == 0 {
            continue;
        }
        if let Entry::Vacant(entry) = routes.entry(candidate) {
            entry.insert(PendingRoute {
                client_request_id,
                origin_conn_id,
                sink,
            });
            return candidate;
        }
    }
}

fn constant_time_capability_matches(candidate: Option<&str>) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let expected = plugin_capability();
    if candidate.len() != expected.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (&candidate, &expected) in candidate.as_bytes().iter().zip(expected.as_bytes()) {
        difference |= candidate ^ expected;
    }
    difference == 0
}

fn send_server_msg(
    out_tx: &tokio::sync::mpsc::UnboundedSender<Message>,
    msg: &ServerMsg,
) -> Result<(), ()> {
    let s = serde_json::to_string(msg).map_err(|_| ())?;
    out_tx.send(Message::Text(s)).map_err(|_| ())
}

fn classify_peer(role: Option<&str>, client_id: Option<&str>) -> Option<PeerKind> {
    match role {
        Some("plugin") | Some("studio-plugin") | Some("roblox-plugin") => {
            return Some(PeerKind::Plugin)
        }
        Some("cli") | Some("agent") => return Some(PeerKind::Client),
        Some("watch") => return Some(PeerKind::Watch),
        Some(_) => return None,
        None => {}
    }

    let client_id = client_id.unwrap_or("").trim();
    (!client_id.is_empty() && client_id.chars().all(|ch| ch.is_ascii_digit()))
        .then_some(PeerKind::Plugin)
}

fn publish_plugin_state(state: &AppState, connected: bool) {
    let _ = state.events.send(
        serde_json::json!({
            "type": "plugin",
            "connected": connected,
        })
        .to_string(),
    );
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
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    struct TestHarness {
        addr: SocketAddr,
        state: AppState,
        _tmp: tempfile::TempDir,
    }

    async fn start_server() -> TestHarness {
        start_server_with_widget(false).await
    }

    async fn start_server_with_widget(widget_owned: bool) -> TestHarness {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let canonical = std::fs::canonicalize(&project).unwrap();
        let (events_tx, _) = broadcast::channel::<String>(64);
        let (request_tx, _) = broadcast::channel::<RequestEnvelope>(64);
        let (shutdown_tx, _) = tokio::sync::watch::channel::<Option<String>>(None);

        let state = AppState {
            project: Arc::new(project),
            canonical_project: Arc::new(canonical.clone()),
            events: events_tx,
            conflict: Arc::new(ConflictEngine::new()),
            artifacts: crate::artifact::ArtifactStore::new(
                canonical.join(".rosync-artifacts"),
                8 * 1024 * 1024,
                Duration::from_secs(60),
            )
            .unwrap(),
            project_name: Arc::new(RwLock::new("test".into())),
            game_id: Arc::new(RwLock::new(None)),
            group_id: Arc::new(RwLock::new(None)),
            place_ids: Arc::new(RwLock::new(Vec::new())),
            wally_enabled: Arc::new(RwLock::new(false)),
            wally_folder: Arc::new(RwLock::new(None)),
            pending_initial: Arc::new(Mutex::new(None)),
            push_quiet: Arc::new(Mutex::new(HashMap::<PathBuf, std::time::Instant>::new())),
            request_tx,
            pending_routes: Arc::new(Mutex::new(HashMap::new())),
            active_plugin: Arc::new(Mutex::new(None)),
            widget_owned,
            widget_owner_token: Arc::new(widget_owned.then(|| "test-widget-token".to_string())),
            widget_last_seen: Arc::new(Mutex::new(None)),
            shutdown_tx,
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

    fn plugin_hello(client_id: &str) -> tungstenite::Message {
        tungstenite::Message::Text(
            serde_json::json!({
                "type": "hello",
                "clientId": client_id,
                "role": "plugin",
                "protocol": PLUGIN_PROTOCOL_VERSION,
                "pluginCapability": plugin_capability(),
            })
            .to_string(),
        )
    }

    fn client_hello(client_id: &str, role: &str) -> tungstenite::Message {
        tungstenite::Message::Text(
            serde_json::json!({
                "type": "hello",
                "clientId": client_id,
                "role": role,
                "protocol": PLUGIN_PROTOCOL_VERSION,
            })
            .to_string(),
        )
    }

    async fn wait_for_active_plugin(harness: &TestHarness) {
        for _ in 0..100 {
            if harness.state.active_plugin.lock().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("plugin hello was not accepted in time");
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

    #[tokio::test]
    async fn http_responses_do_not_grant_cross_origin_browser_access() {
        let h = start_server_with_widget(true).await;
        let client = reqwest::Client::new();
        let base = format!("http://{}", h.addr);

        let response = client
            .get(format!("{base}/hello"))
            .header(reqwest::header::ORIGIN, "https://attacker.example")
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert!(response
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());

        let preflight = client
            .request(reqwest::Method::OPTIONS, format!("{base}/push"))
            .header(reqwest::header::ORIGIN, "https://attacker.example")
            .header(reqwest::header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                reqwest::header::ACCESS_CONTROL_REQUEST_HEADERS,
                "content-type",
            )
            .send()
            .await
            .unwrap();
        assert!(preflight
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());

        for widget_origin in ["null", "terminal64://widget", "http://127.0.0.1:49173"] {
            let response = client
                .get(format!("{base}/hello"))
                .header(reqwest::header::ORIGIN, widget_origin)
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            assert!(response
                .headers()
                .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none());

            let authorized = client
                .get(format!("{base}/hello?widgetToken=test-widget-token"))
                .header(reqwest::header::ORIGIN, widget_origin)
                .send()
                .await
                .unwrap();
            assert_eq!(
                authorized
                    .headers()
                    .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .and_then(|value| value.to_str().ok()),
                Some(widget_origin)
            );
        }

        let trusted_preflight = client
            .request(
                reqwest::Method::OPTIONS,
                format!("{base}/push?widgetToken=test-widget-token"),
            )
            .header(reqwest::header::ORIGIN, "null")
            .header(reqwest::header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                reqwest::header::ACCESS_CONTROL_REQUEST_HEADERS,
                "content-type",
            )
            .send()
            .await
            .unwrap();
        assert!(trusted_preflight.status().is_success());
        assert_eq!(
            trusted_preflight
                .headers()
                .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("null")
        );
    }

    #[tokio::test]
    async fn browser_origin_websocket_handshake_is_rejected() {
        let h = start_server_with_widget(true).await;
        let url = format!("ws://{}/ws?widgetToken=test-widget-token", h.addr);
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::ORIGIN,
            tungstenite::http::HeaderValue::from_static("https://attacker.example"),
        );

        let error = tokio_tungstenite::connect_async(request)
            .await
            .expect_err("browser-origin socket must not upgrade");
        match error {
            tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), tungstenite::http::StatusCode::FORBIDDEN);
            }
            other => panic!("expected HTTP 403 handshake response, got {other}"),
        }
        assert!(h.state.active_plugin.lock().unwrap().is_none());
        assert_eq!(h.state.request_tx.receiver_count(), 0);
    }

    #[tokio::test]
    async fn authorized_widget_origin_websocket_handshakes_are_allowed() {
        let h = start_server_with_widget(true).await;
        let url = format!("ws://{}/ws?widgetToken=test-widget-token", h.addr);

        for trusted_origin in ["null", "terminal64://widget", "http://127.0.0.1:49173"] {
            let mut request = url.clone().into_client_request().unwrap();
            request.headers_mut().insert(
                tungstenite::http::header::ORIGIN,
                tungstenite::http::HeaderValue::from_bytes(trusted_origin.as_bytes()).unwrap(),
            );
            let (mut ws, _) = tokio_tungstenite::connect_async(request)
                .await
                .unwrap_or_else(|error| {
                    panic!("trusted origin {trusted_origin} should upgrade: {error}")
                });
            ws.send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
                .await
                .unwrap();
            let pong = recv_until_type(&mut ws, "pong", Duration::from_secs(3))
                .await
                .unwrap_or_else(|| panic!("trusted origin {trusted_origin} should receive pong"));
            assert_eq!(pong["type"], "pong");
            ws.close(None).await.unwrap();
        }
    }

    #[tokio::test]
    async fn widget_origin_without_capability_is_rejected() {
        let h = start_server_with_widget(true).await;
        for origin in ["null", "terminal64://widget", "http://127.0.0.1:49173"] {
            let url = format!("ws://{}/ws", h.addr);
            let mut request = url.into_client_request().unwrap();
            request.headers_mut().insert(
                tungstenite::http::header::ORIGIN,
                tungstenite::http::HeaderValue::from_bytes(origin.as_bytes()).unwrap(),
            );
            let error = tokio_tungstenite::connect_async(request)
                .await
                .expect_err("widget origin without owner capability must be rejected");
            assert!(matches!(
                error,
                tungstenite::Error::Http(ref response)
                    if response.status() == tungstenite::http::StatusCode::FORBIDDEN
            ));
        }
    }

    #[tokio::test]
    async fn originless_native_websocket_handshake_is_allowed() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        ws.send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        let pong = recv_until_type(&mut ws, "pong", Duration::from_secs(3))
            .await
            .expect("originless native client should remain connected");
        assert_eq!(pong["type"], "pong");
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
        plugin.send(plugin_hello("plugin")).await.unwrap();
        wait_for_active_plugin(&h).await;

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
        cli.send(client_hello("cli", "cli")).await.unwrap();

        let (mut watch, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        watch
            .send(client_hello("terminal64-widget", "watch"))
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
        assert!(
            recv_until_type(&mut watch, "request", Duration::from_millis(200))
                .await
                .is_none(),
            "watch peers must never receive command requests"
        );
        let daemon_request_id = forwarded["request_id"].as_u64().unwrap();
        assert_eq!(forwarded["op"], "get");
        assert_eq!(forwarded["args"]["path"], "Workspace");

        // Plugin replies.
        plugin
            .send(tungstenite::Message::Text(
                serde_json::json!({
                    "type": "response",
                    "request_id": daemon_request_id,
                    "ok": true,
                    "value": {"class": "Workspace", "name": "Workspace"},
                    "error": null,
                })
                .to_string(),
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

    #[tokio::test]
    async fn identical_client_request_ids_get_distinct_daemon_routes() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut plugin, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        plugin.send(plugin_hello("plugin")).await.unwrap();
        wait_for_active_plugin(&h).await;
        let (mut first, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        first.send(client_hello("first", "cli")).await.unwrap();
        let (mut second, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        second.send(client_hello("second", "agent")).await.unwrap();

        first
            .send(tungstenite::Message::Text(
                r#"{"type":"request","request_id":7,"op":"get","args":{"path":"Workspace/First"}}"#
                    .into(),
            ))
            .await
            .unwrap();
        second
            .send(tungstenite::Message::Text(
                r#"{"type":"request","request_id":7,"op":"get","args":{"path":"Workspace/Second"}}"#
                    .into(),
            ))
            .await
            .unwrap();

        let mut daemon_ids = Vec::new();
        for _ in 0..2 {
            let request = recv_until_type(&mut plugin, "request", Duration::from_secs(3))
                .await
                .expect("plugin should receive both requests");
            let daemon_id = request["request_id"].as_u64().unwrap();
            daemon_ids.push(daemon_id);
            plugin
                .send(tungstenite::Message::Text(
                    serde_json::json!({
                        "type": "response",
                        "request_id": daemon_id,
                        "ok": true,
                        "value": { "path": request["args"]["path"] },
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
        }
        assert_ne!(daemon_ids[0], daemon_ids[1]);

        let first_response = recv_until_type(&mut first, "response", Duration::from_secs(3))
            .await
            .expect("first client should receive its response");
        let second_response = recv_until_type(&mut second, "response", Duration::from_secs(3))
            .await
            .expect("second client should receive its response");
        assert_eq!(first_response["request_id"], 7);
        assert_eq!(second_response["request_id"], 7);
        assert_eq!(first_response["value"]["path"], "Workspace/First");
        assert_eq!(second_response["value"]["path"], "Workspace/Second");
    }

    #[tokio::test]
    async fn second_plugin_connection_is_rejected() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);

        let (mut first, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        first.send(plugin_hello("studio-a")).await.unwrap();
        wait_for_active_plugin(&h).await;

        let (mut second, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        second.send(plugin_hello("studio-b")).await.unwrap();

        let shutdown = recv_until_type(&mut second, "shutdown", Duration::from_secs(3))
            .await
            .expect("second plugin should be told to shut down");
        assert_eq!(
            shutdown["reason"],
            "another Roblox Studio plugin is already connected"
        );

        first
            .send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        let pong = recv_until_type(&mut first, "pong", Duration::from_secs(3))
            .await
            .expect("first plugin should remain connected");
        assert_eq!(pong["type"], "pong");
    }

    #[tokio::test]
    async fn plugin_role_requires_the_per_daemon_capability() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut impostor, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        impostor
            .send(tungstenite::Message::Text(
                serde_json::json!({
                    "type": "hello",
                    "clientId": "impostor",
                    "role": "plugin",
                    "protocol": PLUGIN_PROTOCOL_VERSION,
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let shutdown = recv_until_type(&mut impostor, "shutdown", Duration::from_secs(3))
            .await
            .expect("role-only plugin hello must be rejected");
        assert!(shutdown["reason"].as_str().unwrap().contains("capability"));
        assert!(h.state.active_plugin.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn websocket_role_is_immutable_after_the_first_hello() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut client, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        client.send(client_hello("cli", "cli")).await.unwrap();
        client
            .send(client_hello("terminal64-widget", "watch"))
            .await
            .unwrap();
        let shutdown = recv_until_type(&mut client, "shutdown", Duration::from_secs(3))
            .await
            .expect("a second hello must close the connection");
        assert!(shutdown["reason"]
            .as_str()
            .unwrap()
            .contains("only be sent once"));
    }

    #[tokio::test]
    async fn watch_role_cannot_issue_plugin_commands() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut watch, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        watch.send(client_hello("widget", "watch")).await.unwrap();
        watch
            .send(tungstenite::Message::Text(
                r#"{"type":"request","request_id":1,"op":"eval","args":{"source":"return 1"}}"#
                    .into(),
            ))
            .await
            .unwrap();
        let error = recv_until_type(&mut watch, "error", Duration::from_secs(3))
            .await
            .expect("watch command attempt should receive a protocol error");
        assert!(error["error"].as_str().unwrap().contains("cli or agent"));
        assert!(h.state.pending_routes.lock().unwrap().is_empty());
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
        plugin.send(plugin_hello("plugin")).await.unwrap();
        wait_for_active_plugin(&h).await;

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

        ws.send(plugin_hello("test-1")).await.unwrap();
        wait_for_active_plugin(&h).await;
        let (mut watch, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        watch
            .send(client_hello("terminal64-widget", "watch"))
            .await
            .unwrap();

        // `connect_async` returns as soon as the HTTP 101 upgrade completes,
        // but axum's `on_upgrade` callback (which subscribes to the
        // broadcasts) runs independently. Wait until we observe a subscriber
        // so the filtered event below isn't dropped on the floor.
        for _ in 0..50 {
            if h.state.events.receiver_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(h.state.events.receiver_count() >= 1);

        // Materialize a service dir + leaf script on disk, then inject the
        // corresponding watcher op so fs_op_to_plugin_op has a real file to
        // classify.
        let svc_dir = h.state.project.join("Workspace");
        std::fs::create_dir_all(&svc_dir).unwrap();
        let script_path = svc_dir.join("Hello.server.luau");
        std::fs::write(&script_path, b"print('hi')\n").unwrap();

        let watcher_op = Op {
            kind: OpKind::Add,
            path: std::fs::canonicalize(&script_path).unwrap(),
            from: None,
            content: Some(b"print('hi')\n".to_vec()),
        };
        h.state
            .events
            .send(serde_json::json!({ "type": "op", "op": watcher_op }).to_string())
            .unwrap();

        let got = recv_until_type(&mut ws, "op", Duration::from_secs(5))
            .await
            .expect("should receive op frame");
        assert_eq!(got["type"], "op");
        assert!(
            recv_until_type(&mut watch, "op", Duration::from_millis(200))
                .await
                .is_none(),
            "watch peers must never receive filesystem operation payloads"
        );
        // Plugin-shape set op with path segments.
        assert_eq!(got["op"]["op"], "set");

        // Push a synced Folder with a script descendant via the WS channel.
        // Empty plain Folders are ignored, so the child script proves the
        // container still materializes when it is needed for syncable content.
        // No `.meta.json` should ever appear — property sync is ripped out.
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
                        "children": [{
                            "name": "Child",
                            "class": "ModuleScript",
                            "properties": { "Source": "return {}\n" },
                            "children": []
                        }]
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
            bin_dir.join("Child.luau").is_file(),
            "Child script should be on disk"
        );
        assert!(
            !bin_dir.join(".meta.json").exists(),
            ".meta.json must not be emitted for a Folder (property sync is ripped out)"
        );
    }

    #[tokio::test]
    async fn incompatible_plugin_protocol_is_rejected_with_reinstall_message() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut plugin, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        plugin
            .send(tungstenite::Message::Text(
                r#"{"type":"hello","clientId":"plugin","role":"plugin","protocol":99}"#.into(),
            ))
            .await
            .unwrap();

        let shutdown = recv_until_type(&mut plugin, "shutdown", Duration::from_secs(3))
            .await
            .expect("incompatible plugin should be rejected");
        let reason = shutdown["reason"].as_str().unwrap();
        assert!(reason.contains("expected 2"));
        assert!(reason.contains("Reinstall"));
        assert!(h.state.active_plugin.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn keep_local_resolution_reaches_connected_plugin() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut plugin, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        plugin.send(plugin_hello("plugin")).await.unwrap();
        for _ in 0..50 {
            if h.state.active_plugin.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let workspace = h.state.canonical_project.join("Workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let script = workspace.join("Safe.server.luau");
        std::fs::write(&script, b"local-edit\n").unwrap();
        h.state
            .conflict
            .record_sync(&script, crate::conflict::hash(b"base\n"), 1);
        assert_eq!(
            h.state
                .conflict
                .on_studio_push(&script, b"studio-edit\n", Some((b"local-edit\n", 2)),),
            crate::conflict::StudioDecision::Conflict
        );

        let plugin_task = tokio::spawn(async move {
            let got = recv_until_type(&mut plugin, "op", Duration::from_secs(3))
                .await
                .expect("resolved local source should reach Studio");
            let path = serde_json::json!(["Workspace", "Safe"]);
            plugin
                .send(tungstenite::Message::Text(
                    serde_json::json!({
                        "type": "push",
                        "ops": [{
                            "op": "update",
                            "path": path,
                            "properties": { "Source": "local-edit\n" },
                        }],
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
            got
        });

        let response = reqwest::Client::new()
            .post(format!("http://{}/resolve", h.addr))
            .json(&serde_json::json!({
                "path": "Workspace/Safe.server.luau",
                "resolution": "keep-local",
            }))
            .send()
            .await
            .unwrap();
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["ok"], true);

        let got = plugin_task.await.unwrap();
        assert_eq!(got["op"]["op"], "set");
        assert_eq!(got["op"]["node"]["properties"]["Source"], "local-edit\n");
    }
}
