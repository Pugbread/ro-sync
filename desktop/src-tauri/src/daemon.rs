use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
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
const EXIT_CLOSE_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EXIT_CLOSE_WORKERS: usize = 4;
const MAX_LOCAL_JSON_RESPONSE_BYTES: usize = 64 * 1024;
const MANAGED_DAEMON_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct ExactManagedDaemonClaim {
    project: String,
    port: u16,
    pid: u32,
    boot_id: String,
    owner_token: String,
}

pub(crate) struct ManagedDaemonClaims {
    state: Arc<Mutex<ManagedDaemonClaimsState>>,
    supervisor_started: AtomicBool,
}

#[derive(Default)]
struct ManagedDaemonClaimsState {
    exiting: bool,
    by_project: HashMap<String, ExactManagedDaemonClaim>,
}

impl Default for ManagedDaemonClaims {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ManagedDaemonClaimsState::default())),
            supervisor_started: AtomicBool::new(false),
        }
    }
}

impl Drop for ManagedDaemonClaims {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.exiting = true;
        }
    }
}

impl ManagedDaemonClaims {
    fn remember(&self, value: &Value, owner_token: &str) {
        let Some(claim) = exact_managed_daemon_claim(value, owner_token) else {
            return;
        };
        let close_after_exit = match self.state.lock() {
            Ok(state) if state.exiting => true,
            Ok(mut state) => {
                match state.by_project.get(&claim.project) {
                    // An idempotent ensure can rediscover the same daemon with
                    // a fresh, unproven token. Preserve the first exact claim
                    // rather than replacing a working capability.
                    Some(current)
                        if current.boot_id == claim.boot_id
                            && current.pid == claim.pid
                            && current.port == claim.port => {}
                    _ => {
                        state
                            .by_project
                            .insert(claim.project.clone(), claim.clone());
                    }
                }
                false
            }
            Err(_) => true,
        };
        if close_after_exit {
            let _ = close_exact_managed_daemon(&claim);
        } else {
            self.ensure_supervisor();
        }
    }

    fn ensure_supervisor(&self) {
        if self
            .supervisor_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let state = Arc::clone(&self.state);
        if thread::Builder::new()
            .name("rosync-daemon-heartbeats".to_string())
            .spawn(move || loop {
                thread::sleep(MANAGED_DAEMON_HEARTBEAT_INTERVAL);
                let claims = match state.lock() {
                    Ok(state) if state.exiting => return,
                    Ok(state) => state.by_project.values().cloned().collect::<Vec<_>>(),
                    Err(_) => return,
                };
                for claim in claims {
                    if let Err(error) = heartbeat_exact_managed_daemon(&claim) {
                        eprintln!(
                            "[rosync-dbg] heartbeat failed for {} (port {}): {error}",
                            claim.project, claim.port
                        );
                    }
                }
            })
            .is_err()
        {
            self.supervisor_started.store(false, Ordering::Release);
        }
    }

    fn forget(&self, project: &str, boot_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            if state
                .by_project
                .get(project)
                .is_some_and(|claim| claim.boot_id == boot_id)
            {
                state.by_project.remove(project);
            }
        }
    }

    fn list(&self) -> Result<Value, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "managed daemon claim registry is poisoned".to_string())?;
        let mut claims = state.by_project.values().collect::<Vec<_>>();
        claims.sort_by(|left, right| left.project.cmp(&right.project));
        Ok(serde_json::json!({
            "ok": true,
            "daemons": claims.into_iter().map(|claim| serde_json::json!({
                "tracked": true,
                "managedBy": "desktop",
                "project": claim.project,
                "canonicalProject": claim.project,
                "port": claim.port,
                "pid": claim.pid,
                "bootId": claim.boot_id,
            })).collect::<Vec<_>>(),
        }))
    }

    pub(crate) fn mark_exiting(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.exiting = true;
        }
    }

    pub(crate) fn terminate(&self) {
        let claims = self.state.lock().ok().map(|mut state| {
            state.exiting = true;
            state
                .by_project
                .drain()
                .map(|(_, claim)| claim)
                .collect::<Vec<_>>()
        });
        if let Some(claims) = claims {
            close_managed_daemons(claims);
        }
    }
}

