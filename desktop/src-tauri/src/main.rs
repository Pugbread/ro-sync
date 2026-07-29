fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--build-info")) {
        println!(
            "{}",
            serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "buildCommit": env!("ROSYNC_BUILD_COMMIT"),
                "buildDirty": env!("ROSYNC_BUILD_DIRTY") == "true",
                "target": env!("ROSYNC_TARGET_TRIPLE"),
            })
        );
        return;
    }
    ro_sync_desktop::run();
}
