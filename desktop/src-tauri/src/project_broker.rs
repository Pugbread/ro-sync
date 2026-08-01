use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
#[cfg(test)]
use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{
    resources::{display_path, AppPaths},
    storage,
};

const BROKER_PORT_START: u16 = 7867;
const BROKER_PORT_END: u16 = 7870;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_PROJECT_ROOT_ENTRIES: usize = 4096;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectInitEvent {
    pub(crate) request_id: String,
    pub(crate) project_path: String,
    pub(crate) name: String,
    pub(crate) game_name: String,
    pub(crate) place_name: String,
    pub(crate) game_id: String,
    pub(crate) place_id: String,
    pub(crate) group_id: Option<String>,
    pub(crate) creator_type: Option<String>,
    pub(crate) creator_id: Option<String>,
    pub(crate) reused: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectInitRequest {
    plugin_capability: String,
    #[serde(default)]
    request_id: String,
    #[serde(default)]
    game_name: String,
    #[serde(default)]
    place_name: String,
    game_id: String,
    place_id: String,
    #[serde(default)]
    group_id: Option<String>,
    #[serde(default)]
    creator_type: Option<String>,
    #[serde(default)]
    creator_id: Option<String>,
}

struct BrokerShared {
    paths: AppPaths,
    capability: String,
    port: Option<u16>,
    startup_error: Option<String>,
    shutdown: AtomicBool,
    pending: Mutex<VecDeque<ProjectInitEvent>>,
}

pub(crate) struct ProjectInitBroker {
    shared: Arc<BrokerShared>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl ProjectInitBroker {
    pub(crate) fn start(paths: AppPaths) -> Self {
        let capability = random_capability().unwrap_or_default();
        let (listener, port, startup_error) = if capability.is_empty() {
            (
                None,
                None,
                Some("operating-system randomness is unavailable".to_string()),
            )
        } else {
            match bind_listener() {
                Ok((listener, port)) => (Some(listener), Some(port), None),
                Err(error) => (None, None, Some(error)),
            }
        };

        let mut shared = Arc::new(BrokerShared {
            paths,
            capability,
            port,
            startup_error,
            shutdown: AtomicBool::new(false),
            pending: Mutex::new(VecDeque::new()),
        });
        let worker = listener.and_then(|listener| {
            let worker_shared = Arc::clone(&shared);
            match thread::Builder::new()
                .name("rosync-project-broker".into())
                .spawn(move || serve(listener, worker_shared))
            {
                Ok(worker) => Some(worker),
                Err(error) => {
                    if let Some(shared) = Arc::get_mut(&mut shared) {
                        shared.port = None;
                        shared.startup_error =
                            Some(format!("could not start project broker worker: {error}"));
                    }
                    None
                }
            }
        });
        Self {
            shared,
            worker: Mutex::new(worker),
        }
    }

    pub(crate) fn status(&self) -> Value {
        json!({
            "ok": self.shared.port.is_some(),
            "port": self.shared.port,
            "ports": { "start": BROKER_PORT_START, "end": BROKER_PORT_END },
            "error": self.shared.startup_error,
        })
    }

    pub(crate) fn drain(&self) -> Vec<ProjectInitEvent> {
        self.shared
            .pending
            .lock()
            .map(|mut pending| pending.drain(..).collect())
            .unwrap_or_default()
    }

    pub(crate) fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(port) = self.shared.port {
            let _ = TcpStream::connect_timeout(
                &SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into(),
                Duration::from_millis(100),
            );
        }
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for ProjectInitBroker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn random_capability() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("generate project broker capability: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn bind_listener() -> Result<(TcpListener, u16), String> {
    let mut errors = Vec::new();
    for port in BROKER_PORT_START..=BROKER_PORT_END {
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        match TcpListener::bind(address) {
            Ok(listener) => {
                listener
                    .set_nonblocking(true)
                    .map_err(|error| format!("configure project broker listener: {error}"))?;
                return Ok((listener, port));
            }
            Err(error) => errors.push(format!("{port}: {error}")),
        }
    }
    Err(format!(
        "could not bind the project broker on {}-{} ({})",
        BROKER_PORT_START,
        BROKER_PORT_END,
        errors.join(", ")
    ))
}

fn serve(listener: TcpListener, shared: Arc<BrokerShared>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, address)) => {
                if address.ip().is_loopback() {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let _ = handle_connection(&mut stream, &shared);
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn handle_connection(stream: &mut TcpStream, shared: &BrokerShared) -> Result<(), String> {
    let request = read_http_request(stream)?;
    if request.has_origin {
        return write_json_response(
            stream,
            403,
            &json!({ "ok": false, "error": "browser-origin requests are not accepted" }),
        );
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/hello") => write_json_response(stream, 200, &broker_hello(shared)),
        ("POST", "/projects/init") => {
            let payload: ProjectInitRequest = match serde_json::from_slice(&request.body) {
                Ok(payload) => payload,
                Err(error) => {
                    return write_json_response(
                        stream,
                        400,
                        &json!({
                            "ok": false,
                            "error": format!("decode project initialization request: {error}"),
                        }),
                    )
                }
            };
            if !constant_time_eq(
                payload.plugin_capability.as_bytes(),
                shared.capability.as_bytes(),
            ) {
                return write_json_response(
                    stream,
                    403,
                    &json!({ "ok": false, "error": "invalid project broker capability" }),
                );
            }
            match initialize_project(shared, payload) {
                Ok(event) => {
                    let response = json!({
                        "ok": true,
                        "accepted": true,
                        "projectPath": event.project_path,
                        "name": event.name,
                        "gameName": event.game_name,
                        "placeName": event.place_name,
                        "gameId": event.game_id,
                        "placeId": event.place_id,
                        "reused": event.reused,
                    });
                    enqueue_event(shared, event);
                    write_json_response(stream, 200, &response)
                }
                Err(error) => write_json_response(
                    stream,
                    if error.contains("Projects folder") {
                        409
                    } else {
                        400
                    },
                    &json!({ "ok": false, "error": error }),
                ),
            }
        }
        _ => write_json_response(stream, 404, &json!({ "ok": false, "error": "not found" })),
    }
}

// Must match PLUGIN_PROTOCOL_VERSION in daemon/src/ws.rs and plugin/Plugin.luau.
// The plugin refuses a broker's projectInit offer on any mismatch, which
// silently disables Connect → Create Project.
const PLUGIN_PROTOCOL_VERSION: u64 = 5;

fn broker_hello(shared: &BrokerShared) -> Value {
    let (projects_root, projects_root_error) = configured_projects_root(shared);
    json!({
        "ok": true,
        "name": "Ro Sync Desktop",
        "pluginProtocol": PLUGIN_PROTOCOL_VERSION,
        "pluginCapability": shared.capability,
        "projectInit": {
            "available": projects_root.is_some(),
            "projectsRoot": projects_root.as_ref().map(|path| display_path(path)),
            "endpoint": "/projects/init",
            "broker": true,
            "error": projects_root_error,
        },
    })
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
    has_origin: bool,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    let mut bytes = Vec::with_capacity(4096);
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| format!("read project broker request: {error}"))?;
        if read == 0 {
            return Err("project broker request ended before its headers".into());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err("project broker request exceeds the size limit".into());
        }
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };

    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "project broker headers must be UTF-8".to_string())?;
    let mut lines = headers.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "project broker request line is missing".to_string())?;
    let mut request_parts = request_line.split_ascii_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let raw_path = request_parts.next().unwrap_or_default();
    let path = raw_path.split('?').next().unwrap_or_default().to_string();
    if request_parts.next().is_none() || method.is_empty() || path.is_empty() {
        return Err("project broker request line is malformed".into());
    }

    let mut content_length = 0_usize;
    let mut has_origin = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| "project broker content-length is invalid".to_string())?;
        } else if name.eq_ignore_ascii_case("origin") {
            has_origin = true;
        }
    }
    if header_end.saturating_add(content_length) > MAX_REQUEST_BYTES {
        return Err("project broker request exceeds the size limit".into());
    }
    while bytes.len() < header_end + content_length {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| format!("read project broker body: {error}"))?;
        if read == 0 {
            return Err("project broker request body ended early".into());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err("project broker request exceeds the size limit".into());
        }
    }

    Ok(HttpRequest {
        method,
        path,
        body: bytes[header_end..header_end + content_length].to_vec(),
        has_origin,
    })
}

