use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{path::BaseDirectory, AppHandle, Manager};

#[derive(Clone, Debug)]
pub(crate) struct AppPaths {
    pub(crate) data_dir: PathBuf,
    pub(crate) state_file: PathBuf,
    pub(crate) secrets_file: PathBuf,
    pub(crate) authorized_roots_file: PathBuf,
    pub(crate) daemon_data_dir: PathBuf,
    pub(crate) resource_dir: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResourcePaths {
    pub(crate) plugin_path: Option<String>,
    pub(crate) plugin_source_path: Option<String>,
    pub(crate) docs_path: Option<String>,
    pub(crate) tools_dir: Option<String>,
    pub(crate) daemon_path: Option<String>,
    pub(crate) resource_dir: String,
    pub(crate) data_dir: String,
}

impl AppPaths {
    pub(crate) fn initialize(app: &AppHandle) -> Result<Self, String> {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|error| format!("could not resolve application data directory: {error}"))?;
        let daemon_data_dir = shared_cli_data_dir(app)?;
        create_private_dir(&data_dir)?;
        create_private_dir(&daemon_data_dir)?;

        let resource_dir = app
            .path()
            .resource_dir()
            .map_err(|error| format!("could not resolve bundled resource directory: {error}"))?;

        Ok(Self {
            state_file: data_dir.join("state.json"),
            secrets_file: data_dir.join("secrets.json"),
            authorized_roots_file: data_dir.join("authorized-project-roots.json"),
            daemon_data_dir,
            data_dir,
            resource_dir,
        })
    }

    pub(crate) fn resolve_resource(&self, relative: &Path) -> PathBuf {
        self.resource_dir.join(relative)
    }

    pub(crate) fn describe(&self, app: &AppHandle) -> ResourcePaths {
        let existing = |path: PathBuf| path.exists().then(|| display_path(&path));
        ResourcePaths {
            plugin_path: existing(self.resolve_resource(Path::new("plugin/Plugin.rbxm"))),
            plugin_source_path: existing(self.resolve_resource(Path::new("plugin/Plugin.luau"))),
            docs_path: existing(
                self.resolve_resource(Path::new("docs/client-commands.generated.json")),
            ),
            tools_dir: existing(self.resolve_resource(Path::new("tools"))),
            daemon_path: locate_sidecar(app),
            resource_dir: display_path(&self.resource_dir),
            data_dir: display_path(&self.data_dir),
        }
    }
}

fn shared_cli_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    {
        if let Some(base) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(base).join("rosync"));
        }
        return app
            .path()
            .home_dir()
            .map(|home| home.join(".local/state/rosync"))
            .map_err(|error| format!("could not resolve home directory: {error}"));
    }
    #[cfg(not(target_os = "linux"))]
    {
        app.path()
            .local_data_dir()
            .map(|base| base.join("RoSync"))
            .map_err(|error| format!("could not resolve local data directory: {error}"))
    }
}

pub(crate) fn locate_sidecar(app: &AppHandle) -> Option<String> {
    let executable_name = if cfg!(windows) {
        "rosync.exe"
    } else {
        "rosync"
    };
    let mut candidates = Vec::new();

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            candidates.push(parent.join(executable_name));
        }
    }
    if let Ok(resource_dir) = app.path().resource_dir() {
        candidates.push(resource_dir.join(executable_name));
        candidates.push(resource_dir.join("..").join("MacOS").join(executable_name));
    }

    let extension = if cfg!(windows) { ".exe" } else { "" };
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("binaries")
            .join(format!(
                "rosync-{}{}",
                env!("ROSYNC_TARGET_TRIPLE"),
                extension
            )),
    );

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .map(|path| display_path(&path))
}

pub(crate) fn resolve_with_tauri(app: &AppHandle, relative: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(relative, BaseDirectory::Resource)
        .map_err(|error| format!("could not resolve bundled resource {relative}: {error}"))
}

pub(crate) fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("could not create {}: {error}", display_path(path)))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure {}: {error}", display_path(path)))?;
    }
    Ok(())
}