fn exact_managed_daemon_claim(value: &Value, owner_token: &str) -> Option<ExactManagedDaemonClaim> {
    let status = value.get("status").unwrap_or(value);
    let is_owned = status.get("running").and_then(Value::as_bool) == Some(true)
        && status.get("managed").and_then(Value::as_bool) == Some(true)
        && status.get("managedBy").and_then(Value::as_str) == Some("desktop")
        && status.get("externallyManaged").and_then(Value::as_bool) != Some(true);
    let port = status
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port > 0)?;
    let pid = status
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0)?;
    let boot_id = status
        .get("bootId")
        .and_then(Value::as_str)
        .filter(|boot_id| !boot_id.is_empty())?;
    let project = status
        .get("canonicalProject")
        .or_else(|| status.get("project"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|project| !project.is_empty())?;
    if !is_owned || validate_owner_token(owner_token).is_err() {
        return None;
    }
    Some(ExactManagedDaemonClaim {
        project: project.to_string(),
        port,
        pid,
        boot_id: boot_id.to_string(),
        owner_token: owner_token.to_string(),
    })
}

fn close_managed_daemons(claims: Vec<ExactManagedDaemonClaim>) {
    if claims.is_empty() {
        return;
    }

    // Request every normal local shutdown before waiting for any one daemon to
    // release its listener. A small fixed worker pool avoids one OS thread per
    // served project, while one shared deadline keeps app/update exit bounded.
    // Every request still performs the full immutable identity handshake.
    let deadline = Instant::now() + EXIT_CLOSE_TOTAL_TIMEOUT;
    let accepted = (0..claims.len())
        .map(|_| AtomicBool::new(false))
        .collect::<Vec<_>>();
    run_bounded_exit_workers(claims.len(), deadline, |index| {
        if request_exact_managed_daemon_close(&claims[index], deadline).is_ok() {
            accepted[index].store(true, Ordering::Release);
        }
    });

    // Daemons receive the requests concurrently above, so in the healthy case
    // they all complete their graceful 250 ms drain together. Confirm accepted
    // closes with the same bounded worker count and absolute deadline.
    run_bounded_exit_workers(claims.len(), deadline, |index| {
        if accepted[index].load(Ordering::Acquire) {
            let _ = wait_for_exact_managed_daemon_release(&claims[index], deadline);
        }
    });
}

fn close_exact_managed_daemon(claim: &ExactManagedDaemonClaim) -> Result<(), String> {
    let deadline = Instant::now() + EXIT_CLOSE_TOTAL_TIMEOUT;
    request_exact_managed_daemon_close(claim, deadline)?;
    wait_for_exact_managed_daemon_release(claim, deadline)
}

fn request_exact_managed_daemon_close(
    claim: &ExactManagedDaemonClaim,
    deadline: Instant,
) -> Result<(), String> {
    eprintln!(
        "[rosync-dbg] requesting daemon close: project {} port {} pid {}",
        claim.project, claim.port, claim.pid
    );
    let hello = local_json_request_until(claim.port, "GET", "/hello", &[], deadline)?;
    let exact_daemon = hello.get("managed").and_then(Value::as_bool) == Some(true)
        && hello.get("managedBy").and_then(Value::as_str) == Some("desktop")
        && hello.get("project").and_then(Value::as_str) == Some(claim.project.as_str())
        && hello.get("bootId").and_then(Value::as_str) == Some(claim.boot_id.as_str())
        && hello.get("pid").and_then(Value::as_u64) == Some(u64::from(claim.pid))
        && hello.get("port").and_then(Value::as_u64) == Some(u64::from(claim.port));
    if !exact_daemon {
        return Err("managed daemon identity changed before native exit cleanup".into());
    }

    let body = serde_json::to_vec(&serde_json::json!({
        "token": claim.owner_token,
        "reason": "desktop app exited",
        "expectedBootId": claim.boot_id,
        "expectedPid": claim.pid,
        "expectedPort": claim.port,
        "expectedCanonicalProject": claim.project,
    }))
    .map_err(|error| format!("encode managed daemon close request: {error}"))?;
    let response = local_json_request_until(claim.port, "POST", "/manager-close", &body, deadline)?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("managed daemon rejected the authenticated close request".into());
    }
    Ok(())
}

