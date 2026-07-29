// WebSocket transport for plugin ↔ daemon realtime traffic.
//
// Replaces the HTTP long-poll (`/poll`) + SSE (`/events`) pair with a single
// persistent connection. Everything non-realtime (/hello, /snapshot, /push as
// bootstrap, /initial-compare, /initial-decision, /resolve, etc.) still goes
// over HTTP.
//
// Wire framing: serde-tagged JSON over `Message::Text`.
//   ClientMsg (tag "type", lowercase):
//     {"type":"hello","clientId":"<string>","role":"plugin","protocol":6}
//     {"type":"push","ops":[<plugin-shape op>, ...]}
//     {"type":"ping"}   // server replies with pong
//     {"type":"pong"}   // reply to server ping (no-op)
//     {"type":"request","request_id":1,"op":"get","args":{},"timeout_ms":5000}
//                         // timeout is optional for backward-compatible clients
//
//   ServerMsg (tag "type", kebab-case):
//     {"type":"op","op":<plugin-shape op>}
//     {"type":"ping"}            // 10-second heartbeat
//     {"type":"pong"}            // reply to client ping
//     {"type":"shutdown","reason":"...","code":"...","retryable":true}
//                                      // daemon/plugin session is closing
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
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, watch};

use crate::http::{apply_push_ops, event_to_plugin_ops, is_authorized_widget_browser_request};
use crate::AppState;

pub(crate) const PLUGIN_PROTOCOL_VERSION: u64 = 6;
const PLUGIN_INBOUND_TIMEOUT: Duration = Duration::from_secs(45);
const PLUGIN_POST_WAKE_GRACE: Duration = Duration::from_secs(30);
const UNIDENTIFIED_HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const OUTBOUND_QUEUE_CAPACITY: usize = 256;
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING_ROUTES: usize = 1024;
const MAX_PENDING_ROUTES_PER_CONNECTION: usize = 64;
const DEFAULT_PENDING_ROUTE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(crate) const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const PENDING_ROUTE_COMPLETION_GRACE: Duration = Duration::from_secs(10);
const PENDING_ROUTE_SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Routing table for in-flight request/response pairs, keyed by a daemon-owned
/// correlation id. Client-provided ids are retained only for translating the
/// final response back to the originating socket; they can never overwrite a
/// route belonging to another connection.
pub type PendingRoutes = Arc<Mutex<HashMap<u64, PendingRoute>>>;

pub struct PendingRoute {
    client_request_id: u64,
    origin_conn_id: u64,
    sink: OutboundHandle,
    activity_op: String,
    activity_detail: Value,
    activity_started_at: Instant,
    expires_at: Instant,
    publish_activity: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingRouteLimit {
    Global,
    Connection,
}

impl PendingRouteLimit {
    fn code(self) -> &'static str {
        match self {
            Self::Global => "pending-route-capacity",
            Self::Connection => "connection-route-capacity",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Global => "the daemon has reached its pending Studio request capacity",
            Self::Connection => "this connection has reached its pending Studio request capacity",
        }
    }
}

#[derive(Clone, Debug)]
struct ConnectionClose {
    reason: String,
    code: &'static str,
    retryable: bool,
}

impl ConnectionClose {
    fn new(reason: impl Into<String>, code: &'static str, retryable: bool) -> Self {
        Self {
            reason: reason.into(),
            code,
            retryable,
        }
    }
}

/// Bounded normal-priority queue plus an independent close signal. A stalled
/// peer can fill the data queue, but overload still wakes the send task and
/// cannot retain unbounded messages or block connection/plugin cleanup.
#[derive(Clone)]
struct OutboundHandle {
    tx: mpsc::Sender<Message>,
    close_tx: watch::Sender<Option<ConnectionClose>>,
}

impl OutboundHandle {
    fn send(&self, message: Message) -> Result<(), ()> {
        match self.tx.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.close(ConnectionClose::new(
                    "outbound WebSocket queue exceeded its bounded capacity",
                    "outbound-overload",
                    true,
                ));
                Err(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
        }
    }

    fn close(&self, reason: ConnectionClose) {
        if self.close_tx.borrow().is_none() {
            self.close_tx.send_replace(Some(reason));
        }
    }

    fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
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

struct PeerState {
    kind: AtomicU8,
    connected_at: Instant,
    last_inbound: Mutex<Instant>,
}

struct SendPeer {
    conn_id: u64,
    state: Arc<PeerState>,
    enforce_hello_deadline: bool,
}

impl PeerState {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            kind: AtomicU8::new(PeerKind::Unidentified as u8),
            connected_at: now,
            last_inbound: Mutex::new(now),
        }
    }
}

fn unidentified_hello_expired(
    peer_kind: PeerKind,
    enforce_deadline: bool,
    connected_at: Instant,
    now: Instant,
) -> bool {
    enforce_deadline
        && peer_kind == PeerKind::Unidentified
        && now.saturating_duration_since(connected_at) >= UNIDENTIFIED_HELLO_TIMEOUT
}

fn plugin_inbound_expired(peer_kind: PeerKind, last_seen: Instant, now: Instant) -> bool {
    peer_kind == PeerKind::Plugin
        && now.saturating_duration_since(last_seen) > PLUGIN_INBOUND_TIMEOUT
}

fn plugin_inbound_should_close(
    peer_kind: PeerKind,
    last_seen: Instant,
    suspect_since: Option<Instant>,
    now: Instant,
) -> bool {
    plugin_inbound_expired(peer_kind, last_seen, now)
        && suspect_since
            .is_some_and(|since| now.saturating_duration_since(since) > PLUGIN_POST_WAKE_GRACE)
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
        #[serde(default, alias = "timeoutMs")]
        timeout_ms: Option<u64>,
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
        code: &'static str,
        retryable: bool,
    },
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
    let enforce_hello_deadline = headers.get(header::ORIGIN).is_none();
    if let Some(origin) = headers.get(header::ORIGIN) {
        if !is_authorized_widget_browser_request(
            origin,
            &uri,
            state.managed,
            state.manager_owner_token.as_ref().as_deref(),
        ) {
            return (
                StatusCode::FORBIDDEN,
                "browser WebSocket requires an authorized widget origin and capability",
            )
                .into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_ws(socket, state, enforce_hello_deadline))
}

