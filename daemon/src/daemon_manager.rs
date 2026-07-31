use super::*;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DaemonLifecycleStatus {
    pub(super) ok: bool,
    pub(super) running: bool,
    /// The recorded listener still owns its port, but its authenticated
    /// `/hello` identity could not be read before the local HTTP deadline.
    /// This is deliberately distinct from a stopped/stale daemon: lifecycle
    /// callers must preserve the record and retry rather than launch a
    /// duplicate process.
    pub(super) unresponsive: bool,
    pub(super) managed: bool,
    pub(super) managed_by: Option<String>,
    pub(super) project: String,
    pub(super) canonical_project: String,
    pub(super) pid: Option<u32>,
    pub(super) port: Option<u16>,
    pub(super) base_url: Option<String>,
    pub(super) boot_id: Option<String>,
    pub(super) log_path: Option<String>,
    pub(super) started_at: Option<u64>,
    pub(super) plugin_connected: Option<bool>,
    pub(super) stale: bool,
    pub(super) externally_managed: bool,
}

pub(super) fn arm_parent_stdin_lease() -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("rosync-parent-stdin-lease".to_string())
        .spawn(|| {
            let stdin = std::io::stdin();
            monitor_parent_stdin(stdin.lock(), || -> () {
                terminate_lifecycle_after_parent_disconnect()
            });
        })?;
    Ok(())
}

pub(super) fn monitor_parent_stdin<R, F>(mut reader: R, on_disconnect: F)
where
    R: std::io::Read,
    F: FnOnce(),
{
    let mut buffer = [0_u8; 64];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    on_disconnect();
}

#[cfg(unix)]
pub(super) fn terminate_lifecycle_after_parent_disconnect() -> ! {
    // SAFETY: `_exit` immediately terminates this short-lived lifecycle
    // process without running locks or cleanup handlers that may be blocked on
    // another thread. The OS releases its start-lock and pipe handles.
    unsafe { libc::_exit(1) }
}

#[cfg(not(unix))]
pub(super) fn terminate_lifecycle_after_parent_disconnect() -> ! {
    std::process::exit(1)
}

pub(super) async fn run_daemon(args: DaemonArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        DaemonCommand::Start(args) => {
            let raw = args.raw;
            let status = daemon_start(args).await?;
            print_daemon_status(&status, raw)?;
        }
        DaemonCommand::Status(args) => {
            if args.parent_stdin_lease {
                arm_parent_stdin_lease()?;
            }
            let canonical_project =
                lifecycle::canonical_project(&args.project).map_err(|error| {
                    format!(
                        "daemon status: canonicalize {}: {error}",
                        args.project.display()
                    )
                })?;
            let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
            let status = daemon_status(&canonical_project, &paths, true)?;
            print_daemon_status(&status, args.raw)?;
        }
        DaemonCommand::Stop(args) => {
            validate_lifecycle_timeout(args.timeout, "daemon stop")?;
            let canonical_project =
                lifecycle::canonical_project(&args.project).map_err(|error| {
                    format!(
                        "daemon stop: canonicalize {}: {error}",
                        args.project.display()
                    )
                })?;
            let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
            let status = daemon_stop(
                &canonical_project,
                &paths,
                Duration::from_secs_f64(args.timeout),
            )
            .await?;
            print_daemon_status(&status, args.raw)?;
        }
        DaemonCommand::Restart(args) => {
            validate_lifecycle_timeout(args.timeout, "daemon restart")?;
            let canonical_project =
                lifecycle::canonical_project(&args.project).map_err(|error| {
                    format!(
                        "daemon restart: canonicalize {}: {error}",
                        args.project.display()
                    )
                })?;
            let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
            let previous_port = lifecycle::read_record(&paths.record)?.map(|record| record.port);
            let existing = daemon_status(&canonical_project, &paths, false)?;
            if existing.running || existing.stale {
                daemon_stop(
                    &canonical_project,
                    &paths,
                    Duration::from_secs_f64(args.timeout),
                )
                .await?;
            }
            let raw = args.raw;
            let status = daemon_start(DaemonStartArgs {
                project: canonical_project,
                port: args.port.or(previous_port),
                managed_by: args.managed_by,
                owner_token: args.owner_token,
                owner_token_env: args.owner_token_env,
                game_id: args.game_id,
                group_id: args.group_id,
                place_id: args.place_id,
                projects_root: args.projects_root,
                data_dir: args.data_dir,
                timeout: args.timeout,
                parent_stdin_lease: false,
                raw,
            })
            .await?;
            print_daemon_status(&status, raw)?;
        }
        DaemonCommand::Logs(args) => daemon_logs(args).await?,
    }
    Ok(())
}