fn wait_for_exact_managed_daemon_release(
    claim: &ExactManagedDaemonClaim,
    deadline: Instant,
) -> Result<(), String> {
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, claim.port));
    while Instant::now() < deadline {
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        if timeout.is_zero() || TcpStream::connect_timeout(&address, timeout).is_err() {
            return Ok(());
        }
        let pause = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        if !pause.is_zero() {
            thread::sleep(pause);
        }
    }
    Err("managed daemon remained reachable after the authenticated close request".into())
}

fn heartbeat_exact_managed_daemon(claim: &ExactManagedDaemonClaim) -> Result<(), String> {
    let hello = local_json_request(claim.port, "GET", "/hello", &[])?;
    let exact_daemon = hello.get("managed").and_then(Value::as_bool) == Some(true)
        && hello.get("managedBy").and_then(Value::as_str) == Some("desktop")
        && hello.get("project").and_then(Value::as_str) == Some(claim.project.as_str())
        && hello.get("bootId").and_then(Value::as_str) == Some(claim.boot_id.as_str())
        && hello.get("pid").and_then(Value::as_u64) == Some(u64::from(claim.pid))
        && hello.get("port").and_then(Value::as_u64) == Some(u64::from(claim.port));
    if !exact_daemon {
        return Err("managed daemon identity changed before native heartbeat".into());
    }

    let body = serde_json::to_vec(&serde_json::json!({
        "token": claim.owner_token,
        "reason": "native desktop supervisor",
    }))
    .map_err(|error| format!("encode managed daemon heartbeat request: {error}"))?;
    let response = local_json_request(claim.port, "POST", "/manager-heartbeat", &body)?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("managed daemon rejected the authenticated heartbeat".into());
    }
    Ok(())
}

fn local_json_request(port: u16, method: &str, path: &str, body: &[u8]) -> Result<Value, String> {
    local_json_request_until(
        port,
        method,
        path,
        body,
        Instant::now() + EXIT_CLOSE_IO_TIMEOUT.saturating_mul(3),
    )
}

fn local_json_request_until(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    deadline: Instant,
) -> Result<Value, String> {
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    let mut timeout = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| "managed daemon request deadline expired".to_string())?
        .min(EXIT_CLOSE_IO_TIMEOUT);
    if timeout.is_zero() {
        return Err("managed daemon request deadline expired".into());
    }
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| format!("connect to managed daemon: {error}"))?;
    timeout = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| "managed daemon request deadline expired".to_string())?
        .min(EXIT_CLOSE_IO_TIMEOUT);
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("set managed daemon read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
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
    loop {
        timeout = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "managed daemon request deadline expired".to_string())?
            .min(EXIT_CLOSE_IO_TIMEOUT);
        if timeout.is_zero() {
            return Err("managed daemon request deadline expired".into());
        }
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("set managed daemon read timeout: {error}"))?;
        let mut chunk = [0_u8; 4096];
        let count = stream
            .read(&mut chunk)
            .map_err(|error| format!("read managed daemon close response: {error}"))?;
        if count == 0 {
            break;
        }
        if response.len().saturating_add(count) > MAX_LOCAL_JSON_RESPONSE_BYTES {
            return Err("managed daemon response exceeded the 64 KiB limit".into());
        }
        response.extend_from_slice(&chunk[..count]);
    }
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