async fn handle_ws(socket: WebSocket, state: AppState, enforce_hello_deadline: bool) {
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
    let (out_tx, out_rx) = mpsc::channel::<Message>(OUTBOUND_QUEUE_CAPACITY);
    let (close_tx, close_rx) = watch::channel::<Option<ConnectionClose>>(None);
    let outbound = OutboundHandle {
        tx: out_tx,
        close_tx,
    };
    let peer = Arc::new(PeerState::new());

    let mut recv_task = tokio::spawn(recv_loop(
        receiver,
        state.clone(),
        outbound.clone(),
        conn_id,
        peer.clone(),
    ));
    let mut send_task = tokio::spawn(send_loop(
        sender,
        state.clone(),
        out_rx,
        close_rx,
        events_rx,
        request_rx,
        SendPeer {
            conn_id,
            state: peer,
            enforce_hello_deadline,
        },
    ));

    tokio::select! {
        _ = &mut send_task => {
            recv_task.abort();
            let _ = recv_task.await;
        },
        _ = &mut recv_task => {
            send_task.abort();
            let _ = send_task.await;
        },
    }

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
        fail_all_pending_routes(
            &state,
            "plugin-disconnected",
            "Roblox Studio plugin disconnected before responding",
        );
        publish_plugin_state(&state, false);
    }

    // On disconnect, purge any pending routes that originated at this
    // connection or point at a queue whose receiver has gone away.
    let aborted_activities = {
        let mut routes = state.pending_routes.lock().unwrap();
        let mut events = Vec::new();
        routes.retain(|request_id, route| {
            let keep = route.origin_conn_id != conn_id && !route.sink.is_closed();
            if !keep {
                if let Some(event) = command_activity_event(
                    state.boot_id.as_str(),
                    *request_id,
                    route,
                    CommandActivityPhase::Aborted,
                    None,
                ) {
                    events.push(event);
                }
            }
            keep
        });
        events
    };
    for event in aborted_activities {
        let _ = state.events.send(event);
    }
}