pub(super) fn daemon_runtime_paths(
    data_dir: Option<&std::path::Path>,
    canonical_project: &std::path::Path,
) -> Result<lifecycle::RuntimePaths, Box<dyn std::error::Error>> {
    let state_dir = lifecycle::state_dir(data_dir)
        .map_err(|error| format!("resolve Ro Sync state directory: {error}"))?;
    Ok(lifecycle::runtime_paths(state_dir, canonical_project))
}

pub(super) fn validate_lifecycle_timeout(
    timeout: f64,
    context: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !timeout.is_finite() || !(0.1..=300.0).contains(&timeout) {
        return Err(format!("{context}: --timeout must be between 0.1 and 300 seconds").into());
    }
    Ok(())
}

pub(super) fn read_named_secret_env(
    name: &str,
    context: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!("{context}: invalid environment variable name").into());
    }
    let value = std::env::var(name)
        .map_err(|_| format!("{context}: environment variable {name} is missing or not UTF-8"))?;
    if value.is_empty() {
        return Err(format!("{context}: environment variable {name} is empty").into());
    }
    Ok(value)
}

pub(super) fn resolve_optional_secret(
    direct: Option<String>,
    env_name: Option<&str>,
    context: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match (direct, env_name) {
        (Some(value), None) if !value.is_empty() => Ok(Some(value)),
        (Some(_), None) => Err(format!("{context}: secret cannot be empty").into()),
        (None, Some(name)) => Ok(Some(read_named_secret_env(name, context)?)),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(format!("{context}: choose one secret source").into()),
    }
}

pub(super) fn read_widget_owner_token_state_file(
    path: &std::path::Path,
) -> Result<String, Box<dyn std::error::Error>> {
    const MAX_WIDGET_STATE_BYTES: u64 = 4 * 1024 * 1024;
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "serve widget owner token: inspect Terminal 64 state file {}: {error}",
            path.display()
        )
    })?;
    if metadata.len() > MAX_WIDGET_STATE_BYTES {
        return Err(format!(
            "serve widget owner token: Terminal 64 state file {} exceeds the 4 MiB limit",
            path.display()
        )
        .into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "serve widget owner token: Terminal 64 state file {} must have mode 0600",
                path.display()
            )
            .into());
        }
    }
    let bytes = std::fs::read(path).map_err(|error| {
        format!(
            "serve widget owner token: read Terminal 64 state file {}: {error}",
            path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "serve widget owner token: parse Terminal 64 state file {}: {error}",
            path.display()
        )
    })?;
    let token = value
        .get("state")
        .and_then(|state| state.get("daemonOwnerToken"))
        .and_then(serde_json::Value::as_str)
        .filter(|token| {
            (16..=512).contains(&token.len())
                && token.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
                })
        })
        .ok_or_else(|| {
            format!(
                "serve widget owner token: {} has no valid state.daemonOwnerToken",
                path.display()
            )
        })?;
    Ok(token.to_owned())
}

pub(super) fn resolve_widget_owner_token(
    direct: Option<String>,
    state_file: Option<&std::path::Path>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match (direct, state_file) {
        (Some(value), None) if !value.is_empty() => Ok(Some(value)),
        (Some(_), None) => Err("serve widget owner token cannot be empty".into()),
        (None, Some(path)) => read_widget_owner_token_state_file(path).map(Some),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err("serve widget owner token: choose one secret source".into()),
    }
}

pub(super) fn normalize_optional_metadata(
    value: Option<&str>,
    flag: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match value {
        Some(value) if value.trim().is_empty() => Err(format!("{flag} cannot be empty").into()),
        Some(value) => Ok(Some(value.trim().to_string())),
        None => Ok(None),
    }
}

pub(super) fn persist_daemon_start_metadata(
    canonical_project: &std::path::Path,
    game_id: Option<String>,
    group_id: Option<String>,
    place_ids: Option<Vec<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if game_id.is_none() && group_id.is_none() && place_ids.is_none() {
        return Ok(());
    }
    let mut config = project_config::load_or_create(canonical_project)
        .map_err(|error| format!("daemon start: load ro-sync.json: {error}"))?;
    if project_config::apply_overrides(&mut config, game_id, group_id, place_ids) {
        project_config::write(canonical_project, &config)
            .map_err(|error| format!("daemon start: write ro-sync.json: {error}"))?;
    }
    Ok(())
}