fn write_json_response(stream: &mut TcpStream, status: u16, value: &Value) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let body = serde_json::to_vec(value)
        .map_err(|error| format!("encode project broker response: {error}"))?;
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|error| format!("write project broker response: {error}"))
}

fn initialize_project(
    shared: &BrokerShared,
    request: ProjectInitRequest,
) -> Result<ProjectInitEvent, String> {
    let game_id =
        validate_id("gameId", &request.game_id, true)?.expect("required gameId should be present");
    let place_id = validate_id("placeId", &request.place_id, true)?
        .expect("required placeId should be present");
    let mut group_id = validate_optional_id("groupId", request.group_id)?;
    let creator_id = validate_optional_id("creatorId", request.creator_id)?;
    let creator_type = match request.creator_type.as_deref().map(str::trim) {
        None | Some("") => None,
        Some("User") => Some("User".to_string()),
        Some("Group") => Some("Group".to_string()),
        Some(_) => return Err("creatorType must be User or Group".into()),
    };
    if creator_type.is_some() != creator_id.is_some() {
        return Err("creatorType and creatorId must be supplied together".into());
    }
    if creator_type.as_deref() == Some("Group") {
        match (&group_id, &creator_id) {
            (Some(group), Some(creator)) if group != creator => {
                return Err("groupId must match creatorId for a group-owned experience".into())
            }
            (None, Some(creator)) => group_id = Some(creator.clone()),
            _ => {}
        }
    }
    let game_name = validate_display_name("gameName", &request.game_name)?;
    let place_name = validate_display_name("placeName", &request.place_name)?;
    let request_id = sanitize_request_id(&request.request_id, &game_id, &place_id);
    let directory_display_name = preferred_name(&game_name, &place_name, &game_id);

    let (projects_root, projects_root_error) = configured_projects_root(shared);
    let projects_root = projects_root.ok_or_else(|| {
        projects_root_error.unwrap_or_else(|| {
            "Set a Projects folder in Ro Sync Settings before connecting Studio".to_string()
        })
    })?;

    let projects_root =
        storage::open_authorized_directory(&shared.paths.authorized_roots_file, &projects_root)?;
    let (project, reused) =
        find_or_create_project(&projects_root, &directory_display_name, &game_id, &place_id)?;
    let project_path = project.path().to_path_buf();
    let project_name = project_path.file_name().map(OsStr::to_os_string);
    let effective_name = write_project_config(
        &project,
        &ProjectConfigWrite {
            game_name: &game_name,
            place_name: &place_name,
            game_id: &game_id,
            place_id: &place_id,
            group_id: group_id.as_deref(),
            creator_type: creator_type.as_deref(),
            creator_id: creator_id.as_deref(),
        },
    )
    .inspect_err(|_| {
        if !reused {
            if let Some(name) = project_name.as_deref() {
                let _ = projects_root.remove_child_directory_if_matches(name, &project);
            }
        }
    })?;

    Ok(ProjectInitEvent {
        request_id,
        project_path: display_path(&project_path),
        name: effective_name,
        game_name,
        place_name,
        game_id,
        place_id,
        group_id,
        creator_type,
        creator_id,
        reused,
    })
}

