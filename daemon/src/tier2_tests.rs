use super::*;
use clap::CommandFactory;

#[test]
fn managed_start_log_tail_excludes_previous_attempts() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    std::fs::write(
        &log,
        "rosync listening on http://127.0.0.1:7878 (project: stale)\nold failure\n",
    )
    .unwrap();
    let offset = std::fs::metadata(&log).unwrap().len();
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    writeln!(
        file,
        "Error: serve: validate watched filesystem: multiple init source markers"
    )
    .unwrap();

    let tail = read_log_tail_from(&log, offset, 20).unwrap();
    assert_eq!(
        tail,
        "Error: serve: validate watched filesystem: multiple init source markers"
    );
    assert!(!tail.contains("rosync listening"));
    assert!(!tail.contains("old failure"));
}

#[test]
fn managed_start_log_tail_reads_replacement_log_from_start() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    std::fs::write(&log, "a much longer previous daemon log\n").unwrap();
    let stale_offset = std::fs::metadata(&log).unwrap().len();
    std::fs::write(&log, "Error: replacement log\n").unwrap();

    assert_eq!(
        read_log_tail_from(&log, stale_offset, 20).unwrap(),
        "Error: replacement log"
    );
}

#[test]
fn managed_start_validation_error_leads_with_actionable_detail() {
    let message = managed_start_exit_message(
        61160,
        "exit status: 1",
        "Error: serve: validate watched filesystem: multiple init source markers",
    );
    assert_eq!(
        message,
        "daemon start: serve: validate watched filesystem: multiple init source markers"
    );
    assert!(!message.contains("child 61160"));
    assert!(!message.contains("handshake"));
}

#[test]
fn atomic_replace_bytes_replaces_existing_file() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("RoSync.rbxm");
    std::fs::write(&destination, b"old plugin bytes").unwrap();

    atomic_replace_bytes(&destination, b"new plugin bytes", 0o644).unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), b"new plugin bytes");
    let leftovers = std::fs::read_dir(directory.path())
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(leftovers, 0);
}

#[test]
fn parse_duration_seconds_units() {
    assert_eq!(parse_duration_seconds("30").unwrap(), 30.0);
    assert_eq!(parse_duration_seconds("30s").unwrap(), 30.0);
    assert_eq!(parse_duration_seconds("500ms").unwrap(), 0.5);
    assert_eq!(parse_duration_seconds("5m").unwrap(), 300.0);
    assert_eq!(parse_duration_seconds("2h").unwrap(), 7200.0);
    assert!(parse_duration_seconds("").is_err());
    assert!(parse_duration_seconds("30d").is_err());
}

#[test]
fn format_hms_handles_zero_ts() {
    assert_eq!(format_hms_local(0), "--:--:--");
}

#[test]
fn log_level_plugin_str() {
    assert_eq!(LogLevel::Info.as_plugin_str(), "info");
    assert_eq!(LogLevel::Warn.as_plugin_str(), "warn");
    assert_eq!(LogLevel::Error.as_plugin_str(), "error");
}

#[test]
fn owner_heartbeat_expiry_requires_a_seen_and_stale_heartbeat() {
    let timeout = Duration::from_secs(30);
    assert!(!owner_heartbeat_expired(None, timeout));
    assert!(!owner_heartbeat_expired(Some(Instant::now()), timeout));
    assert!(owner_heartbeat_expired(
        Some(Instant::now() - Duration::from_secs(31)),
        timeout,
    ));
    let stale = Some(Instant::now() - Duration::from_secs(31));
    assert!(!owner_heartbeat_should_shutdown(
        stale,
        None,
        timeout,
        Duration::from_secs(10),
    ));
    assert!(!owner_heartbeat_should_shutdown(
        stale,
        Some(Instant::now()),
        timeout,
        Duration::from_secs(10),
    ));
    assert!(owner_heartbeat_should_shutdown(
        stale,
        Some(Instant::now() - Duration::from_secs(11)),
        timeout,
        Duration::from_secs(10),
    ));
}

#[test]
fn synced_service_root_directory_ops_are_filtered() {
    let root = PathBuf::from("ro-sync-test-project");
    let service_op = Op {
        kind: OpKind::Update,
        path: root.join("ReplicatedStorage"),
        from: None,
        content: None,
        is_dir: Some(true),
    };
    let script_op = Op {
        kind: OpKind::Update,
        path: root.join("ReplicatedStorage").join("Client.luau"),
        from: None,
        content: Some(b"return {}".to_vec()),
        is_dir: Some(false),
    };

    assert!(is_synced_service_root_op(&service_op, &root));
    assert!(!is_synced_service_root_op(&script_op, &root));
}

#[test]
fn new_script_materializes_pending_empty_parent_chain_first() {
    let root = PathBuf::from("/tmp/ro-sync-test-project");
    let tools = root.join("Workspace/tools");
    let nested = tools.join("nested");
    let mut pending = HashSet::from([tools.clone(), nested.clone()]);
    let script_op = Op {
        kind: OpKind::Add,
        path: nested.join("Test.luau"),
        from: None,
        content: Some(b"return true".to_vec()),
        is_dir: Some(false),
    };

    assert_eq!(
        take_pending_parent_materializations(&script_op, &root, &mut pending),
        vec![tools, nested]
    );
    assert!(pending.is_empty());
}

#[test]
fn startup_seeds_preexisting_empty_service_directories() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("Workspace");
    let tools = workspace.join("tools");
    let nested = tools.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(temp.path().join("tools/root-only")).unwrap();

    let candidates = collect_existing_parent_candidates(temp.path()).unwrap();

    assert!(candidates.contains(&tools));
    assert!(candidates.contains(&nested));
    assert!(!candidates.contains(&workspace));
    assert!(!candidates.contains(&temp.path().join("tools/root-only")));
}

#[test]
fn failed_barrier_refresh_clears_stale_parent_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let missing_root = temp.path().join("removed-project");
    let stale = missing_root.join("Workspace/Stale");
    let mut candidates = HashSet::from([stale]);

    refresh_parent_candidates_after_barrier(&missing_root, &mut candidates);

    assert!(candidates.is_empty());
}

#[test]
fn watcher_lag_forces_retryable_full_resync() {
    let event: serde_json::Value = serde_json::from_str(&watcher_lag_shutdown(42)).unwrap();
    assert_eq!(event["type"], "shutdown");
    assert_eq!(event["code"], "WATCHER_LAGGED");
    assert_eq!(event["retryable"], true);
    assert_eq!(event["skipped"], 42);
}

#[test]
fn watcher_typed_resync_is_retryable() {
    let event: serde_json::Value = serde_json::from_str(&watcher_resync_shutdown(
        "WATCHER_BATCH_AMBIGUOUS",
        "rename cycle",
    ))
    .unwrap();
    assert_eq!(event["type"], "shutdown");
    assert_eq!(event["code"], "WATCHER_BATCH_AMBIGUOUS");
    assert_eq!(event["retryable"], true);
    assert_eq!(event["reason"], "rename cycle");
}

#[test]
fn watcher_hydrates_only_one_bounded_source_at_the_receiver() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("Workspace/Main.luau");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"--!strict\nreturn true\n").unwrap();
    let mut validation = fs_safety::SyncedPathValidationCache::new(temp.path()).unwrap();
    let mut op = Op {
        kind: OpKind::Update,
        path: source,
        from: None,
        content: None,
        is_dir: Some(false),
    };

    hydrate_watcher_op(&mut op, &mut validation).unwrap();
    assert_eq!(
        op.content.as_deref(),
        Some(&b"--!strict\nreturn true\n"[..])
    );
}

#[test]
fn watcher_oversize_add_and_rename_require_resync_without_reading_payload() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("Workspace/Oversize.luau");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let file = std::fs::File::create(&source).unwrap();
    file.set_len(fs_safety::MAX_SYNCED_SCRIPT_BYTES + 1)
        .unwrap();
    let mut validation = fs_safety::SyncedPathValidationCache::new(temp.path()).unwrap();
    for kind in [OpKind::Add, OpKind::Rename] {
        let mut op = Op {
            kind,
            path: source.clone(),
            from: (kind == OpKind::Rename).then(|| temp.path().join("Workspace/Old.luau")),
            content: None,
            is_dir: Some(false),
        };

        let error = hydrate_watcher_op(&mut op, &mut validation).unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
        assert!(op.content.is_none());
    }
}

#[test]
fn watcher_missing_hydration_maps_to_typed_retryable_resync() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("Workspace")).unwrap();
    let mut validation = fs_safety::SyncedPathValidationCache::new(temp.path()).unwrap();
    let mut op = Op {
        kind: OpKind::Update,
        path: temp.path().join("Workspace/Missing.luau"),
        from: None,
        content: None,
        is_dir: Some(false),
    };

    let error = hydrate_watcher_op(&mut op, &mut validation).unwrap_err();
    let event: serde_json::Value =
        serde_json::from_str(&watcher_hydration_shutdown(&error)).unwrap();
    assert_eq!(event["code"], "WATCHER_HYDRATION_FAILED");
    assert_eq!(event["retryable"], true);
    assert!(op.content.is_none());
}

#[test]
fn watcher_changed_generation_maps_to_typed_retryable_resync() {
    let before = fs_safety::FileGeneration {
        len: 10,
        modified_ns: Some(1),
        identity: fs_safety::FileIdentity {
            device: Some(2),
            file: Some(3),
        },
    };
    let mut after = before.clone();
    after.modified_ns = Some(2);

    let error = ensure_watcher_file_generation_unchanged(
        &before,
        &after,
        std::path::Path::new("/project/Workspace/Main.luau"),
    )
    .unwrap_err();
    let event: serde_json::Value =
        serde_json::from_str(&watcher_hydration_shutdown(&error)).unwrap();
    assert_eq!(event["code"], "WATCHER_HYDRATION_FAILED");
    assert_eq!(event["retryable"], true);
    assert!(event["reason"].as_str().unwrap().contains("changed"));
}

#[test]
fn service_root_quiet_entry_does_not_suppress_genuine_descendant_edit() {
    let root = PathBuf::from("/project");
    let quiet = Arc::new(Mutex::new(HashMap::new()));
    quiet.lock().unwrap().insert(
        root.join("Workspace"),
        Instant::now() + Duration::from_secs(1),
    );
    assert!(is_push_quiet(&quiet, &root.join("Workspace"), &root));
    assert!(!is_push_quiet(
        &quiet,
        &root.join("Workspace/Deep/Main.luau"),
        &root
    ));
    assert!(!is_push_quiet(
        &quiet,
        &root.join("ReplicatedStorage/Main.luau"),
        &root
    ));
    assert!(!is_push_quiet(
        &quiet,
        &root.join(".rosync-stage-x/Workspace/Main.luau"),
        &root
    ));

    quiet.lock().unwrap().insert(
        root.join("ReplicatedStorage"),
        Instant::now() - Duration::from_secs(1),
    );
    assert!(!is_push_quiet(
        &quiet,
        &root.join("ReplicatedStorage/Main.luau"),
        &root
    ));
}

#[test]
fn large_destructive_burst_uses_one_preflight_grace_window() {
    let mut grace_active = false;
    let mut waits = 0;
    for _ in 0..10_000 {
        if destructive_batch_needs_grace(grace_active) {
            waits += 1;
            grace_active = true;
        }
    }
    assert_eq!(waits, 1);
    assert!(!destructive_batch_needs_grace(grace_active));
    grace_active = false;
    assert!(destructive_batch_needs_grace(grace_active));
}

#[test]
fn added_directory_rescan_is_parent_first_and_hydrates_complete_subtree() {
    let temp = tempfile::tempdir().unwrap();
    let service = temp.path().join("ReplicatedStorage");
    let root = service.join("Misc");
    let nested = root.join("Nested");
    let empty = root.join("Empty");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(root.join("init (Notifications).luau"), "return {}\n").unwrap();
    std::fs::write(nested.join("Child.luau"), "return 42\n").unwrap();
    let mut validation = fs_safety::SyncedPathValidationCache::new(temp.path()).unwrap();

    let ops = collect_added_directory_descendants(&root, &mut validation).unwrap();
    let init_index = ops
        .iter()
        .position(|op| op.path == root.join("init (Notifications).luau"))
        .unwrap();
    let nested_index = ops.iter().position(|op| op.path == nested).unwrap();
    let child_index = ops
        .iter()
        .position(|op| op.path == nested.join("Child.luau"))
        .unwrap();
    assert_eq!(
        init_index, 0,
        "init source should classify the carrier first"
    );
    assert!(
        nested_index < child_index,
        "parent directory must precede child"
    );
    assert_eq!(
        ops[child_index].content.as_deref(),
        Some(b"return 42\n".as_slice())
    );

    let mut pending = HashSet::new();
    for op in &ops {
        if op.is_dir == Some(true) {
            pending.insert(op.path.clone());
        }
        let _ = take_pending_parent_materializations(op, temp.path(), &mut pending);
    }
    assert!(
        pending.contains(&empty),
        "empty rescan descendants must remain future parent candidates"
    );
    let future_child = Op {
        kind: OpKind::Add,
        path: empty.join("Later.luau"),
        from: None,
        content: Some(b"return true\n".to_vec()),
        is_dir: Some(false),
    };
    assert_eq!(
        take_pending_parent_materializations(&future_child, temp.path(), &mut pending),
        vec![empty]
    );
}

#[test]
fn empty_directory_rename_rebases_pending_descendant_candidates() {
    let from = PathBuf::from("/project/Workspace/tools");
    let to = PathBuf::from("/project/Workspace/utilities");
    let mut candidates = HashSet::from([from.clone(), from.join("nested")]);

    rebase_pending_parent_candidates(&mut candidates, &from, &to);

    assert_eq!(candidates, HashSet::from([to.clone(), to.join("nested")]));
}

#[test]
fn existing_parent_chain_does_not_add_materialization_noise() {
    let root = PathBuf::from("/tmp/ro-sync-test-project");
    let script_op = Op {
        kind: OpKind::Update,
        path: root.join("ReplicatedStorage/Shared/Config.luau"),
        from: None,
        content: Some(b"return true".to_vec()),
        is_dir: Some(false),
    };

    assert!(
        take_pending_parent_materializations(&script_op, &root, &mut HashSet::new()).is_empty()
    );
}

#[test]
fn watcher_reserved_init_leaf_requires_terminal_canonical_migration() {
    let temp = tempfile::tempdir().unwrap();
    let misc = temp.path().join("ReplicatedStorage/Misc");
    std::fs::create_dir_all(&misc).unwrap();
    let legacy = misc.join("init (Notifications).luau");
    std::fs::write(&legacy, "return true\n").unwrap();
    let op = Op {
        kind: OpKind::Update,
        path: legacy,
        from: None,
        content: Some(b"return true\n".to_vec()),
        is_dir: Some(false),
    };

    let message = watcher_legacy_reserved_leaf_migration(temp.path(), &op)
        .unwrap()
        .expect("legacy reserved leaf must require migration");
    let portable_message = message.replace('\\', "/");
    assert!(portable_message.contains("ReplicatedStorage/Misc/init (Notifications).luau"));
    assert!(portable_message.contains("ReplicatedStorage/Misc/%69nit (Notifications).luau"));

    let shutdown: serde_json::Value =
        serde_json::from_str(&watcher_projection_migration_shutdown(&message)).unwrap();
    assert_eq!(shutdown["type"], "shutdown");
    assert_eq!(shutdown["code"], "WATCHER_PROJECTION_MIGRATION_REQUIRED");
    assert_eq!(shutdown["retryable"], false);
    assert_eq!(shutdown["reason"], message);
}