pub(super) fn validate_existing_daemon_owner(
    record: &lifecycle::RuntimeRecord,
    supplied_owner_token: Option<&str>,
) -> Result<(), &'static str> {
    let Some(supplied) = supplied_owner_token else {
        return Ok(());
    };
    let expected = record.control_token.as_bytes();
    let supplied = supplied.as_bytes();
    if expected.len() != supplied.len() {
        return Err("matching managed daemon is owned by a different lifecycle capability");
    }
    let difference = expected
        .iter()
        .zip(supplied)
        .fold(0_u8, |difference, (expected, supplied)| {
            difference | (expected ^ supplied)
        });
    if difference == 0 {
        Ok(())
    } else {
        Err("matching managed daemon is owned by a different lifecycle capability")
    }
}

pub(super) fn classify_running_daemon_for_manager(
    status: &mut DaemonLifecycleStatus,
    requested_manager: &str,
) {
    if status.running && status.managed && status.managed_by.as_deref() != Some(requested_manager) {
        // A live daemon owned by another manager is useful discovery, not an
        // ownership failure. Do not compare the caller's secret with the other
        // manager's runtime capability, and expose only ordinary /hello state.
        status.externally_managed = true;
        status.log_path = None;
    }
}

pub(super) fn daemon_record_matches_hello(
    record: &lifecycle::RuntimeRecord,
    hello: &serde_json::Value,
    canonical_project: &std::path::Path,
) -> bool {
    daemon_hello_matches_project(hello, canonical_project)
        && hello.get("bootId").and_then(serde_json::Value::as_str) == Some(record.boot_id.as_str())
        && hello.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(record.pid))
        && hello.get("port").and_then(serde_json::Value::as_u64) == Some(u64::from(record.port))
}

pub(super) fn daemon_status_from_record(
    record: &lifecycle::RuntimeRecord,
    hello: Option<&serde_json::Value>,
    running: bool,
    stale: bool,
) -> DaemonLifecycleStatus {
    DaemonLifecycleStatus {
        ok: true,
        running,
        unresponsive: false,
        managed: true,
        managed_by: Some(record.managed_by.clone()),
        project: record.project.clone(),
        canonical_project: record.canonical_project.clone(),
        pid: Some(record.pid),
        port: Some(record.port),
        base_url: Some(format!("http://127.0.0.1:{}", record.port)),
        boot_id: Some(record.boot_id.clone()),
        log_path: Some(record.log_path.clone()),
        started_at: Some(record.started_at),
        plugin_connected: hello.and_then(|value| {
            value
                .get("pluginConnected")
                .and_then(serde_json::Value::as_bool)
        }),
        stale,
        externally_managed: false,
    }
}

pub(super) fn unresponsive_daemon_status(
    record: &lifecycle::RuntimeRecord,
    error: &str,
) -> DaemonLifecycleStatus {
    let mut status = daemon_status_from_record(record, None, true, false);
    status.unresponsive = true;
    // Keep the transport detail in the process log/human diagnostic rather
    // than the lifecycle JSON. The typed boolean is sufficient for callers
    // and avoids turning unstable OS error strings into an API contract.
    eprintln!(
        "Ro Sync daemon boot {} on port {} is unresponsive: {error}",
        record.boot_id, record.port
    );
    status
}

pub(super) fn external_daemon_status(
    canonical_project: &std::path::Path,
    port: u16,
    hello: &serde_json::Value,
) -> DaemonLifecycleStatus {
    let managed = hello
        .get("managed")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            hello
                .get("widgetOwned")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false);
    DaemonLifecycleStatus {
        ok: true,
        running: true,
        unresponsive: false,
        managed,
        managed_by: hello
            .get("managedBy")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        project: hello
            .get("project")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| canonical_project.to_str().unwrap_or(""))
            .to_string(),
        canonical_project: canonical_project.display().to_string(),
        pid: hello
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
        port: Some(port),
        base_url: Some(format!("http://127.0.0.1:{port}")),
        boot_id: hello
            .get("bootId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        log_path: None,
        started_at: hello.get("startedAt").and_then(serde_json::Value::as_u64),
        plugin_connected: hello
            .get("pluginConnected")
            .and_then(serde_json::Value::as_bool),
        stale: false,
        externally_managed: true,
    }
}

pub(super) fn matching_external_daemon_status(
    canonical_project: &std::path::Path,
    port: u16,
    hello: &serde_json::Value,
) -> Option<DaemonLifecycleStatus> {
    daemon_hello_matches_project(hello, canonical_project)
        .then(|| external_daemon_status(canonical_project, port, hello))
}

pub(super) fn stopped_daemon_status(canonical_project: &std::path::Path) -> DaemonLifecycleStatus {
    DaemonLifecycleStatus {
        ok: true,
        running: false,
        unresponsive: false,
        managed: false,
        managed_by: None,
        project: canonical_project.display().to_string(),
        canonical_project: canonical_project.display().to_string(),
        pid: None,
        port: None,
        base_url: None,
        boot_id: None,
        log_path: None,
        started_at: None,
        plugin_connected: None,
        stale: false,
        externally_managed: false,
    }
}

