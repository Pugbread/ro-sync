use std::{path::Path, process::Command};

fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn main() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate must live under the repository root");
    println!("cargo:rerun-if-env-changed=ROSYNC_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=ROSYNC_BUILD_DIRTY");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=../docs/client-commands.generated.json");

    let commit = std::env::var("ROSYNC_BUILD_COMMIT")
        .ok()
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .filter(|value| !value.trim().is_empty())
        // Local source builds intentionally use a stable identity. Watching
        // every tracked file solely to refresh a dirty bit made an edit to
        // docs, the plugin, or the desktop UI recompile the entire daemon.
        // Release jobs provide an explicit commit and dirty state.
        .unwrap_or_else(|| "source".to_string());
    let short_commit: String = commit.chars().take(12).collect();
    println!("cargo:rustc-env=ROSYNC_BUILD_COMMIT={short_commit}");

    let dirty = match std::env::var("ROSYNC_BUILD_DIRTY").as_deref() {
        Ok("true") => true,
        Ok("false") => false,
        _ if short_commit == "source" => false,
        _ => git_output(repo, &["status", "--porcelain", "--untracked-files=no"])
            .is_some_and(|status| !status.is_empty()),
    };
    println!(
        "cargo:rustc-env=ROSYNC_BUILD_DIRTY={}",
        if dirty { "true" } else { "false" }
    );
}
