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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

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

/// Send `{type:"request",request_id,op,args}` to the daemon and return the
/// response `Value` (the full frame, including `ok`/`value`/`error`). Times
/// out after 5s.
pub async fn request(port: u16, op: &str, args: Value) -> Result<Value, String> {
    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| format!("connect {url}: {e}"))?;

    ws.send(Message::Text(
        r#"{"type":"hello","clientId":"rosync-cli"}"#.into(),
    ))
    .await
    .map_err(|e| format!("send hello: {e}"))?;

    let request_id = next_request_id();
    let frame = json!({
        "type": "request",
        "request_id": request_id,
        "op": op,
        "args": args,
    });
    ws.send(Message::Text(frame.to_string()))
        .await
        .map_err(|e| format!("send request: {e}"))?;

    let await_response = async {
        while let Some(frame) = ws.next().await {
            let msg = match frame {
                Ok(m) => m,
                Err(e) => return Err(format!("recv: {e}")),
            };
            let text = match msg {
                Message::Text(t) => t,
                Message::Close(_) => {
                    return Err("daemon closed connection before response".into());
                }
                Message::Ping(p) => {
                    let _ = ws.send(Message::Pong(p)).await;
                    continue;
                }
                _ => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("response") {
                continue;
            }
            if v.get("request_id").and_then(|t| t.as_u64()) != Some(request_id) {
                continue;
            }
            return Ok(v);
        }
        Err("stream ended before response".into())
    };

    let result = tokio::time::timeout(Duration::from_secs(5), await_response)
        .await
        .map_err(|_| "request timed out after 5s (plugin unresponsive?)".to_string())?;
    // Best-effort close; ignore errors.
    let _ = ws.send(Message::Close(None)).await;
    result
}

// ---------------------------------------------------------------------------
// Tests — mock WS responder validates the client-side request/response flow.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt as _, StreamExt as _};
    use std::net::SocketAddr;
    use tokio_tungstenite::tungstenite;

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
}