pub(super) enum ManagedRecordProbe {
    Exact(serde_json::Value),
    Different(serde_json::Value),
    Unresponsive(String),
    Stale(String),
}

pub(super) fn probe_managed_record(
    record: &lifecycle::RuntimeRecord,
    canonical_project: &std::path::Path,
) -> ManagedRecordProbe {
    match fetch_daemon_hello(record.port) {
        Ok(hello) if daemon_record_matches_hello(record, &hello, canonical_project) => {
            ManagedRecordProbe::Exact(hello)
        }
        Ok(hello) => ManagedRecordProbe::Different(hello),
        Err(error) => {
            // A successful bind is positive evidence that no listener owns
            // the recorded port. Any bind failure is ambiguous (the exact
            // daemon may merely be busy, or a foreign process may now own the
            // port), so preserve the capability-bearing runtime record.
            match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, record.port)) {
                Ok(listener) => {
                    drop(listener);
                    ManagedRecordProbe::Stale(error)
                }
                Err(_) => ManagedRecordProbe::Unresponsive(error),
            }
        }
    }
}

pub(super) fn find_daemon_for_project_in_range(
    canonical_project: &std::path::Path,
    ports: std::ops::RangeInclusive<u16>,
) -> Option<(u16, serde_json::Value)> {
    ports.into_iter().find_map(|port| {
        let hello = fetch_daemon_hello(port).ok()?;
        daemon_hello_matches_project(&hello, canonical_project).then_some((port, hello))
    })
}

pub(super) fn find_daemon_for_project(
    canonical_project: &std::path::Path,
) -> Option<(u16, serde_json::Value)> {
    find_daemon_for_project_in_range(
        canonical_project,
        DEFAULT_DAEMON_PORT..=DAEMON_PORT_SCAN_MAX,
    )
}

pub(super) fn daemon_status(
    canonical_project: &std::path::Path,
    paths: &lifecycle::RuntimePaths,
    clean_stale: bool,
) -> Result<DaemonLifecycleStatus, Box<dyn std::error::Error>> {
    if let Some(record) = lifecycle::read_record(&paths.record)? {
        if canonicalize_project_path(std::path::Path::new(&record.canonical_project))
            != canonical_project
        {
            return Err(format!(
                "daemon runtime record {} belongs to {}, not {}",
                paths.record.display(),
                record.canonical_project,
                canonical_project.display()
            )
            .into());
        }
        let probe = probe_managed_record(&record, canonical_project);
        if let ManagedRecordProbe::Exact(hello) = &probe {
            return Ok(daemon_status_from_record(&record, Some(hello), true, false));
        }
        if let ManagedRecordProbe::Unresponsive(error) = &probe {
            return Ok(unresponsive_daemon_status(&record, error));
        }
        if clean_stale {
            lifecycle::remove_record_if_boot(&paths.record, &record.boot_id)?;
        }

        // A stale record only proves that its exact boot is gone. A manual
        // daemon or a daemon adopted by another host may still be serving the
        // same project, either on the recorded port or elsewhere in the
        // discovery range. Prefer that live identity so `daemon start` stays
        // idempotent instead of launching a duplicate.
        if let ManagedRecordProbe::Different(hello) = &probe {
            if let Some(status) =
                matching_external_daemon_status(canonical_project, record.port, hello)
            {
                return Ok(status);
            }
        }
        if let Some((port, hello)) = find_daemon_for_project(canonical_project) {
            return Ok(external_daemon_status(canonical_project, port, &hello));
        }
        let hello = match probe {
            ManagedRecordProbe::Different(hello) => Some(hello),
            ManagedRecordProbe::Stale(error) => {
                eprintln!(
                    "Ro Sync daemon boot {} on port {} is stale: {error}",
                    record.boot_id, record.port
                );
                None
            }
            ManagedRecordProbe::Exact(_) | ManagedRecordProbe::Unresponsive(_) => unreachable!(),
        };
        return Ok(daemon_status_from_record(
            &record,
            hello.as_ref(),
            false,
            true,
        ));
    }

    if let Some((port, hello)) = find_daemon_for_project(canonical_project) {
        return Ok(external_daemon_status(canonical_project, port, &hello));
    }
    Ok(stopped_daemon_status(canonical_project))
}