fn run_bounded_exit_workers<F>(item_count: usize, deadline: Instant, task: F)
where
    F: Fn(usize) + Sync,
{
    if item_count == 0 || Instant::now() >= deadline {
        return;
    }
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        let mut workers = Vec::new();
        for worker_index in 0..item_count.min(MAX_EXIT_CLOSE_WORKERS) {
            let next = &next;
            let task = &task;
            let worker = thread::Builder::new()
                .name(format!("rosync-daemon-close-{}", worker_index + 1))
                .spawn_scoped(scope, move || loop {
                    if Instant::now() >= deadline {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= item_count {
                        break;
                    }
                    task(index);
                });
            if let Ok(worker) = worker {
                workers.push(worker);
            }
        }

        // A heavily constrained host can refuse even a small helper thread.
        // Continue synchronously instead of panicking or dropping every close.
        if workers.is_empty() {
            while Instant::now() < deadline {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= item_count {
                    break;
                }
                task(index);
            }
        }
        for worker in workers {
            let _ = worker.join();
        }
    });
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
    projects_root: Option<String>,
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
    let projects_root = match spec
        .projects_root
        .as_deref()
        .map(str::trim)
        .filter(|root| !root.is_empty())
    {
        Some(root) => {
            let root = validate_project(root)?;
            crate::storage::ensure_authorized_path(&state.paths.authorized_roots_file, &root)?;
            Some(root)
        }
        None => None,
    };
    let owner_token = spec
        .owner_token
        .ok_or_else(|| "managed daemon ownership token is required".to_string())?;
    validate_owner_token(&owner_token)?;
    // Port discovery and listener launch are separate daemon operations. Keep
    // starts for different projects serial inside one Desktop process so two
    // concurrent UI requests cannot both select the same free port.
    let _start_guard = state.daemon_start_lock.lock().await;
    eprintln!(
        "[rosync-dbg] daemon_ensure: project {} preferred_port {:?}",
        display_path(&project),
        spec.preferred_port
    );

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
    if let Some(projects_root) = projects_root.as_deref() {
        base_args.extend(["--projects-root".to_string(), display_path(projects_root)]);
    }
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
    let mut preferred_collision = None;
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
                state.managed_daemons.remember(&value, &owner_token);
                return Ok(value);
            }
            // A timeout is not evidence that the preferred port is occupied.
            // On macOS it can mean a Files & Folders prompt is pending, and a
            // second attempt would only launch another blocked sidecar.
            Err(LifecycleFailure::Timeout(error)) => return Err(error),
            Err(error) => {
                let error = error.message();
                if port.is_some_and(|port| preferred_port_collision(&error, port)) {
                    preferred_collision = Some(error);
                    continue;
                }
                return if let Some(preferred_error) = preferred_collision {
                    Err(format!(
                        "preferred-port start failed: {preferred_error}\nfallback start failed: {error}"
                    ))
                } else {
                    Err(error)
                };
            }
        }
    }

    Err(preferred_collision.unwrap_or_else(|| {
        "managed daemon lifecycle command did not make a start attempt".to_string()
    }))
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