#[test]
fn watcher_parent_init_carrier_and_legacy_cleanup_do_not_require_migration() {
    let temp = tempfile::tempdir().unwrap();
    let misc = temp.path().join("ReplicatedStorage/Misc");
    std::fs::create_dir_all(&misc).unwrap();
    let carrier = misc.join("init (Misc).luau");
    std::fs::write(&carrier, "return true\n").unwrap();
    let carrier_update = Op {
        kind: OpKind::Update,
        path: carrier,
        from: None,
        content: Some(b"return true\n".to_vec()),
        is_dir: Some(false),
    };
    assert!(
        watcher_legacy_reserved_leaf_migration(temp.path(), &carrier_update)
            .unwrap()
            .is_none()
    );

    let legacy_delete = Op {
        kind: OpKind::Delete,
        path: misc.join("init (Notifications).luau"),
        from: None,
        content: None,
        is_dir: Some(false),
    };
    assert!(
        watcher_legacy_reserved_leaf_migration(temp.path(), &legacy_delete)
            .unwrap()
            .is_none(),
        "deleting a legacy spelling must remain possible"
    );
}

#[test]
fn watcher_blocks_disk_delete_when_studio_source_is_divergent() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("Workspace/Controller.server.luau");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"disk edit\n").unwrap();
    let conflicts = ConflictEngine::new();
    conflicts.record_sync(&source, conflict::hash(b"agreed\n"), 1);
    assert_eq!(
        conflicts.on_studio_push(&source, b"studio edit\n", Some((b"disk edit\n", 2))),
        conflict::StudioDecision::Conflict
    );
    std::fs::remove_file(&source).unwrap();

    let op = Op {
        kind: OpKind::Delete,
        path: source.clone(),
        from: None,
        content: None,
        is_dir: Some(false),
    };
    let (events, mut receiver) = broadcast::channel(4);
    let mut validation = fs_safety::SyncedPathValidationCache::new(temp.path()).unwrap();
    begin_fs_destructive_preflight(&op, &mut validation, &conflicts).unwrap();
    let blocked = handle_op(op, &events, &conflicts, temp.path()).expect("delete must be blocked");

    assert_eq!(blocked.kind, "delete");
    let event: serde_json::Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
    assert_eq!(event["type"], "conflict");
    assert!(receiver.try_recv().is_err(), "no delete op may be emitted");
}

#[test]
fn deleted_directory_with_script_suffix_preserves_directory_shape_in_preflight() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("Workspace/Foo.luau");
    let child = directory.join("Child.luau");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(&child, b"disk edit\n").unwrap();
    let conflicts = ConflictEngine::new();
    conflicts.record_sync(&child, conflict::hash(b"agreed\n"), 1);
    assert_eq!(
        conflicts.on_studio_push(&child, b"studio edit\n", Some((b"disk edit\n", 2))),
        conflict::StudioDecision::Conflict
    );
    std::fs::remove_dir_all(&directory).unwrap();

    let op = Op {
        kind: OpKind::Delete,
        path: directory.clone(),
        from: None,
        content: None,
        is_dir: Some(true),
    };
    let mut validation = fs_safety::SyncedPathValidationCache::new(temp.path()).unwrap();
    begin_fs_destructive_preflight(&op, &mut validation, &conflicts).unwrap();
    match conflicts.resolve(&child, conflict::Resolution::KeepLocal) {
        Some(conflict::Resolved::DeleteStudio { path, is_dir, .. }) => {
            assert_eq!(path, directory);
            assert!(is_dir);
        }
        other => panic!("expected directory delete resolution, got {other:?}"),
    }
}

#[test]
fn push_quiet_never_suppresses_a_content_bearing_file_save() {
    let update = Op {
        kind: OpKind::Update,
        path: PathBuf::from("Workspace/Controller.server.luau"),
        from: None,
        content: Some(b"print('real editor save')\n".to_vec()),
        is_dir: Some(false),
    };
    assert!(!watcher_op_can_use_push_quiet(&update));

    let directory_add = Op {
        kind: OpKind::Add,
        path: PathBuf::from("Workspace/Controllers"),
        from: None,
        content: None,
        is_dir: Some(true),
    };
    assert!(!watcher_op_can_use_push_quiet(&directory_add));

    let directory_update = Op {
        kind: OpKind::Update,
        ..directory_add
    };
    assert!(watcher_op_can_use_push_quiet(&directory_update));

    let delete = Op {
        kind: OpKind::Delete,
        path: PathBuf::from("Workspace/Old.server.luau"),
        from: None,
        content: None,
        is_dir: Some(false),
    };
    assert!(watcher_op_can_use_push_quiet(&delete));
}

#[test]
fn watcher_blocks_disk_rename_when_studio_source_is_divergent() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("ReplicatedStorage/Old.luau");
    let to = temp.path().join("ReplicatedStorage/New.luau");
    std::fs::create_dir_all(from.parent().unwrap()).unwrap();
    std::fs::write(&from, b"disk edit\n").unwrap();
    let conflicts = ConflictEngine::new();
    conflicts.record_sync(&from, conflict::hash(b"agreed\n"), 1);
    assert_eq!(
        conflicts.on_studio_push(&from, b"studio edit\n", Some((b"disk edit\n", 2))),
        conflict::StudioDecision::Conflict
    );
    std::fs::rename(&from, &to).unwrap();

    let mut op = Op {
        kind: OpKind::Rename,
        path: to,
        from: Some(from),
        content: None,
        is_dir: Some(false),
    };
    let (events, mut receiver) = broadcast::channel(4);
    let mut validation = fs_safety::SyncedPathValidationCache::new(temp.path()).unwrap();
    hydrate_watcher_op(&mut op, &mut validation).unwrap();
    assert_eq!(op.content.as_deref(), Some(&b"disk edit\n"[..]));
    begin_fs_destructive_preflight(&op, &mut validation, &conflicts).unwrap();
    let blocked = handle_op(op, &events, &conflicts, temp.path()).expect("rename must be blocked");

    assert_eq!(blocked.kind, "rename");
    let event: serde_json::Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
    assert_eq!(event["type"], "conflict");
    assert!(receiver.try_recv().is_err(), "no rename op may be emitted");
}

#[test]
fn status_args_parse_raw_project_and_port() {
    let cli = Cli::try_parse_from([
        "rosync",
        "status",
        "--project",
        ".",
        "--port",
        "9001",
        "--raw",
    ])
    .unwrap();
    let Some(Command::Status(args)) = cli.command else {
        panic!("expected status command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.port, 9001);
    assert!(args.raw);
}

#[test]
fn watch_runner_hello_carries_the_current_protocol() {
    let hello: serde_json::Value = serde_json::from_str(&watch_hello_payload()).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["role"], "watch");
    assert_eq!(hello["clientId"], "rosync-watch");
    assert_eq!(hello["protocol"], ws::PLUGIN_PROTOCOL_VERSION);
}

#[test]
fn snapshot_and_diff_only_treat_structured_not_found_as_vanished() {
    for code in ["NOT_FOUND", "INSTANCE_NOT_FOUND"] {
        let response = serde_json::json!({
            "ok": false,
            "error": { "code": code, "message": "instance disappeared" }
        });
        assert!(response_is_not_found(&response), "{code} must be skippable");
    }

    for response in [
        serde_json::json!({
            "ok": false,
            "error": { "code": "TIMEOUT", "message": "Studio stopped responding" }
        }),
        serde_json::json!({
            "ok": false,
            "error": "permission denied"
        }),
        serde_json::json!({ "ok": true, "value": null }),
    ] {
        assert!(
            !response_is_not_found(&response),
            "transport and permission failures must remain visible: {response}"
        );
    }
}

#[test]
fn serve_args_parse_widget_owner_flags() {
    let cli = Cli::try_parse_from([
        "rosync",
        "serve",
        "--project",
        ".",
        "--widget-owned",
        "--owner-token",
        "secret",
    ])
    .unwrap();
    let Some(Command::Serve(args)) = cli.command else {
        panic!("expected serve command");
    };
    assert_eq!(args.project, PathBuf::from("."));
    assert!(args.widget_owned);
    assert_eq!(args.owner_token.as_deref(), Some("secret"));
    assert!(args.owner_token_state_file.is_none());

    let cli = Cli::try_parse_from([
        "rosync",
        "serve",
        "--project",
        ".",
        "--widget-owned",
        "--owner-token-state-file",
        "/private/widget/state.json",
    ])
    .unwrap();
    let Some(Command::Serve(args)) = cli.command else {
        panic!("expected serve command");
    };
    assert!(args.owner_token.is_none());
    assert_eq!(
        args.owner_token_state_file.as_deref(),
        Some(std::path::Path::new("/private/widget/state.json"))
    );

    assert!(Cli::try_parse_from([
        "rosync",
        "serve",
        "--project",
        ".",
        "--owner-token",
        "secret",
        "--owner-token-state-file",
        "/private/widget/state.json",
    ])
    .is_err());
}

#[test]
fn widget_owner_state_file_reads_only_the_narrow_token_key() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("state.json");
    let unrelated_secret = "must-not-appear-in-errors";
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "secrets": { "robloxCloudApiKey": unrelated_secret },
            "state": { "daemonOwnerToken": "0123456789abcdef0123456789abcdef" }
        }))
        .unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_widget_owner_token_state_file(&path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("mode 0600"));
        assert!(!error.contains(unrelated_secret));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert_eq!(
        read_widget_owner_token_state_file(&path).unwrap(),
        "0123456789abcdef0123456789abcdef"
    );

    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "secrets": { "robloxCloudApiKey": unrelated_secret },
            "state": {}
        }))
        .unwrap(),
    )
    .unwrap();
    let error = read_widget_owner_token_state_file(&path)
        .unwrap_err()
        .to_string();
    assert!(!error.contains(unrelated_secret));
    assert!(error.contains("state.daemonOwnerToken"));
}