fn validate_display_name(label: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(format!(
            "{label} must be 1-256 UTF-8 bytes without control characters"
        ));
    }
    Ok(value.to_string())
}

fn configured_projects_root(shared: &BrokerShared) -> (Option<PathBuf>, Option<String>) {
    let configured = match storage::state_get(&shared.paths.state_file, "state") {
        Ok(state) => state
            .and_then(|value| {
                value
                    .get("projectsRoot")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        Err(error) => {
            return (
                None,
                Some(format!("could not read Ro Sync Settings: {error}")),
            )
        }
    };
    let Some(configured) = configured else {
        return (
            None,
            Some("Set a Projects folder in Ro Sync Settings".into()),
        );
    };
    let root = PathBuf::from(configured);
    if !root.is_absolute() {
        return (
            None,
            Some("The Projects folder in Ro Sync Settings is not absolute".into()),
        );
    }
    let root = match storage::canonicalize_physical_directory(&root) {
        Ok(root) => root,
        Err(error) => {
            return (
                None,
                Some(format!(
                    "The Projects folder in Ro Sync Settings is unavailable: {error}"
                )),
            )
        }
    };
    if let Err(error) = storage::ensure_authorized_path(&shared.paths.authorized_roots_file, &root)
    {
        return (None, Some(error));
    }
    (Some(root), None)
}

fn validate_optional_id(label: &str, value: Option<String>) -> Result<Option<String>, String> {
    match value {
        Some(value) => validate_id(label, &value, false),
        None => Ok(None),
    }
}

fn validate_id(label: &str, value: &str, required: bool) -> Result<Option<String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return if required {
            Err(format!("{label} is required"))
        } else {
            Ok(None)
        };
    }
    if value.len() > 20
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(format!("{label} must be a positive Roblox identifier"));
    }
    Ok(Some(value.trim_start_matches('0').to_string()))
}

