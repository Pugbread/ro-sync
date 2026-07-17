use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, State};
use tauri_plugin_shell::ShellExt;

use crate::{resources::display_path, AppState};

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

    let mut args = vec![
        "daemon".to_string(),
        "start".to_string(),
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
    push_nonblank_flag(&mut args, "--game-id", spec.game_id.as_deref());
    push_nonblank_flag(&mut args, "--group-id", spec.group_id.as_deref());
    for place_id in spec.place_ids {
        push_nonblank_flag(&mut args, "--place-id", Some(&place_id));
    }
    if let Some(port) = spec.preferred_port {
        if port == 0 {
            return Err("preferred daemon port must be greater than zero".into());
        }
        args.splice(4..4, ["--port".to_string(), port.to_string()]);
    }
    run_lifecycle(&app, &state.paths.resource_dir, args, Some(&owner_token)).await
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
        "--project".to_string(),
        display_path(&project),
        "--data-dir".to_string(),
        display_path(&state.paths.daemon_data_dir),
        "--raw".to_string(),
    ];
    run_lifecycle(&app, &state.paths.resource_dir, args, None).await
}

async fn run_lifecycle(
    app: &AppHandle,
    resource_dir: &Path,
    args: Vec<String>,
    secret: Option<&str>,
) -> Result<Value, String> {
    let sidecar = app
        .shell()
        .sidecar("rosync")
        .map_err(|error| format!("could not locate bundled Ro Sync daemon: {error}"))?;
    let mut command = sidecar.args(args).current_dir(resource_dir);
    if let Some((lsp, compiler)) = bundled_luau_tools(resource_dir) {
        command = command
            .env("ROSYNC_LUAU_LSP", lsp)
            .env("ROSYNC_LUAU_COMPILE", compiler);
    }
    if let Some(secret) = secret {
        command = command.env("ROSYNC_OWNER_TOKEN", secret);
    }
    let output = command
        .output()
        .await
        .map_err(|error| format!("could not run bundled Ro Sync lifecycle command: {error}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(value) = serde_json::from_str::<Value>(stdout.trim()) {
        return Ok(normalize_lifecycle(value));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut message = if stderr.trim().is_empty() {
        stdout.trim().to_owned()
    } else {
        stderr.trim().to_owned()
    };
    if let Some(secret) = secret {
        message = message.replace(secret, "[redacted]");
    }
    if message.is_empty() {
        message = if output.status.success() {
            "Ro Sync lifecycle command returned no JSON".into()
        } else {
            format!(
                "Ro Sync lifecycle command exited with code {:?}",
                output.status.code()
            )
        };
    }
    Err(message)
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
}