#[test]
fn conflicts_resolve_and_watch_accept_project() {
    let cli = Cli::try_parse_from(["rosync", "conflicts", "--project", ".", "--raw"]).unwrap();
    let Some(Command::Conflicts(args)) = cli.command else {
        panic!("expected conflicts command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert!(args.raw);

    let cli = Cli::try_parse_from([
        "rosync",
        "resolve",
        "--project",
        ".",
        "--path",
        "ReplicatedStorage/Foo.luau",
        "--disk",
    ])
    .unwrap();
    let Some(Command::Resolve(args)) = cli.command else {
        panic!("expected resolve command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.path, "ReplicatedStorage/Foo.luau");
    assert!(args.disk);

    let cli = Cli::try_parse_from(["rosync", "watch", "--project", ".", "--compact"]).unwrap();
    let Some(Command::Watch(args)) = cli.command else {
        panic!("expected watch command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert!(args.compact);
}

#[test]
fn daemon_hello_project_match_uses_canonical_paths() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("Project");
    std::fs::create_dir_all(&nested).unwrap();

    let hello = serde_json::json!({
        "project": nested.display().to_string(),
    });
    let canonical = std::fs::canonicalize(&nested).unwrap();
    assert!(daemon_hello_matches_project(&hello, &canonical));

    let other = dir.path().join("Other");
    std::fs::create_dir_all(&other).unwrap();
    let other_canonical = std::fs::canonicalize(other).unwrap();
    assert!(!daemon_hello_matches_project(&hello, &other_canonical));
}

#[test]
fn lifecycle_matching_external_daemon_status_requires_the_same_project() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("Project");
    let other = dir.path().join("Other");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let project = std::fs::canonicalize(project).unwrap();
    let other = std::fs::canonicalize(other).unwrap();
    let hello = serde_json::json!({
        "project": project.display().to_string(),
        "pid": 1234,
        "port": 8123,
        "bootId": "external-boot",
        "managed": false,
        "pluginConnected": true,
    });

    let status = matching_external_daemon_status(&project, 8123, &hello).unwrap();
    assert!(status.running);
    assert!(status.externally_managed);
    assert!(!status.managed);
    assert_eq!(status.port, Some(8123));
    assert_eq!(status.boot_id.as_deref(), Some("external-boot"));
    assert!(matching_external_daemon_status(&other, 8123, &hello).is_none());
}

#[test]
fn different_projects_share_one_cross_process_port_allocation_lock() {
    let state = tempfile::tempdir().unwrap();
    let first_project = state.path().join("First");
    let second_project = state.path().join("Second");
    std::fs::create_dir_all(&first_project).unwrap();
    std::fs::create_dir_all(&second_project).unwrap();
    let first = lifecycle::runtime_paths(state.path().to_path_buf(), &first_project);
    let second = lifecycle::runtime_paths(state.path().to_path_buf(), &second_project);

    assert_ne!(first.start_lock, second.start_lock);
    assert_eq!(
        daemon_port_allocation_lock_path(&first).unwrap(),
        daemon_port_allocation_lock_path(&second).unwrap()
    );
}

#[tokio::test]
async fn port_allocation_lock_retries_then_times_out_and_recovers() {
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("ports.start.lock");
    let held = lifecycle::StartLock::acquire_named(&path, "test port allocation").unwrap();

    let started = Instant::now();
    let error = match acquire_daemon_port_allocation_lock(&path, Duration::from_millis(75)).await {
        Ok(_) => panic!("second allocator must time out while the lock is held"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() >= Duration::from_millis(50));

    drop(held);
    let recovered = acquire_daemon_port_allocation_lock(&path, Duration::from_millis(100))
        .await
        .unwrap();
    drop(recovered);
}

#[test]
fn explicit_daemon_port_is_rechecked_under_the_allocation_lock() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(ensure_daemon_port_available(port).is_err());
    drop(listener);
    ensure_daemon_port_available(port).unwrap();
}

#[test]
fn lifecycle_secret_sources_and_metadata_are_validated() {
    assert_eq!(
        resolve_optional_secret(Some("secret".into()), None, "test").unwrap(),
        Some("secret".into())
    );
    assert!(resolve_optional_secret(Some(String::new()), None, "test").is_err());
    assert!(resolve_optional_secret(Some("secret".into()), Some("TOKEN"), "test").is_err());
    assert_eq!(
        normalize_optional_metadata(Some("  123  "), "--game-id").unwrap(),
        Some("123".into())
    );
    assert!(normalize_optional_metadata(Some("  "), "--game-id").is_err());
}

#[test]
fn idempotent_daemon_start_requires_the_original_owner_capability() {
    let record = lifecycle::RuntimeRecord {
        version: lifecycle::RUNTIME_RECORD_VERSION,
        project: "/game".into(),
        canonical_project: "/game".into(),
        pid: 41,
        port: 7878,
        boot_id: "boot".into(),
        control_token: "0123456789abcdef".into(),
        managed_by: "desktop".into(),
        log_path: "/tmp/rosync.log".into(),
        started_at: 1,
    };
    assert!(validate_existing_daemon_owner(&record, None).is_ok());
    assert!(validate_existing_daemon_owner(&record, Some("0123456789abcdef")).is_ok());
    assert!(validate_existing_daemon_owner(&record, Some("fedcba9876543210")).is_err());
    assert!(validate_existing_daemon_owner(&record, Some("short")).is_err());
}

#[test]
fn managed_daemon_close_request_pins_the_runtime_record_identity() {
    let record = lifecycle::RuntimeRecord {
        version: lifecycle::RUNTIME_RECORD_VERSION,
        project: "C:\\Game".into(),
        canonical_project: "\\\\?\\C:\\Game".into(),
        pid: 4242,
        port: 7878,
        boot_id: "boot-exact".into(),
        control_token: "0123456789abcdef".into(),
        managed_by: "desktop".into(),
        log_path: "C:\\state\\rosync.log".into(),
        started_at: 1,
    };

    let request = managed_daemon_close_request(&record, "test stop");

    assert_eq!(request["token"], "0123456789abcdef");
    assert_eq!(request["reason"], "test stop");
    assert_eq!(request["expectedBootId"], "boot-exact");
    assert_eq!(request["expectedPid"], 4242);
    assert_eq!(request["expectedPort"], 7878);
    assert_eq!(request["expectedCanonicalProject"], "\\\\?\\C:\\Game");
}

#[test]
fn transient_hello_timeout_preserves_live_runtime_record_as_unresponsive() {
    let project = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(project.path()).unwrap();
    let state = tempfile::tempdir().unwrap();
    let paths = lifecycle::runtime_paths(state.path().to_path_buf(), &canonical);
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let record = lifecycle::RuntimeRecord {
        version: lifecycle::RUNTIME_RECORD_VERSION,
        project: canonical.display().to_string(),
        canonical_project: canonical.display().to_string(),
        pid: std::process::id(),
        port,
        boot_id: "busy-boot".into(),
        control_token: "0123456789abcdef".into(),
        managed_by: "desktop".into(),
        log_path: state.path().join("daemon.log").display().to_string(),
        started_at: 1,
    };
    lifecycle::write_record(&paths.record, &record).unwrap();
    let server = std::thread::spawn(move || {
        let (_connection, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });

    let status = daemon_status(&canonical, &paths, true).unwrap();
    assert!(status.running);
    assert!(status.unresponsive);
    assert!(!status.stale);
    assert_eq!(
        lifecycle::read_record(&paths.record)
            .unwrap()
            .unwrap()
            .boot_id,
        "busy-boot"
    );
    server.join().unwrap();
}

#[test]
fn definitively_free_runtime_port_is_cleaned_as_stale() {
    let project = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(project.path()).unwrap();
    let state = tempfile::tempdir().unwrap();
    let paths = lifecycle::runtime_paths(state.path().to_path_buf(), &canonical);
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    lifecycle::write_record(
        &paths.record,
        &lifecycle::RuntimeRecord {
            version: lifecycle::RUNTIME_RECORD_VERSION,
            project: canonical.display().to_string(),
            canonical_project: canonical.display().to_string(),
            pid: u32::MAX,
            port,
            boot_id: "stale-boot".into(),
            control_token: "0123456789abcdef".into(),
            managed_by: "desktop".into(),
            log_path: state.path().join("daemon.log").display().to_string(),
            started_at: 1,
        },
    )
    .unwrap();

    let status = daemon_status(&canonical, &paths, true).unwrap();
    assert!(!status.running);
    assert!(!status.unresponsive);
    assert!(status.stale);
    assert!(lifecycle::read_record(&paths.record).unwrap().is_none());
}

#[test]
fn cross_manager_daemon_is_external_before_capability_validation() {
    let mut status = DaemonLifecycleStatus {
        ok: true,
        running: true,
        unresponsive: false,
        managed: true,
        managed_by: Some("cli".into()),
        project: "/game".into(),
        canonical_project: "/game".into(),
        pid: Some(41),
        port: Some(7878),
        base_url: Some("http://127.0.0.1:7878".into()),
        boot_id: Some("cli-boot".into()),
        log_path: Some("/private/manager.log".into()),
        started_at: Some(1),
        plugin_connected: Some(true),
        stale: false,
        externally_managed: false,
    };

    classify_running_daemon_for_manager(&mut status, "desktop");

    assert!(status.externally_managed);
    assert_eq!(status.managed_by.as_deref(), Some("cli"));
    assert!(status.log_path.is_none());
}

#[tokio::test]
async fn daemon_start_returns_cross_manager_boot_without_testing_its_secret_or_mutating_config() {
    let project_root = tempfile::tempdir().unwrap();
    let state_root = tempfile::tempdir().unwrap();
    let canonical_project = std::fs::canonicalize(project_root.path()).unwrap();
    let mut initial_config = project_config::ProjectConfig::default_for(&canonical_project);
    initial_config.game_id = Some("original-game".into());
    initial_config.group_id = Some("original-group".into());
    initial_config.place_ids = vec!["original-place".into()];
    project_config::write(&canonical_project, &initial_config).unwrap();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let paths = daemon_runtime_paths(Some(state_root.path()), &canonical_project).unwrap();
    let record = lifecycle::RuntimeRecord {
        version: lifecycle::RUNTIME_RECORD_VERSION,
        project: canonical_project.display().to_string(),
        canonical_project: canonical_project.display().to_string(),
        pid: 4242,
        port,
        boot_id: "cli-owned-boot".into(),
        control_token: "cli-owner-capability".into(),
        managed_by: "cli".into(),
        log_path: state_root.path().join("private.log").display().to_string(),
        started_at: 1,
    };
    lifecycle::write_record(&paths.record, &record).unwrap();
    let hello_project = canonical_project.display().to_string();
    let server = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut connection, _) = listener.accept().unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0_u8; 2048];
        let count = connection.read(&mut request).unwrap();
        assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /hello "));
        let body = serde_json::json!({
            "managed": true,
            "managedBy": "cli",
            "project": hello_project,
            "bootId": "cli-owned-boot",
            "pid": 4242,
            "port": port,
            "startedAt": 1,
            "pluginConnected": true,
        })
        .to_string();
        write!(
                connection,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
    });

    let status = daemon_start(DaemonStartArgs {
        project: canonical_project.clone(),
        port: Some(port),
        managed_by: "desktop".into(),
        owner_token: Some("different-desktop-capability".into()),
        owner_token_env: None,
        game_id: Some("replacement-game".into()),
        group_id: Some("replacement-group".into()),
        place_id: vec!["replacement-place".into()],
        projects_root: None,
        data_dir: Some(state_root.path().to_path_buf()),
        timeout: 1.0,
        parent_stdin_lease: false,
        raw: true,
    })
    .await
    .unwrap();
    server.join().unwrap();

    assert!(status.running);
    assert!(status.externally_managed);
    assert_eq!(status.managed_by.as_deref(), Some("cli"));
    assert!(status.log_path.is_none());
    let serialized = serde_json::to_string(&status).unwrap();
    assert!(!serialized.contains("cli-owner-capability"));
    assert!(!serialized.contains("different-desktop-capability"));
    let persisted = project_config::read_from_disk(&canonical_project)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.game_id.as_deref(), Some("original-game"));
    assert_eq!(persisted.group_id.as_deref(), Some("original-group"));
    assert_eq!(persisted.place_ids, ["original-place"]);
}

#[tokio::test]
async fn daemon_start_rejects_wrong_capability_before_mutating_config() {
    let project_root = tempfile::tempdir().unwrap();
    let state_root = tempfile::tempdir().unwrap();
    let canonical_project = std::fs::canonicalize(project_root.path()).unwrap();
    let mut initial_config = project_config::ProjectConfig::default_for(&canonical_project);
    initial_config.game_id = Some("owned-game".into());
    initial_config.group_id = Some("owned-group".into());
    initial_config.place_ids = vec!["owned-place".into()];
    project_config::write(&canonical_project, &initial_config).unwrap();

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let paths = daemon_runtime_paths(Some(state_root.path()), &canonical_project).unwrap();
    let record = lifecycle::RuntimeRecord {
        version: lifecycle::RUNTIME_RECORD_VERSION,
        project: canonical_project.display().to_string(),
        canonical_project: canonical_project.display().to_string(),
        pid: 4343,
        port,
        boot_id: "desktop-owned-boot".into(),
        control_token: "original-desktop-capability".into(),
        managed_by: "desktop".into(),
        log_path: state_root.path().join("private.log").display().to_string(),
        started_at: 1,
    };
    lifecycle::write_record(&paths.record, &record).unwrap();
    let hello_project = canonical_project.display().to_string();
    let server = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut connection, _) = listener.accept().unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0_u8; 2048];
        let count = connection.read(&mut request).unwrap();
        assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /hello "));
        let body = serde_json::json!({
            "managed": true,
            "managedBy": "desktop",
            "project": hello_project,
            "bootId": "desktop-owned-boot",
            "pid": 4343,
            "port": port,
            "startedAt": 1,
            "pluginConnected": true,
        })
        .to_string();
        write!(
                connection,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
    });

    let error = daemon_start(DaemonStartArgs {
        project: canonical_project.clone(),
        port: Some(port),
        managed_by: "desktop".into(),
        owner_token: Some("different-desktop-capability".into()),
        owner_token_env: None,
        game_id: Some("replacement-game".into()),
        group_id: Some("replacement-group".into()),
        place_id: vec!["replacement-place".into()],
        projects_root: None,
        data_dir: Some(state_root.path().to_path_buf()),
        timeout: 1.0,
        parent_stdin_lease: false,
        raw: true,
    })
    .await
    .unwrap_err()
    .to_string();
    server.join().unwrap();

    assert!(error.contains("different lifecycle capability"));
    let persisted = project_config::read_from_disk(&canonical_project)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.game_id.as_deref(), Some("owned-game"));
    assert_eq!(persisted.group_id.as_deref(), Some("owned-group"));
    assert_eq!(persisted.place_ids, ["owned-place"]);
}

#[test]
fn lifecycle_json_does_not_expose_the_control_token() {
    let status = DaemonLifecycleStatus {
        ok: true,
        running: true,
        unresponsive: false,
        managed: true,
        managed_by: Some("desktop".into()),
        project: "/tmp/project".into(),
        canonical_project: "/tmp/project".into(),
        pid: Some(1234),
        port: Some(8123),
        base_url: Some("http://127.0.0.1:8123".into()),
        boot_id: Some("boot".into()),
        log_path: Some("/tmp/daemon.log".into()),
        started_at: Some(1),
        plugin_connected: Some(false),
        stale: false,
        externally_managed: false,
    };
    let value = serde_json::to_value(status).unwrap();
    assert!(value.get("controlToken").is_none());
    assert!(value.get("ownerToken").is_none());
}

#[test]
fn lifecycle_cli_accepts_env_tokens_and_rejects_duplicate_sources() {
    let cli = Cli::try_parse_from([
        "rosync",
        "daemon",
        "start",
        "--project",
        ".",
        "--owner-token-env",
        "ROSYNC_DESKTOP_TOKEN",
    ])
    .unwrap();
    let Some(Command::Daemon(DaemonArgs {
        command: DaemonCommand::Start(args),
    })) = cli.command
    else {
        panic!("expected daemon start command");
    };
    assert_eq!(
        args.owner_token_env.as_deref(),
        Some("ROSYNC_DESKTOP_TOKEN")
    );
    assert!(args.owner_token.is_none());

    assert!(Cli::try_parse_from([
        "rosync",
        "daemon",
        "start",
        "--project",
        ".",
        "--owner-token",
        "secret",
        "--owner-token-env",
        "ROSYNC_DESKTOP_TOKEN",
    ])
    .is_err());

    let cli = Cli::try_parse_from([
        "rosync",
        "serve",
        "--project",
        ".",
        "--managed",
        "--control-token-env",
        "ROSYNC_DAEMON_CONTROL_TOKEN",
    ])
    .unwrap();
    let Some(Command::Serve(args)) = cli.command else {
        panic!("expected serve command");
    };
    assert!(args.managed);
    assert_eq!(
        args.control_token_env.as_deref(),
        Some("ROSYNC_DAEMON_CONTROL_TOKEN")
    );
    assert!(args.control_token.is_none());
}

#[test]
fn daemon_build_label_distinguishes_commit_and_dirty_tree() {
    assert_eq!(
        daemon_build_label("0.3.0", "abc123def456", false),
        "rosync 0.3.0 (abc123def456)"
    );
    assert_eq!(
        daemon_build_label("0.3.0", "abc123def456", true),
        "rosync 0.3.0 (abc123def456, dirty)"
    );
}

#[test]
fn parent_stdin_lease_is_scoped_to_tauri_lifecycle_commands() {
    let cli = Cli::try_parse_from([
        "rosync",
        "daemon",
        "start",
        "--project",
        ".",
        "--parent-stdin-lease",
    ])
    .unwrap();
    let Some(Command::Daemon(DaemonArgs {
        command: DaemonCommand::Start(start),
    })) = cli.command
    else {
        panic!("expected daemon start command");
    };
    assert!(start.parent_stdin_lease);

    let cli = Cli::try_parse_from([
        "rosync",
        "daemon",
        "status",
        "--project",
        ".",
        "--parent-stdin-lease",
    ])
    .unwrap();
    let Some(Command::Daemon(DaemonArgs {
        command: DaemonCommand::Status(status),
    })) = cli.command
    else {
        panic!("expected daemon status command");
    };
    assert!(status.parent_stdin_lease);

    let cli = Cli::try_parse_from(["rosync", "daemon", "start", "--project", "."]).unwrap();
    let Some(Command::Daemon(DaemonArgs {
        command: DaemonCommand::Start(start),
    })) = cli.command
    else {
        panic!("expected daemon start command");
    };
    assert!(!start.parent_stdin_lease);

    assert!(
        Cli::try_parse_from(["rosync", "serve", "--project", ".", "--parent-stdin-lease",])
            .is_err()
    );
    assert!(Cli::try_parse_from([
        "rosync",
        "daemon",
        "stop",
        "--project",
        ".",
        "--parent-stdin-lease",
    ])
    .is_err());
}

#[test]
fn parent_stdin_monitor_notifies_promptly_on_eof() {
    let (tx, rx) = std::sync::mpsc::channel();
    let monitor = std::thread::spawn(move || {
        monitor_parent_stdin(std::io::Cursor::new(Vec::<u8>::new()), move || {
            tx.send(()).unwrap();
        });
    });

    rx.recv_timeout(Duration::from_secs(1))
        .expect("stdin EOF should release the parent lease promptly");
    monitor.join().unwrap();
}