fn sanitize_request_id(raw: &str, game_id: &str, place_id: &str) -> String {
    let clean: String = raw
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(96)
        .collect();
    if clean.is_empty() {
        format!("studio-{game_id}-{place_id}")
    } else {
        clean
    }
}

fn preferred_name(game_name: &str, place_name: &str, game_id: &str) -> String {
    for candidate in [game_name, place_name] {
        let candidate = sanitize_folder_name(candidate);
        if !candidate.is_empty() && !candidate.eq_ignore_ascii_case("game") {
            return candidate;
        }
    }
    format!("Roblox {game_id}")
}

fn is_placeholder_project_name(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() || matches!(normalized.as_str(), "game" | "untitled experience") {
        return true;
    }
    normalized.strip_prefix("place").is_some_and(|suffix| {
        suffix.is_empty() || suffix.chars().all(|value| value.is_ascii_digit())
    })
}

fn preferred_project_name(game_name: &str, place_name: &str) -> String {
    for candidate in [game_name, place_name] {
        let candidate = candidate.trim();
        if !is_placeholder_project_name(candidate) {
            return candidate.to_string();
        }
    }
    game_name.trim().to_string()
}

fn sanitize_folder_name(raw: &str) -> String {
    let mut output = String::new();
    let mut pending_space = false;
    for character in raw.trim().chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            if pending_space && !output.is_empty() {
                output.push(' ');
            }
            pending_space = false;
            output.push(character);
        } else if character.is_whitespace() || matches!(character, '.' | '(' | ')' | '[' | ']') {
            pending_space = true;
        }
        if output.len() >= 80 {
            break;
        }
    }
    output.trim_matches([' ', '.', '-']).to_string()
}

fn find_or_create_project(
    projects_root: &storage::PhysicalDirectoryCapability,
    display_name: &str,
    game_id: &str,
    place_id: &str,
) -> Result<(storage::PhysicalDirectoryCapability, bool), String> {
    // One project per place, mirroring the daemon's project_init rules: a
    // same-game directory claiming other places must not be reused — routing
    // a second place into it merges the places, and the place-aware plugin
    // then refuses the daemon and spins on "waiting for the matching daemon".
    // Exact placeIds hit wins; a same-game directory with no recorded places
    // (pre-placeId project) is adoptable; anything else forks a new project.
    let mut existing_names = HashSet::new();
    let mut game_level: Option<storage::PhysicalDirectoryCapability> = None;
    for name in projects_root.entry_names(MAX_PROJECT_ROOT_ENTRIES)? {
        if let Some(name) = name.to_str() {
            existing_names.insert(name.to_ascii_lowercase());
        }
        let Some(project) = projects_root.optional_child_directory(&name)? else {
            continue;
        };
        let Some((cfg_game_id, cfg_place_ids)) = config_identity(&project) else {
            continue;
        };
        if cfg_game_id.as_deref() != Some(game_id) {
            continue;
        }
        if cfg_place_ids.iter().any(|id| id == place_id) {
            return Ok((project, true));
        }
        if cfg_place_ids.is_empty() && game_level.is_none() {
            game_level = Some(project);
        }
    }
    if let Some(project) = game_level {
        return Ok((project, true));
    }

    let base = if display_name.is_empty() {
        format!("Roblox {game_id}")
    } else {
        display_name.to_string()
    };
    for attempt in 0..1000_u16 {
        let name = match attempt {
            0 => base.clone(),
            1 => format!("{base}-{game_id}"),
            2 => format!("{base}-{game_id}-{place_id}"),
            _ => format!("{base}-{game_id}-{place_id}-{attempt}"),
        };
        if existing_names.contains(&name.to_ascii_lowercase()) {
            continue;
        }
        match projects_root.create_child_directory(OsStr::new(&name)) {
            Ok(Some(project)) => return Ok((project, false)),
            Ok(None) => continue,
            Err(error) => return Err(error),
        }
    }
    Err("could not allocate a unique project folder name".into())
}

