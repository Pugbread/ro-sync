use std::{
    env, io,
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::Duration,
};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_shell::ShellExt;
use wait_timeout::ChildExt;

use crate::{
    resources::{display_path, resolve_with_tauri, ResourcePaths},
    secrets::SecretStore,
    storage::{
        atomic_write, atomic_write_authorized, read_authorized_utf8_file, read_utf8_file,
        validate_project_file_path, MAX_PROJECT_FILE_BYTES,
    },
    AppState,
};

const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;
const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
const ROBLOX_CLOUD_SECRET_KEY: &str = "robloxCloudApiKey";
const ROBLOX_CLOUD_SECRET_ENV: &str = "ROSYNC_OPEN_CLOUD_CREDENTIAL";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppInfo {
    kind: &'static str,
    version: String,
    build_commit: &'static str,
    build_dirty: bool,
    target: &'static str,
    platform: &'static str,
    resource_dir: String,
    data_dir: String,
    daemon_path: Option<String>,
    plugin_path: Option<String>,
    plugin_dir: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginInstallResult {
    ok: bool,
    path: String,
    restart_required: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct FixedCommandResult {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[tauri::command]
pub(crate) fn app_info(app: AppHandle, state: State<'_, AppState>) -> AppInfo {
    let resources = state.paths.describe(&app);
    AppInfo {
        kind: "tauri",
        version: app.package_info().version.to_string(),
        build_commit: env!("ROSYNC_BUILD_COMMIT"),
        build_dirty: env!("ROSYNC_BUILD_DIRTY") == "true",
        target: env!("ROSYNC_TARGET_TRIPLE"),
        platform: platform_name(),
        resource_dir: resources.resource_dir,
        data_dir: resources.data_dir,
        daemon_path: resources.daemon_path,
        plugin_path: resources.plugin_path,
        plugin_dir: plugin_directory(&app).ok().map(|path| display_path(&path)),
    }
}

#[tauri::command]
pub(crate) fn resource_paths(app: AppHandle, state: State<'_, AppState>) -> ResourcePaths {
    state.paths.describe(&app)
}

#[tauri::command]
pub(crate) fn state_get(state: State<'_, AppState>, key: String) -> Result<Option<Value>, String> {
    let _guard = state
        .io_lock
        .lock()
        .map_err(|_| "application state lock is poisoned".to_string())?;
    crate::storage::state_get(&state.paths.state_file, &key)
}

#[tauri::command]
pub(crate) fn state_set(
    state: State<'_, AppState>,
    key: String,
    value: Value,
) -> Result<(), String> {
    let _guard = state
        .io_lock
        .lock()
        .map_err(|_| "application state lock is poisoned".to_string())?;
    crate::storage::state_set(&state.paths.state_file, &key, value)
}

#[tauri::command]
pub(crate) async fn secret_get(
    state: State<'_, AppState>,
    key: String,
) -> Result<Option<String>, String> {
    let io_lock = state.io_lock.clone();
    let secrets_file = state.paths.secrets_file.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = io_lock
            .lock()
            .map_err(|_| "application state lock is poisoned".to_string())?;
        SecretStore::new(secrets_file).get(&key)
    })
    .await
    .map_err(|error| format!("secret lookup task failed: {error}"))?
}

#[tauri::command]
pub(crate) async fn secret_set(
    app: AppHandle,
    state: State<'_, AppState>,
    key: String,
    value: String,
) -> Result<(), String> {
    if key == ROBLOX_CLOUD_SECRET_KEY {
        cli_auth_set(&app, &state.paths, &value).await?;
    }
    let _guard = state
        .io_lock
        .lock()
        .map_err(|_| "application state lock is poisoned".to_string())?;
    SecretStore::new(state.paths.secrets_file.clone()).set(&key, &value)
}

#[tauri::command]
pub(crate) async fn secret_delete(
    app: AppHandle,
    state: State<'_, AppState>,
    key: String,
) -> Result<(), String> {
    if key == ROBLOX_CLOUD_SECRET_KEY {
        cli_auth_clear(&app, &state.paths).await?;
    }
    let _guard = state
        .io_lock
        .lock()
        .map_err(|_| "application state lock is poisoned".to_string())?;
    SecretStore::new(state.paths.secrets_file.clone()).delete(&key)
}

async fn cli_auth_set(
    app: &AppHandle,
    paths: &crate::resources::AppPaths,
    secret: &str,
) -> Result<(), String> {
    if secret.is_empty() {
        return cli_auth_clear(app, paths).await;
    }
    let args = vec![
        "auth".to_string(),
        "set".to_string(),
        "--from-env".to_string(),
        ROBLOX_CLOUD_SECRET_ENV.to_string(),
        "--data-dir".to_string(),
        display_path(&paths.daemon_data_dir),
        "--raw".to_string(),
    ];
    run_cli_auth(app, paths, args, Some(secret)).await
}

async fn cli_auth_clear(app: &AppHandle, paths: &crate::resources::AppPaths) -> Result<(), String> {
    let args = vec![
        "auth".to_string(),
        "clear".to_string(),
        "--data-dir".to_string(),
        display_path(&paths.daemon_data_dir),
        "--raw".to_string(),
    ];
    run_cli_auth(app, paths, args, None).await
}

async fn run_cli_auth(
    app: &AppHandle,
    paths: &crate::resources::AppPaths,
    args: Vec<String>,
    secret: Option<&str>,
) -> Result<(), String> {
    let sidecar = app
        .shell()
        .sidecar("rosync")
        .map_err(|error| format!("could not locate bundled Ro Sync CLI: {error}"))?;
    let mut command = sidecar.args(args).current_dir(&paths.resource_dir);
    if let Some(secret) = secret {
        command = command.env(ROBLOX_CLOUD_SECRET_ENV, secret);
    }
    let output = command
        .output()
        .await
        .map_err(|error| format!("could not update the Ro Sync credential store: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(value) = serde_json::from_str::<Value>(stdout.trim()) {
        if value.get("ok").and_then(Value::as_bool) != Some(false) && output.status.success() {
            return Ok(());
        }
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Ro Sync rejected the credential update");
        return Err(redact(message, secret));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    Err(redact(
        if message.is_empty() {
            "Ro Sync credential command returned no result"
        } else {
            message
        },
        secret,
    ))
}

fn redact(message: &str, secret: Option<&str>) -> String {
    match secret.filter(|secret| !secret.is_empty()) {
        Some(secret) => message.replace(secret, "[redacted]"),
        None => message.to_owned(),
    }
}

#[tauri::command]
pub(crate) fn read_project_file(
    state: State<'_, AppState>,
    path: String,
) -> Result<String, String> {
    let path = validate_project_file_path(&path)?;
    read_authorized_utf8_file(
        &state.paths.authorized_roots_file,
        &path,
        MAX_PROJECT_FILE_BYTES,
    )
}

#[tauri::command]
pub(crate) fn write_project_file(
    state: State<'_, AppState>,
    path: String,
    content: String,
) -> Result<(), String> {
    if content.len() as u64 > MAX_PROJECT_FILE_BYTES {
        return Err("project file exceeds the size limit".into());
    }
    let path = validate_project_file_path(&path)?;
    let _guard = state
        .io_lock
        .lock()
        .map_err(|_| "application state lock is poisoned".to_string())?;
    atomic_write_authorized(
        &state.paths.authorized_roots_file,
        &path,
        content.as_bytes(),
        0o644,
    )
}

#[tauri::command]
pub(crate) fn read_resource_file(
    state: State<'_, AppState>,
    path: String,
) -> Result<String, String> {
    let relative = validate_text_resource(&path)?;
    let file = state.paths.resolve_resource(&relative);
    read_utf8_file(&file, MAX_PROJECT_FILE_BYTES)
}

#[tauri::command]
pub(crate) async fn pick_folder(
    app: AppHandle,
    state: State<'_, AppState>,
    prompt: String,
) -> Result<Option<String>, String> {
    let prompt = if prompt.trim().is_empty() {
        "Pick a folder".to_string()
    } else {
        prompt.trim().to_string()
    };
    let io_lock = state.io_lock.clone();
    let authorized_roots_file = state.paths.authorized_roots_file.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(prompt)
        .pick_folder(move |selected| {
            let _ = sender.send(selected);
        });
    let selected = receiver
        .await
        .map_err(|_| "folder picker closed before returning a selection".to_string())?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let selected = selected
        .into_path()
        .map_err(|error| format!("selected folder is not a local path: {error}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = io_lock
            .lock()
            .map_err(|_| "application state lock is poisoned".to_string())?;
        let authorized = crate::storage::authorize_project_root(&authorized_roots_file, &selected)?;
        Ok(Some(display_path(&authorized)))
    })
    .await
    .map_err(|error| format!("folder authorization task failed: {error}"))?
}

#[tauri::command]
pub(crate) fn open_path(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let requested = validate_absolute_path(&path)?;
    if !requested.exists() {
        let allowed_plugin_dir = plugin_directory(&app)?;
        if requested != allowed_plugin_dir {
            return Err(format!("{} does not exist", display_path(&requested)));
        }
        std::fs::create_dir_all(&requested).map_err(|error| {
            format!(
                "could not create Roblox plugin directory {}: {error}",
                display_path(&requested)
            )
        })?;
    }
    let path = if requested == plugin_directory(&app)? {
        crate::storage::canonicalize_physical_directory(&requested)?
    } else {
        crate::storage::resolve_authorized_path(&state.paths.authorized_roots_file, &requested)?
    };
    app.opener()
        .open_path(display_path(&path), None::<&str>)
        .map_err(|error| format!("could not open {}: {error}", display_path(&path)))
}

#[tauri::command]
pub(crate) fn clipboard_write(app: AppHandle, text: String) -> Result<(), String> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err("clipboard text exceeds the size limit".into());
    }
    app.clipboard()
        .write_text(text)
        .map_err(|error| format!("could not write to clipboard: {error}"))
}

#[tauri::command]
pub(crate) fn plugin_install(app: AppHandle) -> Result<PluginInstallResult, String> {
    let source = resolve_with_tauri(&app, "plugin/Plugin.rbxm")?;
    if !source.is_file() {
        return Err(format!(
            "bundled Studio plugin is missing at {}",
            display_path(&source)
        ));
    }
    let destination_dir = plugin_directory(&app)?;
    std::fs::create_dir_all(&destination_dir).map_err(|error| {
        format!(
            "could not create Roblox plugin directory {}: {error}",
            display_path(&destination_dir)
        )
    })?;
    let destination = destination_dir.join("RoSync.rbxm");
    let bytes = std::fs::read(&source)
        .map_err(|error| format!("could not read bundled Studio plugin: {error}"))?;
    atomic_write(&destination, &bytes, 0o644)?;

    for stale in ["RoSync.lua", "RoSync.luau"] {
        let stale = destination_dir.join(stale);
        if stale.is_file() {
            std::fs::remove_file(&stale).map_err(|error| {
                format!(
                    "could not remove stale plugin {}: {error}",
                    display_path(&stale)
                )
            })?;
        }
    }

    Ok(PluginInstallResult {
        ok: true,
        path: display_path(&destination),
        restart_required: true,
    })
}

#[tauri::command]
pub(crate) async fn wally_install(
    app: AppHandle,
    state: State<'_, AppState>,
    cwd: String,
) -> Result<FixedCommandResult, String> {
    let cwd = validate_absolute_path(&cwd)?;
    let cwd = crate::storage::resolve_authorized_path(&state.paths.authorized_roots_file, &cwd)?;
    if !cwd.is_dir() {
        return Err("Wally working directory must be a folder".into());
    }
    if !cwd.join("wally.toml").is_file() {
        return Err(format!(
            "{} does not contain wally.toml",
            display_path(&cwd)
        ));
    }
    let home = app.path().home_dir().ok();
    let local_data = app.path().local_data_dir().ok();
    tauri::async_runtime::spawn_blocking(move || run_wally_install(cwd, home, local_data))
        .await
        .map_err(|error| format!("Wally task failed: {error}"))?
}

fn validate_text_resource(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("resource path must be a clean relative path".into());
    }
    let normalized = path.to_string_lossy().replace('\\', "/");
    if !matches!(
        normalized.as_str(),
        "docs/client-commands.generated.json" | "plugin/Plugin.luau"
    ) {
        return Err("resource is not in the readable text allowlist".into());
    }
    Ok(path)
}