#[test]
fn refresh_args_parse_project_and_raw() {
    let cli = Cli::try_parse_from(["rosync", "refresh", "--project", ".", "--raw"]).unwrap();
    let Some(Command::Refresh(args)) = cli.command else {
        panic!("expected refresh command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert!(args.raw);
}

#[test]
fn lint_args_parse_multiple_paths_and_scope_flags() {
    let cli = Cli::try_parse_from([
        "rosync",
        "lint",
        "--project",
        ".",
        "--path",
        "ReplicatedStorage/Client",
        "--path",
        "ServerScriptService/Server",
        "--ignore",
        "**/Generated/**",
        "--scope-only",
        "--summary",
    ])
    .unwrap();
    let Some(Command::Lint(args)) = cli.command else {
        panic!("expected lint command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(
        args.paths,
        vec![
            PathBuf::from("ReplicatedStorage/Client"),
            PathBuf::from("ServerScriptService/Server")
        ]
    );
    assert_eq!(args.ignores, vec!["**/Generated/**"]);
    assert!(args.scope_only);
    assert!(args.summary);
    assert!(!args.no_vendor_ignores);
    assert_eq!(args.port, DEFAULT_DAEMON_PORT);
    assert_eq!(args.data_model, LintDataModelMode::Auto);
    assert!(!args.raw);
    assert_eq!(args.compile, LintCompileMode::Auto);
    assert!(args.luau_compile.is_none());

    let cli = Cli::try_parse_from(["rosync", "lint", "--owned-only", "--path", "A.luau"]).unwrap();
    let Some(Command::Lint(args)) = cli.command else {
        panic!("expected lint command");
    };
    assert!(args.scope_only);

    let cli = Cli::try_parse_from([
        "rosync",
        "lint",
        "--compile",
        "required",
        "--luau-compile",
        "/tmp/luau-compile",
    ])
    .unwrap();
    let Some(Command::Lint(args)) = cli.command else {
        panic!("expected lint command");
    };
    assert_eq!(args.compile, LintCompileMode::Required);
    assert_eq!(
        args.luau_compile.unwrap(),
        PathBuf::from("/tmp/luau-compile")
    );

    let cli = Cli::try_parse_from([
        "rosync",
        "lint",
        "--port",
        "9004",
        "--data-model",
        "studio",
        "--raw",
    ])
    .unwrap();
    let Some(Command::Lint(args)) = cli.command else {
        panic!("expected lint command");
    };
    assert_eq!(args.port, 9004);
    assert_eq!(args.data_model, LintDataModelMode::Studio);
    assert!(args.raw);
}

#[test]
fn lint_extra_argument_detection_preserves_named_definition_sets() {
    let strings =
        |values: &[&str]| -> Vec<String> { values.iter().map(|value| value.to_string()).collect() };

    assert!(!extra_args_include_roblox_definitions(&strings(&[
        "--definitions:@testez=types/testez.d.luau"
    ])));
    assert!(extra_args_include_roblox_definitions(&strings(&[
        "--definitions:@roblox=types/custom.d.luau"
    ])));
    assert!(extra_args_include_roblox_definitions(&strings(&[
        "--definitions",
        "@roblox=types/custom.d.luau"
    ])));
    assert!(extra_args_include_roblox_definitions(&strings(&[
        "--definitions",
        "types/legacy.d.luau"
    ])));
    assert!(extra_args_include_platform(&strings(&[
        "--platform=roblox"
    ])));
    assert!(extra_args_use_plain_formatter(&strings(&[
        "--formatter=plain"
    ])));
    assert!(extra_args_use_plain_formatter(&strings(&[
        "--formatter",
        "plain"
    ])));
    assert!(!extra_args_use_plain_formatter(&strings(&[
        "--formatter=gnu"
    ])));
    assert!(extra_args_include_settings(&strings(&[
        "--settings",
        "lint-settings.json"
    ])));
    assert!(extra_args_disable_strict_datamodel(&strings(&[
        "--no-strict-dm-types"
    ])));
}

#[test]
fn lint_diagnostic_parser_preserves_ranges_and_messages() {
    let root = PathBuf::from("/tmp/project");
    let diagnostic = parse_lint_diagnostic(
            &root,
            "ReplicatedStorage/Main.luau [game/ReplicatedStorage/Main](2,7,2,19): TypeError: expected boolean, got string",
        )
        .unwrap();
    assert_eq!(diagnostic.path, root.join("ReplicatedStorage/Main.luau"));
    assert_eq!(diagnostic.line, 2);
    assert_eq!(diagnostic.column, 7);
    assert_eq!(diagnostic.end_line, Some(2));
    assert_eq!(diagnostic.end_column, Some(19));
    assert_eq!(diagnostic.category, "TypeError");
    assert_eq!(diagnostic.message, "expected boolean, got string");

    let diagnostic = parse_lint_diagnostic(
            &root,
            "ServerScriptService/Server/MarketService/init (MarketService).luau [game/ServerScriptService/Server/MarketService](12,3): TypeError: bad return type",
        )
        .unwrap();
    assert_eq!(
        diagnostic.path,
        root.join("ServerScriptService/Server/MarketService/init (MarketService).luau")
    );
    assert_eq!(diagnostic.line, 12);
    assert_eq!(diagnostic.column, 3);
    assert_eq!(diagnostic.message, "bad return type");

    let diagnostic = parse_lint_diagnostic(
        &root,
        "ReplicatedStorage/Main.luau:8.4-8.16: TypeError: GNU formatter error",
    )
    .unwrap();
    assert_eq!(diagnostic.path, root.join("ReplicatedStorage/Main.luau"));
    assert_eq!((diagnostic.line, diagnostic.column), (8, 4));
    assert_eq!(
        (diagnostic.end_line, diagnostic.end_column),
        (Some(8), Some(16))
    );
    assert_eq!(diagnostic.message, "GNU formatter error");

    let diagnostic = parse_lint_diagnostic(
        &root,
        "ReplicatedStorage/Main.luau:8:4-16: (W0) TypeError: plain formatter error",
    )
    .unwrap();
    assert_eq!(diagnostic.path, root.join("ReplicatedStorage/Main.luau"));
    assert_eq!((diagnostic.line, diagnostic.column), (8, 4));
    assert_eq!(
        (diagnostic.end_line, diagnostic.end_column),
        (Some(8), Some(16))
    );
    assert_eq!(diagnostic.category, "TypeError");
    assert_eq!(diagnostic.message, "plain formatter error");

    let diagnostic = parse_lint_diagnostic(
            &root,
            "ReplicatedStorage/Main.luau [game/ReplicatedStorage/Foo](1,2): TypeError: decoy](21,7): TypeError: real default error",
        )
        .unwrap();
    assert_eq!(diagnostic.path, root.join("ReplicatedStorage/Main.luau"));
    assert_eq!((diagnostic.line, diagnostic.column), (21, 7));
    assert_eq!(diagnostic.message, "real default error");

    let diagnostic = parse_lint_diagnostic(
            &root,
            "ReplicatedStorage/Main.luau [game/ReplicatedStorage/Foo]:1.2-1.3: TypeError: decoy]:22.8-22.19: TypeError: real GNU error",
        )
        .unwrap();
    assert_eq!(diagnostic.path, root.join("ReplicatedStorage/Main.luau"));
    assert_eq!((diagnostic.line, diagnostic.column), (22, 8));
    assert_eq!(
        (diagnostic.end_line, diagnostic.end_column),
        (Some(22), Some(19))
    );
    assert_eq!(diagnostic.message, "real GNU error");

    let output = "[INFO] loading definitions\nReplicatedStorage/Main.luau:8.4-8.16: TypeError: GNU formatter error\nfatal configuration error\n";
    assert_eq!(
        lint_unparsed_lines(&root, output),
        vec!["[INFO] loading definitions", "fatal configuration error"]
    );
    assert!(lint_has_unparsed_failure(&root, output));
    assert!(!lint_has_unparsed_failure(
        &root,
        "[INFO] loading definitions\nReplicatedStorage/Main.luau(1,1): TypeError: hidden\n"
    ));
    assert!(!lint_has_unparsed_failure(
            &root,
            "[INFO] loading definitions\n[WARN] client does not allow didChangeWatchedFiles registration - automatic updating on sourcemap changes disabled\nReplicatedStorage/Main.luau(1,1): TypeError: hidden\n"
        ));
    assert!(lint_has_unparsed_failure(
            &root,
            "[INFO] loading definitions\n[ERROR] failed to load configuration\nReplicatedStorage/Main.luau(1,1): TypeError: hidden\n"
        ));
    assert!(lint_analyzer_effective_success(false, true, 1, 1, false));
    assert!(lint_analyzer_effective_success(true, true, 1, 0, true));
    assert!(lint_analyzer_effective_success(true, false, 1, 0, false));
    assert!(!lint_analyzer_effective_success(true, false, 1, 0, true));
    assert!(!lint_analyzer_effective_success(true, false, 0, 0, false));
}

#[test]
fn lint_strict_settings_and_version_parsing_are_pinned() {
    let path = write_temp_lint_settings().unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(value["luau-lsp.diagnostics.strictDatamodelTypes"], true);
    assert_eq!(value["luau-lsp.platform.type"], "roblox");
    assert_eq!(parse_semver_triplet("1.68.1"), Some((1, 68, 1)));
    assert_eq!(parse_semver_triplet("luau-lsp v1.68.1"), Some((1, 68, 1)));
    assert_eq!(parse_semver_triplet("unknown"), None);
}

#[test]
fn bundled_roblox_definitions_match_pinned_hash() {
    use sha2::{Digest as _, Sha256};

    let bytes = include_bytes!("../../tools/luau-lsp/roblox/globalTypes.d.luau");
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        ROBLOX_DEFINITIONS_SHA256
    );
}

#[test]
fn doctor_reports_lint_and_editor_definition_snapshots_separately() {
    let dir = tempfile::tempdir().unwrap();
    let editor_copy = dir.path().join(snapshot::ROBLOX_DEFINITIONS_PATH);
    std::fs::create_dir_all(editor_copy.parent().unwrap()).unwrap();
    std::fs::write(&editor_copy, "stale definitions\n").unwrap();

    let check = check_luau_definitions(dir.path());
    assert_eq!(check.status, DoctorStatus::Warn);
    assert!(check.detail.contains("lint:"), "{}", check.detail);
    assert!(check.detail.contains("editor:"), "{}", check.detail);
    assert!(check.detail.contains("stale sha256"), "{}", check.detail);
}

#[test]
fn doctor_flags_obsolete_luaurc_definitions_key() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(snapshot::LUAURC),
        r#"{"languageMode":"strict","definitions":["old.d.luau"]}"#,
    )
    .unwrap();
    let check = check_luaurc(dir.path());
    assert_eq!(check.status, DoctorStatus::Warn);
    assert!(check.detail.contains("unsupported `definitions`"));

    std::fs::write(
        dir.path().join(snapshot::LUAURC),
        r#"{"languageMode":"strict"}"#,
    )
    .unwrap();
    let check = check_luaurc(dir.path());
    assert_eq!(check.status, DoctorStatus::Ok);
    assert!(check.detail.contains("languageMode=strict"));

    std::fs::write(dir.path().join(snapshot::LUAURC), "[]").unwrap();
    assert_eq!(check_luaurc(dir.path()).status, DoctorStatus::Fail);

    std::fs::write(
        dir.path().join(snapshot::LUAURC),
        r#"{"languageMode":"strcit"}"#,
    )
    .unwrap();
    assert_eq!(check_luaurc(dir.path()).status, DoctorStatus::Fail);
}

#[cfg(unix)]
#[test]
fn doctor_refuses_linked_project_tooling_paths() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let luaurc_sentinel = external.path().join("outside.luaurc");
    std::fs::write(&luaurc_sentinel, r#"{"languageMode":"strict"}"#).unwrap();
    symlink(&luaurc_sentinel, project.path().join(snapshot::LUAURC)).unwrap();
    assert_eq!(check_luaurc(project.path()).status, DoctorStatus::Fail);
    assert_eq!(
        std::fs::read_to_string(&luaurc_sentinel).unwrap(),
        r#"{"languageMode":"strict"}"#
    );

    std::fs::remove_file(project.path().join(snapshot::LUAURC)).unwrap();
    symlink(external.path(), project.path().join("tools")).unwrap();
    assert_eq!(
        check_luau_definitions(project.path()).status,
        DoctorStatus::Fail
    );
    assert!(!external.path().join("luau-lsp").exists());
}

#[cfg(unix)]
#[test]
fn doctor_rejects_a_direct_project_root_symlink() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let physical = temporary.path().join("physical");
    let linked = temporary.path().join("linked");
    std::fs::create_dir(&physical).unwrap();
    symlink(&physical, &linked).unwrap();

    assert_eq!(check_project_path(&linked).status, DoctorStatus::Fail);
}

#[test]
fn lint_compile_source_collection_respects_scope_and_ignores() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let main = root.join("ReplicatedStorage/Main.luau");
    let dotted_module = root.join("ReplicatedStorage/Foo.d.luau");
    let vendor = root.join("ReplicatedStorage/Packages/Dep.luau");
    let ignored = root.join("Generated/Skip.lua");
    let definitions = root.join("types.d.luau");
    for path in [&main, &dotted_module, &vendor, &ignored, &definitions] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "return true\n").unwrap();
    }

    let sources = collect_lint_compile_sources(
        root,
        &[root.to_path_buf()],
        true,
        &["**/Generated/**".to_string()],
    )
    .unwrap();
    assert_eq!(
        sources,
        vec![
            normalize_existing_path(&dotted_module),
            normalize_existing_path(&main)
        ]
    );

    let sources =
        collect_lint_compile_sources(root, std::slice::from_ref(&definitions), false, &[]).unwrap();
    assert_eq!(sources, vec![normalize_existing_path(&definitions)]);

    let sources =
        collect_lint_compile_sources(root, std::slice::from_ref(&vendor), false, &[]).unwrap();
    assert_eq!(sources, vec![normalize_existing_path(&vendor)]);

    let sources =
        collect_lint_compile_sources(root, &[main], false, &["**/Main.luau".to_string()]).unwrap();
    assert!(sources.is_empty());
}

#[cfg(unix)]
#[test]
fn lint_compile_collection_refuses_links_in_synced_services() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let workspace = project.path().join("Workspace");
    std::fs::create_dir(&workspace).unwrap();
    let sentinel = external.path().join("External.luau");
    std::fs::write(&sentinel, "return 'external'\n").unwrap();
    symlink(external.path(), workspace.join("Linked")).unwrap();

    let error =
        collect_lint_compile_sources(project.path(), &[project.path().to_path_buf()], false, &[])
            .unwrap_err();
    assert!(error.to_string().contains("linked/reparse"));
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "return 'external'\n"
    );
}

#[test]
fn lint_compile_globs_match_recursive_vendor_paths() {
    assert!(lint_glob_matches(
        "**/Packages/**",
        "ReplicatedStorage/Packages/Dep.luau"
    ));
    assert!(lint_glob_matches("**/Packages/**", "Packages/Dep.luau"));
    assert!(lint_glob_matches(
        "**/Madwork*/**",
        "ReplicatedStorage/Madwork/Profile.luau"
    ));
    assert!(lint_glob_matches(
        "**/.rosync-backups/**",
        ".rosync-backups/123/ServerScriptService/Old.server.luau"
    ));
    assert!(!lint_glob_matches(
        "**/Packages/**",
        "ReplicatedStorage/Package/Dep.luau"
    ));
}

#[test]
fn bundled_luau_compile_path_uses_platform_tool_layout() {
    let expected_name = if cfg!(windows) {
        "luau-compile.exe"
    } else {
        "luau-compile"
    };
    assert_eq!(
        bundled_luau_compile_relative_path(),
        PathBuf::from("tools")
            .join("luau")
            .join(platform_tool_triple())
            .join(expected_name)
    );
    let explicit = PathBuf::from("/definitely/custom/luau-compile");
    assert_eq!(
        resolve_luau_compile(Some(explicit.clone())),
        Some(explicit.into_os_string())
    );
}

#[test]
fn lint_compiler_auto_skips_missing_tool_but_required_rejects_it() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Main.luau");
    let missing = dir.path().join("missing-luau-compile");
    std::fs::write(&source, "return true\n").unwrap();

    let auto = run_lint_compiler(
        dir.path(),
        std::slice::from_ref(&source),
        true,
        LintCompileMode::Auto,
        Some(missing.clone()),
        false,
        &[],
    )
    .unwrap();
    assert_eq!(auto.status, "skipped");
    assert!(auto.note.unwrap().contains("could not run"));

    let required = run_lint_compiler(
        dir.path(),
        std::slice::from_ref(&source),
        true,
        LintCompileMode::Required,
        Some(missing),
        false,
        &[],
    );
    assert!(required.is_err());
    assert!(required.unwrap_err().to_string().contains("could not run"));
}

#[cfg(unix)]
#[test]
fn lint_compiler_runs_all_optimization_levels_and_reports_compile_errors() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Main.luau");
    let compiler = dir.path().join("luau-compile");
    std::fs::write(&source, "return true\n").unwrap();
    std::fs::write(
        &compiler,
        "#!/bin/sh\n\
             if [ \"$1\" = \"--help\" ]; then exit 0; fi\n\
             if [ \"$2\" = \"-O0\" ] || [ \"$2\" = \"-O2\" ]; then\n\
               echo \"$3(2,7): CompileError: Out of local registers: exceeded limit 200\" >&2\n\
               exit 1\n\
             fi\n\
             exit 0\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&compiler).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&compiler, permissions).unwrap();

    let report = run_lint_compiler(
        dir.path(),
        &[source],
        true,
        LintCompileMode::Required,
        Some(compiler),
        false,
        &[],
    )
    .unwrap();
    assert_eq!(report.status, "failed");
    assert_eq!(report.optimizations_checked, vec![0, 1, 2]);
    assert_eq!(report.failures.len(), 2);
    assert_eq!(
        report
            .failures
            .iter()
            .map(|failure| failure.optimization)
            .collect::<Vec<_>>(),
        vec![0, 2]
    );
    assert!(report.failures[0].output.contains("Out of local registers"));
    let diagnostics = lint_diagnostics(dir.path(), &report.failures[0].output);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].category, "CompileError");
}

