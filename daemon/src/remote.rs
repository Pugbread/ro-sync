// CLI-side client for the WebSocket request/response bridge.
//
// A short-lived `rosync get|set|ls|tree|find|eval` invocation uses
// `remote::request` to open a WebSocket to the running daemon's `/ws`, send a
// `{type:"request",...}` frame, and wait (up to 5s) for a matching
// `{type:"response",...}` frame forwarded back by the plugin. Multiplexing is
// keyed on `request_id`; the daemon routes the response to whichever CLI
// connection initiated the request (see `ws.rs`).
//
// The plugin POSTs to `/writelog` itself on successful `set` / `eval`, so the
// CLI doesn't need its own HTTP client here — the WS round-trip is enough.

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Per-process counter so parallel calls (e.g. batch-mode) don't collide.
static NEXT_REQ_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    // Mix in a high-entropy seed once at process start so two daemons don't
    // accidentally confuse each other's routes if two rosync processes hit the
    // same daemon simultaneously.
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        NEXT_REQ_ID.store(seed.wrapping_mul(1_000_003).max(1), Ordering::Relaxed);
    });
    NEXT_REQ_ID.fetch_add(1, Ordering::Relaxed)
}

fn validate_request_timeout(timeout: Duration) -> Result<(), String> {
    if timeout.is_zero() {
        return Err("request timeout must be greater than zero".into());
    }
    if timeout > crate::ws::MAX_REQUEST_TIMEOUT {
        return Err(format!(
            "request timeout must not exceed {} seconds",
            crate::ws::MAX_REQUEST_TIMEOUT.as_secs()
        ));
    }
    Ok(())
}

fn request_timeout_millis(timeout: Duration) -> u64 {
    let rounded_up =
        timeout.as_millis() + u128::from(!timeout.subsec_nanos().is_multiple_of(1_000_000));
    rounded_up.min(u128::from(u64::MAX)) as u64
}

fn request_frame(request_id: u64, op: &str, args: Value, timeout: Duration) -> Value {
    json!({
        "type": "request",
        "request_id": request_id,
        "op": op,
        "args": args,
        "timeout_ms": request_timeout_millis(timeout),
    })
}

/// Send `{type:"request",request_id,op,args}` to the daemon and return the
/// response `Value` (the full frame, including `ok`/`value`/`error`). Times
/// out after 5s.
pub async fn request(port: u16, op: &str, args: Value) -> Result<Value, String> {
    request_with_timeout(port, op, args, Duration::from_secs(5)).await
}

pub async fn request_with_timeout(
    port: u16,
    op: &str,
    args: Value,
    timeout: Duration,
) -> Result<Value, String> {
    validate_request_timeout(timeout)?;
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "request timeout overflow".to_string())?;
    let mut session = RemoteSession::connect_until(port, deadline).await?;
    let result = session.request_until(op, args, deadline, timeout).await;
    // Best-effort close; a short-lived command must not turn close-handshake
    // failures into command failures after the response has arrived. It still
    // shares the absolute operation deadline, so a stalled close can never
    // extend the caller's timeout.
    let _ = session.close_until(deadline).await;
    result
}

type CliSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One operation in a sequential request batch sent over a [`RemoteSession`].
///
/// Requests in a batch may use different deadlines. A transport failure stops
/// the batch; a plugin response with `ok: false` is still a valid response and
/// is retained in the returned vector, matching [`request`] semantics.
#[derive(Clone, Debug)]
pub struct RemoteRequest {
    pub op: String,
    pub args: Value,
    pub timeout: Duration,
}

impl RemoteRequest {
    pub fn new(op: impl Into<String>, args: Value, timeout: Duration) -> Self {
        Self {
            op: op.into(),
            args,
            timeout,
        }
    }
}

/// A parsed plugin-level error from a response frame.
///
/// Older plugins use a plain string in the `error` field. Newer plugins may
/// return an object such as `{code,message,details,retryable}`. This type
/// accepts both without changing the long-standing behavior where transport
/// helpers return the full response frame, including `ok: false` responses.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginError {
    pub code: Option<String>,
    pub message: String,
    pub details: Option<Value>,
    pub retryable: Option<bool>,
    /// The original `error` JSON value for forward-compatible inspection.
    pub raw: Value,
}

