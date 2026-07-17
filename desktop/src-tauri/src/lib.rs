mod commands;
mod daemon;
mod resources;
mod secrets;
mod storage;

use std::sync::Mutex;

use resources::AppPaths;
use tauri::Manager;

pub(crate) struct AppState {
    paths: AppPaths,
    io_lock: Mutex<()>,
}

impl AppState {
    fn new(paths: AppPaths) -> Self {
        Self {
            paths,
            io_lock: Mutex::new(()),
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let paths = AppPaths::initialize(app.handle()).map_err(std::io::Error::other)?;
            app.manage(AppState::new(paths));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::resource_paths,
            commands::state_get,
            commands::state_set,
            commands::secret_get,
            commands::secret_set,
            commands::secret_delete,
            commands::read_project_file,
            commands::write_project_file,
            commands::read_resource_file,
            commands::pick_folder,
            commands::open_path,
            commands::clipboard_write,
            commands::plugin_install,
            commands::wally_install,
            daemon::daemon_ensure,
            daemon::daemon_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Ro Sync desktop");
}