#[test]
fn lint_summary_includes_compiler_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir
        .path()
        .join("ReplicatedStorage")
        .join("RegisterLimit.luau");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "return true\n").unwrap();
    let compiler = LintCompileReport {
        requested: "required".to_string(),
        status: "failed".to_string(),
        executable: Some("luau-compile".to_string()),
        source_files: 1,
        optimizations_checked: vec![0],
        failures: vec![LintCompileFailure {
            optimization: 0,
            batch: 1,
            exit_code: Some(1),
            output:
                "./ReplicatedStorage/RegisterLimit.luau(3,7): CompileError: exceeded limit 200\n"
                    .to_string(),
        }],
        note: None,
    };

    let (by_category, by_file) = lint_summary_counts(dir.path(), "", &compiler);
    assert_eq!(by_category.get("CompileError"), Some(&1));
    assert_eq!(
        by_file.get("ReplicatedStorage/RegisterLimit.luau"),
        Some(&1)
    );
}

#[test]
fn lint_scope_filter_keeps_only_requested_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let owned = root.join("ReplicatedStorage").join("Client");
    let vendor = root.join("ReplicatedStorage").join("Packages");
    std::fs::create_dir_all(&owned).unwrap();
    std::fs::create_dir_all(&vendor).unwrap();
    std::fs::write(owned.join("Main.luau"), "local x: number = \"bad\"\n").unwrap();
    std::fs::write(vendor.join("Dep.luau"), "local y: number = \"bad\"\n").unwrap();

    let output = "\
[INFO] sourcemap loaded
ReplicatedStorage/Client/Main.luau(1,1): TypeError: owned
ReplicatedStorage/Packages/Dep.luau(1,1): TypeError: vendor
";
    let owned = normalize_existing_path(&owned);
    let filtered = filter_lint_output_to_targets(&root, std::slice::from_ref(&owned), output);
    assert!(filtered.contains("[INFO] sourcemap loaded"));
    assert!(filtered.contains("Client/Main.luau"));
    assert!(!filtered.contains("Packages/Dep.luau"));

    let plain_output = "\
[INFO] sourcemap loaded
ReplicatedStorage/Client/Main.luau:1:1-8: (W0) TypeError: owned
ReplicatedStorage/Packages/Dep.luau:1:1-8: (W0) TypeError: vendor
";
    let filtered = filter_lint_output_to_targets(&root, std::slice::from_ref(&owned), plain_output);
    assert!(filtered.contains("[INFO] sourcemap loaded"));
    assert!(filtered.contains("Client/Main.luau"));
    assert!(!filtered.contains("Packages/Dep.luau"));
}

#[test]
fn commands_registry_contains_command_docs() {
    let bundle: serde_json::Value = serde_json::from_str(COMMANDS_BUNDLE_JSON).unwrap();
    let commands = bundle["commands"].as_array().unwrap();
    assert!(commands.iter().any(|command| command["name"] == "commands"));
    assert!(commands.iter().any(|command| command["name"] == "context"));
    assert!(commands.iter().any(|command| command["name"] == "plan"));
    assert!(commands
        .iter()
        .any(|command| command["name"] == "monetization"));
    assert!(commands.iter().any(|command| command["name"] == "get"));

    let cli = Cli::try_parse_from(["rosync", "commands", "get"]).unwrap();
    let Some(Command::Commands(args)) = cli.command else {
        panic!("expected commands command");
    };
    assert_eq!(args.name.as_deref(), Some("get"));
    assert!(!args.compact);

    let cli = Cli::try_parse_from(["rosync", "commands", "--compact"]).unwrap();
    let Some(Command::Commands(args)) = cli.command else {
        panic!("expected commands command");
    };
    assert!(args.compact);
    let compact = compact_command_registry(&bundle, Some("set")).unwrap();
    assert_eq!(compact["commands"][0]["name"], "set");
    assert_eq!(compact["commands"][0]["safety"], "mutates-studio");
    assert!(compact["commands"][0]["requires"]
        .as_array()
        .is_some_and(|requirements| requirements.iter().any(|item| item == "project")));
    let compact_commands = compact_command_registry(&bundle, Some("commands")).unwrap();
    assert_eq!(
        compact_commands["commands"][0]["requires"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    let compact_diff = compact_command_registry(&bundle, Some("diff")).unwrap();
    assert!(compact_diff["commands"][0]["requires"]
        .as_array()
        .is_some_and(|requirements| requirements.iter().any(|item| item == "studio-plugin")));
    let compact_playtest = compact_command_registry(&bundle, Some("playtest")).unwrap();
    assert!(compact_playtest["commands"][0]["subcommands"]
        .as_array()
        .is_some_and(|subcommands| subcommands.iter().any(|command| command == "run")));

    let cli = Cli::try_parse_from([
        "rosync",
        "context",
        "--project",
        ".",
        "--port",
        "9001",
        "--full-commands",
    ])
    .unwrap();
    let Some(Command::Context(args)) = cli.command else {
        panic!("expected context command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.port, 9001);
    assert!(args.full_commands);

    let cli = Cli::try_parse_from([
        "rosync",
        "plan",
        "set",
        "--path",
        "ReplicatedStorage/Config",
        "--prop",
        "Source",
        "--value",
        "\"return {}\"",
    ])
    .unwrap();
    let Some(Command::Plan(args)) = cli.command else {
        panic!("expected plan command");
    };
    match args.command {
        PlanCommand::Set(args) => {
            assert_eq!(args.path, "ReplicatedStorage/Config");
            assert_eq!(args.prop, "Source");
        }
        _ => panic!("expected plan set"),
    }
}

#[test]
fn command_docs_match_the_clap_registry_exactly() {
    fn visible_descendant_paths(command: &clap::Command, prefix: &str, paths: &mut Vec<String>) {
        for subcommand in command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set() && subcommand.get_name() != "help")
        {
            let path = if prefix.is_empty() {
                subcommand.get_name().to_string()
            } else {
                format!("{prefix} {}", subcommand.get_name())
            };
            paths.push(path.clone());
            visible_descendant_paths(subcommand, &path, paths);
        }
    }

    fn visible_leaf_paths(command: &clap::Command, prefix: &str, paths: &mut Vec<String>) {
        for subcommand in command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set() && subcommand.get_name() != "help")
        {
            let path = if prefix.is_empty() {
                subcommand.get_name().to_string()
            } else {
                format!("{prefix} {}", subcommand.get_name())
            };
            let has_visible_child = subcommand
                .get_subcommands()
                .any(|child| !child.is_hide_set() && child.get_name() != "help");
            if has_visible_child {
                visible_leaf_paths(subcommand, &path, paths);
            } else {
                paths.push(path);
            }
        }
    }

    let bundle: serde_json::Value = serde_json::from_str(COMMANDS_BUNDLE_JSON).unwrap();
    let mut documented = command_names_from_bundle(&bundle);
    documented.sort();
    let clap = Cli::command();
    let mut clap_commands: Vec<String> = clap
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| command.get_name().to_string())
        .collect();
    clap_commands.sort();
    assert_eq!(documented, clap_commands);

    let documented_commands = bundle["commands"].as_array().unwrap();
    for command in clap
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
    {
        let mut clap_subcommands = Vec::new();
        visible_descendant_paths(command, "", &mut clap_subcommands);
        clap_subcommands.sort();

        let documented_command = documented_commands
            .iter()
            .find(|entry| entry["name"] == command.get_name())
            .unwrap();
        let mut documented_subcommands: Vec<String> = documented_command
            .get("subcommands")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        documented_subcommands.extend(
            documented_command
                .get("subcommandPaths")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string),
        );
        documented_subcommands.sort();
        assert_eq!(
            documented_subcommands,
            clap_subcommands,
            "documented subcommands drifted for {}",
            command.get_name()
        );

        if let Some(metadata) = documented_command
            .get("subcommandMetadata")
            .and_then(serde_json::Value::as_object)
        {
            for (path, details) in metadata {
                assert!(
                    documented_subcommands.contains(path),
                    "metadata references undocumented subcommand path {} {path}",
                    command.get_name()
                );
                assert!(
                    details
                        .get("safety")
                        .and_then(serde_json::Value::as_str)
                        .is_some(),
                    "{} {path} metadata needs a safety class",
                    command.get_name()
                );
                assert!(
                    details
                        .get("requires")
                        .and_then(serde_json::Value::as_array)
                        .is_some(),
                    "{} {path} metadata needs a requirements array",
                    command.get_name()
                );
            }
        }
        if !clap_subcommands.is_empty() {
            let metadata = documented_command
                .get("subcommandMetadata")
                .and_then(serde_json::Value::as_object)
                .unwrap_or_else(|| {
                    panic!(
                        "{} needs metadata for its executable leaves",
                        command.get_name()
                    )
                });
            let mut leaf_paths = Vec::new();
            visible_leaf_paths(command, "", &mut leaf_paths);
            for path in leaf_paths {
                assert!(
                    metadata.contains_key(&path),
                    "{} {path} needs machine-readable safety and requirements",
                    command.get_name()
                );
            }
        }
    }
}

#[test]
fn command_audit_surface_includes_hidden_leaves_and_every_command_alias() {
    fn collect_leaf_paths(
        command: &clap::Command,
        canonical_prefix: &str,
        invocation_prefixes: &[String],
        canonical: &mut Vec<String>,
        aliases: &mut std::collections::BTreeMap<String, String>,
    ) {
        let children = command
            .get_subcommands()
            .filter(|child| child.get_name() != "help")
            .collect::<Vec<_>>();
        for child in children {
            let canonical_path = if canonical_prefix.is_empty() {
                child.get_name().to_string()
            } else {
                format!("{canonical_prefix} {}", child.get_name())
            };
            let names = std::iter::once(child.get_name())
                .chain(child.get_all_aliases())
                .collect::<Vec<_>>();
            let child_invocations = invocation_prefixes
                .iter()
                .flat_map(|prefix| {
                    names.iter().map(move |name| {
                        if prefix.is_empty() {
                            (*name).to_string()
                        } else {
                            format!("{prefix} {name}")
                        }
                    })
                })
                .collect::<Vec<_>>();
            let has_children = child
                .get_subcommands()
                .any(|grandchild| grandchild.get_name() != "help");
            if has_children {
                collect_leaf_paths(
                    child,
                    &canonical_path,
                    &child_invocations,
                    canonical,
                    aliases,
                );
                continue;
            }
            canonical.push(canonical_path.clone());
            for invocation in child_invocations {
                if invocation != canonical_path {
                    assert!(
                        aliases
                            .insert(invocation.clone(), canonical_path.clone())
                            .is_none(),
                        "duplicate command alias {invocation}"
                    );
                }
            }
        }
    }

    let mut canonical = Vec::new();
    let mut aliases = std::collections::BTreeMap::new();
    collect_leaf_paths(
        &Cli::command(),
        "",
        &[String::new()],
        &mut canonical,
        &mut aliases,
    );
    canonical.sort();
    canonical.dedup();
    assert_eq!(
        canonical.len(),
        106,
        "update the explicit command validation matrix when executable leaves change"
    );
    assert!(canonical.iter().any(|path| path == "img"));
    assert!(canonical.iter().any(|path| path == "imgs"));

    let mut expected_aliases = std::collections::BTreeMap::new();
    expected_aliases.insert("decide".to_string(), "decision".to_string());
    for (alias, canonical_group) in [
        ("gamepasses", "gamepass"),
        ("gp", "gamepass"),
        ("pass", "gamepass"),
        ("products", "product"),
        ("dp", "product"),
        ("devproduct", "product"),
    ] {
        for action in ["discover", "list", "create", "edit", "image", "images"] {
            expected_aliases.insert(
                format!("monetization {alias} {action}"),
                format!("monetization {canonical_group} {action}"),
            );
        }
    }
    assert_eq!(
        aliases, expected_aliases,
        "update alias probes in scripts/check-command-validation.mjs"
    );
}

#[test]
fn command_audit_parses_playtest_capture_and_daemon_lifecycle_boundaries() {
    let cli = Cli::try_parse_from([
        "rosync",
        "playtest",
        "capture",
        "--project",
        "/tmp/audit",
        "--context",
        "client:1",
        "--region",
        "1,2,320,180",
        "--output-size",
        "640x360",
        "--output",
        "/tmp/audit.png",
        "--timeout",
        "3",
        "--raw",
    ])
    .unwrap();
    let Some(Command::Playtest(PlaytestArgs {
        command: PlaytestCommand::Capture(capture),
    })) = cli.command
    else {
        panic!("expected playtest capture");
    };
    assert_eq!(capture.context, "client:1");
    assert_eq!(capture.region.as_deref(), Some("1,2,320,180"));
    assert_eq!(capture.output_size.as_deref(), Some("640x360"));
    assert_eq!(capture.output, PathBuf::from("/tmp/audit.png"));
    assert_eq!(capture.timeout, 3.0);
    assert!(capture.raw);

    for action in ["stop", "restart"] {
        let cli = Cli::try_parse_from([
            "rosync",
            "daemon",
            action,
            "--project",
            "/tmp/audit",
            "--data-dir",
            "/tmp/audit-state",
            "--timeout",
            "4",
            "--raw",
        ])
        .unwrap();
        let Some(Command::Daemon(daemon)) = cli.command else {
            panic!("expected daemon {action}");
        };
        match daemon.command {
            DaemonCommand::Stop(args) if action == "stop" => {
                assert_eq!(args.project, PathBuf::from("/tmp/audit"));
                assert_eq!(args.data_dir, Some(PathBuf::from("/tmp/audit-state")));
                assert_eq!(args.timeout, 4.0);
                assert!(args.raw);
            }
            DaemonCommand::Restart(args) if action == "restart" => {
                assert_eq!(args.project, PathBuf::from("/tmp/audit"));
                assert_eq!(args.data_dir, Some(PathBuf::from("/tmp/audit-state")));
                assert_eq!(args.timeout, 4.0);
                assert!(args.raw);
            }
            _ => panic!("expected daemon {action} args"),
        }
    }
}