impl PluginError {
    /// Parse the `error` field of a response. Returns `None` for a successful
    /// response, even if it happens to contain diagnostic error metadata.
    pub fn from_response(response: &Value) -> Option<Self> {
        if response.get("ok").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        Some(Self::from_value(
            response.get("error").unwrap_or(&Value::Null),
        ))
    }

    /// Parse a legacy string or a structured plugin error value.
    pub fn from_value(error: &Value) -> Self {
        match error {
            Value::String(message) => Self {
                code: None,
                message: message.clone(),
                details: None,
                retryable: None,
                raw: error.clone(),
            },
            Value::Object(object) => {
                let code = object
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let message = object
                    .get("message")
                    .or_else(|| object.get("error"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        code.as_deref()
                            .map(|value| value.replace('_', " "))
                            .filter(|value| !value.is_empty())
                            .unwrap_or_else(|| "request failed".to_string())
                    });
                let details = object
                    .get("details")
                    .or_else(|| object.get("data"))
                    .cloned();
                let retryable = object.get("retryable").and_then(Value::as_bool);
                Self {
                    code,
                    message,
                    details,
                    retryable,
                    raw: error.clone(),
                }
            }
            Value::Null => Self {
                code: None,
                message: "request failed".to_string(),
                details: None,
                retryable: None,
                raw: Value::Null,
            },
            other => Self {
                code: None,
                message: other.to_string(),
                details: None,
                retryable: None,
                raw: other.clone(),
            },
        }
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code.as_deref() {
            Some(code) => write!(formatter, "{code}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for PluginError {}

/// Parse a plugin-level error from a full response frame.
pub fn plugin_error(response: &Value) -> Option<PluginError> {
    PluginError::from_response(response)
}

/// A reusable CLI connection to the daemon's request/response bridge.
///
/// Unlike [`request`], this keeps one WebSocket open for an entire workflow or
/// artifact transfer. Calls are intentionally sequential (`&mut self`), which
/// keeps response routing simple while still eliminating reconnect and hello
/// overhead between steps.
pub struct RemoteSession {
    socket: CliSocket,
}

impl RemoteSession {
    /// Connect to the local daemon and perform the CLI hello handshake.
    pub async fn connect(port: u16) -> Result<Self, String> {
        Self::connect_with_timeout(port, Duration::from_secs(5)).await
    }

    pub async fn connect_with_timeout(port: u16, timeout: Duration) -> Result<Self, String> {
        if timeout.is_zero() {
            return Err("WebSocket connect timeout must be greater than zero".into());
        }
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "WebSocket connect timeout overflow".to_string())?;
        Self::connect_until(port, deadline).await
    }

    async fn connect_until(port: u16, deadline: tokio::time::Instant) -> Result<Self, String> {
        let url = format!("ws://127.0.0.1:{port}/ws");
        let (mut socket, _) =
            tokio::time::timeout_at(deadline, tokio_tungstenite::connect_async(&url))
                .await
                .map_err(|_| format!("connect/handshake {url} timed out"))?
                .map_err(|e| format!("connect {url}: {e}"))?;

        tokio::time::timeout_at(
            deadline,
            socket.send(Message::Text(format!(
                r#"{{"type":"hello","clientId":"rosync-cli","role":"cli","protocol":{}}}"#,
                crate::ws::PLUGIN_PROTOCOL_VERSION
            ))),
        )
        .await
        .map_err(|_| "send WebSocket hello timed out".to_string())?
        .map_err(|e| format!("send hello: {e}"))?;

        Ok(Self { socket })
    }

    /// Send one request and wait for its matching response frame.
    ///
    /// Plugin-level failures (`ok: false`) are returned as response values so
    /// existing CLI response handling keeps working. Use [`plugin_error`] when
    /// a typed view of either the legacy or structured error is useful.
    pub async fn request(
        &mut self,
        op: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        validate_request_timeout(timeout)?;
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "request timeout overflow".to_string())?;
        self.request_until(op, args, deadline, timeout).await
    }

    async fn request_until(
        &mut self,
        op: &str,
        args: Value,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<Value, String> {
        let request_id = next_request_id();
        let request = request_frame(request_id, op, args, timeout);

        let exchange = async {
            self.socket
                .send(Message::Text(request.to_string()))
                .await
                .map_err(|e| format!("send request: {e}"))?;
            self.wait_for_response(request_id).await
        };

        tokio::time::timeout_at(deadline, exchange)
            .await
            .map_err(|_| {
                format!(
                    "request timed out after {:.0}s (plugin unresponsive?)",
                    timeout.as_secs_f64()
                )
            })?
    }

    /// Execute a sequence over this connection, preserving response order.
    ///
    /// A transport error or timeout stops the sequence. Plugin-declared
    /// failures remain ordinary response frames, allowing workflows to inspect
    /// structured errors or implement their own continue/abort policy.
    pub async fn request_many<I>(&mut self, requests: I) -> Result<Vec<Value>, String>
    where
        I: IntoIterator<Item = RemoteRequest>,
    {
        let requests = requests.into_iter();
        let (lower_bound, _) = requests.size_hint();
        let mut responses = Vec::with_capacity(lower_bound);
        for request in requests {
            responses.push(
                self.request(&request.op, request.args, request.timeout)
                    .await?,
            );
        }
        Ok(responses)
    }

    /// Gracefully close the persistent connection.
    pub async fn close(&mut self) -> Result<(), String> {
        self.close_with_timeout(Duration::from_secs(5)).await
    }

    pub async fn close_with_timeout(&mut self, timeout: Duration) -> Result<(), String> {
        if timeout.is_zero() {
            return Err("WebSocket close timeout must be greater than zero".into());
        }
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "WebSocket close timeout overflow".to_string())?;
        self.close_until(deadline).await
    }

    async fn close_until(&mut self, deadline: tokio::time::Instant) -> Result<(), String> {
        tokio::time::timeout_at(deadline, self.socket.send(Message::Close(None)))
            .await
            .map_err(|_| "close connection timed out".to_string())?
            .map_err(|e| format!("close connection: {e}"))
    }

    async fn wait_for_response(&mut self, request_id: u64) -> Result<Value, String> {
        while let Some(frame) = self.socket.next().await {
            let message = frame.map_err(|e| format!("recv: {e}"))?;
            let text = match message {
                Message::Text(text) => text,
                Message::Close(frame) => {
                    let reason = frame
                        .map(|frame| frame.reason.to_string())
                        .filter(|reason| !reason.is_empty());
                    return Err(match reason {
                        Some(reason) => {
                            format!("daemon closed connection before response: {reason}")
                        }
                        None => "daemon closed connection before response".to_string(),
                    });
                }
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|e| format!("send pong: {e}"))?;
                    continue;
                }
                // Pong, Binary, and raw Frame messages cannot contain a JSON
                // response in the current protocol and are safe to ignore.
                _ => continue,
            };

            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            match value.get("type").and_then(Value::as_str) {
                Some("response")
                    if value.get("request_id").and_then(Value::as_u64) == Some(request_id) =>
                {
                    return Ok(value);
                }
                // The daemon heartbeat is a JSON protocol frame rather than
                // a WebSocket control-frame ping.
                Some("ping") => {
                    self.socket
                        .send(Message::Text(r#"{"type":"pong"}"#.into()))
                        .await
                        .map_err(|e| format!("send heartbeat pong: {e}"))?;
                }
                Some("shutdown") => {
                    let reason = value
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("daemon is shutting down");
                    return Err(format!("daemon shut down before response: {reason}"));
                }
                Some("lagged") => {
                    return Err("daemon connection lagged before response".to_string());
                }
                Some("error") => {
                    let error = value.get("error").unwrap_or(&Value::Null);
                    return Err(format!("daemon error: {}", PluginError::from_value(error)));
                }
                // Other response IDs can arrive after an earlier timeout;
                // daemon events and requests for other peers may also share
                // this socket. None belong to the active sequential call.
                _ => {}
            }
        }
        Err("stream ended before response".into())
    }
}

// ---------------------------------------------------------------------------
// Tests — mock WS responder validates the client-side request/response flow.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio_tungstenite::tungstenite;

    #[test]
    fn request_frame_carries_a_bounded_rounded_up_timeout() {
        let frame = request_frame(
            7,
            "get",
            json!({"path": "Workspace"}),
            Duration::from_secs(10 * 60),
        );
        assert_eq!(frame["timeout_ms"], 600_000);
        assert_eq!(request_timeout_millis(Duration::from_micros(1_001)), 2);
        assert!(validate_request_timeout(crate::ws::MAX_REQUEST_TIMEOUT).is_ok());
        assert!(validate_request_timeout(
            crate::ws::MAX_REQUEST_TIMEOUT + Duration::from_millis(1)
        )
        .is_err());
    }

    /// Bind a TCP listener, accept one WebSocket connection, echo back a
    /// response matching the request_id of whatever request the client sends,
    /// using `respond` to produce the value/ok/error.
    async fn start_mock_responder<F>(respond: F) -> SocketAddr
    where
        F: Fn(&str, &Value) -> (bool, Value, Option<String>) + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(msg) = ws.next().await {
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
                let op = v
                    .get("op")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = v.get("args").cloned().unwrap_or(Value::Null);
                let (ok, value, error) = respond(&op, &args);
                let resp = json!({
                    "type": "response",
                    "request_id": rid,
                    "ok": ok,
                    "value": value,
                    "error": error,
                });
                ws.send(tungstenite::Message::Text(resp.to_string()))
                    .await
                    .unwrap();
            }
        });
        addr
    }

    /// The mock doesn't speak HTTP `/ws` pathing — it accepts the bare ws
    /// upgrade. `remote::request` builds `ws://127.0.0.1:<port>/ws` so we
    /// need the mock to accept any path. `tokio_tungstenite::accept_async`
    /// does exactly that.
    #[tokio::test]
    async fn round_trip_get() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "get");
            assert_eq!(args["path"], "Workspace/Baseplate");
            (
                true,
                json!({ "class": "Part", "name": "Baseplate", "properties": { "Anchored": true } }),
                None,
            )
        })
        .await;

        let resp = request(addr.port(), "get", json!({ "path": "Workspace/Baseplate" }))
            .await
            .expect("request");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"]["class"], "Part");
        assert_eq!(resp["value"]["properties"]["Anchored"], true);
    }

    #[tokio::test]
    async fn persistent_session_reuses_one_socket_for_a_sequence() {
        let addr = start_mock_responder(|op, args| match op {
            "get" => (true, json!({ "path": args["path"], "name": "Part" }), None),
            "set" => (true, json!({ "applied": args["value"] }), None),
            other => panic!("unexpected op: {other}"),
        })
        .await;

        // The mock only accepts one TCP connection, so both successful
        // responses prove the session reused its original WebSocket.
        let mut session = RemoteSession::connect(addr.port()).await.unwrap();
        let responses = session
            .request_many(vec![
                RemoteRequest::new(
                    "get",
                    json!({ "path": "Workspace/Part" }),
                    Duration::from_secs(1),
                ),
                RemoteRequest::new(
                    "set",
                    json!({ "path": "Workspace/Part", "value": 7 }),
                    Duration::from_secs(1),
                ),
            ])
            .await
            .unwrap();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["value"]["path"], "Workspace/Part");
        assert_eq!(responses[1]["value"]["applied"], 7);
        let _ = session.close().await;
    }

    #[tokio::test]
    async fn session_tolerates_heartbeats_and_unrelated_frames() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            let request_id = loop {
                let Some(Ok(tungstenite::Message::Text(text))) = ws.next().await else {
                    panic!("client closed before sending a request");
                };
                let value: Value = serde_json::from_str(&text).unwrap();
                if value.get("type").and_then(Value::as_str) == Some("request") {
                    break value.get("request_id").and_then(Value::as_u64).unwrap();
                }
            };

            ws.send(tungstenite::Message::Ping(vec![1, 2, 3]))
                .await
                .unwrap();
            ws.send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
                .await
                .unwrap();
            ws.send(tungstenite::Message::Text(
                json!({
                    "type": "response",
                    "request_id": request_id.wrapping_add(1),
                    "ok": true,
                    "value": "stale",
                })
                .to_string(),
            ))
            .await
            .unwrap();
            ws.send(tungstenite::Message::Text(
                r#"{"type":"config-changed"}"#.into(),
            ))
            .await
            .unwrap();
            ws.send(tungstenite::Message::Text(
                json!({
                    "type": "response",
                    "request_id": request_id,
                    "ok": true,
                    "value": { "pong": true },
                    "error": null,
                })
                .to_string(),
            ))
            .await
            .unwrap();

            // Keep the peer open until the client has answered the JSON heartbeat.
            // Dropping immediately after queueing the response races the client's
            // pong write and makes this transport-tolerance test flaky under load.
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let Some(Ok(message)) = ws.next().await else {
                        panic!("client closed before answering the heartbeat");
                    };
                    if let tungstenite::Message::Text(text) = message {
                        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        if value.get("type").and_then(Value::as_str) == Some("pong") {
                            break;
                        }
                    }
                }
            })
            .await
            .expect("client should answer the heartbeat within one second");
        });

        let mut session = RemoteSession::connect(addr.port()).await.unwrap();
        let response = session
            .request("ping", json!({}), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(response["value"]["pong"], true);
    }

    #[test]
    fn parses_legacy_and_structured_plugin_errors() {
        let legacy = plugin_error(&json!({
            "ok": false,
            "error": "instance not found",
        }))
        .unwrap();
        assert_eq!(legacy.code, None);
        assert_eq!(legacy.message, "instance not found");
        assert_eq!(legacy.to_string(), "instance not found");

        let structured = plugin_error(&json!({
            "ok": false,
            "error": {
                "code": "INSTANCE_NOT_FOUND",
                "message": "No instance exists at that path",
                "details": { "path": "Workspace/Nope" },
                "retryable": false,
            },
        }))
        .unwrap();
        assert_eq!(structured.code.as_deref(), Some("INSTANCE_NOT_FOUND"));
        assert_eq!(structured.message, "No instance exists at that path");
        assert_eq!(
            structured.details.as_ref().unwrap()["path"],
            "Workspace/Nope"
        );
        assert_eq!(structured.retryable, Some(false));
        assert_eq!(
            structured.to_string(),
            "INSTANCE_NOT_FOUND: No instance exists at that path"
        );

        assert!(plugin_error(&json!({
            "ok": true,
            "error": { "message": "diagnostic only" },
        }))
        .is_none());
    }

    #[tokio::test]
    async fn round_trip_set() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "set");
            assert_eq!(args["path"], "Workspace/Part1");
            assert_eq!(args["prop"], "BrickColor");
            (true, json!({ "applied": true }), None)
        })
        .await;
        let resp = request(
            addr.port(),
            "set",
            json!({ "path": "Workspace/Part1", "prop": "BrickColor", "value": "Bright red" }),
        )
        .await
        .expect("request");
        assert_eq!(resp["ok"], true);
    }

    #[tokio::test]
    async fn round_trip_ls() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "ls");
            (
                true,
                json!([
                    { "name": "Baseplate", "class": "Part" },
                    { "name": "SpawnLocation", "class": "SpawnLocation" }
                ]),
                None,
            )
        })
        .await;
        let resp = request(addr.port(), "ls", json!({ "path": "Workspace" }))
            .await
            .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn round_trip_tree() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "tree");
            (true, json!({ "name": "Workspace", "children": [] }), None)
        })
        .await;
        let resp = request(
            addr.port(),
            "tree",
            json!({ "path": "Workspace", "depth": 2 }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"]["name"], "Workspace");
    }

    #[tokio::test]
    async fn round_trip_find() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "find");
            assert_eq!(args["className"], "Part");
            (true, json!([{ "path": "Workspace/Part1" }]), None)
        })
        .await;
        let resp = request(addr.port(), "find", json!({ "className": "Part" }))
            .await
            .unwrap();
        assert_eq!(resp["value"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn round_trip_eval() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "eval");
            assert_eq!(args["source"], "return 1 + 1");
            (true, json!(2), None)
        })
        .await;
        let resp = request(addr.port(), "eval", json!({ "source": "return 1 + 1" }))
            .await
            .unwrap();
        assert_eq!(resp["value"], 2);
    }

    #[tokio::test]
    async fn surfaces_plugin_error() {
        let addr = start_mock_responder(|_op, _args| {
            (false, Value::Null, Some("instance not found".into()))
        })
        .await;
        let resp = request(addr.port(), "get", json!({ "path": "Nope" }))
            .await
            .unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"], "instance not found");
    }

    // -----------------------------------------------------------------
    // Tier 2 ops — logs / save / undo / redo / waypoint / version / ping.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn round_trip_logs() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "logs");
            assert_eq!(args["level_min"], "warn");
            assert_eq!(args["limit"], 10);
            (
                true,
                json!({
                    "entries": [
                        { "t": 12.5, "wall": 1700000000, "level": "warn", "message": "hi", "seq": 1 },
                    ],
                    "now": 13.0,
                    "wall": 1700000001,
                }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "logs",
            json!({ "since_seconds": 30, "level_min": "warn", "limit": 10 }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"]["entries"][0]["level"], "warn");
        assert_eq!(resp["value"]["entries"][0]["seq"], 1);
    }

    #[tokio::test]
    async fn round_trip_save() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "save");
            (true, json!({ "ok": true, "started": true }), None)
        })
        .await;
        let resp = request(addr.port(), "save", json!({})).await.unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"]["started"], true);
    }

    #[tokio::test]
    async fn round_trip_undo() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "undo");
            (true, json!({ "ok": true }), None)
        })
        .await;
        let resp = request(addr.port(), "undo", json!({})).await.unwrap();
        assert_eq!(resp["ok"], true);
    }

    #[tokio::test]
    async fn round_trip_redo() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "redo");
            (true, json!({ "ok": true }), None)
        })
        .await;
        let resp = request(addr.port(), "redo", json!({})).await.unwrap();
        assert_eq!(resp["ok"], true);
    }

    #[tokio::test]
    async fn round_trip_waypoint() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "waypoint");
            assert_eq!(args["name"], "batch-start");
            (true, json!({ "ok": true, "name": "batch-start" }), None)
        })
        .await;
        let resp = request(addr.port(), "waypoint", json!({ "name": "batch-start" }))
            .await
            .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"]["name"], "batch-start");
    }

    #[tokio::test]
    async fn round_trip_ping() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "ping");
            (true, json!({ "pong": 42.0 }), None)
        })
        .await;
        let resp = request(addr.port(), "ping", json!({})).await.unwrap();
        assert_eq!(resp["value"]["pong"], 42.0);
    }

    #[tokio::test]
    async fn round_trip_version() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "version");
            (
                true,
                json!({
                    "plugin_version": "1.0.0",
                    "protocol": 1,
                    "studio_version": "0.666.0.0",
                }),
                None,
            )
        })
        .await;
        let resp = request(addr.port(), "version", json!({})).await.unwrap();
        assert_eq!(resp["value"]["plugin_version"], "1.0.0");
        assert_eq!(resp["value"]["protocol"], 1);
    }

    // -----------------------------------------------------------------
    // Tier 1 ops — construction/destruction/reparent/attr/tag/call/select.
    // Each verifies the CLI-side args plumbed through and a representative
    // response shape is surfaced.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn round_trip_new() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "new");
            assert_eq!(args["parent"], "Workspace");
            assert_eq!(args["class"], "Part");
            assert_eq!(args["name"], "Box");
            assert_eq!(args["initial_props"]["Anchored"], true);
            (
                true,
                json!({ "path": "Workspace/Box", "class": "Part", "name": "Box" }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "new",
            json!({
                "parent": "Workspace",
                "class": "Part",
                "name": "Box",
                "initial_props": { "Anchored": true },
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"]["path"], "Workspace/Box");
    }

    #[tokio::test]
    async fn round_trip_rm() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "rm");
            assert_eq!(args["path"], "Workspace/Box");
            (
                true,
                json!({ "path": "Workspace/Box", "destroyed": true }),
                None,
            )
        })
        .await;
        let resp = request(addr.port(), "rm", json!({ "path": "Workspace/Box" }))
            .await
            .unwrap();
        assert_eq!(resp["value"]["destroyed"], true);
    }

    #[tokio::test]
    async fn round_trip_mv() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "mv");
            assert_eq!(args["from"], "Workspace/Box");
            assert_eq!(args["to"], "Workspace/Folder");
            assert_eq!(args["force"], false);
            (
                true,
                json!({ "path": "Workspace/Folder/Box", "parent": "Workspace/Folder" }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "mv",
            json!({ "from": "Workspace/Box", "to": "Workspace/Folder", "force": false }),
        )
        .await
        .unwrap();
        assert_eq!(resp["value"]["parent"], "Workspace/Folder");
    }

    #[tokio::test]
    async fn round_trip_set_attr() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "set_attr");
            assert_eq!(args["path"], "Workspace/Box");
            assert_eq!(args["name"], "Speed");
            assert_eq!(args["value"], 12.5);
            (
                true,
                json!({ "path": "Workspace/Box", "name": "Speed" }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "set_attr",
            json!({ "path": "Workspace/Box", "name": "Speed", "value": 12.5 }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
    }

    #[tokio::test]
    async fn round_trip_rm_attr() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "rm_attr");
            assert_eq!(args["name"], "Speed");
            (
                true,
                json!({ "path": "Workspace/Box", "name": "Speed", "cleared": true }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "rm_attr",
            json!({ "path": "Workspace/Box", "name": "Speed" }),
        )
        .await
        .unwrap();
        assert_eq!(resp["value"]["cleared"], true);
    }

    #[tokio::test]
    async fn round_trip_attr_ls() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "attr_ls");
            assert_eq!(args["path"], "Workspace/Box");
            (true, json!({ "Speed": 12.5, "Team": "Red" }), None)
        })
        .await;
        let resp = request(addr.port(), "attr_ls", json!({ "path": "Workspace/Box" }))
            .await
            .unwrap();
        assert_eq!(resp["value"]["Speed"], 12.5);
    }

    #[tokio::test]
    async fn round_trip_add_tag() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "add_tag");
            assert_eq!(args["tag"], "Enemy");
            (
                true,
                json!({ "path": "Workspace/Box", "tag": "Enemy", "added": true }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "add_tag",
            json!({ "path": "Workspace/Box", "tag": "Enemy" }),
        )
        .await
        .unwrap();
        assert_eq!(resp["value"]["added"], true);
    }

    #[tokio::test]
    async fn round_trip_rm_tag() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "rm_tag");
            assert_eq!(args["tag"], "Enemy");
            (
                true,
                json!({ "path": "Workspace/Box", "tag": "Enemy", "removed": true }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "rm_tag",
            json!({ "path": "Workspace/Box", "tag": "Enemy" }),
        )
        .await
        .unwrap();
        assert_eq!(resp["value"]["removed"], true);
    }

    #[tokio::test]
    async fn round_trip_call() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "call");
            assert_eq!(args["path"], "Workspace/Folder");
            assert_eq!(args["method"], "FindFirstChild");
            assert_eq!(args["args"][0], "Box");
            (
                true,
                json!({ "__type": "Instance", "path": "Workspace/Folder/Box", "class": "Part" }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "call",
            json!({
                "path": "Workspace/Folder",
                "method": "FindFirstChild",
                "args": ["Box"],
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp["value"]["__type"], "Instance");
    }

    #[tokio::test]
    async fn round_trip_select_get() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "select_get");
            (
                true,
                json!(["Workspace/Box", "Workspace/SpawnLocation"]),
                None,
            )
        })
        .await;
        let resp = request(addr.port(), "select_get", json!({})).await.unwrap();
        let arr = resp["value"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn round_trip_select_set() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "select_set");
            assert_eq!(args["paths"][0], "Workspace/Box");
            assert_eq!(args["paths"][1], "Workspace/SpawnLocation");
            (true, json!({ "count": 2 }), None)
        })
        .await;
        let resp = request(
            addr.port(),
            "select_set",
            json!({ "paths": ["Workspace/Box", "Workspace/SpawnLocation"] }),
        )
        .await
        .unwrap();
        assert_eq!(resp["value"]["count"], 2);
    }

    // ----- Tier 3 —— class_info / enums / enum_list / find_by_attr / scoped find -----

    #[tokio::test]
    async fn round_trip_class_info() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "class_info");
            assert_eq!(args["class_name"], "BasePart");
            (
                true,
                json!({
                    "properties": [
                        { "name": "Anchored", "category": "Behavior", "type": "bool" },
                        { "name": "Position", "category": "Data", "type": "Vector3" },
                    ],
                    "methods": ["GetMass", "Destroy"],
                }),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "class_info",
            json!({ "class_name": "BasePart" }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
        let props = resp["value"]["properties"].as_array().unwrap();
        assert_eq!(props.len(), 2);
        assert_eq!(props[0]["name"], "Anchored");
        assert_eq!(resp["value"]["methods"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn round_trip_enums() {
        let addr = start_mock_responder(|op, _args| {
            assert_eq!(op, "enums");
            (true, json!(["Material", "Font", "KeyCode"]), None)
        })
        .await;
        let resp = request(addr.port(), "enums", json!({})).await.unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn round_trip_enum_list() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "enum_list");
            assert_eq!(args["enum_name"], "Material");
            (
                true,
                json!([
                    { "name": "Plastic", "value": 256 },
                    { "name": "Wood", "value": 512 }
                ]),
                None,
            )
        })
        .await;
        let resp = request(addr.port(), "enum_list", json!({ "enum_name": "Material" }))
            .await
            .unwrap();
        assert_eq!(resp["ok"], true);
        let items = resp["value"].as_array().unwrap();
        assert_eq!(items[0]["name"], "Plastic");
        assert_eq!(items[1]["value"], 512);
    }

    #[tokio::test]
    async fn round_trip_find_with_under() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "find");
            assert_eq!(args["className"], "Part");
            assert_eq!(args["under"], "Workspace/Map");
            (
                true,
                json!(["Workspace/Map/Part1", "Workspace/Map/Stuff/Part2"]),
                None,
            )
        })
        .await;
        let resp = request(
            addr.port(),
            "find",
            json!({ "className": "Part", "under": "Workspace/Map" }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn round_trip_find_by_attr() {
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "find_by_attr");
            assert_eq!(args["name"], "Health");
            assert_eq!(args["under"], "Workspace");
            (true, json!(["Workspace/Mob1", "Workspace/Boss"]), None)
        })
        .await;
        let resp = request(
            addr.port(),
            "find_by_attr",
            json!({ "name": "Health", "under": "Workspace" }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn round_trip_find_by_attr_with_value() {
        // Verifies the tagged-value codec flows through untouched.
        let addr = start_mock_responder(|op, args| {
            assert_eq!(op, "find_by_attr");
            assert_eq!(args["name"], "Color");
            assert_eq!(args["value"]["__type"], "Color3");
            assert_eq!(args["value"]["r"], 1.0);
            (true, json!(["Workspace/RedPart"]), None)
        })
        .await;
        let resp = request(
            addr.port(),
            "find_by_attr",
            json!({
                "name": "Color",
                "value": { "__type": "Color3", "r": 1.0, "g": 0.0, "b": 0.0 }
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["value"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn round_trip_class_info_error_when_unknown() {
        let addr = start_mock_responder(|_op, _args| {
            (
                false,
                Value::Null,
                Some("no class info available for: ZZZ".into()),
            )
        })
        .await;
        let resp = request(addr.port(), "class_info", json!({ "class_name": "ZZZ" }))
            .await
            .unwrap();
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().unwrap().contains("ZZZ"));
    }

    #[tokio::test]
    async fn times_out_when_no_responder_replies() {
        // Bind but never respond — request should fail with a timeout.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Drain until peer closes.
            while ws.next().await.is_some() {}
        });
        let start = std::time::Instant::now();
        let err = request(addr.port(), "get", json!({})).await.unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(8),
            "timeout fired too late: {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_secs(4),
            "timeout fired too early: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn end_to_end_timeout_includes_websocket_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            // Keep the TCP connection open without completing the HTTP
            // WebSocket upgrade.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let started = std::time::Instant::now();
        let error = request_with_timeout(addr.port(), "get", json!({}), Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(error.contains("connect/handshake"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "handshake exceeded the end-to-end deadline: {:?}",
            started.elapsed()
        );
    }
}
