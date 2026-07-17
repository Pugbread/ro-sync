use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    path::{Component, Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, State};
use tauri_plugin_shell::{
    process::{CommandChild, CommandEvent},
    ShellExt,
};

use crate::{resources::display_path, AppState};

const LIFECYCLE_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_CLOSE_IO_TIMEOUT: Duration = Duration::from_millis(750);
const EXIT_CLOSE_WAIT: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct ExactManagedDaemonClaim {
    port: u16,
    boot_id: String,
    owner_token: String,
}

#[derive(Default)]
pub(crate) struct ManagedDaemonClaim {
    state: Mutex<ManagedDaemonClaimState>,
}

#[derive(Default)]
struct ManagedDaemonClaimState {
    exiting: bool,
    current: Option<ExactManagedDaemonClaim>,
}

impl ManagedDaemonClaim {
    fn remember(&self, value: &Value, owner_token: &str) {
        let status = value.get("status").unwrap_or(value);
        let is_owned = status.get("running").and_then(Value::as_bool) == Some(true)
            && status.get("managed").and_then(Value::as_bool) == Some(true)
            && status.get("managedBy").and_then(Value::as_str) == Some("desktop")
            && status.get("externallyManaged").and_then(Value::as_bool) != Some(true);
        let port = status
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port > 0);
        let boot_id = status
            .get("bootId")
            .and_then(Value::as_str)
            .filter(|boot_id| !boot_id.is_empty());
        if !is_owned
            || port.is_none()
            || boot_id.is_none()
            || validate_owner_token(owner_token).is_err()
        {
            return;
        }
        let claim = ExactManagedDaemonClaim {
            port: port.unwrap(),
            boot_id: boot_id.unwrap().to_string(),
            owner_token: owner_token.to_string(),
        };
        let close_after_exit = match self.state.lock() {
            Ok(state) if state.exiting => true,
            Ok(mut state) => {
                state.current = Some(claim.clone());
                false
            }
            Err(_) => true,
        };
        if close_after_exit {
            let _ = close_exact_managed_daemon(&claim);
        }
    }

    pub(crate) fn mark_exiting(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.exiting = true;
        }
    }

    pub(crate) fn terminate(&self) {
        let claim = self.state.lock().ok().and_then(|mut state| {
            state.exiting = true;
            state.current.take()
        });
        if let Some(claim) = claim {
            let _ = close_exact_managed_daemon(&claim);
        }
    }
}

fn close_exact_managed_daemon(claim: &ExactManagedDaemonClaim) -> Result<(), String> {
    let hello = local_json_request(claim.port, "GET", "/hello", &[])?;
    let exact_daemon = hello.get("managed").and_then(Value::as_bool) == Some(true)
        && hello.get("managedBy").and_then(Value::as_str) == Some("desktop")
        && hello.get("bootId").and_then(Value::as_str) == Some(claim.boot_id.as_str())
        && hello.get("port").and_then(Value::as_u64) == Some(u64::from(claim.port));
    if !exact_daemon {
        return Err("managed daemon identity changed before native exit cleanup".into());
    }

    let body = serde_json::to_vec(&serde_json::json!({
        "token": claim.owner_token,
        "reason": "desktop app exited",
    }))
    .map_err(|error| format!("encode managed daemon close request: {error}"))?;
    let response = local_json_request(claim.port, "POST", "/manager-close", &body)?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("managed daemon rejected the authenticated close request".into());
    }

    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, claim.port));
    let deadline = Instant::now() + EXIT_CLOSE_WAIT;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("managed daemon remained reachable after the authenticated close request".into())
}