pub(super) async fn daemon_start(
    args: DaemonStartArgs,
) -> Result<DaemonLifecycleStatus, Box<dyn std::error::Error>> {
    if args.parent_stdin_lease {
        arm_parent_stdin_lease()?;
    }
    validate_lifecycle_timeout(args.timeout, "daemon start")?;
    let timeout = Duration::from_secs_f64(args.timeout);
    if args.managed_by.trim().is_empty() {
        return Err("daemon start: --managed-by cannot be empty".into());
    }
    let supplied_owner_token = resolve_optional_secret(
        args.owner_token.clone(),
        args.owner_token_env.as_deref(),
        "daemon start owner token",
    )?;
    let canonical_project = lifecycle::canonical_project(&args.project).map_err(|error| {
        format!(
            "daemon start: canonicalize {}: {error}",
            args.project.display()
        )
    })?;
    let projects_root = args
        .projects_root
        .as_deref()
        .map(project_init::resolve_projects_root)
        .transpose()
        .map_err(|error| format!("daemon start: {error}"))?;
    let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
    let _lock = lifecycle::StartLock::acquire(&paths.start_lock)?;

    let game_id = normalize_optional_metadata(args.game_id.as_deref(), "--game-id")?;
    let group_id = normalize_optional_metadata(args.group_id.as_deref(), "--group-id")?;
    let place_ids = if args.place_id.is_empty() {
        None
    } else {
        Some(
            args.place_id
                .iter()
                .map(|value| {
                    normalize_optional_metadata(Some(value), "--place-id")?
                        .ok_or_else(|| "--place-id cannot be empty".into())
                })
                .collect::<Result<Vec<String>, Box<dyn std::error::Error>>>()?,
        )
    };
    let mut current = daemon_status(&canonical_project, &paths, true)?;
    classify_running_daemon_for_manager(&mut current, args.managed_by.trim());
    if current.unresponsive {
        return Err(format!(
            "daemon start: managed daemon on port {} is unresponsive; its runtime record was preserved and no duplicate was launched",
            current.port.unwrap_or_default()
        )
        .into());
    }
    if current.running {
        if current.externally_managed {
            return Ok(current);
        }
        // An idempotent start is only idempotent for the same lifecycle
        // capability. Returning an existing boot to a caller that supplied a
        // different token makes that caller believe it owns a daemon it can
        // neither authenticate nor safely stop.
        if !current.externally_managed {
            let record = lifecycle::read_record(&paths.record)?.ok_or_else(|| {
                format!(
                    "daemon start: managed daemon is running but runtime record {} is missing",
                    paths.record.display()
                )
            })?;
            validate_existing_daemon_owner(&record, supplied_owner_token.as_deref())
                .map_err(|error| format!("daemon start: {error}"))?;
        }
        if let Some(requested_root) = projects_root.as_deref() {
            let advertised_root = current
                .port
                .and_then(|port| fetch_daemon_hello(port).ok())
                .and_then(|hello| {
                    hello
                        .pointer("/projectInit/projectsRoot")
                        .and_then(serde_json::Value::as_str)
                        .map(PathBuf::from)
                });
            if advertised_root.as_deref() != Some(requested_root) {
                return Err(format!(
                    "daemon start: the matching daemon is already running without the requested projects root {}; restart it to enable Studio project creation",
                    requested_root.display()
                )
                .into());
            }
        }
        if let Some(requested_port) = args.port {
            if current.port != Some(requested_port) {
                if let Ok(hello) = fetch_daemon_hello(requested_port) {
                    let daemon_project = hello
                        .get("project")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown project");
                    return Err(format!(
                        "daemon start: requested port {requested_port} serves {daemon_project}; matching project already runs on port {}",
                        current.port.unwrap_or_default()
                    )
                    .into());
                }
            }
        }
        // Metadata belongs to the lifecycle owner just as shutdown authority
        // does. Defer disk writes until the exact live boot has accepted this
        // caller's capability and all idempotent-start checks will succeed.
        persist_daemon_start_metadata(&canonical_project, game_id, group_id, place_ids)?;
        return Ok(current);
    }

    if let Some(requested_port) = args.port {
        if let Ok(hello) = fetch_daemon_hello(requested_port) {
            if let Some(status) =
                matching_external_daemon_status(&canonical_project, requested_port, &hello)
            {
                return Ok(status);
            }
            let daemon_project = hello
                .get("project")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown project");
            return Err(format!(
                "daemon start: port {requested_port} is already serving {daemon_project}, not {}",
                canonical_project.display()
            )
            .into());
        }
    }

    // Project start locks are intentionally independent, but TCP ports are
    // process-global. Serialize selection through the shared daemon state
    // directory and hold this lock until the child has bound and completed its
    // exact boot handshake. Without this guard, two different projects can
    // both probe the same fallback port before either child starts.
    let port_lock_path = daemon_port_allocation_lock_path(&paths)?;
    let _port_lock = acquire_daemon_port_allocation_lock(&port_lock_path, timeout)
        .await
        .map_err(|error| format!("daemon start: {error}"))?;

    // No matching live daemon exists after the locked status/port probes, so
    // these explicit launch overrides can safely seed the process about to be
    // created. A foreign live daemon always returned above without mutation.
    persist_daemon_start_metadata(&canonical_project, game_id, group_id, place_ids)?;

    let port = match args.port {
        Some(0) => reserve_ephemeral_port()?,
        Some(port) => {
            ensure_daemon_port_available(port)?;
            port
        }
        None => find_available_daemon_port().ok_or_else(|| {
            format!(
                "daemon start: no available port in {DEFAULT_DAEMON_PORT}-{DAEMON_PORT_SCAN_MAX}"
            )
        })?,
    };
    let control_token = match supplied_owner_token {
        Some(token) => token,
        None => artifact::random_hex(32)?,
    };
    let boot_id = artifact::random_hex(32)?;
    let started_at = unix_secs();
    spawn_managed_daemon(ManagedDaemonLaunch {
        canonical_project: &canonical_project,
        paths: &paths,
        port,
        managed_by: &args.managed_by,
        control_token: &control_token,
        boot_id: &boot_id,
        started_at,
        timeout,
        owner_token_env: args.owner_token_env.as_deref(),
        projects_root: projects_root.as_deref(),
    })
    .await
}