fn config_identity(
    project: &storage::PhysicalDirectoryCapability,
) -> Option<(Option<String>, Vec<String>)> {
    let text = project
        .read_optional_utf8(OsStr::new("ro-sync.json"), MAX_CONFIG_BYTES)
        .ok()??;
    let value: Value = serde_json::from_str(&text).ok()?;
    let game_id = value
        .get("gameId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let place_ids = value
        .get("placeIds")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some((game_id, place_ids))
}

struct ProjectConfigWrite<'a> {
    game_name: &'a str,
    place_name: &'a str,
    game_id: &'a str,
    place_id: &'a str,
    group_id: Option<&'a str>,
    creator_type: Option<&'a str>,
    creator_id: Option<&'a str>,
}

fn write_project_config(
    project: &storage::PhysicalDirectoryCapability,
    values: &ProjectConfigWrite<'_>,
) -> Result<String, String> {
    let path = project.path().join("ro-sync.json");
    let existing = project.read_optional_utf8(OsStr::new("ro-sync.json"), MAX_CONFIG_BYTES)?;
    let mut config = match existing {
        Some(text) => serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("{} contains invalid JSON: {error}", display_path(&path)))?
            .as_object()
            .cloned()
            .ok_or_else(|| format!("{} must contain a JSON object", display_path(&path)))?,
        None => Map::new(),
    };

    if let Some(existing) = config.get("gameId").and_then(Value::as_str) {
        if existing != values.game_id {
            return Err(format!(
                "{} is already linked to a different game",
                display_path(project.path())
            ));
        }
    }
    let incoming_name = preferred_project_name(values.game_name, values.place_name);
    let effective_name = config
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && !is_placeholder_project_name(name))
        .map(str::to_string)
        .unwrap_or(incoming_name);
    config.insert("name".into(), Value::String(effective_name.clone()));
    config.insert(
        "gameName".into(),
        Value::String(values.game_name.to_string()),
    );
    config.insert("gameId".into(), Value::String(values.game_id.to_string()));
    if !values.place_name.is_empty() {
        config.insert(
            "placeName".into(),
            Value::String(values.place_name.to_string()),
        );
    }
    if let Some(group_id) = values.group_id {
        config.insert("groupId".into(), Value::String(group_id.to_string()));
    }
    if let Some(creator_type) = values.creator_type {
        config.insert(
            "creatorType".into(),
            Value::String(creator_type.to_string()),
        );
    }
    if let Some(creator_id) = values.creator_id {
        config.insert("creatorId".into(), Value::String(creator_id.to_string()));
    }
    let mut place_ids = config
        .get("placeIds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    if !place_ids.iter().any(|value| value == values.place_id) {
        place_ids.push(values.place_id.to_string());
    }
    place_ids.sort();
    place_ids.dedup();
    config.insert(
        "placeIds".into(),
        Value::Array(place_ids.into_iter().map(Value::String).collect()),
    );
    config.entry("version").or_insert(json!(1));
    config
        .entry("AutoReconnect")
        .or_insert(Value::String("on".into()));

    let mut bytes = serde_json::to_vec_pretty(&config)
        .map_err(|error| format!("encode project config: {error}"))?;
    bytes.push(b'\n');
    project.atomic_write(OsStr::new("ro-sync.json"), &bytes, 0o644)?;
    Ok(effective_name)
}