async fn recv_loop(
    mut receiver: futures::stream::SplitStream<WebSocket>,
    state: AppState,
    out_tx: OutboundHandle,
    conn_id: u64,
    peer: Arc<PeerState>,
) {
    let mut rejecting = false;
    let mut peer_kind = PeerKind::Unidentified;
    while let Some(frame) = receiver.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(_) => break,
        };
        if let Ok(mut seen) = peer.last_inbound.lock() {
            *seen = Instant::now();
        }
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
                                code: "hello-already-sent",
                                retryable: false,
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
                                code: "unrecognized-role",
                                retryable: false,
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
                                code: "protocol-mismatch",
                                retryable: false,
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
                                    code: "stale-plugin-capability",
                                    retryable: true,
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
                                    PeerKind::Plugin.store(&peer.kind);
                                    (false, false)
                                }
                                _ => {
                                    *active = Some(conn_id);
                                    PeerKind::Plugin.store(&peer.kind);
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
                                    code: "plugin-already-connected",
                                    retryable: true,
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
                        classified_peer.store(&peer.kind);
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
                    timeout_ms,
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
                    if state.active_plugin.lock().unwrap().is_none() {
                        let _ = send_server_msg(
                            &out_tx,
                            &ServerMsg::Response {
                                request_id,
                                ok: false,
                                value: Value::Null,
                                error: Some(serde_json::json!({
                                    "code": "plugin-unavailable",
                                    "message": "Roblox Studio plugin is not connected",
                                    "retryable": true,
                                })),
                            },
                        );
                        continue;
                    }
                    // Stash the route so whoever responds later can find us.
                    let daemon_request_id = match register_pending_route(
                        &state.pending_routes,
                        request_id,
                        conn_id,
                        out_tx.clone(),
                        &op,
                        &args,
                        timeout_ms,
                    ) {
                        Ok(request_id) => request_id,
                        Err(limit) => {
                            let _ = send_server_msg(
                                &out_tx,
                                &ServerMsg::Response {
                                    request_id,
                                    ok: false,
                                    value: Value::Null,
                                    error: Some(serde_json::json!({
                                        "code": limit.code(),
                                        "message": limit.message(),
                                        "retryable": true,
                                    })),
                                },
                            );
                            continue;
                        }
                    };
                    publish_command_activity_started(&state, daemon_request_id);
                    // Broadcast to every other connection's send-loop.
                    let broadcast_failed = state
                        .request_tx
                        .send(RequestEnvelope {
                            origin: conn_id,
                            request_id: daemon_request_id,
                            op,
                            args,
                        })
                        .is_err();
                    if broadcast_failed || state.active_plugin.lock().unwrap().is_none() {
                        fail_pending_route(
                            &state,
                            daemon_request_id,
                            "request-broadcast-unavailable",
                            "request could not be delivered to the Studio plugin",
                        );
                    }
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
                        if let Some(event) = command_activity_event(
                            state.boot_id.as_str(),
                            request_id,
                            &route,
                            CommandActivityPhase::Completed,
                            Some(ok),
                        ) {
                            let _ = state.events.send(event);
                        }
                        let msg = ServerMsg::Response {
                            request_id: route.client_request_id,
                            ok,
                            value,
                            error,
                        };
                        if send_server_msg(&route.sink, &msg).is_err() {
                            route.sink.close(ConnectionClose::new(
                                "response could not enter the bounded outbound queue",
                                "response-delivery-overload",
                                true,
                            ));
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
    mut out_rx: mpsc::Receiver<Message>,
    mut close_rx: watch::Receiver<Option<ConnectionClose>>,
    mut events_rx: broadcast::Receiver<String>,
    mut request_rx: broadcast::Receiver<RequestEnvelope>,
    peer: SendPeer,
) {
    let conn_id = peer.conn_id;
    let enforce_hello_deadline = peer.enforce_hello_deadline;
    let peer = peer.state;
    let mut ping_interval = tokio::time::interval(Duration::from_secs(10));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut route_sweep_interval = tokio::time::interval(PENDING_ROUTE_SWEEP_INTERVAL);
    route_sweep_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let hello_deadline_at = peer
        .connected_at
        .checked_add(UNIDENTIFIED_HELLO_TIMEOUT)
        .unwrap_or_else(Instant::now);
    let hello_deadline = tokio::time::sleep_until(hello_deadline_at.into());
    tokio::pin!(hello_deadline);
    // Skip the immediate first tick so we don't blast a ping at connect time.
    ping_interval.tick().await;
    // Likewise, the first pending-route sweep should happen after one full
    // interval instead of on every connection's first poll.
    route_sweep_interval.tick().await;
    let mut plugin_suspect_since = None;

    loop {
        tokio::select! {
            _ = &mut hello_deadline, if enforce_hello_deadline
                && PeerKind::load(&peer.kind) == PeerKind::Unidentified => {
                if unidentified_hello_expired(
                    PeerKind::load(&peer.kind),
                    enforce_hello_deadline,
                    peer.connected_at,
                    Instant::now(),
                ) {
                    close_connection(
                        &mut sender,
                        conn_id,
                        &ConnectionClose::new(
                            "originless WebSocket did not authenticate with a hello before the deadline",
                            "hello-timeout",
                            true,
                        ),
                    )
                    .await;
                    break;
                }
            }
            close_changed = close_rx.changed() => {
                if close_changed.is_err() {
                    break;
                }
                let close = close_rx.borrow().clone();
                if let Some(close) = close {
                    close_connection(&mut sender, conn_id, &close).await;
                    break;
                }
            }
            outgoing = out_rx.recv() => {
                let Some(msg) = outgoing else { break };
                if !send_ws_frame(&mut sender, msg, conn_id).await { break; }
            }
            req_res = request_rx.recv() => {
                match req_res {
                    Ok(env) => {
                        if env.origin == conn_id
                            || PeerKind::load(&peer.kind) != PeerKind::Plugin
                            || *state.active_plugin.lock().unwrap() != Some(conn_id)
                        {
                            continue;
                        }
                        let msg = ServerMsg::Request {
                            request_id: env.request_id,
                            op: env.op,
                            args: env.args,
                        };
                        if !send_ws_msg(&mut sender, &msg, conn_id).await {
                            fail_pending_route(
                                &state,
                                env.request_id,
                                "plugin-write-failed",
                                "request could not be written to the Studio plugin",
                            );
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        if PeerKind::load(&peer.kind) == PeerKind::Plugin
                            && *state.active_plugin.lock().unwrap() == Some(conn_id)
                        {
                            fail_all_pending_routes(
                                &state,
                                "request-broadcast-lagged",
                                &format!(
                                    "Studio plugin missed {skipped} request broadcast frame(s)"
                                ),
                            );
                            close_connection(
                                &mut sender,
                                conn_id,
                                &ConnectionClose::new(
                                    format!(
                                        "Studio plugin missed {skipped} request broadcast frame(s)"
                                    ),
                                    "request-broadcast-lagged",
                                    true,
                                ),
                            )
                            .await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            ev_res = events_rx.recv() => {
                match ev_res {
                    Ok(s) => {
                        let plugin_ops = event_to_plugin_ops(state.canonical_project.as_path(), &s);
                        if !plugin_ops.is_empty() {
                            let current_peer = PeerKind::load(&peer.kind);
                            if current_peer == PeerKind::Plugin {
                                let is_active = *state.active_plugin.lock().unwrap() == Some(conn_id);
                                let mut failed = false;
                                if is_active {
                                    for op in plugin_ops {
                                        if !send_ws_msg(&mut sender, &ServerMsg::Op { op }, conn_id).await {
                                            failed = true;
                                            break;
                                        }
                                    }
                                }
                                if failed {
                                    break;
                                }
                            } else if current_peer == PeerKind::Watch {
                                let mut failed = false;
                                for op in plugin_ops {
                                    if let Some(activity) = sanitized_sync_activity(&op) {
                                        if !send_ws_frame(
                                            &mut sender,
                                            Message::Text(activity.to_string()),
                                            conn_id,
                                        )
                                        .await {
                                            failed = true;
                                            break;
                                        }
                                    }
                                }
                                if failed {
                                    break;
                                }
                            }
                            continue;
                        }
                        let is_shutdown = has_type(&s, "shutdown");
                        if !is_shutdown && PeerKind::load(&peer.kind) != PeerKind::Watch {
                            continue;
                        }
                        if !send_ws_frame(&mut sender, Message::Text(s), conn_id).await { break; }
                        if is_shutdown {
                            let _ = send_ws_frame(&mut sender, Message::Close(None), conn_id).await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        close_connection(
                            &mut sender,
                            conn_id,
                            &ConnectionClose::new(
                                format!("WebSocket peer missed {skipped} daemon event frame(s)"),
                                "event-broadcast-lagged",
                                true,
                            ),
                        )
                        .await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = route_sweep_interval.tick() => {
                if PeerKind::load(&peer.kind) == PeerKind::Plugin
                    && *state.active_plugin.lock().unwrap() == Some(conn_id)
                {
                    fail_expired_pending_routes(&state, Instant::now());
                }
            }
            _ = ping_interval.tick() => {
                let current_peer = PeerKind::load(&peer.kind);
                let now = Instant::now();
                let plugin_timed_out = peer
                    .last_inbound
                    .lock()
                    .map_or(current_peer == PeerKind::Plugin, |seen| {
                        plugin_inbound_should_close(
                            current_peer,
                            *seen,
                            plugin_suspect_since,
                            now,
                        )
                    });
                if plugin_timed_out {
                    close_connection(
                        &mut sender,
                        conn_id,
                        &ConnectionClose::new(
                            "Studio plugin heartbeat remained silent after the post-wake grace period",
                            "plugin-heartbeat-timeout",
                            true,
                        ),
                    )
                    .await;
                    break;
                }
                let inbound_expired = peer
                    .last_inbound
                    .lock()
                    .map_or(current_peer == PeerKind::Plugin, |seen| {
                        plugin_inbound_expired(current_peer, *seen, now)
                    });
                if inbound_expired {
                    plugin_suspect_since.get_or_insert(now);
                } else {
                    plugin_suspect_since = None;
                }
                if !send_ws_msg(&mut sender, &ServerMsg::Ping, conn_id).await {
                    break;
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum CommandActivityPhase {
    Started,
    Completed,
    Aborted,
}

impl CommandActivityPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
        }
    }
}

fn publish_command_activity_started(state: &AppState, request_id: u64) {
    let event = {
        let routes = state.pending_routes.lock().unwrap();
        routes.get(&request_id).and_then(|route| {
            command_activity_event(
                state.boot_id.as_str(),
                request_id,
                route,
                CommandActivityPhase::Started,
                None,
            )
        })
    };
    if let Some(event) = event {
        let _ = state.events.send(event);
    }
}

fn command_activity_event(
    boot_id: &str,
    request_id: u64,
    route: &PendingRoute,
    phase: CommandActivityPhase,
    ok: Option<bool>,
) -> Option<String> {
    if !route.publish_activity {
        return None;
    }
    let mut event = serde_json::Map::new();
    event.insert("type".into(), Value::String("command-activity".into()));
    event.insert(
        "activityId".into(),
        Value::String(command_activity_id(boot_id, request_id)),
    );
    event.insert("phase".into(), Value::String(phase.as_str().into()));
    event.insert("op".into(), Value::String(route.activity_op.clone()));
    event.insert("detail".into(), route.activity_detail.clone());
    if !matches!(phase, CommandActivityPhase::Started) {
        event.insert(
            "durationMs".into(),
            Value::from(
                u64::try_from(route.activity_started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            ),
        );
    }
    if let Some(ok) = ok {
        event.insert("ok".into(), Value::Bool(ok));
        if !ok {
            // Plugin error values can contain source excerpts, property values,
            // or other private command inputs. The activity feed only needs a
            // stable failure label; the originating CLI still receives the full
            // error response through its private route.
            event.insert("error".into(), Value::String("Command failed".into()));
        }
    } else if matches!(phase, CommandActivityPhase::Aborted) {
        event.insert("ok".into(), Value::Bool(false));
        event.insert(
            "error".into(),
            Value::String("Requester disconnected".into()),
        );
    }
    serde_json::to_string(&Value::Object(event)).ok()
}

fn command_activity_id(boot_id: &str, request_id: u64) -> String {
    // A request counter is unique only for one daemon process. Scope it to a
    // non-reversible digest of the daemon boot identity so a desktop feed can
    // survive reconnects without merging a new command into a stale card.
    let digest = crate::conflict::hash(boot_id.as_bytes());
    let mut generation_bytes = [0u8; 8];
    generation_bytes.copy_from_slice(&digest[..8]);
    let generation = u64::from_be_bytes(generation_bytes);
    format!("request:{generation:016x}:{request_id}")
}

fn should_publish_command_activity(op: &str) -> bool {
    !matches!(
        op,
        "ping"
            | "version"
            | "capture_read"
            | "capture_close"
            | "photo_read"
            | "photo_close"
            | "transmit_read"
            | "transmit_close"
            | "playtest_run_poll"
            | "playtest_wait"
            | "playtest_status"
    )
}

fn sanitized_command_op(op: &str) -> String {
    let trimmed = op.trim();
    if !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        trimmed.to_string()
    } else {
        "unknown".to_string()
    }
}

fn sanitized_command_detail(op: &str, args: &Value) -> Value {
    let mut detail = serde_json::Map::new();
    match op {
        "get" | "inspect_ref" => {
            copy_activity_string(&mut detail, args, "path", "path", 512);
            copy_activity_string_alias(&mut detail, args, &["prop", "property"], "property", 128);
            copy_activity_string_alias(
                &mut detail,
                args,
                &["expectedClass", "class"],
                "expectedClass",
                128,
            );
            copy_activity_array_count(&mut detail, args, "props", "propertyCount");
        }
        "set" => {
            copy_activity_string(&mut detail, args, "path", "path", 512);
            copy_activity_string_alias(&mut detail, args, &["prop", "property"], "property", 128);
        }
        "ls" | "tree" => {
            copy_activity_string(&mut detail, args, "path", "path", 512);
            copy_activity_u64(&mut detail, args, "depth", "depth");
        }
        "query" => {
            copy_activity_string(&mut detail, args, "selector", "selector", 512);
            copy_activity_u64(&mut detail, args, "limit", "limit");
            copy_activity_array_count(&mut detail, args, "props", "propertyCount");
        }
        "find" => {
            copy_activity_string_alias(&mut detail, args, &["under", "root"], "under", 512);
            copy_activity_string(&mut detail, args, "className", "class", 128);
            copy_activity_string(&mut detail, args, "name", "name", 256);
        }
        "find_by_attr" => {
            copy_activity_string_alias(&mut detail, args, &["under", "root"], "under", 512);
            copy_activity_string(&mut detail, args, "name", "attribute", 128);
        }
        "new" => {
            copy_activity_string_alias(&mut detail, args, &["parent", "path"], "path", 512);
            copy_activity_string(&mut detail, args, "class", "class", 128);
            copy_activity_string(&mut detail, args, "name", "name", 256);
        }
        "rm" | "attr_ls" | "tag_ls" => {
            copy_activity_string(&mut detail, args, "path", "path", 512);
        }
        "mv" => {
            copy_activity_string(&mut detail, args, "from", "from", 512);
            copy_activity_string(&mut detail, args, "to", "to", 512);
        }
        "set_attr" | "rm_attr" => {
            copy_activity_string(&mut detail, args, "path", "path", 512);
            copy_activity_string(&mut detail, args, "name", "attribute", 128);
        }
        "add_tag" | "rm_tag" => {
            copy_activity_string(&mut detail, args, "path", "path", 512);
            copy_activity_string(&mut detail, args, "tag", "tag", 128);
        }
        "call" => {
            copy_activity_string(&mut detail, args, "path", "path", 512);
            copy_activity_string(&mut detail, args, "method", "method", 128);
            copy_activity_array_count(&mut detail, args, "args", "argumentCount");
        }
        "select_set" => {
            copy_activity_array_count(&mut detail, args, "paths", "selectionCount");
        }
        "clipboard_copy" => {
            let count = args
                .get("paths")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            detail.insert("itemCount".into(), Value::from(count as u64));
            detail.insert(
                "selectionMode".into(),
                Value::String(if count == 0 { "selection" } else { "paths" }.into()),
            );
        }
        "clipboard_paste" => {
            copy_activity_string(&mut detail, args, "parent", "parent", 512);
            copy_activity_array_count(&mut detail, args, "roots", "itemCount");
            copy_activity_u64(&mut detail, args, "byteLength", "byteLength");
        }
        "transaction_begin" | "transaction_finish" => {
            copy_activity_string(&mut detail, args, "id", "transaction", 128);
            copy_activity_string(&mut detail, args, "action", "action", 32);
            copy_activity_string(&mut detail, args, "name", "name", 256);
        }
        "waypoint" => {
            copy_activity_string(&mut detail, args, "name", "name", 256);
        }
        "class_info" => {
            copy_activity_string_alias(
                &mut detail,
                args,
                &["class_name", "className"],
                "class",
                128,
            );
        }
        "enum_list" => {
            copy_activity_string_alias(&mut detail, args, &["enum_name", "name"], "enum", 128);
        }
        "eval" => {
            copy_activity_string_length(&mut detail, args, "source", "sourceBytes");
        }
        "capture_prepare" | "photo_prepare" => {
            copy_activity_string(&mut detail, args, "focus", "focus", 512);
            copy_activity_string_alias(
                &mut detail,
                args,
                &["uiTarget", "ui_target"],
                "uiTarget",
                512,
            );
            copy_activity_string(&mut detail, args, "ui", "ui", 32);
            copy_activity_string(&mut detail, args, "view", "view", 32);
        }
        "capture_export" => {
            copy_activity_string(&mut detail, args, "format", "format", 32);
        }
        "playtest_start" | "playtest_run_start" => {
            copy_activity_string(&mut detail, args, "mode", "mode", 32);
            copy_activity_u64(&mut detail, args, "players", "players");
            copy_activity_string(&mut detail, args, "context", "context", 64);
            copy_activity_string(&mut detail, args, "identity", "identity", 32);
        }
        "playtest_request" => {
            copy_activity_string(&mut detail, args, "context", "context", 64);
            if let Some(operation) = args
                .get("op")
                .and_then(Value::as_str)
                .map(sanitized_command_op)
            {
                detail.insert("operation".into(), Value::String(operation));
            }
        }
        "playtest_capture" => {
            copy_activity_string(&mut detail, args, "context", "context", 64);
        }
        "playtest_stop" => {
            copy_activity_string(&mut detail, args, "jobId", "jobId", 128);
        }
        "logs" => {
            copy_activity_string(&mut detail, args, "level_min", "level", 16);
            copy_activity_u64(&mut detail, args, "limit", "limit");
        }
        _ => {}
    }
    Value::Object(detail)
}

fn copy_activity_string(
    out: &mut serde_json::Map<String, Value>,
    args: &Value,
    input_key: &str,
    output_key: &str,
    max_chars: usize,
) {
    if let Some(value) = args
        .get(input_key)
        .and_then(Value::as_str)
        .and_then(|value| sanitized_activity_string(value, max_chars))
    {
        out.insert(output_key.into(), Value::String(value));
    }
}

fn copy_activity_string_alias(
    out: &mut serde_json::Map<String, Value>,
    args: &Value,
    input_keys: &[&str],
    output_key: &str,
    max_chars: usize,
) {
    for input_key in input_keys {
        if let Some(value) = args
            .get(*input_key)
            .and_then(Value::as_str)
            .and_then(|value| sanitized_activity_string(value, max_chars))
        {
            out.insert(output_key.into(), Value::String(value));
            return;
        }
    }
}

fn copy_activity_string_length(
    out: &mut serde_json::Map<String, Value>,
    args: &Value,
    input_key: &str,
    output_key: &str,
) {
    if let Some(value) = args.get(input_key).and_then(Value::as_str) {
        out.insert(output_key.into(), Value::from(value.len() as u64));
    }
}

fn copy_activity_u64(
    out: &mut serde_json::Map<String, Value>,
    args: &Value,
    input_key: &str,
    output_key: &str,
) {
    if let Some(value) = args.get(input_key).and_then(Value::as_u64) {
        out.insert(output_key.into(), Value::from(value));
    }
}

fn copy_activity_array_count(
    out: &mut serde_json::Map<String, Value>,
    args: &Value,
    input_key: &str,
    output_key: &str,
) {
    if let Some(values) = args.get(input_key).and_then(Value::as_array) {
        out.insert(output_key.into(), Value::from(values.len() as u64));
    }
}

fn sanitized_activity_string(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut sanitized = String::new();
    for ch in value.chars().filter(|ch| !ch.is_control()).take(max_chars) {
        sanitized.push(ch);
    }
    (!sanitized.is_empty()).then_some(sanitized)
}

fn sanitized_sync_activity(plugin_op: &Value) -> Option<Value> {
    let operation = plugin_op.get("op")?.as_str()?;
    if !matches!(operation, "set" | "delete" | "rename" | "class_change") {
        return None;
    }
    let mut event = serde_json::Map::new();
    event.insert("type".into(), Value::String("sync-activity".into()));
    event.insert("op".into(), Value::String(operation.to_string()));

    copy_sync_path(&mut event, plugin_op, "path", "path");
    copy_sync_path(&mut event, plugin_op, "from", "from");
    copy_sync_path(&mut event, plugin_op, "to", "to");
    if operation == "set" {
        if let Some(node) = plugin_op.get("node") {
            copy_activity_string(&mut event, node, "class", "class", 128);
            copy_activity_string(&mut event, node, "name", "name", 256);
        }
    } else {
        copy_activity_string(&mut event, plugin_op, "class", "class", 128);
        let naming_path = plugin_op
            .get("to")
            .or_else(|| plugin_op.get("path"))
            .and_then(sanitized_instance_path);
        if let Some(name) = naming_path
            .as_deref()
            .and_then(|path| path.rsplit('/').next())
            .and_then(|name| sanitized_activity_string(name, 256))
        {
            event.insert("name".into(), Value::String(name));
        }
    }
    Some(Value::Object(event))
}

fn copy_sync_path(
    out: &mut serde_json::Map<String, Value>,
    source: &Value,
    input_key: &str,
    output_key: &str,
) {
    if let Some(path) = source.get(input_key).and_then(sanitized_instance_path) {
        out.insert(output_key.into(), Value::String(path));
    }
}

fn sanitized_instance_path(value: &Value) -> Option<String> {
    if let Some(path) = value.as_str() {
        return sanitized_activity_string(path, 512);
    }
    let segments = value.as_array()?;
    if segments.len() > 128 {
        return None;
    }
    let mut safe_segments = Vec::with_capacity(segments.len());
    for segment in segments {
        safe_segments.push(sanitized_activity_string(segment.as_str()?, 128)?);
    }
    sanitized_activity_string(&safe_segments.join("/"), 512)
}

fn register_pending_route(
    routes: &PendingRoutes,
    client_request_id: u64,
    origin_conn_id: u64,
    sink: OutboundHandle,
    op: &str,
    args: &Value,
    timeout_ms: Option<u64>,
) -> Result<u64, PendingRouteLimit> {
    let activity_op = sanitized_command_op(op);
    let activity_detail = sanitized_command_detail(op, args);
    let publish_activity = should_publish_command_activity(op);
    let activity_started_at = Instant::now();
    let expires_at = activity_started_at
        .checked_add(
            pending_route_timeout(timeout_ms)
                .checked_add(PENDING_ROUTE_COMPLETION_GRACE)
                .expect("bounded pending-route grace must fit Duration"),
        )
        .expect("bounded pending-route timeout must fit the monotonic clock");
    let mut routes = routes.lock().unwrap();
    if routes.len() >= MAX_PENDING_ROUTES {
        return Err(PendingRouteLimit::Global);
    }
    if routes
        .values()
        .filter(|route| route.origin_conn_id == origin_conn_id)
        .count()
        >= MAX_PENDING_ROUTES_PER_CONNECTION
    {
        return Err(PendingRouteLimit::Connection);
    }
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
                activity_op: activity_op.clone(),
                activity_detail: activity_detail.clone(),
                activity_started_at,
                expires_at,
                publish_activity,
            });
            return Ok(candidate);
        }
    }
}

fn pending_route_timeout(timeout_ms: Option<u64>) -> Duration {
    timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_PENDING_ROUTE_TIMEOUT)
        .clamp(Duration::from_millis(1), MAX_REQUEST_TIMEOUT)
}

fn fail_expired_pending_routes(state: &AppState, now: Instant) {
    let expired_request_ids = {
        let routes = state.pending_routes.lock().unwrap();
        routes
            .iter()
            .filter_map(|(request_id, route)| (now >= route.expires_at).then_some(*request_id))
            .collect::<Vec<_>>()
    };
    for request_id in expired_request_ids {
        fail_pending_route(
            state,
            request_id,
            "request-timeout",
            "Studio did not respond before this request's bounded deadline",
        );
    }
}

fn fail_pending_route(state: &AppState, request_id: u64, code: &str, message: &str) {
    let route = {
        let mut routes = state.pending_routes.lock().unwrap();
        routes.remove(&request_id)
    };
    let Some(route) = route else {
        return;
    };
    if let Some(event) = command_activity_event(
        state.boot_id.as_str(),
        request_id,
        &route,
        CommandActivityPhase::Completed,
        Some(false),
    ) {
        let _ = state.events.send(event);
    }
    let response = ServerMsg::Response {
        request_id: route.client_request_id,
        ok: false,
        value: Value::Null,
        error: Some(serde_json::json!({
            "code": code,
            "message": message,
            "retryable": true,
        })),
    };
    if send_server_msg(&route.sink, &response).is_err() {
        route.sink.close(ConnectionClose::new(
            "request failure could not enter the bounded outbound queue",
            "response-delivery-overload",
            true,
        ));
    }
}

fn fail_all_pending_routes(state: &AppState, code: &str, message: &str) {
    let request_ids = {
        let routes = state.pending_routes.lock().unwrap();
        routes.keys().copied().collect::<Vec<_>>()
    };
    for request_id in request_ids {
        fail_pending_route(state, request_id, code, message);
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

fn send_server_msg(out_tx: &OutboundHandle, msg: &ServerMsg) -> Result<(), ()> {
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
    conn_id: u64,
) -> bool {
    let Ok(s) = serde_json::to_string(msg) else {
        return true;
    };
    send_ws_frame(sender, Message::Text(s), conn_id).await
}

async fn send_ws_frame(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    message: Message,
    conn_id: u64,
) -> bool {
    match tokio::time::timeout(WS_WRITE_TIMEOUT, sender.send(message)).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            eprintln!("WebSocket connection {conn_id} write failed: {error}");
            false
        }
        Err(_) => {
            eprintln!(
                "WebSocket connection {conn_id} write timed out after {}s",
                WS_WRITE_TIMEOUT.as_secs()
            );
            false
        }
    }
}

async fn close_connection(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    conn_id: u64,
    close: &ConnectionClose,
) {
    eprintln!(
        "WebSocket connection {conn_id} closing [{}]: {}",
        close.code, close.reason
    );
    let _ = send_ws_msg(
        sender,
        &ServerMsg::Shutdown {
            reason: close.reason.clone(),
            code: close.code,
            retryable: close.retryable,
        },
        conn_id,
    )
    .await;
    let _ = send_ws_frame(sender, Message::Close(None), conn_id).await;
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

    #[test]
    fn only_silent_plugin_peers_expire() {
        let last_seen = Instant::now();
        let expired_at = last_seen + PLUGIN_INBOUND_TIMEOUT + Duration::from_millis(1);
        assert!(plugin_inbound_expired(
            PeerKind::Plugin,
            last_seen,
            expired_at
        ));
        assert!(!plugin_inbound_expired(
            PeerKind::Plugin,
            last_seen,
            last_seen + PLUGIN_INBOUND_TIMEOUT
        ));
        assert!(!plugin_inbound_expired(
            PeerKind::Client,
            last_seen,
            expired_at
        ));
        assert!(!plugin_inbound_expired(
            PeerKind::Watch,
            last_seen,
            expired_at
        ));
        assert!(!plugin_inbound_should_close(
            PeerKind::Plugin,
            last_seen,
            None,
            expired_at,
        ));
        assert!(!plugin_inbound_should_close(
            PeerKind::Plugin,
            last_seen,
            Some(expired_at),
            expired_at + PLUGIN_POST_WAKE_GRACE,
        ));
        assert!(plugin_inbound_should_close(
            PeerKind::Plugin,
            last_seen,
            Some(expired_at),
            expired_at + PLUGIN_POST_WAKE_GRACE + Duration::from_millis(1),
        ));
    }

    #[test]
    fn bounded_outbound_queue_signals_typed_overload_without_growing() {
        let (tx, mut rx) = mpsc::channel(1);
        let (close_tx, close_rx) = watch::channel(None);
        let outbound = OutboundHandle { tx, close_tx };
        assert!(outbound.send(Message::Text("first".into())).is_ok());
        assert!(outbound.send(Message::Text("second".into())).is_err());
        assert_eq!(rx.try_recv().unwrap(), Message::Text("first".into()));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let close = close_rx.borrow().clone().expect("typed overload close");
        assert_eq!(close.code, "outbound-overload");
        assert!(close.retryable);
    }

    #[test]
    fn only_unidentified_originless_peers_hit_the_hello_deadline() {
        let connected_at = Instant::now();
        assert!(!unidentified_hello_expired(
            PeerKind::Unidentified,
            true,
            connected_at,
            connected_at + UNIDENTIFIED_HELLO_TIMEOUT - Duration::from_millis(1),
        ));
        assert!(unidentified_hello_expired(
            PeerKind::Unidentified,
            true,
            connected_at,
            connected_at + UNIDENTIFIED_HELLO_TIMEOUT,
        ));
        assert!(!unidentified_hello_expired(
            PeerKind::Client,
            true,
            connected_at,
            connected_at + UNIDENTIFIED_HELLO_TIMEOUT,
        ));
        assert!(!unidentified_hello_expired(
            PeerKind::Unidentified,
            false,
            connected_at,
            connected_at + UNIDENTIFIED_HELLO_TIMEOUT,
        ));
    }

    fn test_outbound() -> (
        OutboundHandle,
        mpsc::Receiver<Message>,
        watch::Receiver<Option<ConnectionClose>>,
    ) {
        let (tx, rx) = mpsc::channel(8);
        let (close_tx, close_rx) = watch::channel(None);
        (OutboundHandle { tx, close_tx }, rx, close_rx)
    }

    #[test]
    fn pending_routes_are_bounded_per_connection_and_globally() {
        let per_connection_routes = Arc::new(Mutex::new(HashMap::new()));
        let (outbound, _rx, _close_rx) = test_outbound();
        for request_id in 0..MAX_PENDING_ROUTES_PER_CONNECTION {
            register_pending_route(
                &per_connection_routes,
                request_id as u64,
                7,
                outbound.clone(),
                "get",
                &serde_json::json!({"path": "Workspace"}),
                None,
            )
            .expect("route below the per-connection limit");
        }
        assert_eq!(
            register_pending_route(
                &per_connection_routes,
                999,
                7,
                outbound.clone(),
                "get",
                &serde_json::json!({}),
                None,
            ),
            Err(PendingRouteLimit::Connection),
        );
        register_pending_route(
            &per_connection_routes,
            1000,
            8,
            outbound.clone(),
            "get",
            &serde_json::json!({}),
            None,
        )
        .expect("a separate connection retains its own capacity");

        let global_routes = Arc::new(Mutex::new(HashMap::new()));
        for request_id in 0..MAX_PENDING_ROUTES {
            register_pending_route(
                &global_routes,
                request_id as u64,
                request_id as u64 + 1,
                outbound.clone(),
                "get",
                &serde_json::json!({}),
                None,
            )
            .expect("route below the global limit");
        }
        assert_eq!(
            register_pending_route(
                &global_routes,
                2000,
                5000,
                outbound,
                "get",
                &serde_json::json!({}),
                None,
            ),
            Err(PendingRouteLimit::Global),
        );
    }

    #[test]
    fn long_pending_route_survives_five_minutes_then_expires_at_its_deadline() {
        let (state, _tmp) = test_state_with_widget(false);
        let (outbound, mut rx, _close_rx) = test_outbound();
        let request_id = register_pending_route(
            &state.pending_routes,
            41,
            7,
            outbound,
            "get",
            &serde_json::json!({"path": "Workspace/Long"}),
            Some(10 * 60 * 1_000),
        )
        .unwrap();
        let (started_at, expires_at) = {
            let routes = state.pending_routes.lock().unwrap();
            let route = routes.get(&request_id).unwrap();
            (route.activity_started_at, route.expires_at)
        };
        assert_eq!(
            expires_at.saturating_duration_since(started_at),
            Duration::from_secs(10 * 60) + PENDING_ROUTE_COMPLETION_GRACE
        );

        fail_expired_pending_routes(&state, started_at + DEFAULT_PENDING_ROUTE_TIMEOUT);
        assert!(rx.try_recv().is_err());
        assert!(state
            .pending_routes
            .lock()
            .unwrap()
            .contains_key(&request_id));

        fail_expired_pending_routes(&state, expires_at - Duration::from_millis(1));
        assert!(rx.try_recv().is_err());
        assert!(state
            .pending_routes
            .lock()
            .unwrap()
            .contains_key(&request_id));

        fail_expired_pending_routes(&state, expires_at);

        let Message::Text(response) = rx.try_recv().expect("explicit timeout response") else {
            panic!("timeout response must be a text frame");
        };
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["type"], "response");
        assert_eq!(response["request_id"], 41);
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "request-timeout");
        let routes = state.pending_routes.lock().unwrap();
        assert!(!routes.contains_key(&request_id));
        assert_eq!(pending_route_timeout(None), DEFAULT_PENDING_ROUTE_TIMEOUT);
        assert_eq!(pending_route_timeout(Some(u64::MAX)), MAX_REQUEST_TIMEOUT);
    }

    fn test_state_with_widget(widget_owned: bool) -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let canonical = std::fs::canonicalize(&project).unwrap();
        let (events_tx, _) = broadcast::channel::<String>(64);
        let (request_tx, _) = broadcast::channel::<RequestEnvelope>(64);
        let (shutdown_tx, _) = tokio::sync::watch::channel::<Option<String>>(None);

        let state = AppState {
            project: Arc::new(project),
            canonical_project: Arc::new(canonical.clone()),
            projects_root: Arc::new(None),
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
            managed: widget_owned,
            managed_by: Arc::new(if widget_owned {
                "test-widget".to_string()
            } else {
                "test-manual".to_string()
            }),
            boot_id: Arc::new("test-boot".to_string()),
            listen_port: 0,
            process_id: std::process::id(),
            started_at: 1,
            manager_owner_token: Arc::new(widget_owned.then(|| "test-widget-token".to_string())),
            manager_last_seen: Arc::new(Mutex::new(None)),
            widget_owner_token: Arc::new(widget_owned.then(|| "test-widget-token".to_string())),
            widget_last_seen: Arc::new(Mutex::new(None)),
            shutdown_tx,
        };
        (state, tmp)
    }

    async fn start_server_with_widget(widget_owned: bool) -> TestHarness {
        let (state, tmp) = test_state_with_widget(widget_owned);
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
        watch
            .send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        recv_until_type(&mut watch, "pong", Duration::from_secs(3))
            .await
            .expect("watch hello should be processed before the request");

        for _ in 0..50 {
            if h.state.request_tx.receiver_count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(h.state.request_tx.receiver_count() >= 2);

        // CLI sends a request.
        cli.send(tungstenite::Message::Text(
            r#"{"type":"request","request_id":42,"op":"get","args":{"path":"Workspace","source":"SECRET_SOURCE","value":"SECRET_VALUE","apiKey":"SECRET_KEY"}}"#.into(),
        ))
        .await
        .unwrap();

        let started = recv_until_type(&mut watch, "command-activity", Duration::from_secs(3))
            .await
            .expect("watch should receive a sanitized started activity");
        assert_eq!(started["phase"], "started");
        assert_eq!(started["op"], "get");
        assert_eq!(
            started["detail"],
            serde_json::json!({ "path": "Workspace" })
        );
        let started_text = started.to_string();
        assert!(!started_text.contains("SECRET_SOURCE"));
        assert!(!started_text.contains("SECRET_VALUE"));
        assert!(!started_text.contains("SECRET_KEY"));

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

        let completed = recv_until_type(&mut watch, "command-activity", Duration::from_secs(3))
            .await
            .expect("watch should receive a completed activity");
        assert_eq!(completed["activityId"], started["activityId"]);
        assert_eq!(completed["phase"], "completed");
        assert_eq!(completed["ok"], true);
        assert!(completed["durationMs"].is_u64());

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
        assert_eq!(shutdown["code"], "plugin-already-connected");
        assert_eq!(shutdown["retryable"], true);

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
        assert_eq!(shutdown["code"], "stale-plugin-capability");
        assert_eq!(shutdown["retryable"], true);
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
        assert_eq!(shutdown["code"], "hello-already-sent");
        assert_eq!(shutdown["retryable"], false);
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

    #[tokio::test]
    async fn cli_request_without_plugin_fails_immediately_without_leaking_route() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut cli, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        cli.send(client_hello("cli", "cli")).await.unwrap();
        cli.send(tungstenite::Message::Text(
            r#"{"type":"request","request_id":77,"op":"get","args":{"path":"Workspace"}}"#.into(),
        ))
        .await
        .unwrap();

        let response = recv_until_type(&mut cli, "response", Duration::from_secs(1))
            .await
            .expect("missing plugin should fail immediately");
        assert_eq!(response["request_id"], 77);
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "plugin-unavailable");
        assert!(h.state.pending_routes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn requester_disconnect_publishes_sanitized_aborted_activity() {
        let h = start_server().await;
        let url = format!("ws://{}/ws", h.addr);
        let (mut plugin, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        plugin.send(plugin_hello("plugin")).await.unwrap();
        wait_for_active_plugin(&h).await;
        let (mut watch, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        watch.send(client_hello("widget", "watch")).await.unwrap();
        watch
            .send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        recv_until_type(&mut watch, "pong", Duration::from_secs(3))
            .await
            .expect("watch hello should be processed");

        let (mut cli, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        cli.send(client_hello("cli", "agent")).await.unwrap();
        cli.send(tungstenite::Message::Text(
            r#"{"type":"request","request_id":9,"op":"eval","args":{"source":"SECRET_SOURCE","value":"SECRET_VALUE"}}"#.into(),
        ))
        .await
        .unwrap();

        let started = recv_until_type(&mut watch, "command-activity", Duration::from_secs(3))
            .await
            .expect("watch should receive started activity");
        assert_eq!(started["phase"], "started");
        assert_eq!(started["detail"]["sourceBytes"], 13);
        assert!(!started.to_string().contains("SECRET_SOURCE"));

        cli.close(None).await.unwrap();
        let aborted = recv_until_type(&mut watch, "command-activity", Duration::from_secs(3))
            .await
            .expect("watch should receive aborted activity");
        assert_eq!(aborted["activityId"], started["activityId"]);
        assert_eq!(aborted["phase"], "aborted");
        assert_eq!(aborted["ok"], false);
        assert_eq!(aborted["error"], "Requester disconnected");
        assert!(!aborted.to_string().contains("SECRET_SOURCE"));
    }

    #[test]
    fn command_activity_sanitizer_never_copies_private_values_or_bulk_arrays() {
        let args = serde_json::json!({
            "path": "Workspace/Part",
            "prop": "Color",
            "value": { "token": "SECRET_VALUE" },
            "source": "SECRET_SOURCE",
            "apiKey": "SECRET_KEY",
            "args": ["SECRET_ARG", 2, 3],
        });
        let set_detail = sanitized_command_detail("set", &args);
        assert_eq!(
            set_detail,
            serde_json::json!({ "path": "Workspace/Part", "property": "Color" })
        );
        let call_detail = sanitized_command_detail("call", &args);
        assert_eq!(call_detail["path"], "Workspace/Part");
        assert_eq!(call_detail["argumentCount"], 3);
        let serialized = format!("{set_detail}{call_detail}");
        for secret in ["SECRET_VALUE", "SECRET_SOURCE", "SECRET_KEY", "SECRET_ARG"] {
            assert!(!serialized.contains(secret));
        }

        let clipboard_copy = sanitized_command_detail(
            "clipboard_copy",
            &serde_json::json!({
                "paths": ["Workspace/One", "ReplicatedStorage/Two"],
                "lease": { "id": "SECRET_ARTIFACT", "token": "SECRET_TOKEN" },
            }),
        );
        assert_eq!(
            clipboard_copy,
            serde_json::json!({ "itemCount": 2, "selectionMode": "paths" })
        );
        let clipboard_paste = sanitized_command_detail(
            "clipboard_paste",
            &serde_json::json!({
                "artifactId": "SECRET_ARTIFACT",
                "sha256": "SECRET_SHA",
                "parent": "Workspace/Imported",
                "byteLength": 4096,
                "roots": [
                    { "name": "SECRET_ROOT_ONE" },
                    { "name": "SECRET_ROOT_TWO" }
                ],
            }),
        );
        assert_eq!(
            clipboard_paste,
            serde_json::json!({
                "parent": "Workspace/Imported",
                "itemCount": 2,
                "byteLength": 4096,
            })
        );
        let clipboard_serialized = format!("{clipboard_copy}{clipboard_paste}");
        for secret in [
            "SECRET_ARTIFACT",
            "SECRET_TOKEN",
            "SECRET_SHA",
            "SECRET_ROOT_ONE",
            "SECRET_ROOT_TWO",
        ] {
            assert!(!clipboard_serialized.contains(secret));
        }

        for op in [
            "ping",
            "capture_read",
            "photo_close",
            "transmit_read",
            "playtest_run_poll",
            "playtest_wait",
            "playtest_status",
        ] {
            assert!(!should_publish_command_activity(op));
        }
        assert!(should_publish_command_activity("set"));
    }

    #[test]
    fn command_activity_ids_are_scoped_to_the_daemon_boot() {
        let first = command_activity_id("boot-a", 7);
        let repeated = command_activity_id("boot-a", 7);
        let restarted = command_activity_id("boot-b", 7);
        assert_eq!(first, repeated);
        assert_ne!(first, restarted);
        assert!(first.starts_with("request:"));
        assert!(first.ends_with(":7"));
        assert!(!first.contains("boot-a"));
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
        watch
            .send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .unwrap();
        recv_until_type(&mut watch, "pong", Duration::from_secs(3))
            .await
            .expect("watch hello should be processed before the sync event");

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
            is_dir: Some(false),
        };
        h.state
            .events
            .send(serde_json::json!({ "type": "op", "op": watcher_op }).to_string())
            .unwrap();

        let got = recv_until_type(&mut ws, "op", Duration::from_secs(5))
            .await
            .expect("should receive op frame");
        assert_eq!(got["type"], "op");
        let activity = recv_until_type(&mut watch, "sync-activity", Duration::from_secs(3))
            .await
            .expect("watch peers should receive sanitized sync activity");
        assert_eq!(activity["op"], "set");
        assert_eq!(activity["path"], "Workspace");
        assert_eq!(activity["class"], "Script");
        assert_eq!(activity["name"], "Hello");
        let activity_text = activity.to_string();
        assert!(!activity_text.contains("Source"));
        assert!(!activity_text.contains("print('hi')"));
        assert!(
            recv_until_type(&mut watch, "op", Duration::from_millis(200))
                .await
                .is_none(),
            "watch peers must never receive the source-bearing plugin operation"
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
        assert!(reason.contains(&format!("expected {}", PLUGIN_PROTOCOL_VERSION)));
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