fn preferred_port_collision(error: &str, port: u16) -> bool {
    [
        format!("daemon start: port {port} is already serving "),
        format!("daemon start: requested port {port} serves "),
        format!("daemon start: requested port {port} is unavailable:"),
        format!("serve: bind 127.0.0.1:{port}:"),
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

fn push_nonblank_flag(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    args.push(flag.to_owned());
    args.push(value.to_owned());
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonStopSpec {
    project: String,
    boot_id: Option<String>,
    owner_token: Option<String>,
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
    daemon_status_for_project(&app, state.inner(), &project).await
}

#[tauri::command]
pub(crate) async fn daemon_stop(
    app: AppHandle,
    state: State<'_, AppState>,
    spec: DaemonStopSpec,
) -> Result<Value, String> {
    let project = validate_project(&spec.project)?;
    crate::storage::ensure_authorized_path(&state.paths.authorized_roots_file, &project)?;
    let expected_boot_id = spec
        .boot_id
        .as_deref()
        .ok_or_else(|| "managed daemon boot ID is required".to_string())?;
    validate_lifecycle_identity(expected_boot_id, "managed daemon boot ID")?;
    let owner_token = spec
        .owner_token
        .as_deref()
        .ok_or_else(|| "managed daemon ownership token is required".to_string())?;
    validate_owner_token(owner_token)?;

    let value = daemon_status_for_project(&app, state.inner(), &project).await?;
    let status = value.get("status").unwrap_or(&value);
    let canonical_project = display_path(&project);
    if status.get("running").and_then(Value::as_bool) != Some(true) {
        state
            .managed_daemons
            .forget(&canonical_project, expected_boot_id);
        return Ok(serde_json::json!({
            "ok": true,
            "stopped": true,
            "running": false,
            "project": canonical_project,
            "bootId": expected_boot_id,
        }));
    }

    let claim = exact_managed_daemon_claim(&value, owner_token).ok_or_else(|| {
        "daemon is not an exact Desktop-managed lifecycle target; it was left running".to_string()
    })?;
    if claim.project != canonical_project {
        return Err("managed daemon project identity changed; it was left running".into());
    }
    if claim.boot_id != expected_boot_id {
        return Err("managed daemon boot identity changed; it was left running".into());
    }

    // Make an exact stop discovered after an app relaunch visible to native
    // exit cleanup before the blocking authenticated close begins.
    state.managed_daemons.remember(&value, owner_token);
    let stopped_project = claim.project.clone();
    let stopped_boot_id = claim.boot_id.clone();
    let stopped_port = claim.port;
    let stopped_pid = claim.pid;
    tokio::task::spawn_blocking(move || close_exact_managed_daemon(&claim))
        .await
        .map_err(|error| format!("managed daemon close task failed: {error}"))??;
    state
        .managed_daemons
        .forget(&stopped_project, &stopped_boot_id);
    Ok(serde_json::json!({
        "ok": true,
        "stopped": true,
        "running": false,
        "project": stopped_project,
        "canonicalProject": stopped_project,
        "bootId": stopped_boot_id,
        "port": stopped_port,
        "pid": stopped_pid,
    }))
}

#[tauri::command]
pub(crate) fn daemon_list(state: State<'_, AppState>) -> Result<Value, String> {
    state.managed_daemons.list()
}

async fn daemon_status_for_project(
    app: &AppHandle,
    state: &AppState,
    project: &Path,
) -> Result<Value, String> {
    let args = vec![
        "daemon".to_string(),
        "status".to_string(),
        "--parent-stdin-lease".to_string(),
        "--project".to_string(),
        display_path(project),
        "--data-dir".to_string(),
        display_path(&state.paths.daemon_data_dir),
        "--raw".to_string(),
    ];
    run_lifecycle(
        app,
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
    eprintln!("[rosync-dbg] run_lifecycle: rosync {}", args.join(" "));
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
                // Raw lifecycle commands emit exactly one JSON document. The
                // shell plugin can delay its separate Terminated event after
                // stdout has already closed, especially when the command
                // spawned a detached daemon. Treat a complete JSON document
                // as the terminal success signal and close only this exact
                // short-lived sidecar handle.
                if let Some(value) = parse_lifecycle_stdout(&stdout) {
                    let _ = lifecycle_children.terminate(pid);
                    let mut shown = value.to_string();
                    if let Some(secret) = secret {
                        shown = shown.replace(secret, "[redacted]");
                    }
                    eprintln!("[rosync-dbg] run_lifecycle: ok — {shown}");
                    return Ok(normalize_lifecycle(value));
                }
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
                eprintln!(
                    "[rosync-dbg] run_lifecycle: child pid {pid} terminated, code {status_code:?}"
                );
                break;
            }
            None => {
                let _ = lifecycle_children.terminate(pid);
                break;
            }
            Some(_) => {}
        }
    }

    if let Some(value) = parse_lifecycle_stdout(&stdout) {
        return Ok(normalize_lifecycle(value));
    }

    let stdout = String::from_utf8_lossy(&stdout);
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
    eprintln!("[rosync-dbg] run_lifecycle: failed — {message}");
    Err(LifecycleFailure::Message(message))
}

fn parse_lifecycle_stdout(stdout: &[u8]) -> Option<Value> {
    let stdout = String::from_utf8_lossy(stdout);
    serde_json::from_str(stdout.trim()).ok()
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
    validate_lifecycle_identity(token, "managed daemon ownership token")
}

fn validate_lifecycle_identity(value: &str, label: &str) -> Result<(), String> {
    if !(16..=512).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
    {
        return Err(format!("{label} has an invalid format"));
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
        assert!(validate_lifecycle_identity("0123456789abcdef", "boot ID").is_ok());
        assert!(validate_lifecycle_identity("short", "boot ID").is_err());
    }

    #[test]
    fn exit_close_worker_pool_is_bounded_under_load() {
        const ITEM_COUNT: usize = 200;
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let visited = (0..ITEM_COUNT)
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>();

        run_bounded_exit_workers(
            ITEM_COUNT,
            Instant::now() + Duration::from_secs(5),
            |index| {
                let concurrent = active.fetch_add(1, Ordering::AcqRel) + 1;
                peak.fetch_max(concurrent, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(2));
                visited[index].store(true, Ordering::Release);
                active.fetch_sub(1, Ordering::AcqRel);
            },
        );

        assert!(visited
            .iter()
            .all(|visited| visited.load(Ordering::Acquire)));
        assert!(peak.load(Ordering::Acquire) <= MAX_EXIT_CLOSE_WORKERS);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn exit_close_worker_pool_stops_dispatching_after_the_deadline() {
        let calls = AtomicUsize::new(0);
        let started = Instant::now();
        run_bounded_exit_workers(1_000, started + Duration::from_millis(20), |_| {
            calls.fetch_add(1, Ordering::AcqRel);
            thread::sleep(Duration::from_millis(60));
        });

        assert!(calls.load(Ordering::Acquire) <= MAX_EXIT_CLOSE_WORKERS);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn lifecycle_command_specs_keep_existing_fields_and_accept_multi_project_fields() {
        let ensure = serde_json::from_value::<DaemonEnsureSpec>(serde_json::json!({
            "project": "/projects/alpha",
            "projectsRoot": "/projects",
            "preferredPort": 7878,
            "ownerToken": "0123456789abcdef",
        }))
        .unwrap();
        assert_eq!(ensure.project, "/projects/alpha");
        assert_eq!(ensure.projects_root.as_deref(), Some("/projects"));
        assert_eq!(ensure.preferred_port, Some(7878));

        let stop = serde_json::from_value::<DaemonStopSpec>(serde_json::json!({
            "project": "/projects/alpha",
            "bootId": "0123456789abcdef",
            "ownerToken": "fedcba9876543210",
        }))
        .unwrap();
        assert_eq!(stop.project, "/projects/alpha");
        assert_eq!(stop.boot_id.as_deref(), Some("0123456789abcdef"));
        assert_eq!(stop.owner_token.as_deref(), Some("fedcba9876543210"));
    }

    #[test]
    fn preferred_port_falls_back_without_overwriting_its_listener() {
        assert_eq!(preferred_port_attempts(Some(7878)), vec![Some(7878), None]);
        assert_eq!(preferred_port_attempts(None), vec![None]);
        assert!(preferred_port_collision(
            "daemon start: port 7878 is already serving /other",
            7878,
        ));
        assert!(preferred_port_collision(
            "daemon start: child exited\nError: serve: bind 127.0.0.1:7878: Address already in use",
            7878,
        ));
        assert!(preferred_port_collision(
            "daemon start: requested port 7878 is unavailable: Address already in use",
            7878,
        ));
        assert!(!preferred_port_collision(
            "daemon start: matching managed daemon is owned by a different lifecycle capability",
            7878,
        ));
        assert!(!preferred_port_collision(
            "daemon start: the matching daemon is already running without the requested projects root",
            7878,
        ));
    }

    #[test]
    fn native_supervisor_heartbeats_only_the_exact_desktop_daemon() {
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
                    "project": "/heartbeat-project",
                    "bootId": "heartbeat-boot",
                    "pid": 5252,
                    "port": port,
                })
                .to_string(),
            );
            drop(hello_connection);

            let (mut heartbeat_connection, _) = listener.accept().unwrap();
            let heartbeat_request = read_request(&mut heartbeat_connection);
            respond(&mut heartbeat_connection, "{\"ok\":true}");
            heartbeat_request
        });
        let claim = ExactManagedDaemonClaim {
            project: "/heartbeat-project".into(),
            port,
            pid: 5252,
            boot_id: "heartbeat-boot".into(),
            owner_token: "heartbeat-owner-token".into(),
        };

        heartbeat_exact_managed_daemon(&claim).unwrap();

        let request = server.join().unwrap();
        assert!(request.starts_with("POST /manager-heartbeat HTTP/1.1\r\n"));
        assert!(request.contains("\"token\":\"heartbeat-owner-token\""));
        assert!(request.contains("native desktop supervisor"));
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
    fn complete_lifecycle_json_is_a_terminal_result() {
        assert!(parse_lifecycle_stdout(br#"{"ok":true"#).is_none());
        let value = parse_lifecycle_stdout(b"  {\"ok\":true,\"port\":7879}\n").unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["port"], 7879);
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
                    "project": "/project-exact",
                    "bootId": "boot-exact",
                    "pid": 4242,
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

        let claims = ManagedDaemonClaims::default();
        claims.remember(
            &serde_json::json!({
                "running": true,
                "managed": true,
                "managedBy": "desktop",
                "externallyManaged": false,
                "canonicalProject": "/project-exact",
                "port": port,
                "pid": 4242,
                "bootId": "boot-exact",
            }),
            "0123456789abcdef",
        );
        claims.terminate();

        let request = server.join().unwrap();
        assert!(request.starts_with("POST /manager-close HTTP/1.1\r\n"));
        assert!(request.contains("\"token\":\"0123456789abcdef\""));
        assert!(request.contains("\"expectedBootId\":\"boot-exact\""));
        assert!(request.contains("\"expectedPid\":4242"));
        assert!(request.contains(&format!("\"expectedPort\":{port}")));
        assert!(request.contains("\"expectedCanonicalProject\":\"/project-exact\""));
        assert!(claims.state.lock().unwrap().by_project.is_empty());
    }

    #[test]
    fn native_exit_close_never_claims_external_daemons() {
        let claims = ManagedDaemonClaims::default();
        claims.remember(
            &serde_json::json!({
                "running": true,
                "managed": true,
                "managedBy": "cli",
                "externallyManaged": true,
                "canonicalProject": "/external-project",
                "port": 7878,
                "pid": 4242,
                "bootId": "external-boot",
            }),
            "0123456789abcdef",
        );
        assert!(claims.state.lock().unwrap().by_project.is_empty());
    }

    #[test]
    fn native_exit_tracks_owned_projects_independently() {
        let claims = ManagedDaemonClaims::default();
        for (project, port, pid, boot_id, token) in [
            ("/project-a", 7878, 4101, "boot-a", "token-project-a1"),
            ("/project-b", 7879, 4102, "boot-b", "token-project-b2"),
        ] {
            claims.remember(
                &serde_json::json!({
                    "running": true,
                    "managed": true,
                    "managedBy": "desktop",
                    "externallyManaged": false,
                    "canonicalProject": project,
                    "port": port,
                    "pid": pid,
                    "bootId": boot_id,
                }),
                token,
            );
        }

        {
            let state = claims.state.lock().unwrap();
            assert_eq!(state.by_project.len(), 2);
            assert_eq!(state.by_project["/project-a"].boot_id, "boot-a");
            assert_eq!(state.by_project["/project-b"].boot_id, "boot-b");
        }
        let listed = claims.list().unwrap();
        assert_eq!(listed["daemons"].as_array().unwrap().len(), 2);
        assert_eq!(listed["daemons"][0]["project"], "/project-a");
        assert_eq!(listed["daemons"][1]["project"], "/project-b");
        assert!(listed["daemons"][0].get("ownerToken").is_none());
        assert!(listed["daemons"][0].get("running").is_none());
    }

    #[test]
    fn native_exit_closes_all_owned_projects() {
        fn spawn_daemon(
            project: &'static str,
            pid: u32,
            boot_id: &'static str,
        ) -> (u16, std::thread::JoinHandle<String>) {
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
                        "project": project,
                        "bootId": boot_id,
                        "pid": pid,
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
            (port, server)
        }

        let (port_a, server_a) = spawn_daemon("/parallel-a", 5101, "parallel-boot-a");
        let (port_b, server_b) = spawn_daemon("/parallel-b", 5102, "parallel-boot-b");
        let claims = ManagedDaemonClaims::default();
        for (project, port, pid, boot_id, token) in [
            (
                "/parallel-a",
                port_a,
                5101,
                "parallel-boot-a",
                "parallel-token-a",
            ),
            (
                "/parallel-b",
                port_b,
                5102,
                "parallel-boot-b",
                "parallel-token-b",
            ),
        ] {
            claims.remember(
                &serde_json::json!({
                    "running": true,
                    "managed": true,
                    "managedBy": "desktop",
                    "externallyManaged": false,
                    "canonicalProject": project,
                    "port": port,
                    "pid": pid,
                    "bootId": boot_id,
                }),
                token,
            );
        }

        claims.terminate();
        let request_a = server_a.join().unwrap();
        let request_b = server_b.join().unwrap();
        assert!(request_a.contains("\"token\":\"parallel-token-a\""));
        assert!(request_b.contains("\"token\":\"parallel-token-b\""));
        assert!(claims.state.lock().unwrap().by_project.is_empty());
    }

    #[test]
    fn idempotent_ensure_does_not_replace_an_existing_exact_capability() {
        let claims = ManagedDaemonClaims::default();
        let status = serde_json::json!({
            "running": true,
            "managed": true,
            "managedBy": "desktop",
            "externallyManaged": false,
            "canonicalProject": "/same-project",
            "port": 7878,
            "pid": 4200,
            "bootId": "same-boot",
        });
        claims.remember(&status, "first-valid-token");
        claims.remember(&status, "second-new-token");

        let state = claims.state.lock().unwrap();
        assert_eq!(state.by_project.len(), 1);
        assert_eq!(
            state.by_project["/same-project"].owner_token,
            "first-valid-token"
        );
    }

    #[test]
    fn daemon_that_finishes_starting_during_exit_is_closed_not_remembered() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let claims = ManagedDaemonClaims::default();
        claims.mark_exiting();
        claims.remember(
            &serde_json::json!({
                "running": true,
                "managed": true,
                "managedBy": "desktop",
                "externallyManaged": false,
                "canonicalProject": "/late-project",
                "port": port,
                "pid": 4343,
                "bootId": "late-boot",
            }),
            "0123456789abcdef",
        );

        let state = claims.state.lock().unwrap();
        assert!(state.exiting);
        assert!(state.by_project.is_empty());
    }
}