fn validate_absolute_path(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err("path must be absolute".into());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("path must not contain . or .. components".into());
    }
    Ok(path)
}

fn plugin_directory(app: &AppHandle) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        app.path()
            .home_dir()
            .map(|home| home.join("Documents/Roblox/Plugins"))
            .map_err(|error| format!("could not resolve home directory: {error}"))
    }
    #[cfg(target_os = "windows")]
    {
        app.path()
            .local_data_dir()
            .map(|local| local.join("Roblox").join("Plugins"))
            .map_err(|error| format!("could not resolve local data directory: {error}"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = app;
        Err("Roblox Studio plugin installation is supported on macOS and Windows".into())
    }
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "darwin"
    }
}

fn run_wally_install(
    cwd: PathBuf,
    home: Option<PathBuf>,
    local_data: Option<PathBuf>,
) -> Result<FixedCommandResult, String> {
    let path = augmented_path(home.as_deref(), local_data.as_deref())?;
    let output = match run_fixed_program("wally", &["install"], &cwd, &path) {
        Ok(output) => output,
        Err(ProgramError::NotFound) => {
            run_fixed_program("aftman", &["run", "wally", "install"], &cwd, &path)
                .map_err(program_error_message)?
        }
        Err(error) => return Err(program_error_message(error)),
    };

    let code = output.status.code();
    Ok(FixedCommandResult {
        ok: output.status.success(),
        code,
        stdout: bounded_output(&output.stdout),
        stderr: bounded_output(&output.stderr),
    })
}