pub(super) fn daemon_port_allocation_lock_path(
    paths: &lifecycle::RuntimePaths,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let daemon_dir = paths
        .start_lock
        .parent()
        .ok_or("daemon start: invalid per-project lock path")?;
    Ok(daemon_dir.join("ports.start.lock"))
}

pub(super) async fn acquire_daemon_port_allocation_lock(
    path: &std::path::Path,
    timeout: Duration,
) -> std::io::Result<lifecycle::StartLock> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon port-allocation timeout overflow",
        )
    })?;
    loop {
        match lifecycle::StartLock::acquire_named(path, "daemon port allocation") {
            Ok(lock) => return Ok(lock),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for another daemon to finish port allocation (lock {})",
                        path.display()
                    ),
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn ensure_daemon_port_available(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .map_err(|error| format!("daemon start: requested port {port} is unavailable: {error}"))?;
    drop(listener);
    Ok(())
}

pub(super) fn reserve_ephemeral_port() -> Result<u16, Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

pub(super) fn find_available_daemon_port() -> Option<u16> {
    (DEFAULT_DAEMON_PORT..=DAEMON_PORT_SCAN_MAX)
        .find(|port| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port)).is_ok())
}

pub(super) struct ManagedDaemonLaunch<'a> {
    canonical_project: &'a std::path::Path,
    paths: &'a lifecycle::RuntimePaths,
    port: u16,
    managed_by: &'a str,
    control_token: &'a str,
    boot_id: &'a str,
    started_at: u64,
    timeout: Duration,
    owner_token_env: Option<&'a str>,
    projects_root: Option<&'a std::path::Path>,
}

