use serde::Serialize;
#[cfg(target_os = "windows")]
use tauri::Manager;
use tauri::{AppHandle, State};
use tauri_plugin_updater::UpdaterExt;

use crate::AppState;

const MAX_VERSION_BYTES: usize = 128;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateCheckResult {
    configured: bool,
    available: bool,
    current_version: String,
    version: Option<String>,
    date: Option<String>,
    body: Option<String>,
}

pub(crate) fn embedded_public_key() -> Option<String> {
    option_env!("ROSYNC_UPDATER_PUBLIC_KEY")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn validate_expected_version(value: &str) -> Result<&str, &'static str> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_VERSION_BYTES {
        return Err("the requested update version is invalid");
    }
    Ok(value)
}

#[tauri::command]
pub(crate) async fn update_check(app: AppHandle) -> Result<UpdateCheckResult, String> {
    let current_version = app.package_info().version.to_string();
    if embedded_public_key().is_none() {
        return Ok(UpdateCheckResult {
            configured: false,
            available: false,
            current_version,
            version: None,
            date: None,
            body: None,
        });
    }

    let update = app
        .updater()
        .map_err(|error| format!("could not initialize the signed updater: {error}"))?
        .check()
        .await
        .map_err(|error| format!("could not check GitHub for an update: {error}"))?;

    Ok(match update {
        Some(update) => UpdateCheckResult {
            configured: true,
            available: true,
            current_version,
            version: Some(update.version),
            date: update.date.map(|date| date.to_string()),
            body: update.body,
        },
        None => UpdateCheckResult {
            configured: true,
            available: false,
            current_version,
            version: None,
            date: None,
            body: None,
        },
    })
}

#[tauri::command]
pub(crate) async fn update_install(
    app: AppHandle,
    state: State<'_, AppState>,
    expected_version: String,
) -> Result<(), String> {
    let expected_version = validate_expected_version(&expected_version).map_err(str::to_owned)?;
    if embedded_public_key().is_none() {
        return Err("signed application updates are not configured for this build".into());
    }

    let _install_guard = state.update_install_lock.lock().await;
    let updater_builder = app.updater_builder();
    #[cfg(target_os = "windows")]
    let updater_builder = {
        let exit_app = app.clone();
        updater_builder.on_before_exit(move || {
            // The Windows updater launches the installer and then calls
            // process::exit, bypassing App::run's RunEvent::Exit callback.
            // Close native-owned work here, then preserve Tauri's own cleanup
            // hook before the updater terminates the process.
            exit_app.state::<AppState>().cleanup_runtime_once();
            exit_app.cleanup_before_exit();
        })
    };
    let update = updater_builder
        .build()
        .map_err(|error| format!("could not initialize the signed updater: {error}"))?
        .check()
        .await
        .map_err(|error| format!("could not re-check GitHub for the update: {error}"))?
        .ok_or_else(|| "the update is no longer available".to_string())?;

    if update.version != expected_version {
        return Err(format!(
            "the available update changed from {expected_version} to {}; review it before installing",
            update.version
        ));
    }

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|error| format!("the signed update could not be installed: {error}"))?;

    // macOS installation returns here and follows the normal RunEvent::Exit
    // cleanup path. Windows exits inside the updater after its hook above.
    app.request_restart();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_update_versions_are_trimmed_and_bounded() {
        assert_eq!(validate_expected_version(" 1.2.3 "), Ok("1.2.3"));
        assert!(validate_expected_version("   ").is_err());
        assert!(validate_expected_version(&"x".repeat(MAX_VERSION_BYTES + 1)).is_err());
    }
}