#[test]
fn new_client_commands_parse() {
    let cli = Cli::try_parse_from([
        "rosync",
        "source",
        "--project",
        ".",
        "--path",
        "ReplicatedStorage/Client/App",
        "--disk",
        "--raw",
    ])
    .unwrap();
    let Some(Command::Source(args)) = cli.command else {
        panic!("expected source command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.path, "ReplicatedStorage/Client/App");
    assert!(args.disk);
    assert!(args.raw);

    let cli = Cli::try_parse_from(["rosync", "resolve", "--path", "a.luau", "--studio"]).unwrap();
    let Some(Command::Resolve(args)) = cli.command else {
        panic!("expected resolve command");
    };
    assert_eq!(args.path, "a.luau");
    assert!(args.studio);
    assert!(!args.disk);

    let cli =
        Cli::try_parse_from(["rosync", "repair", "sourcemap", "--output", "map.json"]).unwrap();
    let Some(Command::Repair(args)) = cli.command else {
        panic!("expected repair command");
    };
    match args.command {
        RepairCommand::Sourcemap(args) => {
            assert_eq!(args.output.unwrap(), PathBuf::from("map.json"));
        }
        RepairCommand::Tree(_) => panic!("expected repair sourcemap command"),
    }

    let cli = Cli::try_parse_from(["rosync", "repair", "tree", "--depth", "32"]).unwrap();
    let Some(Command::Repair(args)) = cli.command else {
        panic!("expected repair command");
    };
    match args.command {
        RepairCommand::Tree(args) => {
            assert_eq!(args.depth, 32);
        }
        RepairCommand::Sourcemap(_) => panic!("expected repair tree command"),
    }
}

#[test]
fn studio_clipboard_commands_parse_selection_paths_and_destination() {
    let cli = Cli::try_parse_from(["rosync", "copy", "--project", "."]).unwrap();
    let Some(Command::Copy(args)) = cli.command else {
        panic!("expected copy command");
    };
    assert!(args.path.is_empty());
    assert!(args.paths.is_empty());
    assert_eq!(args.project.unwrap(), PathBuf::from("."));

    let cli = Cli::try_parse_from([
        "rosync",
        "copy",
        "Workspace/One",
        "ReplicatedStorage/Two",
        "--path",
        "StarterGui/HUD",
        "--timeout",
        "45",
    ])
    .unwrap();
    let Some(Command::Copy(args)) = cli.command else {
        panic!("expected copy command");
    };
    assert_eq!(args.path, ["StarterGui/HUD"]);
    assert_eq!(args.paths, ["Workspace/One", "ReplicatedStorage/Two"]);
    assert_eq!(args.timeout, 45.0);

    let cli = Cli::try_parse_from([
        "rosync",
        "paste",
        "--project",
        ".",
        "--parent",
        "Workspace/Imported",
        "--no-select",
        "--raw",
    ])
    .unwrap();
    let Some(Command::Paste(args)) = cli.command else {
        panic!("expected paste command");
    };
    assert_eq!(args.to.as_deref(), Some("Workspace/Imported"));
    assert!(args.no_select);
    assert!(args.raw);
}

#[test]
fn status_json_uses_stable_keys_and_flags() {
    assert_eq!(status_json_key("project"), "project_path");
    assert_eq!(status_json_key("ro-sync.json"), "project_config");
    assert_eq!(status_json_key("writes.log"), "writes_log");

    let plugin = doctor_check("plugin", DoctorStatus::Ok, "v1, Studio test");
    let value = status_check_json(&plugin);
    assert_eq!(value["status"], "ok");
    assert_eq!(value["connected"], true);

    let config = doctor_check("ro-sync.json", DoctorStatus::Warn, "missing");
    let value = status_check_json(&config);
    assert_eq!(value["present"], false);
}

#[test]
fn upload_args_parse_project_and_bearer_auth() {
    let cli = Cli::try_parse_from([
        "rosync",
        "upload",
        "icon.png",
        "--project",
        ".",
        "--auth",
        "bearer",
        "--api-key-env",
        "ROBLOX_OAUTH_TOKEN",
    ])
    .unwrap();
    let Some(Command::Upload(args)) = cli.command else {
        panic!("expected upload command");
    };
    assert_eq!(args.inputs, vec![PathBuf::from("icon.png")]);
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.auth, ImgAuth::Bearer);
    assert_eq!(args.api_key_env.as_deref(), Some("ROBLOX_OAUTH_TOKEN"));
    assert_eq!(args.asset_type, None);
}

#[test]
fn transmit_args_parse_source_file_from_and_output() {
    let cli = Cli::try_parse_from([
        "rosync",
        "transmit",
        "--project",
        ".",
        "--source-file",
        "render.luau",
        "--from",
        "Workspace/Exports",
        "--output",
        "renders",
        "--timeout",
        "90",
    ])
    .unwrap();
    let Some(Command::Transmit(args)) = cli.command else {
        panic!("expected transmit command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.source_file.unwrap(), PathBuf::from("render.luau"));
    assert_eq!(args.from_path.unwrap(), "Workspace/Exports");
    assert_eq!(args.output, PathBuf::from("renders"));
    assert_eq!(args.timeout, 90.0);
}

#[test]
fn transmit_sanitizes_and_deduplicates_file_names() {
    let mut used = HashMap::new();
    let first = unique_transmit_stem(sanitize_transmit_stem("../Cool Ball.png"), &mut used, 0);
    let second = unique_transmit_stem(sanitize_transmit_stem("../Cool Ball.png"), &mut used, 1);
    assert_eq!(first, "Cool_Ball_png");
    assert_eq!(second, "Cool_Ball_png-2");
    assert_eq!(sanitize_transmit_stem("..."), "image");
}

#[test]
fn img_args_parse_project_and_bearer_auth() {
    let cli = Cli::try_parse_from([
        "rosync",
        "img",
        "icon.png",
        "--project",
        ".",
        "--auth",
        "bearer",
        "--api-key-env",
        "ROBLOX_OAUTH_TOKEN",
    ])
    .unwrap();
    let Some(Command::Img(args)) = cli.command else {
        panic!("expected img command");
    };
    assert_eq!(args.path, PathBuf::from("icon.png"));
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.auth, ImgAuth::Bearer);
    assert_eq!(args.api_key_env.as_deref(), Some("ROBLOX_OAUTH_TOKEN"));
}

#[test]
fn imgs_args_parse_manifest_and_concurrency() {
    let cli = Cli::try_parse_from([
        "rosync",
        "imgs",
        "icons",
        "banner.png",
        "--project",
        ".",
        "--manifest",
        "uploaded-assets.json",
        "--concurrency",
        "4",
        "--raw",
    ])
    .unwrap();
    let Some(Command::Imgs(args)) = cli.command else {
        panic!("expected imgs command");
    };
    assert_eq!(
        args.inputs,
        vec![PathBuf::from("icons"), PathBuf::from("banner.png")]
    );
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(
        args.manifest.unwrap(),
        PathBuf::from("uploaded-assets.json")
    );
    assert_eq!(args.concurrency, 4);
    assert!(args.raw);
}

#[test]
fn monetization_args_parse_aliases_and_create_entry() {
    let cli = Cli::try_parse_from([
        "rosync",
        "monetization",
        "gp",
        "create",
        "VIP 499 robux",
        "--project",
        ".",
    ])
    .unwrap();
    let Some(Command::Monetization(args)) = cli.command else {
        panic!("expected monetization command");
    };
    let MonetizationCommand::Gamepass(args) = args.command else {
        panic!("expected gamepass command");
    };
    let MonetizationAction::Create(args) = args.command else {
        panic!("expected create command");
    };
    assert_eq!(args.common.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.entries, vec!["VIP 499 robux".to_string()]);

    let spec = parse_monetization_create_entry("Coins Small 49 robux").unwrap();
    assert_eq!(spec.name, "Coins Small");
    assert_eq!(spec.price, 49);
}

#[test]
fn monetization_args_parse_product_image_by_name() {
    let cli = Cli::try_parse_from([
        "rosync",
        "monetization",
        "dp",
        "image",
        "--name",
        "Coins Small",
        "coins-small.png",
        "--project",
        ".",
    ])
    .unwrap();
    let Some(Command::Monetization(args)) = cli.command else {
        panic!("expected monetization command");
    };
    let MonetizationCommand::Product(args) = args.command else {
        panic!("expected product command");
    };
    let MonetizationAction::Image(args) = args.command else {
        panic!("expected image command");
    };
    assert_eq!(args.name.as_deref(), Some("Coins Small"));
    assert_eq!(args.file, PathBuf::from("coins-small.png"));
    assert_eq!(args.common.project.unwrap(), PathBuf::from("."));
}

#[test]
fn collect_upload_jobs_recurses_and_skips_directory_junk() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.png"), b"not a real png").unwrap();
    std::fs::write(dir.path().join("note.txt"), b"skip me").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested").join("b.jpg"), b"not a real jpg").unwrap();

    let mut failures = Vec::new();
    let jobs =
        collect_upload_jobs(&[dir.path().to_path_buf()], true, None, None, &mut failures).unwrap();
    let names: Vec<String> = jobs
        .iter()
        .filter_map(|job| {
            job.file
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(names, vec!["a.png".to_string(), "b.jpg".to_string()]);
    assert!(failures.is_empty());
}

#[test]
fn collect_upload_jobs_reports_explicit_unsupported_file() {
    let dir = tempfile::tempdir().unwrap();
    let gif = dir.path().join("bad.gif");
    std::fs::write(&gif, b"gif").unwrap();
    let mut failures = Vec::new();
    let jobs = collect_upload_jobs(&[gif], true, None, None, &mut failures).unwrap();
    assert!(jobs.is_empty());
    assert_eq!(failures.len(), 1);
    assert!(failures[0]
        .error
        .as_deref()
        .unwrap_or("")
        .contains("unsupported or ambiguous asset type"));
}

#[test]
fn upload_media_infers_common_asset_types() {
    let png = resolve_upload_media(std::path::Path::new("icon.png"), None, None, true).unwrap();
    assert_eq!(png.asset_type, UploadAssetType::Image);
    assert_eq!(png.content_type, "image/png");

    let mp3 = resolve_upload_media(std::path::Path::new("sound.mp3"), None, None, true).unwrap();
    assert_eq!(mp3.asset_type, UploadAssetType::Audio);
    assert_eq!(mp3.content_type, "audio/mpeg");

    let model = resolve_upload_media(std::path::Path::new("thing.glb"), None, None, true).unwrap();
    assert_eq!(model.asset_type, UploadAssetType::Model);
    assert_eq!(model.content_type, "model/gltf-binary");

    assert!(resolve_upload_media(std::path::Path::new("clip.rbxm"), None, None, true).is_err());
    let animation = resolve_upload_media(
        std::path::Path::new("clip.rbxm"),
        Some(UploadAssetType::Animation),
        None,
        true,
    )
    .unwrap();
    assert_eq!(animation.asset_type, UploadAssetType::Animation);
    assert_eq!(animation.content_type, "model/x-rbxm");
}

#[test]
fn active_widget_project_group_id_uses_active_project() {
    let value = serde_json::json!({
        "state": {
            "activeProjectId": "p2",
            "projects": [
                { "id": "p1", "groupId": "111" },
                { "id": "p2", "groupId": "222" }
            ]
        }
    });
    assert_eq!(group_id_from_widget_state(&value).as_deref(), Some("222"));
}

#[test]
fn snapshot_args_parse_output_project_and_port() {
    let cli = Cli::try_parse_from([
        "rosync",
        "snapshot",
        "--project",
        ".",
        "--port",
        "9002",
        "--output",
        "snapshots/live.json",
        "--raw",
    ])
    .unwrap();
    let Some(Command::Snapshot(args)) = cli.command else {
        panic!("expected snapshot command");
    };
    assert_eq!(args.project.unwrap(), PathBuf::from("."));
    assert_eq!(args.port, 9002);
    assert_eq!(args.output.unwrap(), PathBuf::from("snapshots/live.json"));
    assert!(args.raw);
}

#[test]
fn snapshot_output_path_defaults_to_project_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let out = snapshot_output_path(None, Some(dir.path()), 123).expect("path");
    assert_eq!(out, dir.path().join("rosync-snapshot-123.json"));
}

#[test]
fn snapshot_output_path_accepts_existing_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = snapshot_output_path(Some(dir.path()), None, 456).expect("path");
    assert_eq!(out, dir.path().join("rosync-snapshot-456.json"));
}

#[test]
fn snapshot_node_merges_inspections_and_sorts_children_and_tags() {
    let tree = serde_json::json!({
        "class": "DataModel",
        "name": "Game",
        "children": [
            { "class": "Folder", "name": "Zed", "children": [] },
            { "class": "Part", "name": "Alpha", "children": [] }
        ]
    });
    let mut inspections = BTreeMap::new();
    inspections.insert(
        "".into(),
        serde_json::json!({
            "class": "DataModel",
            "name": "Game",
            "path": "",
            "properties": {},
            "attributes": {},
            "tags": []
        }),
    );
    inspections.insert(
        "Alpha".into(),
        serde_json::json!({
            "class": "Part",
            "name": "Alpha",
            "path": "Alpha",
            "properties": { "Size": { "z": 1, "x": 2, "y": 3 } },
            "attributes": { "Health": 100 },
            "tags": ["Enemy", "A"]
        }),
    );
    inspections.insert(
        "Zed".into(),
        serde_json::json!({
            "class": "Folder",
            "name": "Zed",
            "path": "Zed",
            "properties": {},
            "attributes": {},
            "tags": []
        }),
    );

    let node = build_snapshot_node(&tree, "", &inspections);
    let children = node["children"].as_array().unwrap();
    assert_eq!(children[0]["name"], "Alpha");
    assert_eq!(children[0]["tags"], serde_json::json!(["A", "Enemy"]));
    assert_eq!(children[0]["properties"]["Size"]["x"], 2);
    assert_eq!(children[1]["name"], "Zed");
}

// ---- Tier 3: set Parent guardrail -----------------------------------

fn set_args_for_parent(force_parent: bool) -> SetArgs {
    SetArgs {
        project: None,
        port: 1, // never connected — guardrail must reject before any IO
        path: Some("Workspace/Foo".into()),
        prop: Some("Parent".into()),
        value: Some("\"Workspace\"".into()),
        yes: true,
        batch: None,
        keep_going: false,
        waypoint: None,
        force_parent,
        raw: false,
    }
}

#[tokio::test]
async fn set_parent_is_refused_without_force_parent() {
    let err = run_set(set_args_for_parent(false))
        .await
        .expect_err("set Parent must be refused without --force-parent");
    let msg = format!("{err}");
    assert!(
        msg.contains("refusing to set .Parent") && msg.contains("--force-parent"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn set_parent_with_force_parent_tries_daemon() {
    // With --force-parent we must NOT hit the guardrail — instead the CLI
    // tries to contact the (nonexistent) daemon on port 1 and surfaces a
    // connection error. Either outcome proves we passed the guardrail.
    let err = run_set(set_args_for_parent(true))
        .await
        .expect_err("no daemon listening on port 1");
    let msg = format!("{err}");
    assert!(
        !msg.contains("refusing to set .Parent"),
        "--force-parent should bypass the guardrail: {msg}"
    );
}

#[tokio::test]
async fn set_batch_parent_is_refused_before_network_io() {
    let temp = tempfile::tempdir().unwrap();
    let batch = temp.path().join("writes.json");
    std::fs::write(
        &batch,
        r#"[{"path":"Workspace/Foo","prop":"Parent","value":"Workspace"}]"#,
    )
    .unwrap();
    let mut args = set_args_for_parent(false);
    args.batch = Some(batch.clone());
    args.path = None;
    args.prop = None;
    args.value = None;

    let err = run_set_batch(args, batch)
        .await
        .expect_err("batch Parent must be refused without --force-parent");
    let msg = err.to_string();
    assert!(msg.contains("batch entry 1"));
    assert!(msg.contains("--force-parent"));
    assert!(
        !msg.contains("connect"),
        "guardrail must run before network IO: {msg}"
    );
}

#[test]
fn every_documented_command_has_an_explicit_safety_class() {
    let bundle: serde_json::Value = serde_json::from_str(COMMANDS_BUNDLE_JSON).unwrap();
    let unclassified: Vec<String> = command_names_from_bundle(&bundle)
        .into_iter()
        .filter(|name| command_safety_class(name) == "unclassified-assume-mutating")
        .collect();
    assert!(
        unclassified.is_empty(),
        "commands missing an explicit safety class: {unclassified:?}"
    );
}

#[test]
fn every_documented_command_has_an_explicit_output_cost() {
    let bundle: serde_json::Value = serde_json::from_str(COMMANDS_BUNDLE_JSON).unwrap();
    let unknown: Vec<String> = command_names_from_bundle(&bundle)
        .into_iter()
        .filter(|name| command_output_cost(name) == "unknown")
        .collect();
    assert!(
        unknown.is_empty(),
        "commands missing an explicit output cost: {unknown:?}"
    );
}

#[test]
fn workflow_idempotency_records_are_content_bound_and_atomic() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("record.json");
    let outcome = serde_json::json!({
        "ok": true,
        "workflowHash": "hash-a",
        "replayed": false
    });
    write_json_atomic(&path, &outcome).unwrap();
    assert!(workflow_replay_idempotency(&path, "hash-a").unwrap());
    let collision = workflow_replay_idempotency(&path, "hash-b").unwrap_err();
    assert!(collision.to_string().contains("idempotencyKey collision"));
    assert!(temp.path().read_dir().unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("tmp-")));
}

#[test]
fn workflow_idempotency_lock_prevents_parallel_execution() {
    let temp = tempfile::tempdir().unwrap();
    let record = temp.path().join("record.json");
    let first = WorkflowIdempotencyLock::acquire(&record).unwrap();
    assert!(WorkflowIdempotencyLock::acquire(&record).is_err());
    drop(first);
    WorkflowIdempotencyLock::acquire(&record).unwrap();
}

#[test]
fn capture_photo_args_parse_defaults() {
    let cli = Cli::try_parse_from(["rosync", "capture", "photo"]).unwrap();
    let Some(Command::Capture(args)) = cli.command else {
        panic!("expected capture command");
    };
    let CaptureCommand::Photo(args) = args.command else {
        panic!("expected capture photo command");
    };

    assert!(args.project.is_none());
    assert_eq!(args.port, DEFAULT_DAEMON_PORT);
    assert!(args.focus.is_none());
    assert!(args.region.is_none());
    assert!(args.size.is_none());
    assert_eq!(args.view, CaptureView::Isometric);
    assert!(args.direction.is_none());
    assert!(args.camera_cframe.is_none());
    assert_eq!(args.padding, 1.25);
    assert_eq!(args.fov, 32.0);
    assert_eq!(args.background, CapturePhotoBackground::Transparent);
    assert!(!args.alpha_bleed);
    assert!(!args.include_world);
    assert!(!args.no_tight_crop);
    assert!(args.ui.is_none());
    assert!(args.ui_target.is_none());
    assert!(!args.include_ui);
    assert_eq!(args.delay, 0.05);
    assert_eq!(args.output, PathBuf::from("rosync-photo.png"));
    assert_eq!(args.timeout, 120.0);
    assert!(!args.raw);
}

#[test]
fn capture_photo_background_uses_standalone_wire_values() {
    assert_eq!(
        CapturePhotoBackground::Transparent.as_wire_str(),
        "transparent"
    );
    assert_eq!(CapturePhotoBackground::Scene.as_wire_str(), "scene");
    assert_eq!(CapturePhotoUiMode::None.as_wire_str(), "none");
    assert_eq!(CapturePhotoUiMode::Overlay.as_wire_str(), "overlay");
    assert_eq!(CapturePhotoUiMode::Only.as_wire_str(), "only");

    let prepared: PhotoPrepared = serde_json::from_value(serde_json::json!({
        "sessionId": "session-1",
        "width": 32,
        "height": 16,
        "byteLength": 2048,
        "background": "transparent",
        "uiMode": "only",
        "cameraCFrame": {
            "__type": "CFrame",
            "components": [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]
        },
        "uiTarget": "StarterGui/Hud/Panel",
        "uiTargetClass": "Frame",
        "fieldOfView": 45,
        "tightCrop": false,
        "regionSource": "ui-target"
    }))
    .unwrap();
    assert_eq!(prepared.background.as_deref(), Some("transparent"));
    assert_eq!(prepared.ui_mode.as_deref(), Some("only"));
    assert_eq!(
        prepared
            .camera_cframe
            .as_ref()
            .and_then(|value| value.get("__type"))
            .and_then(serde_json::Value::as_str),
        Some("CFrame")
    );
    assert_eq!(prepared.ui_target.as_deref(), Some("StarterGui/Hud/Panel"));
    assert_eq!(prepared.ui_target_class.as_deref(), Some("Frame"));
    assert_eq!(prepared.field_of_view, Some(45.0));
    assert_eq!(prepared.tight_crop, Some(false));
    assert_eq!(prepared.region_source.as_deref(), Some("ui-target"));
}

#[test]
fn capture_photo_tight_crop_defaults_to_isolated_transparent_focus_only() {
    let mut args = capture_photo_args_for_validation();
    assert!(!capture_photo_uses_tight_crop(&args));

    args.focus = Some("Workspace/Car".into());
    assert!(capture_photo_uses_tight_crop(&args));
    let request = build_capture_photo_request(
        &args,
        CapturePhotoUiMode::None,
        None,
        Some([1024, 1024]),
        None,
        None,
    );
    assert_eq!(request.get("tightCrop"), Some(&serde_json::json!(true)));

    args.no_tight_crop = true;
    assert!(!capture_photo_uses_tight_crop(&args));
    args.no_tight_crop = false;
    args.include_world = true;
    assert!(!capture_photo_uses_tight_crop(&args));
    args.include_world = false;
    args.background = CapturePhotoBackground::Scene;
    assert!(!capture_photo_uses_tight_crop(&args));

    let request = build_capture_photo_request(
        &args,
        CapturePhotoUiMode::None,
        None,
        Some([1024, 1024]),
        None,
        None,
    );
    assert_eq!(request.get("tightCrop"), Some(&serde_json::json!(false)));

    let prepared: PhotoPrepared = serde_json::from_value(serde_json::json!({
        "sessionId": "session-tight",
        "width": 1024,
        "height": 1024,
        "byteLength": 4_194_304,
        "tightCrop": true,
        "regionSource": "subject-alpha"
    }))
    .unwrap();
    assert_eq!(prepared.tight_crop, Some(true));
    assert_eq!(prepared.region_source.as_deref(), Some("subject-alpha"));
}

fn capture_photo_args_for_validation() -> CapturePhotoArgs {
    CapturePhotoArgs {
        project: None,
        port: 1,
        focus: None,
        region: None,
        size: Some("1x1".into()),
        view: CaptureView::Isometric,
        direction: None,
        camera_cframe: None,
        padding: 1.25,
        fov: 32.0,
        background: CapturePhotoBackground::Transparent,
        alpha_bleed: false,
        include_world: false,
        no_tight_crop: false,
        ui: None,
        ui_target: None,
        include_ui: false,
        delay: 0.05,
        output: PathBuf::from("unused.png"),
        timeout: 120.0,
        raw: true,
    }
}

#[tokio::test]
async fn capture_photo_rejects_timeout_and_delay_mismatch_before_network_io() {
    let mut short = capture_photo_args_for_validation();
    short.timeout = 0.5;
    let error = run_capture_photo(short).await.unwrap_err().to_string();
    assert!(error.contains("--timeout must be between 1 and 120"));
    assert!(!error.contains("connect"));

    let mut delayed = capture_photo_args_for_validation();
    delayed.timeout = 1.0;
    delayed.delay = 1.0;
    let error = run_capture_photo(delayed).await.unwrap_err().to_string();
    assert!(error.contains("--delay must be shorter than --timeout"));
    assert!(!error.contains("connect"));
}

#[tokio::test]
async fn capture_photo_no_tight_crop_requires_focus_before_network_io() {
    assert!(Cli::try_parse_from(["rosync", "capture", "photo", "--no-tight-crop",]).is_err());

    let mut args = capture_photo_args_for_validation();
    args.no_tight_crop = true;
    let error = run_capture_photo(args).await.unwrap_err().to_string();
    assert!(error.contains("--no-tight-crop requires --focus"));
    assert!(!error.contains("connect"));
}

#[test]
fn capture_scene_forwards_tight_crop_default_and_opt_out() {
    let parse = |extra: &[&str]| {
        let mut argv = vec!["rosync", "capture", "scene", "--focus", "Workspace/Car"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).unwrap();
        let Some(Command::Capture(args)) = cli.command else {
            panic!("expected capture command");
        };
        let CaptureCommand::Scene(args) = args.command else {
            panic!("expected capture scene command");
        };
        args
    };

    let photo = capture_scene_photo_args(parse(&[]));
    assert!(!photo.no_tight_crop);
    assert!(capture_photo_uses_tight_crop(&photo));

    let photo = capture_scene_photo_args(parse(&["--no-tight-crop"]));
    assert!(photo.no_tight_crop);
    assert!(!capture_photo_uses_tight_crop(&photo));
}

#[tokio::test]
async fn capture_scene_rejects_pixelated_resampling_before_network_io() {
    let error = run_capture_scene(CaptureSceneArgs {
        project: None,
        port: 1,
        focus: "Workspace/Test".into(),
        view: CaptureView::Isometric,
        padding: 1.25,
        size: "64x64".into(),
        resample: CaptureResampleMode::Pixelated,
        no_tight_crop: false,
        output: PathBuf::from("unused.png"),
        timeout: 30.0,
        raw: true,
    })
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("--resample pixelated is not supported"));
    assert!(!error.contains("connect"));
}

#[test]
fn capture_photo_close_requires_positive_plugin_confirmation() {
    assert!(confirm_photo_close_response(&serde_json::json!({
        "ok": true,
        "value": true,
    }))
    .is_ok());
    assert!(confirm_photo_close_response(&serde_json::json!({
        "ok": true,
        "value": false,
    }))
    .is_err());
    assert!(confirm_photo_close_response(&serde_json::json!({
        "ok": false,
        "error": "session still active",
    }))
    .is_err());
}

#[test]
fn capture_photo_args_parse_all_custom_flags() {
    let cli = Cli::try_parse_from([
        "rosync",
        "capture",
        "photo",
        "--project",
        "/tmp/race-stars",
        "--port",
        "9003",
        "--focus",
        "Workspace/Map/Boss",
        "--size",
        "2048x1024",
        "--view",
        "top",
        "--direction",
        "-1,2,3",
        "--padding",
        "1.75",
        "--fov",
        "50.5",
        "--background",
        "scene",
        "--alpha-bleed",
        "--include-world",
        "--no-tight-crop",
        "--include-ui",
        "--delay",
        "0.25",
        "--output",
        "captures/boss.png",
        "--timeout",
        "45",
        "--raw",
    ])
    .unwrap();
    let Some(Command::Capture(args)) = cli.command else {
        panic!("expected capture command");
    };
    let CaptureCommand::Photo(args) = args.command else {
        panic!("expected capture photo command");
    };

    assert_eq!(args.project, Some(PathBuf::from("/tmp/race-stars")));
    assert_eq!(args.port, 9003);
    assert_eq!(args.focus.as_deref(), Some("Workspace/Map/Boss"));
    assert!(args.region.is_none());
    assert_eq!(args.size.as_deref(), Some("2048x1024"));
    assert_eq!(args.view, CaptureView::Top);
    assert_eq!(args.direction.as_deref(), Some("-1,2,3"));
    assert!(args.camera_cframe.is_none());
    assert_eq!(args.padding, 1.75);
    assert_eq!(args.fov, 50.5);
    assert_eq!(args.background, CapturePhotoBackground::Scene);
    assert!(args.alpha_bleed);
    assert!(args.include_world);
    assert!(args.no_tight_crop);
    assert!(args.ui.is_none());
    assert!(args.ui_target.is_none());
    assert!(args.include_ui);
    assert_eq!(args.delay, 0.25);
    assert_eq!(args.output, PathBuf::from("captures/boss.png"));
    assert_eq!(args.timeout, 45.0);
    assert!(args.raw);
}

const VALID_CAMERA_CFRAME: &str =
    "-10,5,20,1,0,0,0,0.939692621,-0.342020143,0,0.342020143,0.939692621";

#[test]
fn capture_photo_camera_cframe_parses_with_fov_and_world_context() {
    let cli = Cli::try_parse_from([
        "rosync",
        "capture",
        "photo",
        "--focus",
        "Workspace/Car",
        "--camera-cframe",
        VALID_CAMERA_CFRAME,
        "--fov",
        "47.5",
        "--include-world",
    ])
    .unwrap();
    let Some(Command::Capture(args)) = cli.command else {
        panic!("expected capture command");
    };
    let CaptureCommand::Photo(args) = args.command else {
        panic!("expected capture photo command");
    };
    assert_eq!(args.focus.as_deref(), Some("Workspace/Car"));
    assert_eq!(args.camera_cframe.as_deref(), Some(VALID_CAMERA_CFRAME));
    assert_eq!(args.fov, 47.5);
    assert!(args.include_world);

    assert!(Cli::try_parse_from([
        "rosync",
        "capture",
        "photo",
        "--camera-cframe",
        VALID_CAMERA_CFRAME,
    ])
    .is_err());
    for conflicting in [
        ["--view", "isometric"],
        ["--direction", "1,0,0"],
        ["--padding", "1.25"],
    ] {
        assert!(Cli::try_parse_from([
            "rosync",
            "capture",
            "photo",
            "--focus",
            "Workspace/Car",
            "--camera-cframe",
            VALID_CAMERA_CFRAME,
            conflicting[0],
            conflicting[1],
        ])
        .is_err());
    }
}

#[test]
fn capture_photo_camera_cframe_parser_requires_a_rigid_right_handed_transform() {
    let components = parse_capture_camera_cframe(VALID_CAMERA_CFRAME).unwrap();
    assert_eq!(&components[..3], &[-10.0, 5.0, 20.0]);
    assert!((components[7] - 0.939692621).abs() < 1e-12);

    for invalid in [
        "",
        "0,0,0,1,0,0,0,1,0,0,0",
        "0,0,0,1,0,0,0,1,0,0,0,1,2",
        "NaN,0,0,1,0,0,0,1,0,0,0,1",
        "0,0,0,inf,0,0,0,1,0,0,0,1",
        "0,0,0,2,0,0,0,1,0,0,0,1",
        "0,0,0,1,0.5,0,0,1,0,0,0,1",
        "0,0,0,1,0,0,0,1,0,0,0,-1",
    ] {
        assert!(
            parse_capture_camera_cframe(invalid).is_err(),
            "accepted invalid camera CFrame {invalid:?}"
        );
    }
}

#[test]
fn capture_photo_camera_cframe_uses_tagged_wire_value_and_skips_auto_framing() {
    let mut args = capture_photo_args_for_validation();
    args.focus = Some("Workspace/Car".into());
    args.camera_cframe = Some(VALID_CAMERA_CFRAME.into());
    args.fov = 47.5;
    args.include_world = true;
    let camera_cframe = parse_capture_camera_cframe(VALID_CAMERA_CFRAME).unwrap();
    let request = build_capture_photo_request(
        &args,
        CapturePhotoUiMode::None,
        None,
        Some([640, 360]),
        None,
        Some(camera_cframe),
    );
    assert_eq!(
        request.get("focus"),
        Some(&serde_json::json!("Workspace/Car"))
    );
    assert_eq!(request.get("fieldOfView"), Some(&serde_json::json!(47.5)));
    assert_eq!(request.get("isolate"), Some(&serde_json::json!(false)));
    assert_eq!(
        request.get("cameraCFrame"),
        Some(&serde_json::json!({
            "__type": "CFrame",
            "components": camera_cframe,
        }))
    );
    assert!(!request.contains_key("view"));
    assert!(!request.contains_key("direction"));
    assert!(!request.contains_key("padding"));
}

#[tokio::test]
async fn capture_photo_camera_cframe_rejects_programmatic_invalid_combinations_before_connect() {
    let mut no_focus = capture_photo_args_for_validation();
    no_focus.camera_cframe = Some(VALID_CAMERA_CFRAME.into());
    let error = run_capture_photo(no_focus).await.unwrap_err().to_string();
    assert!(error.contains("--camera-cframe requires --focus"));
    assert!(!error.contains("connect"));

    let mut auto_framing = capture_photo_args_for_validation();
    auto_framing.focus = Some("Workspace/Car".into());
    auto_framing.camera_cframe = Some(VALID_CAMERA_CFRAME.into());
    auto_framing.direction = Some("1,0,0".into());
    let error = run_capture_photo(auto_framing)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot be combined"));
    assert!(!error.contains("connect"));

    let mut malformed = capture_photo_args_for_validation();
    malformed.focus = Some("Workspace/Car".into());
    malformed.camera_cframe = Some("0,0,0,1,0,0,0,1,0,0,0,-1".into());
    let error = run_capture_photo(malformed).await.unwrap_err().to_string();
    assert!(error.contains("orthonormal right-handed"));
    assert!(!error.contains("connect"));
}

#[test]
fn capture_photo_region_parses_with_optional_output_size() {
    let cli = Cli::try_parse_from([
        "rosync",
        "capture",
        "photo",
        "--region",
        "10,20,640,480",
        "--size",
        "1280x960",
    ])
    .unwrap();
    let Some(Command::Capture(args)) = cli.command else {
        panic!("expected capture command");
    };
    let CaptureCommand::Photo(args) = args.command else {
        panic!("expected capture photo command");
    };
    assert_eq!(args.region.as_deref(), Some("10,20,640,480"));
    assert_eq!(args.size.as_deref(), Some("1280x960"));
}

#[test]
fn capture_photo_ui_mode_parses_and_legacy_alias_conflicts() {
    let cli = Cli::try_parse_from([
        "rosync",
        "capture",
        "photo",
        "--ui",
        "only",
        "--size",
        "1920x1080",
    ])
    .unwrap();
    let Some(Command::Capture(args)) = cli.command else {
        panic!("expected capture command");
    };
    let CaptureCommand::Photo(args) = args.command else {
        panic!("expected capture photo command");
    };
    assert_eq!(args.ui, Some(CapturePhotoUiMode::Only));
    assert!(!args.include_ui);

    assert!(Cli::try_parse_from([
        "rosync",
        "capture",
        "photo",
        "--ui",
        "overlay",
        "--include-ui",
    ])
    .is_err());
}

#[test]
fn capture_photo_ui_target_allows_region_and_size_and_serializes_both() {
    let cli = Cli::try_parse_from([
        "rosync",
        "capture",
        "photo",
        "--ui-target",
        "StarterGui/Hud/Panel",
        "--region",
        "10,20,640,480",
        "--size",
        "1280x960",
    ])
    .unwrap();
    let Some(Command::Capture(args)) = cli.command else {
        panic!("expected capture command");
    };
    let CaptureCommand::Photo(args) = args.command else {
        panic!("expected capture photo command");
    };
    assert_eq!(args.ui_target.as_deref(), Some("StarterGui/Hud/Panel"));
    assert_eq!(args.region.as_deref(), Some("10,20,640,480"));
    assert_eq!(args.size.as_deref(), Some("1280x960"));
    assert!(args.ui.is_none());

    let region = parse_capture_region(args.region.as_deref().unwrap()).unwrap();
    let size = parse_capture_size(args.size.as_deref().unwrap()).unwrap();
    let request = build_capture_photo_request(
        &args,
        CapturePhotoUiMode::Only,
        Some(region),
        Some(size),
        None,
        None,
    );
    assert_eq!(request.get("uiMode"), Some(&serde_json::json!("only")));
    assert_eq!(
        request.get("uiTarget"),
        Some(&serde_json::json!("StarterGui/Hud/Panel"))
    );
    assert_eq!(
        request.get("nativeRect"),
        Some(&serde_json::json!({
            "x": 10,
            "y": 20,
            "width": 640,
            "height": 480,
        }))
    );
    assert_eq!(
        request.get("outputSize"),
        Some(&serde_json::json!({ "x": 1280, "y": 960 }))
    );
}

#[tokio::test]
async fn capture_photo_ui_target_rejects_incompatible_modes_before_connect() {
    let mut empty = capture_photo_args_for_validation();
    empty.ui_target = Some("   ".into());
    let error = run_capture_photo(empty).await.unwrap_err().to_string();
    assert!(error.contains("non-empty Studio instance path"));
    assert!(!error.contains("connect"));

    for mode in [CapturePhotoUiMode::None, CapturePhotoUiMode::Overlay] {
        let mut args = capture_photo_args_for_validation();
        args.ui_target = Some("StarterGui/Hud/Panel".into());
        args.ui = Some(mode);
        let error = run_capture_photo(args).await.unwrap_err().to_string();
        assert!(error.contains("implies --ui only"));
        assert!(!error.contains("connect"));
    }

    let mut include_ui = capture_photo_args_for_validation();
    include_ui.ui_target = Some("StarterGui/Hud/Panel".into());
    include_ui.include_ui = true;
    let error = run_capture_photo(include_ui).await.unwrap_err().to_string();
    assert!(error.contains("--include-ui"));
    assert!(!error.contains("connect"));

    let mut focused = capture_photo_args_for_validation();
    focused.ui_target = Some("StarterGui/Hud/Panel".into());
    focused.focus = Some("Workspace/Car".into());
    let error = run_capture_photo(focused).await.unwrap_err().to_string();
    assert!(error.contains("--ui-target cannot be combined with --focus"));
    assert!(!error.contains("connect"));

    let mut scene = capture_photo_args_for_validation();
    scene.ui_target = Some("StarterGui/Hud/Panel".into());
    scene.background = CapturePhotoBackground::Scene;
    let error = run_capture_photo(scene).await.unwrap_err().to_string();
    assert!(error.contains("requires --background transparent"));
    assert!(!error.contains("connect"));
}

#[tokio::test]
async fn capture_photo_ui_only_rejects_world_options_before_network_io() {
    let mut focused = capture_photo_args_for_validation();
    focused.ui = Some(CapturePhotoUiMode::Only);
    focused.focus = Some("Workspace/Test".into());
    let error = run_capture_photo(focused).await.unwrap_err().to_string();
    assert!(error.contains("--ui only") && error.contains("--focus"));
    assert!(!error.contains("connect"));

    let mut scene = capture_photo_args_for_validation();
    scene.ui = Some(CapturePhotoUiMode::Only);
    scene.background = CapturePhotoBackground::Scene;
    let error = run_capture_photo(scene).await.unwrap_err().to_string();
    assert!(error.contains("--ui only requires --background transparent"));
    assert!(!error.contains("connect"));
}

#[test]
fn capture_photo_size_parser_accepts_dimensions_and_rejects_invalid_values() {
    assert_eq!(parse_capture_size("1x1").unwrap(), [1, 1]);
    assert_eq!(parse_capture_size(" 2048 X 1024 ").unwrap(), [2048, 1024]);

    for invalid in [
        "",
        "1024",
        "1024x",
        "x1024",
        "1024x768x2",
        "0x1",
        "1x0",
        "NaNx1",
        "infx1",
        "-1x1",
        "1.5x2",
    ] {
        assert!(
            parse_capture_size(invalid).is_err(),
            "accepted invalid Photo size {invalid:?}"
        );
    }
}

#[test]
fn capture_photo_direction_parser_rejects_zero_nonfinite_and_malformed_values() {
    let direction = parse_capture_direction(" 1, -2.5, 3 ").unwrap();
    let magnitude = direction[0].hypot(direction[1]).hypot(direction[2]);
    assert!((magnitude - 1.0).abs() < 1e-12);
    assert!((direction[1] / direction[0] + 2.5).abs() < 1e-12);
    assert!((direction[2] / direction[0] - 3.0).abs() < 1e-12);

    let huge = parse_capture_direction("1e308,1e308,0").unwrap();
    assert!(huge.iter().all(|component| component.is_finite()));
    assert!((huge[0].hypot(huge[1]).hypot(huge[2]) - 1.0).abs() < 1e-12);

    for invalid in [
        "",
        "1,2",
        "1,2,3,4",
        "one,2,3",
        "0,0,0",
        "0,-0,0.0",
        "0.000001,0,0",
        "NaN,0,1",
        "inf,0,1",
        "-inf,0,1",
        "1.7976931348623157e308,1.7976931348623157e308,0",
    ] {
        assert!(
            parse_capture_direction(invalid).is_err(),
            "accepted invalid Photo direction {invalid:?}"
        );
    }
}

#[test]
fn capture_photo_dimensions_enforce_exact_native_caps() {
    assert_eq!(
        u64::from(PHOTO_MAX_DIMENSION) * u64::from(PHOTO_MAX_DIMENSION),
        PHOTO_MAX_PIXELS
    );
    assert!(validate_photo_dimensions(0, 1).is_err());
    assert!(validate_photo_dimensions(1, 0).is_err());
    assert!(validate_photo_dimensions(1, 1).is_ok());
    assert!(validate_photo_dimensions(PHOTO_MAX_DIMENSION, PHOTO_MAX_DIMENSION).is_ok());

    for (width, height) in [(PHOTO_MAX_DIMENSION + 1, 1), (1, PHOTO_MAX_DIMENSION + 1)] {
        let error = validate_photo_dimensions(width, height)
            .expect_err("dimensions above the Photo cap must be rejected")
            .to_string();
        assert!(error.contains("Photo limit"), "unexpected error: {error}");
    }

    let huge_error = validate_photo_dimensions(u32::MAX, u32::MAX)
        .expect_err("huge dimensions must be rejected without arithmetic overflow")
        .to_string();
    assert!(huge_error.contains("per-axis limit"));
}

#[test]
fn capture_photo_png_encoder_preserves_exact_rgba_and_dimensions() {
    let rgba = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let encoded = encode_photo_png(2, 2, &rgba).unwrap();
    assert!(encoded.starts_with(b"\x89PNG\r\n\x1a\n"));

    let decoder = png::Decoder::new(std::io::Cursor::new(&encoded));
    let mut reader = decoder.read_info().unwrap();
    let mut decoded = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut decoded).unwrap();
    assert_eq!((info.width, info.height), (2, 2));
    assert_eq!(info.color_type, png::ColorType::Rgba);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    assert_eq!(&decoded[..info.buffer_size()], &rgba);
}