pub(super) async fn spawn_managed_daemon(
    launch: ManagedDaemonLaunch<'_>,
) -> Result<DaemonLifecycleStatus, Box<dyn std::error::Error>> {
    let ManagedDaemonLaunch {
        canonical_project,
        paths,
        port,
        managed_by,
        control_token,
        boot_id,
        started_at,
        timeout,
        owner_token_env,
        projects_root,
    } = launch;
    let executable = std::env::current_exe()?;
    let stdout = lifecycle::open_private_log(&paths.log)?;
    // The managed log intentionally survives daemon restarts, but startup
    // diagnostics must describe only this child. Remember where this attempt
    // begins so an old successful "listening" line or an earlier validation
    // failure cannot be repeated in the lifecycle error shown by the UI.
    let log_start_offset = stdout.metadata()?.len();
    let stderr = stdout.try_clone()?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("serve")
        .arg("--project")
        .arg(canonical_project)
        .arg("--port")
        .arg(port.to_string())
        .arg("--managed")
        .arg("--managed-by")
        .arg(managed_by)
        .arg("--control-token-env")
        .arg("ROSYNC_DAEMON_CONTROL_TOKEN")
        .arg("--boot-id")
        .arg(boot_id)
        .arg("--runtime-record")
        .arg(&paths.record)
        .arg("--log-path")
        .arg(&paths.log)
        .arg("--started-at")
        .arg(started_at.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(projects_root) = projects_root {
        command.arg("--projects-root").arg(projects_root);
    }
    // The short-lived lifecycle process receives the manager secret through
    // an environment variable, but the long-lived daemon needs only its
    // dedicated control-token copy. Do not retain or propagate the source
    // variable into daemon-launched tools.
    command.env_remove("ROSYNC_OWNER_TOKEN");
    if let Some(owner_token_env) = owner_token_env {
        command.env_remove(owner_token_env);
    }
    command.env("ROSYNC_DAEMON_CONTROL_TOKEN", control_token);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `setsid` is async-signal-safe and the closure performs no
        // allocation or shared-memory access between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = command.spawn().map_err(|error| {
        format!(
            "daemon start: launch managed daemon for {}: {error}",
            canonical_project.display()
        )
    })?;
    let child_pid = child.id();
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("daemon start: timeout overflow")?;
    loop {
        if let Ok(hello) = fetch_daemon_hello(port) {
            let expected = lifecycle::RuntimeRecord {
                version: lifecycle::RUNTIME_RECORD_VERSION,
                project: canonical_project.display().to_string(),
                canonical_project: canonical_project.display().to_string(),
                pid: child_pid,
                port,
                boot_id: boot_id.to_string(),
                control_token: control_token.to_string(),
                managed_by: managed_by.to_string(),
                log_path: paths.log.display().to_string(),
                started_at,
            };
            if daemon_record_matches_hello(&expected, &hello, canonical_project) {
                let record = lifecycle::read_record(&paths.record)?.ok_or_else(|| {
                    format!(
                        "daemon start: exact daemon answered but runtime record {} is missing",
                        paths.record.display()
                    )
                })?;
                if record.boot_id != boot_id || record.pid != child_pid || record.port != port {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        "daemon start: runtime record did not match the spawned process".into(),
                    );
                }
                return Ok(daemon_status_from_record(
                    &record,
                    Some(&hello),
                    true,
                    false,
                ));
            }
        }
        if let Some(exit) = child.try_wait()? {
            lifecycle::remove_record_if_boot(&paths.record, boot_id)?;
            let tail = read_log_tail_from(&paths.log, log_start_offset, 20).unwrap_or_default();
            return Err(managed_start_exit_message(child_pid, &exit.to_string(), &tail).into());
        }
        if Instant::now() >= deadline {
            // This is the exact child handle created above, never a PID read
            // from disk. It is therefore safe to terminate on failed startup.
            let _ = child.kill();
            let _ = child.wait();
            lifecycle::remove_record_if_boot(&paths.record, boot_id)?;
            let tail = read_log_tail_from(&paths.log, log_start_offset, 20).unwrap_or_default();
            return Err(format!(
                "daemon start: timed out waiting for project/boot handshake on port {port}{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\n{tail}")
                }
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) fn managed_start_exit_message(child_pid: u32, exit: &str, tail: &str) -> String {
    // Normal validation failures are already rendered by the child as
    // `Error: <actionable detail>`. Surface that detail directly instead of
    // leading with process/handshake internals that obscure the actual fix.
    if let Some(detail) = tail
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("Error: "))
        .filter(|detail| !detail.is_empty())
    {
        return format!("daemon start: {detail}");
    }
    format!(
        "daemon start: child {child_pid} exited with {exit} before the exact handshake{}",
        if tail.is_empty() {
            String::new()
        } else {
            format!("\n{tail}")
        }
    )
}

pub(super) fn managed_daemon_close_request(
    record: &lifecycle::RuntimeRecord,
    reason: &str,
) -> serde_json::Value {
    serde_json::json!({
        "token": record.control_token,
        "reason": reason,
        "expectedBootId": record.boot_id,
        "expectedPid": record.pid,
        "expectedPort": record.port,
        "expectedCanonicalProject": record.canonical_project,
    })
}

