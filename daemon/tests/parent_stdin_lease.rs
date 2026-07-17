use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn project_key(project: &std::path::Path) -> String {
    let mut digest = Sha256::new();
    digest.update(project.to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())
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
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lifecycle process");
    let parent_stdin = child.stdin.take().expect("piped lifecycle stdin");

    accepted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("lifecycle process should reach the blocked hello probe");
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

    assert_eq!(status.code(), Some(1));
    assert!(
        disconnected_at.elapsed() < Duration::from_secs(1),
        "parent EOF termination should be prompt"
    );
    server.join().expect("hanging hello server");
}