fn local_json_request(port: u16, method: &str, path: &str, body: &[u8]) -> Result<Value, String> {
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, EXIT_CLOSE_IO_TIMEOUT)
        .map_err(|error| format!("connect to managed daemon: {error}"))?;
    stream
        .set_read_timeout(Some(EXIT_CLOSE_IO_TIMEOUT))
        .map_err(|error| format!("set managed daemon read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(EXIT_CLOSE_IO_TIMEOUT))
        .map_err(|error| format!("set managed daemon write timeout: {error}"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|error| format!("send managed daemon close request: {error}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("read managed daemon close response: {error}"))?;
    let header_end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or_else(|| "managed daemon returned an invalid HTTP response".to_string())?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    if !headers
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200 "))
    {
        return Err("managed daemon returned a non-success HTTP response".into());
    }
    serde_json::from_slice(&response[header_end + 4..])
        .map_err(|error| format!("decode managed daemon response: {error}"))
}

#[derive(Default)]
pub(crate) struct LifecycleChildren {
    state: Mutex<LifecycleChildState>,
}

#[derive(Default)]
struct LifecycleChildState {
    exiting: bool,
    children: HashMap<u32, CommandChild>,
}

impl LifecycleChildren {
    fn register(&self, child: CommandChild) -> Result<u32, String> {
        let pid = child.pid();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                let _ = child.kill();
                return Err(
                    "lifecycle child registry is poisoned; the new sidecar was terminated"
                        .to_string(),
                );
            }
        };
        if state.exiting {
            let _ = child.kill();
            return Err(format!(
                "Ro Sync is exiting; lifecycle child process {pid} was terminated"
            ));
        }
        if state.children.contains_key(&pid) {
            let _ = child.kill();
            return Err(format!(
                "lifecycle child process {pid} is already registered; the duplicate was terminated"
            ));
        }
        state.children.insert(pid, child);
        Ok(pid)
    }

    fn forget(&self, pid: u32) {
        if let Ok(mut state) = self.state.lock() {
            state.children.remove(&pid);
        }
    }

    fn terminate(&self, pid: u32) -> Option<String> {
        let child = match self.state.lock() {
            Ok(mut state) => state.children.remove(&pid),
            Err(_) => {
                return Some(
                    "lifecycle child registry is poisoned; termination could not be confirmed"
                        .to_string(),
                )
            }
        };
        child.and_then(|child| child.kill().err().map(|error| error.to_string()))
    }

    pub(crate) fn terminate_all(&self) {
        let children = match self.state.lock() {
            Ok(mut state) => {
                state.exiting = true;
                state
                    .children
                    .drain()
                    .map(|(_, child)| child)
                    .collect::<Vec<_>>()
            }
            Err(_) => return,
        };
        for child in children {
            let _ = child.kill();
        }
    }
}

#[derive(Debug)]
enum LifecycleFailure {
    Message(String),
    Timeout(String),
}

impl LifecycleFailure {
    fn message(self) -> String {
        match self {
            Self::Message(message) | Self::Timeout(message) => message,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonEnsureSpec {
    project: String,
    preferred_port: Option<u16>,
    game_id: Option<String>,
    group_id: Option<String>,
    #[serde(default)]
    place_ids: Vec<String>,
    owner_token: Option<String>,
}

#[tauri::command]
pub(crate) async fn daemon_ensure(
    app: AppHandle,
    state: State<'_, AppState>,
    spec: DaemonEnsureSpec,
) -> Result<Value, String> {
    let project = validate_project(&spec.project)?;
    crate::storage::ensure_authorized_path(&state.paths.authorized_roots_file, &project)?;
    let owner_token = spec
        .owner_token
        .ok_or_else(|| "managed daemon ownership token is required".to_string())?;
    validate_owner_token(&owner_token)?;

    let mut base_args = vec![
        "daemon".to_string(),
        "start".to_string(),
        "--parent-stdin-lease".to_string(),
        "--project".to_string(),
        display_path(&project),
        "--managed-by".to_string(),
        "desktop".to_string(),
        "--owner-token-env".to_string(),
        "ROSYNC_OWNER_TOKEN".to_string(),
        "--data-dir".to_string(),
        display_path(&state.paths.daemon_data_dir),
        "--timeout".to_string(),
        "10".to_string(),
        "--raw".to_string(),
    ];
    push_nonblank_flag(&mut base_args, "--game-id", spec.game_id.as_deref());
    push_nonblank_flag(&mut base_args, "--group-id", spec.group_id.as_deref());
    for place_id in spec.place_ids {
        push_nonblank_flag(&mut base_args, "--place-id", Some(&place_id));
    }
    if let Some(port) = spec.preferred_port {
        if port == 0 {
            return Err("preferred daemon port must be greater than zero".into());
        }
    }

    let attempts = preferred_port_attempts(spec.preferred_port);
    let mut errors = Vec::with_capacity(attempts.len());
    for port in attempts {
        let mut args = base_args.clone();
        if let Some(port) = port {
            args.extend(["--port".to_string(), port.to_string()]);
        }
        match run_lifecycle(
            &app,
            &state.paths.resource_dir,
            &state.lifecycle_children,
            args,
            Some(&owner_token),
        )
        .await
        {
            Ok(value) => {
                state.managed_daemon.remember(&value, &owner_token);
                return Ok(value);
            }
            // A timeout is not evidence that the preferred port is occupied.
            // On macOS it can mean a Files & Folders prompt is pending, and a
            // second attempt would only launch another blocked sidecar.
            Err(LifecycleFailure::Timeout(error)) => return Err(error),
            Err(error) => errors.push(error.message()),
        }
    }

    let fallback_error = errors.pop().unwrap_or_else(|| {
        "managed daemon lifecycle command did not make a start attempt".to_string()
    });
    if let Some(preferred_error) = errors.pop() {
        Err(format!(
            "preferred-port start failed: {preferred_error}\nfallback start failed: {fallback_error}"
        ))
    } else {
        Err(fallback_error)
    }
}

// A preferred port preserves a connected Studio plugin across controlled
// restarts. It is not permission to evict another project: if that exact port
// cannot be used, retry without `--port` and let the daemon's read-only port
// scan choose a free listener. Both attempts use the same owner capability,
// and `daemon start` is idempotent for an already-started matching project.
fn preferred_port_attempts(preferred: Option<u16>) -> Vec<Option<u16>> {
    match preferred {
        Some(port) => vec![Some(port), None],
        None => vec![None],
    }
}

fn push_nonblank_flag(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    args.push(flag.to_owned());
    args.push(value.to_owned());
}

#[tauri::command]
pub(crate) async fn daemon_status(
    app: AppHandle,
    state: State<'_, AppState>,
    project: Option<String>,
) -> Result<Value, String> {
    let Some(project) = project else {
        return Ok(serde_json::json!({ "ok": true, "running": false }));
    };
    let project = validate_project(&project)?;
    crate::storage::ensure_authorized_path(&state.paths.authorized_roots_file, &project)?;
    let args = vec![
        "daemon".to_string(),
        "status".to_string(),
        "--parent-stdin-lease".to_string(),
        "--project".to_string(),
        display_path(&project),
        "--data-dir".to_string(),
        display_path(&state.paths.daemon_data_dir),
        "--raw".to_string(),
    ];
    run_lifecycle(
        &app,
        &state.paths.resource_dir,
        &state.lifecycle_children,
        args,
        None,
    )
    .await
    .map_err(LifecycleFailure::message)
}

async fn run_lifecycle(
    app: &AppHandle,
    resource_dir: &Path,
    lifecycle_children: &LifecycleChildren,
    args: Vec<String>,
    secret: Option<&str>,
) -> Result<Value, LifecycleFailure> {
    let sidecar = app.shell().sidecar("rosync").map_err(|error| {
        LifecycleFailure::Message(format!("could not locate bundled Ro Sync daemon: {error}"))
    })?;
    let mut command = sidecar.args(args).current_dir(resource_dir);
    if let Some((lsp, compiler)) = bundled_luau_tools(resource_dir) {
        command = command
            .env("ROSYNC_LUAU_LSP", lsp)
            .env("ROSYNC_LUAU_COMPILE", compiler);
    }
    if let Some(secret) = secret {
        command = command.env("ROSYNC_OWNER_TOKEN", secret);
    }
    let (mut events, child) = command.spawn().map_err(|error| {
        LifecycleFailure::Message(format!(
            "could not run bundled Ro Sync lifecycle command: {error}"
        ))
    })?;
    let pid = lifecycle_children
        .register(child)
        .map_err(LifecycleFailure::Message)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status_code = None;
    let deadline = tokio::time::Instant::now() + LIFECYCLE_COMMAND_TIMEOUT;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, events.recv()).await {
            Ok(event) => event,
            Err(_) => {
                let kill_error = lifecycle_children.terminate(pid);
                return Err(LifecycleFailure::Timeout(lifecycle_timeout_message(
                    pid,
                    &stdout,
                    &stderr,
                    secret,
                    kill_error.as_deref(),
                )));
            }
        };

        match event {
            Some(CommandEvent::Stdout(line)) => {
                stdout.extend(line);
                stdout.push(b'\n');
            }
            Some(CommandEvent::Stderr(line)) => {
                stderr.extend(line);
                stderr.push(b'\n');
            }
            Some(CommandEvent::Error(error)) => {
                stderr.extend_from_slice(error.as_bytes());
                stderr.push(b'\n');
            }
            Some(CommandEvent::Terminated(payload)) => {
                lifecycle_children.forget(pid);
                status_code = payload.code;
                break;
            }
            None => {
                let _ = lifecycle_children.terminate(pid);
                break;
            }
            Some(_) => {}
        }
    }

    let stdout = String::from_utf8_lossy(&stdout);
    if let Ok(value) = serde_json::from_str::<Value>(stdout.trim()) {
        return Ok(normalize_lifecycle(value));
    }

    let stderr = String::from_utf8_lossy(&stderr);
    let mut message = if stderr.trim().is_empty() {
        stdout.trim().to_owned()
    } else {
        stderr.trim().to_owned()
    };
    if let Some(secret) = secret {
        message = message.replace(secret, "[redacted]");
    }
    if message.is_empty() {
        message = if status_code == Some(0) {
            "Ro Sync lifecycle command returned no JSON".into()
        } else {
            format!(
                "Ro Sync lifecycle command exited with code {:?}",
                status_code
            )
        };
    }
    Err(LifecycleFailure::Message(message))
}

fn lifecycle_timeout_message(
    pid: u32,
    stdout: &[u8],
    stderr: &[u8],
    secret: Option<&str>,
    kill_error: Option<&str>,
) -> String {
    let mut message = format!(
        "Ro Sync lifecycle command timed out after {} seconds; termination was requested for process {pid}",
        LIFECYCLE_COMMAND_TIMEOUT.as_secs()
    );
    let output = if stderr.iter().any(|byte| !byte.is_ascii_whitespace()) {
        stderr
    } else {
        stdout
    };
    let detail = String::from_utf8_lossy(output);
    let detail = redact_lifecycle_secret(detail.trim(), secret);
    if !detail.is_empty() {
        message.push_str(": ");
        message.push_str(&detail);
    }
    if let Some(error) = kill_error {
        message.push_str("; could not confirm termination: ");
        message.push_str(error);
    }
    #[cfg(target_os = "macos")]
    message.push_str(
        ". macOS may be waiting for Files & Folders permission; allow the selected project folder, or choose it again with Browse in Ro Sync",
    );
    message
}

fn redact_lifecycle_secret(message: &str, secret: Option<&str>) -> String {
    match secret {
        Some(secret) if !secret.is_empty() => message.replace(secret, "[redacted]"),
        _ => message.to_owned(),
    }
}

fn bundled_luau_tools(resource_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let platform = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x86_64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else {
        return None;
    };
    let extension = if cfg!(windows) { ".exe" } else { "" };
    let lsp = resource_dir
        .join("tools/luau-lsp")
        .join(platform)
        .join(format!("luau-lsp{extension}"));
    let compiler = resource_dir
        .join("tools/luau")
        .join(platform)
        .join(format!("luau-compile{extension}"));
    (lsp.is_file() && compiler.is_file()).then_some((lsp, compiler))
}

fn normalize_lifecycle(mut value: Value) -> Value {
    redact_secret_fields(&mut value);
    if let Some(object) = value.as_object_mut() {
        if !object.contains_key("base") {
            if let Some(base_url) = object.get("baseUrl").cloned() {
                object.insert("base".into(), base_url);
            }
        }
        if let Some(status) = object.get_mut("status") {
            *status = normalize_lifecycle(status.take());
        }
    }
    value
}

fn redact_secret_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for key in ["ownerToken", "owner_token", "browserToken", "controlToken"] {
                object.remove(key);
            }
            for child in object.values_mut() {
                redact_secret_fields(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                redact_secret_fields(child);
            }
        }
        _ => {}
    }
}

fn validate_project(raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err("daemon project path must be absolute".into());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("daemon project path must not contain . or .. components".into());
    }
    let canonical = path.canonicalize().map_err(|error| {
        format!(
            "could not resolve daemon project {}: {error}",
            display_path(path)
        )
    })?;
    if !canonical.is_dir() {
        return Err("daemon project must be a folder".into());
    }
    Ok(canonical)
}