#[test]
fn capture_photo_png_encoder_rejects_short_and_long_rgba_buffers() {
    for rgba in [vec![0; 15], vec![0; 17]] {
        let error = encode_photo_png(2, 2, &rgba)
            .expect_err("RGBA byte length mismatch must be rejected")
            .to_string();
        assert!(
            error.contains("expected 16 for 2x2"),
            "unexpected error: {error}"
        );
    }
}

fn tiny_test_png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&vec![0x7f; width as usize * height as usize * 4])
            .unwrap();
    }
    bytes
}

fn test_artifact_metadata(id: &str, path: PathBuf, bytes: &[u8]) -> artifact::ArtifactMetadata {
    use sha2::{Digest as _, Sha256};
    artifact::ArtifactMetadata {
        id: id.to_string(),
        filename: "capture.png".into(),
        mime: "image/png".into(),
        path,
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        created_at_unix_ms: 1,
        finalized_at_unix_ms: 2,
    }
}

async fn spawn_raw_http_responses(
    responses: Vec<Vec<u8>>,
) -> (u16, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let first_line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            recorded.lock().unwrap().push(first_line);
            socket.write_all(&response).await.unwrap();
            let _ = socket.shutdown().await;
        }
    });
    (port, requests, task)
}

fn raw_json_response(value: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap();
    let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
    response.extend_from_slice(&body);
    response
}