pub(super) async fn daemon_stop(
    canonical_project: &std::path::Path,
    paths: &lifecycle::RuntimePaths,
    timeout: Duration,
) -> Result<DaemonLifecycleStatus, Box<dyn std::error::Error>> {
    let Some(record) = lifecycle::read_record(&paths.record)? else {
        if let Some((port, _)) = find_daemon_for_project(canonical_project) {
            return Err(format!(
                "daemon stop: project is running on port {port}, but no matching managed runtime record exists; refusing PID-only or unauthenticated shutdown"
            )
            .into());
        }
        return Ok(stopped_daemon_status(canonical_project));
    };
    if canonicalize_project_path(std::path::Path::new(&record.canonical_project))
        != canonical_project
    {
        return Err(format!(
            "daemon stop: runtime record belongs to {}, not {}",
            record.canonical_project,
            canonical_project.display()
        )
        .into());
    }
    let _hello = match probe_managed_record(&record, canonical_project) {
        ManagedRecordProbe::Exact(hello) => hello,
        ManagedRecordProbe::Unresponsive(error) => {
            return Err(format!(
                "daemon stop: managed daemon on port {} is unresponsive ({error}); its runtime record was preserved",
                record.port
            )
            .into())
        }
        ManagedRecordProbe::Different(hello) => {
            lifecycle::remove_record_if_boot(&paths.record, &record.boot_id)?;
            return Ok(daemon_status_from_record(
                &record,
                Some(&hello),
                false,
                true,
            ));
        }
        ManagedRecordProbe::Stale(_) => {
            lifecycle::remove_record_if_boot(&paths.record, &record.boot_id)?;
            return Ok(daemon_status_from_record(&record, None, false, true));
        }
    };

    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("daemon stop: timeout overflow")?;
    let response = http_post_json_until(
        record.port,
        "/manager-close",
        &managed_daemon_close_request(&record, "managed daemon stop requested"),
        deadline,
    )
    .await
    .map_err(|error| format!("daemon stop: authenticated shutdown request: {error}"))?;
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!("daemon stop: daemon rejected shutdown: {response}").into());
    }

    loop {
        let exact_still_running = matches!(
            probe_managed_record(&record, canonical_project),
            ManagedRecordProbe::Exact(_) | ManagedRecordProbe::Unresponsive(_)
        );
        if !exact_still_running {
            lifecycle::remove_record_if_boot(&paths.record, &record.boot_id)?;
            let mut status = daemon_status_from_record(&record, None, false, false);
            status.plugin_connected = response
                .get("pluginConnected")
                .and_then(serde_json::Value::as_bool);
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "daemon stop: timed out waiting for boot {} on port {} to stop; no PID signal was sent",
                record.boot_id, record.port
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) fn print_daemon_status(
    status: &DaemonLifecycleStatus,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if raw {
        println!("{}", serde_json::to_string(status)?);
        return Ok(());
    }
    if status.running {
        if status.unresponsive {
            println!(
                "Ro Sync has an unresponsive managed daemon for {} on {}; its runtime state was preserved.",
                status.canonical_project,
                status
                    .base_url
                    .as_deref()
                    .unwrap_or("the recorded local port")
            );
            return Ok(());
        }
        let ownership = if status.externally_managed {
            "external"
        } else if status.managed {
            "managed"
        } else {
            "manual"
        };
        println!(
            "Ro Sync is running for {} on {} ({ownership}).",
            status.canonical_project,
            status.base_url.as_deref().unwrap_or("unknown address")
        );
    } else if status.stale {
        println!(
            "Ro Sync is not running for {} (removed stale runtime state).",
            status.canonical_project
        );
    } else {
        println!("Ro Sync is not running for {}.", status.canonical_project);
    }
    Ok(())
}

pub(super) fn read_log_tail(path: &std::path::Path, lines: usize) -> std::io::Result<String> {
    read_log_tail_from(path, 0, lines)
}

pub(super) fn read_log_tail_from(
    path: &std::path::Path,
    start_offset: u64,
    lines: usize,
) -> std::io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    // A rotated/truncated log invalidates the old offset. In that case the
    // replacement file contains only newer diagnostics, so read it from zero.
    let effective_offset = if length < start_offset {
        0
    } else {
        start_offset
    };
    file.seek(SeekFrom::Start(effective_offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut selected = text.lines().rev().take(lines).collect::<Vec<_>>();
    selected.reverse();
    Ok(selected.join("\n"))
}

pub(super) async fn daemon_logs(args: DaemonLogsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let canonical_project = lifecycle::canonical_project(&args.project).map_err(|error| {
        format!(
            "daemon logs: canonicalize {}: {error}",
            args.project.display()
        )
    })?;
    let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
    let path = lifecycle::read_record(&paths.record)?
        .map(|record| PathBuf::from(record.log_path))
        .unwrap_or(paths.log);
    let content = match read_log_tail(&path, args.lines) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("daemon logs: read {}: {error}", path.display()).into()),
    };
    if args.raw {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "lines": args.lines,
                "content": content,
            }))?
        );
        return Ok(());
    }
    if !content.is_empty() {
        println!("{content}");
    }
    if !args.follow {
        return Ok(());
    }

    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|error| format!("daemon logs: follow {}: {error}", path.display()))?;
    let mut offset = file.metadata()?.len();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                let length = file.metadata()?.len();
                if length < offset {
                    offset = 0;
                }
                if length > offset {
                    file.seek(SeekFrom::Start(offset))?;
                    let mut appended = String::new();
                    file.read_to_string(&mut appended)?;
                    offset = length;
                    print!("{appended}");
                    std::io::stdout().flush()?;
                }
            }
        }
    }
    Ok(())
}
