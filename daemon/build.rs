use std::{
    path::{Path, PathBuf},
    process::Command,
};

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

fn git_tracked_files(repo: &Path) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["-c", "core.quotePath=false", "ls-files", "-z"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter(|file| !file.is_empty() && !file.contains('\r') && !file.contains('\n'))
            .map(|file| repo.join(file))
            .collect(),
    )
}

fn git_path(repo: &Path, args: &[&str]) -> Option<PathBuf> {
    let path = PathBuf::from(git_output(repo, args)?);
    Some(if path.is_absolute() {
        path
    } else {
        repo.join(path)
    })
}

fn watch_git_identity_inputs(repo: &Path) {
    for args in [
        ["rev-parse", "--git-path=HEAD"].as_slice(),
        ["rev-parse", "--git-path=index"].as_slice(),
        ["rev-parse", "--git-path=logs/HEAD"].as_slice(),
        ["rev-parse", "--git-path=packed-refs"].as_slice(),
    ] {
        if let Some(path) = git_path(repo, args) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(reference) = git_output(repo, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_path(repo, &["rev-parse", &format!("--git-path={reference}")]) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    // buildDirty is derived from the complete tracked worktree, not just this
    // Cargo package. Watching each tracked path prevents Cargo from reusing a
    // clean/dirty identity after an unrelated tracked file changes.
    if let Some(files) = git_tracked_files(repo) {
        for file in files {
            println!("cargo:rerun-if-changed={}", file.display());
        }
    }
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
    watch_git_identity_inputs(repo);

    let commit = std::env::var("ROSYNC_BUILD_COMMIT")
        .ok()
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_output(repo, &["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let short_commit: String = commit.chars().take(12).collect();
    println!("cargo:rustc-env=ROSYNC_BUILD_COMMIT={short_commit}");

    let dirty = match std::env::var("ROSYNC_BUILD_DIRTY").as_deref() {
        Ok("true") => true,
        Ok("false") => false,
        _ => git_output(repo, &["status", "--porcelain", "--untracked-files=no"])
            .is_some_and(|status| !status.is_empty()),
    };
    println!(
        "cargo:rustc-env=ROSYNC_BUILD_DIRTY={}",
        if dirty { "true" } else { "false" }
    );
}