#[test]
fn capture_artifact_ids_are_strictly_bounded_hex() {
    assert!(validate_artifact_id(&"a".repeat(48)).is_ok());
    for invalid in [
        "a".repeat(47),
        "a".repeat(49),
        format!("{}g", "a".repeat(47)),
        "../../artifacts/capture".into(),
    ] {
        assert!(
            validate_artifact_id(&invalid).is_err(),
            "accepted {invalid}"
        );
    }
}

fn capture_screen_args_for_fallback(ui: CaptureUiMode) -> CaptureScreenArgs {
    CaptureScreenArgs {
        project: None,
        port: DEFAULT_DAEMON_PORT,
        region: None,
        output_size: None,
        ui,
        resample: CaptureResampleMode::Default,
        output: PathBuf::from("unused.png"),
        timeout: 30.0,
        raw: true,
        focus: None,
        view: None,
        padding: None,
    }
}

#[test]
fn macos_window_fallback_is_narrowly_scoped_to_ui_all_provider_errors() {
    let all = capture_screen_args_for_fallback(CaptureUiMode::All);
    let unsupported = "capture prepare: Studio screenshot provider is unsupported after explicit capture authorization: StudioCaptureService: Feature not supported yet.";
    assert_eq!(
        capture_error_allows_macos_window_fallback(&all, unsupported),
        cfg!(target_os = "macos")
    );
    assert!(!capture_error_allows_macos_window_fallback(
        &all,
        "capture prepare: PERMISSION_REQUIRED: screenshot permission is not granted"
    ));
    assert!(!capture_error_allows_macos_window_fallback(
        &all,
        "capture prepare: request timed out"
    ));

    let none = capture_screen_args_for_fallback(CaptureUiMode::None);
    assert!(!capture_error_allows_macos_window_fallback(
        &none,
        unsupported
    ));
    let mut scene = capture_screen_args_for_fallback(CaptureUiMode::All);
    scene.focus = Some("Workspace/Map".into());
    assert!(!capture_error_allows_macos_window_fallback(
        &scene,
        unsupported
    ));
}

#[test]
fn capture_provider_selection_requires_explicit_unsupported_state() {
    let native = native_capture::NativePermissionStatus {
        available: true,
        authorized: true,
    };
    assert_eq!(capture_effective_provider(true, false, native), "studio");
    assert_eq!(
        capture_effective_provider(false, true, native),
        "macos-window"
    );
    assert_eq!(capture_effective_provider(false, false, native), "none");
}

#[test]
fn capture_png_verification_decodes_structure_and_dimensions() {
    let bytes = tiny_test_png(2, 3);
    let deadline = Instant::now() + Duration::from_secs(1);
    assert_eq!(
        verify_capture_png(&bytes, Some((2, 3)), deadline).unwrap(),
        (2, 3)
    );
    assert!(verify_capture_png(&bytes, Some((3, 2)), deadline).is_err());

    let truncated = &bytes[..bytes.len() / 2];
    assert!(verify_capture_png(truncated, Some((2, 3)), deadline).is_err());
}

#[test]
fn bounded_capture_read_rejects_file_size_drift() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("capture.png");
    std::fs::write(&path, b"two bytes").unwrap();
    let mut metadata = test_artifact_metadata(&"b".repeat(48), path, b"x");
    metadata.size = 1;
    let error =
        read_bounded_capture_file(&metadata, Instant::now() + Duration::from_secs(1)).unwrap_err();
    assert!(error.contains("file size"), "unexpected error: {error}");
}

#[tokio::test]
async fn artifact_is_consumed_even_when_png_verification_fails() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"not a png".to_vec();
    let path = temp.path().join("capture.bin");
    std::fs::write(&path, &bytes).unwrap();
    let id = "c".repeat(48);
    let metadata = test_artifact_metadata(&id, path, &bytes);
    let lookup = raw_json_response(&serde_json::json!({
        "ok": true,
        "artifact": metadata,
    }));
    let consumed = raw_json_response(&serde_json::json!({
        "ok": true,
        "consumed": true,
    }));
    let (port, requests, server) = spawn_raw_http_responses(vec![lookup, consumed]).await;
    let error = materialize_capture_artifact(
        port,
        &id,
        Some(bytes.len() as u64),
        None,
        None,
        Instant::now() + Duration::from_secs(2),
        "test capture",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("not a PNG"));
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(&format!("/artifacts/{id} ")));
    assert!(requests[1].contains(&format!("/artifacts/{id}/consume ")));
}

#[tokio::test]
async fn artifact_sha_mismatch_is_rejected_and_consumed() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = tiny_test_png(1, 1);
    let path = temp.path().join("capture.png");
    std::fs::write(&path, &bytes).unwrap();
    let id = "d".repeat(48);
    let mut metadata = test_artifact_metadata(&id, path, &bytes);
    metadata.sha256 = "0".repeat(64);
    let lookup = raw_json_response(&serde_json::json!({
        "ok": true,
        "artifact": metadata,
    }));
    let consumed = raw_json_response(&serde_json::json!({
        "ok": true,
        "consumed": true,
    }));
    let (port, requests, server) = spawn_raw_http_responses(vec![lookup, consumed]).await;
    let error = materialize_capture_artifact(
        port,
        &id,
        Some(bytes.len() as u64),
        Some((1, 1)),
        None,
        Instant::now() + Duration::from_secs(2),
        "test capture",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("SHA-256 mismatch"));
    server.await.unwrap();
    assert!(requests.lock().unwrap()[1].contains("/consume "));
}

#[tokio::test]
async fn bounded_http_json_rejects_declared_oversize_response() {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        LOCAL_HTTP_MAX_JSON_BYTES + 1
    )
    .into_bytes();
    let (port, _, server) = spawn_raw_http_responses(vec![response]).await;
    let error = http_get_json_until(port, "/oversize", Instant::now() + Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(error.contains("JSON limit"), "unexpected error: {error}");
    server.await.unwrap();
}

#[tokio::test]
async fn local_http_request_obeys_one_absolute_deadline() {
    use tokio::io::AsyncReadExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let started = Instant::now();
    let error = http_get_json_until(port, "/stall", Instant::now() + Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(!error.is_empty(), "timeout should return a diagnostic");
    assert!(started.elapsed() < Duration::from_secs(1));
    server.abort();
}