fn enqueue_event(shared: &BrokerShared, event: ProjectInitEvent) {
    if let Ok(mut pending) = shared.pending.lock() {
        pending.retain(|current| current.game_id != event.game_id);
        while pending.len() >= 64 {
            pending.pop_front();
        }
        pending.push_back(event);
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[tauri::command]
pub(crate) fn project_broker_status(state: tauri::State<'_, crate::AppState>) -> Value {
    state.project_broker.status()
}

#[tauri::command]
pub(crate) fn project_init_drain(
    state: tauri::State<'_, crate::AppState>,
) -> Vec<ProjectInitEvent> {
    state.project_broker.drain()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory_capability(path: &Path) -> storage::PhysicalDirectoryCapability {
        storage::open_physical_directory(&path.canonicalize().unwrap()).unwrap()
    }

    fn test_shared(data: &Path) -> BrokerShared {
        BrokerShared {
            paths: AppPaths {
                data_dir: data.to_path_buf(),
                state_file: data.join("state.json"),
                secrets_file: data.join("secrets.json"),
                authorized_roots_file: data.join("authorized-roots.json"),
                daemon_data_dir: data.join("daemon-data"),
                resource_dir: data.join("resources"),
            },
            capability: "a".repeat(64),
            port: Some(BROKER_PORT_START),
            startup_error: None,
            shutdown: AtomicBool::new(false),
            pending: Mutex::new(VecDeque::new()),
        }
    }

    #[test]
    fn hello_exposes_only_an_authorized_configured_projects_root() {
        let data = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let shared = test_shared(data.path());

        let unavailable = broker_hello(&shared);
        assert_eq!(unavailable["pluginProtocol"], PLUGIN_PROTOCOL_VERSION);
        assert_eq!(unavailable["projectInit"]["available"], false);
        assert!(unavailable["projectInit"]["error"].is_string());

        let projects =
            storage::authorize_project_root(&shared.paths.authorized_roots_file, projects.path())
                .unwrap();
        storage::state_set(
            &shared.paths.state_file,
            "state",
            json!({ "projectsRoot": display_path(&projects) }),
        )
        .unwrap();
        let available = broker_hello(&shared);
        assert_eq!(available["projectInit"]["available"], true);
        assert_eq!(
            available["projectInit"]["projectsRoot"],
            display_path(&projects)
        );
        assert_eq!(available["projectInit"]["endpoint"], "/projects/init");
        assert_eq!(available["pluginCapability"], "a".repeat(64));
    }

    #[test]
    fn configured_broker_initializes_and_queues_a_project_event() {
        let data = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let shared = test_shared(data.path());
        let projects =
            storage::authorize_project_root(&shared.paths.authorized_roots_file, projects.path())
                .unwrap();
        storage::state_set(
            &shared.paths.state_file,
            "state",
            json!({ "projectsRoot": display_path(&projects) }),
        )
        .unwrap();

        let event = initialize_project(
            &shared,
            ProjectInitRequest {
                plugin_capability: shared.capability.clone(),
                request_id: "studio-request".into(),
                game_name: "Race Stars".into(),
                place_name: "Main Place".into(),
                game_id: "123".into(),
                place_id: "456".into(),
                group_id: Some("789".into()),
                creator_type: Some("Group".into()),
                creator_id: Some("789".into()),
            },
        )
        .unwrap();
        enqueue_event(&shared, event.clone());

        assert!(Path::new(&event.project_path)
            .join("ro-sync.json")
            .is_file());
        assert_eq!(event.game_id, "123");
        assert_eq!(event.place_id, "456");
        assert_eq!(event.name, "Race Stars");
        assert_eq!(event.game_name, "Race Stars");
        assert_eq!(event.place_name, "Main Place");
        assert_eq!(shared.pending.lock().unwrap().front(), Some(&event));
    }

    #[test]
    fn folder_names_are_portable_and_bounded() {
        assert_eq!(
            sanitize_folder_name("  Race: Stars / Two  "),
            "Race Stars Two"
        );
        assert_eq!(sanitize_folder_name("../../"), "");
        assert!(sanitize_folder_name(&"A".repeat(200)).len() <= 80);
    }

    #[test]
    fn identifiers_are_strict_and_positive() {
        assert_eq!(
            validate_id("gameId", "123", true).unwrap(),
            Some("123".into())
        );
        assert!(validate_id("gameId", "0", true).is_err());
        assert!(validate_id("gameId", "00", true).is_err());
        assert!(validate_id("gameId", "1/2", true).is_err());
        assert_eq!(
            validate_id("gameId", "00123", true).unwrap(),
            Some("123".into())
        );
        assert_eq!(validate_id("groupId", "", false).unwrap(), None);
    }

    #[test]
    fn project_metadata_rejects_incomplete_or_mismatched_creators() {
        let data = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let shared = test_shared(data.path());
        let projects =
            storage::authorize_project_root(&shared.paths.authorized_roots_file, projects.path())
                .unwrap();
        storage::state_set(
            &shared.paths.state_file,
            "state",
            json!({ "projectsRoot": display_path(&projects) }),
        )
        .unwrap();
        let request =
            |creator_type: Option<&str>, creator_id: Option<&str>, group_id: Option<&str>| {
                ProjectInitRequest {
                    plugin_capability: shared.capability.clone(),
                    request_id: String::new(),
                    game_name: "Race Stars".into(),
                    place_name: "Main Place".into(),
                    game_id: "123".into(),
                    place_id: "456".into(),
                    group_id: group_id.map(str::to_string),
                    creator_type: creator_type.map(str::to_string),
                    creator_id: creator_id.map(str::to_string),
                }
            };
        assert!(initialize_project(&shared, request(Some("Group"), None, None)).is_err());
        assert!(
            initialize_project(&shared, request(Some("Group"), Some("789"), Some("790")),).is_err()
        );
        assert!(initialize_project(&shared, request(Some("Team"), Some("789"), None)).is_err());
    }

    #[test]
    fn project_creation_is_idempotent_by_game_id() {
        let directory = tempfile::tempdir().unwrap();
        let projects = test_directory_capability(directory.path());
        let (first, reused) = find_or_create_project(&projects, "Race Stars", "123").unwrap();
        assert!(!reused);
        write_project_config(
            &first,
            &ProjectConfigWrite {
                game_name: "Race Stars",
                place_name: "Main Place",
                game_id: "123",
                place_id: "456",
                group_id: Some("9"),
                creator_type: Some("Group"),
                creator_id: Some("9"),
            },
        )
        .unwrap();
        let (second, reused) = find_or_create_project(&projects, "Renamed", "123").unwrap();
        assert!(reused);
        assert_eq!(first.path(), second.path());
    }

    #[cfg(unix)]
    #[test]
    fn project_reuse_rejects_a_link_to_another_authorized_root() {
        use std::os::unix::fs::symlink;

        let data = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let shared = test_shared(data.path());
        let projects =
            storage::authorize_project_root(&shared.paths.authorized_roots_file, projects.path())
                .unwrap();
        let outside =
            storage::authorize_project_root(&shared.paths.authorized_roots_file, outside.path())
                .unwrap();
        fs::write(
            outside.join("ro-sync.json"),
            br#"{"name":"Outside","gameId":"123","placeIds":["456"]}"#,
        )
        .unwrap();
        symlink(&outside, projects.join("Linked Project")).unwrap();
        storage::state_set(
            &shared.paths.state_file,
            "state",
            json!({ "projectsRoot": display_path(&projects) }),
        )
        .unwrap();

        let result = initialize_project(
            &shared,
            ProjectInitRequest {
                plugin_capability: shared.capability.clone(),
                request_id: "linked-project".into(),
                game_name: "Race Stars".into(),
                place_name: "Main Place".into(),
                game_id: "123".into(),
                place_id: "456".into(),
                group_id: None,
                creator_type: None,
                creator_id: None,
            },
        );
        assert!(result.is_err());
        let outside_config: Value =
            serde_json::from_slice(&fs::read(outside.join("ro-sync.json")).unwrap()).unwrap();
        assert_eq!(outside_config["name"], "Outside");
    }

    #[cfg(unix)]
    #[test]
    fn project_capabilities_survive_root_and_project_path_swaps() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let projects_path = base.join("projects");
        let moved_projects = base.join("moved-projects");
        let outside = base.join("outside");
        fs::create_dir(&projects_path).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("ro-sync.json"), "outside sentinel").unwrap();
        let projects = test_directory_capability(&projects_path);

        fs::rename(&projects_path, &moved_projects).unwrap();
        symlink(&outside, &projects_path).unwrap();
        let (project, reused) = find_or_create_project(&projects, "Race Stars", "123").unwrap();
        assert!(!reused);

        let original_project_path = moved_projects.join("Race Stars");
        let relocated_project = moved_projects.join("relocated-project");
        fs::rename(&original_project_path, &relocated_project).unwrap();
        symlink(&outside, &original_project_path).unwrap();
        write_project_config(
            &project,
            &ProjectConfigWrite {
                game_name: "Race Stars",
                place_name: "Main Place",
                game_id: "123",
                place_id: "456",
                group_id: None,
                creator_type: None,
                creator_id: None,
            },
        )
        .unwrap();

        let value: Value =
            serde_json::from_slice(&fs::read(relocated_project.join("ro-sync.json")).unwrap())
                .unwrap();
        assert_eq!(value["gameId"], "123");
        assert_eq!(
            fs::read_to_string(outside.join("ro-sync.json")).unwrap(),
            "outside sentinel"
        );
    }

    #[test]
    fn colliding_folder_names_receive_a_stable_id_suffix() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("Race Stars")).unwrap();
        let projects = test_directory_capability(directory.path());
        let (created, reused) = find_or_create_project(&projects, "Race Stars", "123").unwrap();
        assert!(!reused);
        assert_eq!(created.path().file_name().unwrap(), "Race Stars-123");
    }

    #[test]
    fn config_merge_preserves_fields_and_adds_places() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("ro-sync.json"),
            br#"{"name":"Old","gameId":"123","placeIds":["1"],"custom":true}"#,
        )
        .unwrap();
        let directory_capability = test_directory_capability(directory.path());
        write_project_config(
            &directory_capability,
            &ProjectConfigWrite {
                game_name: "New",
                place_name: "Second Place",
                game_id: "123",
                place_id: "2",
                group_id: Some("9"),
                creator_type: Some("Group"),
                creator_id: Some("9"),
            },
        )
        .unwrap();
        let value: Value =
            serde_json::from_slice(&fs::read(directory.path().join("ro-sync.json")).unwrap())
                .unwrap();
        assert_eq!(value["custom"], true);
        assert_eq!(value["name"], "Old");
        assert_eq!(value["gameName"], "New");
        assert_eq!(value["placeIds"], json!(["1", "2"]));
        assert_eq!(value["placeName"], "Second Place");
        assert_eq!(value["creatorType"], "Group");
        assert!(value.get("InitialSyncPriority").is_none());
    }

    #[test]
    fn config_merge_upgrades_only_placeholder_display_names() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("ro-sync.json"),
            br#"{"name":"Place1","gameId":"123","placeIds":["1"]}"#,
        )
        .unwrap();
        let directory_capability = test_directory_capability(directory.path());
        let effective = write_project_config(
            &directory_capability,
            &ProjectConfigWrite {
                game_name: "Place1",
                place_name: "Race Stars",
                game_id: "123",
                place_id: "2",
                group_id: None,
                creator_type: None,
                creator_id: None,
            },
        )
        .unwrap();
        assert_eq!(effective, "Race Stars");
        let value: Value =
            serde_json::from_slice(&fs::read(directory.path().join("ro-sync.json")).unwrap())
                .unwrap();
        assert_eq!(value["name"], "Race Stars");
        assert_eq!(value["gameName"], "Place1");
    }

    #[test]
    fn constant_time_comparison_requires_exact_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
