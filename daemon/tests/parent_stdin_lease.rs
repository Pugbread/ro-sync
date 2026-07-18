use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

fn project_key(project: &std::path::Path) -> String {
    let mut digest = Sha256::new();
    digest.update(project.to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())
}

fn take_child_stderr(child: &mut std::process::Child) -> String {
    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut text = String::new();
    let _ = stderr.read_to_string(&mut text);
    text
}

fn wait_for_lifecycle_probe(child: &mut std::process::Child, accepted: &Receiver<()>) {
    // Process creation can take more than two seconds on a cold Windows CI
    // worker. Wait for the actual TCP probe while still detecting an early
    // child exit, rather than guessing when startup should have completed.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            let _ = child.wait();
            let stderr = take_child_stderr(child);
            panic!(
                "lifecycle process did not reach the blocked hello probe within ten seconds; stderr: {stderr}"
            );
        }

        match accepted.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(()) => return,
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = take_child_stderr(child);
                panic!(
                    "hanging hello server exited before accepting the lifecycle probe; stderr: {stderr}"
                );
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        if let Some(status) = child
            .try_wait()
            .expect("poll lifecycle process during startup")
        {
            let stderr = take_child_stderr(child);
            panic!(
                "lifecycle process exited before its blocked hello probe: {status}; stderr: {stderr}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn daemon_status_survives_a_one_mib_process_stack() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let project = temporary.path().join("project");
    let state_dir = temporary.path().join("state");
    std::fs::create_dir(&project).expect("project directory");

    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "ulimit -s 1024 && exec \"$@\"", "rosync-stack-test"])
        .arg(env!("CARGO_BIN_EXE_rosync"))
        .args([
            "daemon",
            "status",
            "--project",
            project.to_str().expect("UTF-8 project path"),
            "--data-dir",
            state_dir.to_str().expect("UTF-8 state path"),
            "--raw",
        ])
        .stdin(Stdio::null());
    // Match the default Windows executable stack that exposed the regression.
    // The CLI coordinator must stay within it and run the command future on
    // the explicitly sized worker stack.

    let output = command.output().expect("run daemon status");
    assert!(
        output.status.success(),
        "daemon status failed with {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let status: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("daemon status JSON");
    assert_eq!(status.get("running"), Some(&serde_json::Value::Bool(false)));
}

#[test]
fn closing_parent_stdin_terminates_a_blocked_lifecycle_process() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let project = temporary.path().join("project");
    let state_dir = temporary.path().join("state");
    std::fs::create_dir(&project).expect("project directory");
    std::fs::create_dir_all(state_dir.join("daemons")).expect("daemon state directory");
    let project = std::fs::canonicalize(project).expect("canonical project");

    // Hold /hello open without replying so `daemon status` is definitely in a
    // blocking lifecycle operation when its parent-side stdin lease closes.
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind hanging hello server");
    let port = listener.local_addr().expect("listener address").port();
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept lifecycle probe");
        accepted_tx.send(()).expect("report accepted probe");
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
    });

    let record = serde_json::json!({
        "version": 1,
        "project": project.display().to_string(),
        "canonicalProject": project.display().to_string(),
        "pid": 999_999,
        "port": port,
        "bootId": "blocked-lifecycle-test",
        "controlToken": "test-control-token",
        "managedBy": "desktop",
        "logPath": state_dir.join("daemon.log").display().to_string(),
        "startedAt": 1,
    });
    let record_path = state_dir
        .join("daemons")
        .join(format!("{}.json", project_key(&project)));
    std::fs::write(
        record_path,
        serde_json::to_vec_pretty(&record).expect("encode runtime record"),
    )
    .expect("write runtime record");

    let mut child = Command::new(env!("CARGO_BIN_EXE_rosync"))
        .args([
            "daemon",
            "status",
            "--project",
            project.to_str().expect("UTF-8 project path"),
            "--data-dir",
            state_dir.to_str().expect("UTF-8 state path"),
            "--parent-stdin-lease",
            "--raw",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lifecycle process");
    let parent_stdin = child.stdin.take().expect("piped lifecycle stdin");

    wait_for_lifecycle_probe(&mut child, &accepted_rx);
    let disconnected_at = Instant::now();
    drop(parent_stdin);

    let deadline = disconnected_at + Duration::from_secs(1);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll lifecycle process") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lifecycle process survived parent stdin EOF for more than one second");
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let stderr = take_child_stderr(&mut child);
    assert_eq!(status.code(), Some(1), "lifecycle stderr: {stderr}");
    assert!(
        disconnected_at.elapsed() < Duration::from_secs(1),
        "parent EOF termination should be prompt"
    );
    server.join().expect("hanging hello server");
}