fn augmented_path(
    home: Option<&Path>,
    local_data: Option<&Path>,
) -> Result<std::ffi::OsString, String> {
    let mut entries = Vec::new();
    if let Some(home) = home {
        entries.push(home.join(".aftman/bin"));
        entries.push(home.join(".local/share/aftman/bin"));
        entries.push(home.join(".wally/bin"));
    }
    if let Some(local) = local_data {
        entries.push(local.join("aftman/bin"));
    }
    if let Some(current) = env::var_os("PATH") {
        entries.extend(env::split_paths(&current));
    }
    env::join_paths(entries).map_err(|error| format!("could not construct tool PATH: {error}"))
}

#[derive(Debug)]
enum ProgramError {
    NotFound,
    Timeout,
    Io(io::Error),
}

fn run_fixed_program(
    program: &str,
    args: &[&str],
    cwd: &Path,
    path: &std::ffi::OsStr,
) -> Result<Output, ProgramError> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ProgramError::NotFound
            } else {
                ProgramError::Io(error)
            }
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProgramError::Io(io::Error::other("stdout pipe was not created")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProgramError::Io(io::Error::other("stderr pipe was not created")))?;
    // Drain both pipes while the process runs. Waiting first can deadlock when
    // Wally emits more than the operating-system pipe buffer.
    let stdout_reader = thread::spawn(move || drain_bounded(stdout));
    let stderr_reader = thread::spawn(move || drain_bounded(stderr));

    let status = if let Some(status) = child
        .wait_timeout(Duration::from_secs(120))
        .map_err(ProgramError::Io)?
    {
        status
    } else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        return Err(ProgramError::Timeout);
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| ProgramError::Io(io::Error::other("stdout reader panicked")))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| ProgramError::Io(io::Error::other("stderr reader panicked")))??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn drain_bounded(mut reader: impl Read) -> Result<Vec<u8>, ProgramError> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut chunk).map_err(ProgramError::Io)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_COMMAND_OUTPUT_BYTES.saturating_sub(kept.len());
        let retained = read.min(remaining);
        kept.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
    if truncated {
        kept.extend_from_slice(b"\n[output truncated]");
    }
    Ok(kept)
}

fn program_error_message(error: ProgramError) -> String {
    match error {
        ProgramError::NotFound => "wally not found on PATH and aftman is not available".into(),
        ProgramError::Timeout => "wally install exceeded the 120 second limit".into(),
        ProgramError::Io(error) => format!("could not run wally install: {error}"),
    }
}

fn bounded_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_resources_are_narrowly_allowlisted() {
        assert!(validate_text_resource("docs/client-commands.generated.json").is_ok());
        assert!(validate_text_resource("plugin/Plugin.luau").is_ok());
        assert!(validate_text_resource("../state.json").is_err());
        assert!(validate_text_resource("plugin/Plugin.rbxm").is_err());
    }

    #[test]
    fn command_output_is_drained_but_bounded() {
        let input = vec![b'x'; MAX_COMMAND_OUTPUT_BYTES + 4096];
        let output = drain_bounded(std::io::Cursor::new(input)).unwrap();
        assert!(output.len() < MAX_COMMAND_OUTPUT_BYTES + 64);
        assert!(output.ends_with(b"[output truncated]"));
    }
}
