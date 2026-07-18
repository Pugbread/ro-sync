mod commands;
mod daemon;
mod project_broker;
mod resources;
mod secrets;
mod storage;
mod updater;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use resources::AppPaths;
use tauri::Manager;

pub(crate) struct AppState {
    paths: AppPaths,
    io_lock: Arc<Mutex<()>>,
    daemon_start_lock: tokio::sync::Mutex<()>,
    lifecycle_children: daemon::LifecycleChildren,
    managed_daemons: daemon::ManagedDaemonClaims,
    project_broker: project_broker::ProjectInitBroker,
    update_install_lock: tokio::sync::Mutex<()>,
    exit_cleanup_started: AtomicBool,
}

impl AppState {
    fn new(paths: AppPaths) -> Self {
        let project_broker = project_broker::ProjectInitBroker::start(paths.clone());
        Self {
            paths,
            io_lock: Arc::new(Mutex::new(())),
            daemon_start_lock: tokio::sync::Mutex::new(()),
            lifecycle_children: daemon::LifecycleChildren::default(),
            managed_daemons: daemon::ManagedDaemonClaims::default(),
            project_broker,
            update_install_lock: tokio::sync::Mutex::new(()),
            exit_cleanup_started: AtomicBool::new(false),
        }
    }

    pub(crate) fn cleanup_runtime_once(&self) {
        if self.exit_cleanup_started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.managed_daemons.mark_exiting();
        self.project_broker.stop();
        self.lifecycle_children.terminate_all();
        self.managed_daemons.terminate();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let updater_public_key = updater::embedded_public_key().unwrap_or_default();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_updater::Builder::new()
                .pubkey(updater_public_key)
                .build(),
        )
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
            daemon::daemon_list,
            daemon::daemon_status,
            daemon::daemon_stop,
            project_broker::project_broker_status,
            project_broker::project_init_drain,
            updater::update_check,
            updater::update_install,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Ro Sync desktop");
    app.run(|app, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            app.state::<AppState>().cleanup_runtime_once();
        }
    });
}