fn validate_owner_token(token: &str) -> Result<(), String> {
    if !(16..=512).contains(&token.len())
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
    {
        return Err("managed daemon ownership token has an invalid format".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_json_gets_renderer_alias_without_secrets() {
        let value = normalize_lifecycle(serde_json::json!({
            "ok": true,
            "baseUrl": "http://127.0.0.1:7878",
            "ownerToken": "do-not-return"
        }));
        assert_eq!(value["base"], "http://127.0.0.1:7878");
        assert!(value.get("ownerToken").is_none());
    }

    #[test]
    fn owner_tokens_are_strictly_validated() {
        assert!(validate_owner_token("0123456789abcdef").is_ok());
        assert!(validate_owner_token("too short").is_err());
        assert!(validate_owner_token("0123456789abcdef\n--flag").is_err());
    }

    #[test]
    fn preferred_port_falls_back_without_overwriting_its_listener() {
        assert_eq!(preferred_port_attempts(Some(7878)), vec![Some(7878), None]);
        assert_eq!(preferred_port_attempts(None), vec![None]);
    }

    #[test]
    fn lifecycle_timeout_preserves_diagnostics_without_leaking_secrets() {
        let message = lifecycle_timeout_message(
            42,
            b"partial stdout\n",
            b"owner abcdefghijklmnop\n",
            Some("abcdefghijklmnop"),
            None,
        );
        assert!(message.contains("timed out after 20 seconds"));
        assert!(message.contains("process 42"));
        assert!(message.contains("owner [redacted]"));
        assert!(!message.contains("abcdefghijklmnop"));
    }

    #[test]
    fn lifecycle_registry_stays_closed_after_exit_cleanup() {
        let registry = LifecycleChildren::default();
        registry.terminate_all();
        assert!(registry.state.lock().unwrap().exiting);
    }

    #[test]
    fn native_exit_close_uses_the_exact_desktop_ownership_capability() {
        fn read_request(connection: &mut std::net::TcpStream) -> String {
            connection
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let count = connection.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let body_bytes = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                if request.len() >= header_end + 4 + body_bytes {
                    return String::from_utf8_lossy(&request).into_owned();
                }
            }
        }

        fn respond(connection: &mut std::net::TcpStream, body: &str) {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(response.as_bytes()).unwrap();
        }

        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut hello_connection, _) = listener.accept().unwrap();
            let hello_request = read_request(&mut hello_connection);
            assert!(hello_request.starts_with("GET /hello HTTP/1.1\r\n"));
            respond(
                &mut hello_connection,
                &serde_json::json!({
                    "managed": true,
                    "managedBy": "desktop",
                    "bootId": "boot-exact",
                    "port": port,
                })
                .to_string(),
            );
            drop(hello_connection);

            let (mut close_connection, _) = listener.accept().unwrap();
            let close_request = read_request(&mut close_connection);
            respond(&mut close_connection, "{\"ok\":true}");
            close_request
        });

        let claim = ManagedDaemonClaim::default();
        claim.remember(
            &serde_json::json!({
                "running": true,
                "managed": true,
                "managedBy": "desktop",
                "externallyManaged": false,
                "port": port,
                "bootId": "boot-exact",
            }),
            "0123456789abcdef",
        );
        claim.terminate();

        let request = server.join().unwrap();
        assert!(request.starts_with("POST /manager-close HTTP/1.1\r\n"));
        assert!(request.contains("\"token\":\"0123456789abcdef\""));
        assert!(claim.state.lock().unwrap().current.is_none());
    }

    #[test]
    fn native_exit_close_never_claims_external_daemons() {
        let claim = ManagedDaemonClaim::default();
        claim.remember(
            &serde_json::json!({
                "running": true,
                "managed": true,
                "managedBy": "cli",
                "externallyManaged": true,
                "port": 7878,
            }),
            "0123456789abcdef",
        );
        assert!(claim.state.lock().unwrap().current.is_none());
    }

    #[test]
    fn daemon_that_finishes_starting_during_exit_is_closed_not_remembered() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let claim = ManagedDaemonClaim::default();
        claim.mark_exiting();
        claim.remember(
            &serde_json::json!({
                "running": true,
                "managed": true,
                "managedBy": "desktop",
                "externallyManaged": false,
                "port": port,
                "bootId": "late-boot",
            }),
            "0123456789abcdef",
        );

        let state = claim.state.lock().unwrap();
        assert!(state.exiting);
        assert!(state.current.is_none());
    }
}
