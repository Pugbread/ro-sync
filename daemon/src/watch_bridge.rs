use super::*;

/// Bridges the filesystem-watcher's typed operation/resync stream into the
/// shared `broadcast::Sender<String>` that `/events` streams. Each Op is first run
/// through `ConflictEngine::on_fs_change` so that echoes of our own writes
/// (baseline matches) are dropped and conflicts are surfaced as their own
/// event type rather than a propagation op.
pub(super) fn spawn_watch_bridge(
    watcher: Watch,
    root: PathBuf,
    events: broadcast::Sender<String>,
    conflicts: Arc<ConflictEngine>,
    push_quiet: Arc<Mutex<HashMap<PathBuf, Instant>>>,
) -> Result<(), String> {
    // Subscribe before the startup scans so a concurrent filesystem mutation
    // is queued and validated after the captured generation instead of falling
    // into a scan/subscribe gap.
    let mut rx = watcher.subscribe();
    let synced_services = snapshot::SYNCED_SERVICES
        .iter()
        .map(|service| (*service).to_string())
        .collect::<Vec<_>>();
    http::reject_legacy_reserved_init_leafs(&root, &synced_services)?;
    let initial_parent_dirs = collect_existing_parent_candidates(&root)?;
    let hydration_validation = fs_safety::SyncedPathValidationCache::new(&root)
        .map_err(|error| format!("initialize watcher hydration safety cache: {error}"))?;
    // Move the Watch into the task so the debouncer stays alive for the lifetime
    // of the daemon.
    tokio::spawn(async move {
        let _watcher = watcher;
        // Empty folders are intentionally absent from Studio. Seed every
        // existing disk directory and remember newly created ones so the first
        // script added beneath an unmaterialized folder can create its parent
        // chain before its own `set` arrives. Without this ordering, `mkdir
        // Workspace/tools`, followed later (even after a daemon restart) by
        // `Workspace/tools/Test.luau`, sends a child op whose parent does not
        // exist in Studio and the plugin has no safe way to infer it.
        let mut pending_parent_dirs = initial_parent_dirs;
        let mut hydration_validation = hydration_validation;
        let mut destructive_batch_grace_active = false;
        let mut rescanned_added_dirs: HashSet<PathBuf> = HashSet::new();
        loop {
            // The watcher publishes one debounced batch synchronously. Keep
            // the destructive grace armed until that receiver backlog drains,
            // regardless of how long a very large batch takes to process.
            if rx.is_empty() {
                destructive_batch_grace_active = false;
            }
            match rx.recv().await {
                Ok(watch::WatchEvent::Op(mut op)) => {
                    // Drop content hashes for paths touched by the watcher.
                    // Generation checks keep stale entries safe, while eager
                    // invalidation keeps the bounded cache useful during long runs.
                    http::invalidate_cached_content_hash(&op.path);
                    if let Some(from) = &op.from {
                        http::invalidate_cached_content_hash(from);
                    }
                    if is_synced_service_root_op(&op, &root) {
                        continue;
                    }
                    if let Err(error) = hydrate_watcher_op(&mut op, &mut hydration_validation) {
                        reset_watch_bridge_after_barrier(
                            &_watcher,
                            &mut rx,
                            &root,
                            &mut pending_parent_dirs,
                        );
                        rescanned_added_dirs.clear();
                        let _ = events.send(watcher_hydration_shutdown(&error));
                        continue;
                    }
                    // A time/path-only quiet token must never discard a real
                    // file save. Content-bearing add/update events flow through
                    // the conflict engine's exact normalized hash instead.
                    // Structural operations still need the token because a
                    // daemon-authored delete or rename has no surviving bytes
                    // with which to prove its identity.
                    if watcher_op_can_use_push_quiet(&op) {
                        let destination_quiet = is_push_quiet(&push_quiet, &op.path, &root);
                        let source_quiet = op
                            .from
                            .as_deref()
                            .is_some_and(|from| is_push_quiet(&push_quiet, from, &root));
                        if destination_quiet || source_quiet {
                            continue;
                        }
                    }
                    match watcher_legacy_reserved_leaf_migration(&root, &op) {
                        Ok(Some(error)) => {
                            emit_sync_error(&events, &op.path, &error);
                            reset_watch_bridge_after_barrier(
                                &_watcher,
                                &mut rx,
                                &root,
                                &mut pending_parent_dirs,
                            );
                            rescanned_added_dirs.clear();
                            let _ = events.send(watcher_projection_migration_shutdown(&error));
                            let _ = http::write_log_entry(axum::Json(serde_json::json!({
                                "source": "filesystem-sync-validation",
                                "op": match op.kind {
                                    OpKind::Add => "add",
                                    OpKind::Update => "update",
                                    OpKind::Rename => "rename",
                                    OpKind::Delete => "delete",
                                },
                                "path": op.path,
                                "from": op.from,
                                "outcome": "blocked-projection-migration",
                                "error": error,
                            })));
                            continue;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            emit_sync_error(&events, &op.path, &error);
                            reset_watch_bridge_after_barrier(
                                &_watcher,
                                &mut rx,
                                &root,
                                &mut pending_parent_dirs,
                            );
                            rescanned_added_dirs.clear();
                            let _ = events.send(watcher_hydration_shutdown(&error));
                            continue;
                        }
                    }
                    if op.kind == OpKind::Rename && op.is_dir == Some(true) {
                        if let Some(from) = &op.from {
                            rebase_pending_parent_candidates(
                                &mut pending_parent_dirs,
                                from,
                                &op.path,
                            );
                            rescanned_added_dirs.retain(|candidate| !candidate.starts_with(from));
                        }
                        rescanned_added_dirs.retain(|candidate| !candidate.starts_with(&op.path));
                    } else if op.kind == OpKind::Delete && op.is_dir != Some(false) {
                        rescanned_added_dirs.retain(|candidate| !candidate.starts_with(&op.path));
                    }
                    if op.kind == OpKind::Add && op.content.is_none() && op.is_dir == Some(true) {
                        pending_parent_dirs.insert(op.path.clone());
                    }
                    let added_directory = (op.kind == OpKind::Add && op.is_dir == Some(true))
                        .then(|| op.path.clone())
                        .filter(|directory| !rescanned_added_dirs.remove(directory));
                    for parent in
                        take_pending_parent_materializations(&op, &root, &mut pending_parent_dirs)
                    {
                        emit_op(
                            &events,
                            &root,
                            &Op {
                                kind: OpKind::Update,
                                path: parent,
                                from: None,
                                content: None,
                                is_dir: Some(true),
                            },
                        );
                    }
                    if matches!(op.kind, OpKind::Delete | OpKind::Rename) {
                        if let Err(error) = begin_fs_destructive_preflight(
                            &op,
                            &mut hydration_validation,
                            &conflicts,
                        ) {
                            emit_sync_error(&events, &op.path, &error);
                            reset_watch_bridge_after_barrier(
                                &_watcher,
                                &mut rx,
                                &root,
                                &mut pending_parent_dirs,
                            );
                            rescanned_added_dirs.clear();
                            let _ = events.send(watcher_hydration_shutdown(&error));
                            let _ = http::write_log_entry(axum::Json(serde_json::json!({
                                "source": "filesystem-sync-conflict",
                                "op": match op.kind {
                                    OpKind::Delete => "delete",
                                    OpKind::Rename => "rename",
                                    OpKind::Add | OpKind::Update => "update",
                                },
                                "path": op.path,
                                "from": op.from,
                                "outcome": "blocked-read-error",
                                "error": error,
                            })));
                            continue;
                        }
                        // ScriptDocument callbacks and filesystem notifications
                        // are independent streams. Hold the first destructive
                        // op in a burst briefly so an already-in-flight Studio
                        // source push can prove whether Studio still matches
                        // the agreed baseline. Arm the receiver batch after
                        // that one wait: every delete/rename already queued in
                        // the same burst then proceeds without another sleep.
                        if destructive_batch_needs_grace(destructive_batch_grace_active) {
                            destructive_batch_grace_active = true;
                            tokio::time::sleep(Duration::from_millis(FS_DESTRUCTIVE_PREFLIGHT_MS))
                                .await;
                        }
                    }
                    if let Some(blocked) = handle_op(op, &events, &conflicts, &root) {
                        let _ = http::write_log_entry(axum::Json(serde_json::json!({
                            "source": "filesystem-sync-conflict",
                            "op": blocked.kind,
                            "path": blocked.path,
                            "from": blocked.from,
                            "outcome": "blocked-conflict",
                        })));
                    }
                    if let Some(directory) = added_directory {
                        match collect_added_directory_descendants(
                            &directory,
                            &mut hydration_validation,
                        ) {
                            Ok(descendants) => {
                                let mut projection_problem = None;
                                for descendant in &descendants {
                                    match watcher_legacy_reserved_leaf_migration(&root, descendant)
                                    {
                                        Ok(Some(error)) => {
                                            projection_problem =
                                                Some((descendant.path.clone(), error, true));
                                            break;
                                        }
                                        Ok(None) => {}
                                        Err(error) => {
                                            projection_problem =
                                                Some((descendant.path.clone(), error, false));
                                            break;
                                        }
                                    }
                                }
                                if let Some((path, error, migration_required)) = projection_problem
                                {
                                    emit_sync_error(&events, &path, &error);
                                    reset_watch_bridge_after_barrier(
                                        &_watcher,
                                        &mut rx,
                                        &root,
                                        &mut pending_parent_dirs,
                                    );
                                    rescanned_added_dirs.clear();
                                    let shutdown = if migration_required {
                                        watcher_projection_migration_shutdown(&error)
                                    } else {
                                        watcher_hydration_shutdown(&error)
                                    };
                                    let _ = events.send(shutdown);
                                    continue;
                                }
                                if !descendants.is_empty() {
                                    pending_parent_dirs.remove(&directory);
                                }
                                for descendant in descendants {
                                    if descendant.is_dir == Some(true) {
                                        // The directory op is filtered when
                                        // this descendant is empty. Retain it
                                        // as a future parent candidate; a
                                        // later file descendant in this same
                                        // rescan removes/materializes it.
                                        pending_parent_dirs.insert(descendant.path.clone());
                                        rescanned_added_dirs.insert(descendant.path.clone());
                                    }
                                    for parent in take_pending_parent_materializations(
                                        &descendant,
                                        &root,
                                        &mut pending_parent_dirs,
                                    ) {
                                        emit_op(
                                            &events,
                                            &root,
                                            &Op {
                                                kind: OpKind::Update,
                                                path: parent,
                                                from: None,
                                                content: None,
                                                is_dir: Some(true),
                                            },
                                        );
                                    }
                                    let _ = handle_op(descendant, &events, &conflicts, &root);
                                }
                            }
                            Err(error) => {
                                reset_watch_bridge_after_barrier(
                                    &_watcher,
                                    &mut rx,
                                    &root,
                                    &mut pending_parent_dirs,
                                );
                                rescanned_added_dirs.clear();
                                let _ = events.send(watcher_hydration_shutdown(&error));
                            }
                        }
                    }
                }
                Ok(watch::WatchEvent::Resync { reason }) => {
                    reset_watch_bridge_after_barrier(
                        &_watcher,
                        &mut rx,
                        &root,
                        &mut pending_parent_dirs,
                    );
                    rescanned_added_dirs.clear();
                    let _ =
                        events.send(watcher_resync_shutdown("WATCHER_BATCH_AMBIGUOUS", &reason));
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    reset_watch_bridge_after_barrier(
                        &_watcher,
                        &mut rx,
                        &root,
                        &mut pending_parent_dirs,
                    );
                    rescanned_added_dirs.clear();
                    let _ = events.send(watcher_lag_shutdown(skipped));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Ok(())
}

pub(super) fn reset_watch_bridge_after_barrier(
    watcher: &Watch,
    receiver: &mut broadcast::Receiver<watch::WatchEvent>,
    root: &std::path::Path,
    pending_parent_dirs: &mut HashSet<PathBuf>,
) {
    watcher.discard_retained_tail(receiver);
    refresh_parent_candidates_after_barrier(root, pending_parent_dirs);
}

pub(super) fn refresh_parent_candidates_after_barrier(
    root: &std::path::Path,
    pending_parent_dirs: &mut HashSet<PathBuf>,
) {
    // A failed refresh must not retain pre-barrier candidates: those paths
    // describe a filesystem generation that the reconnect is discarding.
    *pending_parent_dirs = collect_existing_parent_candidates(root).unwrap_or_default();
}

// ScriptEditorService source notifications are debounced for 350 ms in the
// plugin. Keep this slightly above that bound so an edit and a filesystem
// delete/rename started together are ordered deterministically in practice.
pub(super) const FS_DESTRUCTIVE_PREFLIGHT_MS: u64 = 500;

pub(super) fn destructive_batch_needs_grace(grace_active: bool) -> bool {
    !grace_active
}

pub(super) fn deleted_sync_path_is_dir(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    fs_map::classify_script_file(name).is_none() && !fs_map::is_init_file(name)
}

pub(super) fn ensure_watcher_file_generation_unchanged(
    before: &fs_safety::FileGeneration,
    after: &fs_safety::FileGeneration,
    path: &std::path::Path,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    Err(format!(
        "watcher source changed while hydrating; retry exact sync: {}",
        path.display()
    ))
}

pub(super) fn read_stable_watcher_file(
    path: &std::path::Path,
    validation: &mut fs_safety::SyncedPathValidationCache,
) -> Result<Vec<u8>, String> {
    let validated = validation
        .validate(path, false)
        .map_err(|error| format!("validate watcher source {}: {error}", path.display()))?;
    let before = fs_safety::file_generation_no_follow(&validated)?;
    if before.len > fs_safety::MAX_SYNCED_SCRIPT_BYTES {
        return Err(format!(
            "watcher source exceeds {} byte limit ({} bytes): {}",
            fs_safety::MAX_SYNCED_SCRIPT_BYTES,
            before.len,
            path.display()
        ));
    }
    let bytes =
        fs_safety::read_file_no_follow_bounded(&validated, fs_safety::MAX_SYNCED_SCRIPT_BYTES)
            .map_err(|error| format!("read bounded watcher source {}: {error}", path.display()))?
            .ok_or_else(|| {
                format!(
                    "watcher source grew beyond {} byte limit while reading: {}",
                    fs_safety::MAX_SYNCED_SCRIPT_BYTES,
                    path.display()
                )
            })?;
    validation
        .validate(path, false)
        .map_err(|error| format!("revalidate watcher source {}: {error}", path.display()))?;
    let after = fs_safety::file_generation_no_follow(&validated)?;
    ensure_watcher_file_generation_unchanged(&before, &after, path)?;
    Ok(bytes)
}

pub(super) fn hydrate_watcher_op(
    op: &mut Op,
    validation: &mut fs_safety::SyncedPathValidationCache,
) -> Result<(), String> {
    if matches!(op.kind, OpKind::Add | OpKind::Update | OpKind::Rename) && op.is_dir == Some(false)
    {
        op.content = Some(read_stable_watcher_file(&op.path, validation)?);
    }
    Ok(())
}

pub(super) fn collect_added_directory_descendants(
    directory: &std::path::Path,
    validation: &mut fs_safety::SyncedPathValidationCache,
) -> Result<Vec<Op>, String> {
    let root_index = fs_safety::PortableDirectoryIndex::read(directory).map_err(|error| {
        format!(
            "scan newly added directory {}: {error}",
            directory.display()
        )
    })?;
    let mut stack = root_index
        .entries()
        .iter()
        .filter(|entry| {
            entry.kind == fs_safety::SafeEntryKind::Directory
                || fs_map::classify_script_file(&entry.fragment).is_some()
                || fs_map::is_init_file(&entry.fragment)
        })
        .map(|entry| (entry.path.clone(), entry.kind, entry.fragment.clone()))
        .collect::<Vec<_>>();
    stack.sort_by(|left, right| {
        added_subtree_priority(&left.2, left.1)
            .cmp(&added_subtree_priority(&right.2, right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    stack.reverse();

    let mut ops = Vec::new();
    while let Some((path, kind, _fragment)) = stack.pop() {
        match kind {
            fs_safety::SafeEntryKind::File => {
                let content = read_stable_watcher_file(&path, validation)?;
                ops.push(Op {
                    kind: OpKind::Add,
                    path,
                    from: None,
                    content: Some(content),
                    is_dir: Some(false),
                });
            }
            fs_safety::SafeEntryKind::Directory => {
                ops.push(Op {
                    kind: OpKind::Add,
                    path: path.clone(),
                    from: None,
                    content: None,
                    is_dir: Some(true),
                });
                let index = fs_safety::PortableDirectoryIndex::read(&path).map_err(|error| {
                    format!("scan newly added directory {}: {error}", path.display())
                })?;
                let mut children = index
                    .entries()
                    .iter()
                    .filter(|entry| {
                        entry.kind == fs_safety::SafeEntryKind::Directory
                            || fs_map::classify_script_file(&entry.fragment).is_some()
                            || fs_map::is_init_file(&entry.fragment)
                    })
                    .map(|entry| (entry.path.clone(), entry.kind, entry.fragment.clone()))
                    .collect::<Vec<_>>();
                children.sort_by(|left, right| {
                    added_subtree_priority(&left.2, left.1)
                        .cmp(&added_subtree_priority(&right.2, right.1))
                        .then_with(|| left.2.cmp(&right.2))
                });
                children.reverse();
                stack.extend(children);
            }
        }
    }
    Ok(ops)
}

pub(super) fn added_subtree_priority(fragment: &str, kind: fs_safety::SafeEntryKind) -> u8 {
    if kind == fs_safety::SafeEntryKind::File && fs_map::is_init_file(fragment) {
        0
    } else if kind == fs_safety::SafeEntryKind::Directory {
        1
    } else {
        2
    }
}

pub(super) fn begin_fs_destructive_preflight(
    op: &Op,
    validation: &mut fs_safety::SyncedPathValidationCache,
    conflicts: &ConflictEngine,
) -> Result<(), String> {
    match op.kind {
        OpKind::Delete => {
            conflicts.begin_fs_delete(
                &op.path,
                op.is_dir
                    .unwrap_or_else(|| deleted_sync_path_is_dir(&op.path)),
            );
        }
        OpKind::Rename => {
            if let Some(from) = &op.from {
                let validated = validation.validate(&op.path, false).map_err(|error| {
                    format!(
                        "validate retained rename destination {}: {error}",
                        op.path.display()
                    )
                })?;
                let metadata = fs_safety::metadata_no_follow(&validated)
                    .map_err(|error| {
                        format!(
                            "inspect retained rename destination {}: {error}",
                            op.path.display()
                        )
                    })?
                    .ok_or_else(|| {
                        format!(
                            "retained rename destination disappeared: {}",
                            op.path.display()
                        )
                    })?;
                let is_dir = metadata.is_dir();
                if op.is_dir.is_some_and(|expected| expected != is_dir) {
                    return Err(format!(
                        "retained rename destination changed filesystem shape: {}",
                        op.path.display()
                    ));
                }
                let retained_bytes = if is_dir {
                    let before =
                        fs_safety::directory_generation_no_follow(&validated).map_err(|error| {
                            format!(
                                "inspect retained rename directory {}: {error}",
                                op.path.display()
                            )
                        })?;
                    validation.validate(&op.path, false).map_err(|error| {
                        format!(
                            "revalidate retained rename directory {}: {error}",
                            op.path.display()
                        )
                    })?;
                    let after =
                        fs_safety::directory_generation_no_follow(&validated).map_err(|error| {
                            format!(
                                "reinspect retained rename directory {}: {error}",
                                op.path.display()
                            )
                        })?;
                    if before != after {
                        return Err(format!(
                            "retained rename directory changed during preflight: {}",
                            op.path.display()
                        ));
                    }
                    None
                } else {
                    Some(op.content.clone().ok_or_else(|| {
                        format!(
                            "retained file rename was not hydrated before preflight: {}",
                            op.path.display()
                        )
                    })?)
                };
                conflicts.begin_fs_rename(from, &op.path, is_dir, retained_bytes);
            }
        }
        OpKind::Add | OpKind::Update => {}
    }
    Ok(())
}

pub(super) fn watcher_lag_shutdown(skipped: u64) -> String {
    serde_json::json!({
        "type": "shutdown",
        "reason": "filesystem watcher lagged; reconnect to rebuild exact sync state",
        "code": "WATCHER_LAGGED",
        "retryable": true,
        "skipped": skipped,
    })
    .to_string()
}

pub(super) fn watcher_resync_shutdown(code: &str, reason: &str) -> String {
    serde_json::json!({
        "type": "shutdown",
        "reason": reason,
        "code": code,
        "retryable": true,
    })
    .to_string()
}

pub(super) fn watcher_hydration_shutdown(reason: &str) -> String {
    watcher_resync_shutdown("WATCHER_HYDRATION_FAILED", reason)
}

pub(super) fn watcher_projection_migration_shutdown(reason: &str) -> String {
    serde_json::json!({
        "type": "shutdown",
        "reason": reason,
        "code": http::PROJECTION_MIGRATION_REQUIRED_CODE,
        "retryable": false,
    })
    .to_string()
}

pub(super) fn watcher_legacy_reserved_leaf_migration(
    root: &std::path::Path,
    op: &Op,
) -> Result<Option<String>, String> {
    if !matches!(op.kind, OpKind::Add | OpKind::Update | OpKind::Rename) || op.is_dir == Some(true)
    {
        return Ok(None);
    }
    fs_map::legacy_reserved_init_leaf_migration_message(root, &op.path)
        .map_err(|error| format!("inspect watcher projection {}: {error}", op.path.display()))
}

pub(super) fn collect_existing_parent_candidates(
    root: &std::path::Path,
) -> Result<HashSet<PathBuf>, String> {
    let mut candidates = HashSet::new();
    for service in snapshot::SYNCED_SERVICES {
        let tree = fs_safety::capture_tree_metadata(root, service)?;
        candidates.extend(
            tree.entries()
                .iter()
                .filter(|entry| entry.kind == fs_safety::SafeEntryKind::Directory)
                .map(|entry| root.join(service).join(&entry.relative)),
        );
    }
    Ok(candidates)
}

pub(super) fn rebase_pending_parent_candidates(
    candidates: &mut HashSet<PathBuf>,
    from: &std::path::Path,
    to: &std::path::Path,
) {
    let moved = candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .strip_prefix(from)
                .ok()
                .map(|suffix| (candidate.clone(), to.join(suffix)))
        })
        .collect::<Vec<_>>();
    for (old, new) in moved {
        candidates.remove(&old);
        candidates.insert(new);
    }
}

pub(super) fn take_pending_parent_materializations(
    op: &Op,
    root: &std::path::Path,
    pending_empty_dirs: &mut HashSet<PathBuf>,
) -> Vec<PathBuf> {
    if op.content.is_none() || !matches!(op.kind, OpKind::Add | OpKind::Update) {
        return Vec::new();
    }

    let mut parents = Vec::new();
    let mut cursor = op.path.parent();
    while let Some(parent) = cursor {
        let Ok(relative) = parent.strip_prefix(root) else {
            break;
        };
        // The first relative component is the existing Studio service. Never
        // attempt to materialize or replace that root as a Folder.
        if relative.components().count() <= 1 {
            break;
        }
        if pending_empty_dirs.remove(parent) {
            parents.push(parent.to_path_buf());
        }
        cursor = parent.parent();
    }
    parents.reverse();
    parents
}

pub(super) fn is_synced_service_root_op(op: &Op, root: &std::path::Path) -> bool {
    if op.content.is_some() {
        return false;
    }
    let Ok(rel) = op.path.strip_prefix(root) else {
        return false;
    };
    if rel.components().count() != 1 {
        return false;
    }
    let Some(name) = rel.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    snapshot::SYNCED_SERVICES.contains(&name)
}

pub(super) fn watcher_op_can_use_push_quiet(op: &Op) -> bool {
    match op.kind {
        OpKind::Delete | OpKind::Rename => true,
        // A real folder recreation can race a daemon-authored delete at the
        // same path. Let adds through: the resulting set is idempotent and it
        // preserves pending-parent materialization for later children.
        OpKind::Add => false,
        OpKind::Update => op.content.is_none(),
    }
}

pub(super) fn is_push_quiet(
    push_quiet: &Arc<Mutex<HashMap<PathBuf, Instant>>>,
    path: &std::path::Path,
    project_root: &std::path::Path,
) -> bool {
    let Ok(relative) = path.strip_prefix(project_root) else {
        return false;
    };
    let Some(service) = relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    if !snapshot::SYNCED_SERVICES.contains(&service) {
        return false;
    }

    let now = Instant::now();
    let mut guard = push_quiet.lock().unwrap();
    // Prune amortized rather than walking the full map for every filesystem
    // notification during a large commit.
    static QUIET_PRUNE_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if QUIET_PRUNE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xff == 0 {
        guard.retain(|_, deadline| *deadline > now);
    }

    // Quiet entries describe exact daemon-authored paths, not subtrees. A
    // service commit may mark its root while a user concurrently saves a
    // descendant; treating the root as an ancestor wildcard permanently loses
    // that genuine edit because watcher notifications are edge-triggered.
    guard.get(path).is_some_and(|deadline| *deadline > now)
}

/// Watch `<project>/ro-sync.json` itself. On change, re-parse and if gameId,
/// groupId, or placeIds differ from AppState's current snapshot, update state
/// and broadcast a `{"type":"config-changed",...}` event.
pub(super) fn spawn_config_hot_reload(state: AppState) {
    // Use a fresh watcher scoped to the config file rather than reusing the
    // project-wide watcher: we want this event even during push-quiet windows.
    let config_path = state.canonical_project.join(project_config::CONFIG_FILE);
    let project_root = state.canonical_project.clone();
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};
        let (raw_tx, raw_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = raw_tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("rosync: config hot-reload watcher init failed: {e}");
                return;
            }
        };
        // Watch the parent dir (notify refuses to watch a non-existent file
        // directly on some backends). Filter by filename inside the loop.
        if watcher
            .watch(project_root.as_path(), RecursiveMode::NonRecursive)
            .is_err()
        {
            return;
        }
        loop {
            match raw_rx.recv() {
                Ok(Ok(ev)) => {
                    let touches_config = ev.paths.iter().any(|p| {
                        p.file_name().and_then(|n| n.to_str()) == Some(project_config::CONFIG_FILE)
                    });
                    if !touches_config {
                        continue;
                    }
                    // Debounce: re-read after a tick in case the writer is still flushing.
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = reload_config(&state, &config_path);
                }
                Ok(Err(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

pub(super) fn reload_config(state: &AppState, _config_path: &std::path::Path) -> Option<()> {
    let cfg = match project_config::read_from_disk(state.canonical_project.as_path()) {
        Ok(Some(c)) => c,
        _ => return None,
    };
    let prev_game_id = state.game_id.read().unwrap().clone();
    let prev_group_id = state.group_id.read().unwrap().clone();
    let prev_place_ids = state.place_ids.read().unwrap().clone();
    let prev_name = state.project_name.read().unwrap().clone();
    let prev_wally_enabled = *state.wally_enabled.read().unwrap();
    let prev_wally_folder = state.wally_folder.read().unwrap().clone();

    // Deleting this field from ro-sync.json clears the remembered decision,
    // so mirror it even when no connection-routing field changed.
    *state.initial_choice_default.write().unwrap() = cfg.initial_choice_default.clone();

    let changed = prev_game_id != cfg.game_id
        || prev_group_id != cfg.group_id
        || prev_place_ids != cfg.place_ids
        || prev_name != cfg.name
        || prev_wally_enabled != cfg.wally_enabled
        || prev_wally_folder != cfg.wally_folder;
    if !changed {
        return Some(());
    }

    *state.project_name.write().unwrap() = cfg.name.clone();
    *state.game_id.write().unwrap() = cfg.game_id.clone();
    *state.group_id.write().unwrap() = cfg.group_id.clone();
    *state.place_ids.write().unwrap() = cfg.place_ids.clone();
    *state.wally_enabled.write().unwrap() = cfg.wally_enabled;
    *state.wally_folder.write().unwrap() = cfg.wally_folder.clone();

    let evt = serde_json::json!({
        "type": "config-changed",
        "name": cfg.name,
        "gameId": cfg.game_id,
        "groupId": cfg.group_id,
        "placeIds": cfg.place_ids,
        "wallyEnabled": cfg.wally_enabled,
        "wallyFolder": cfg.wally_folder,
    });
    if let Ok(s) = serde_json::to_string(&evt) {
        let _ = state.events.send(s);
    }
    Some(())
}

#[derive(Debug, Clone)]
pub(super) struct BlockedFsDestructive {
    pub(super) kind: &'static str,
    pub(super) path: PathBuf,
    pub(super) from: Option<PathBuf>,
}

pub(super) fn handle_op(
    op: Op,
    events: &broadcast::Sender<String>,
    conflicts: &ConflictEngine,
    root: &Path,
) -> Option<BlockedFsDestructive> {
    match op.kind {
        OpKind::Add | OpKind::Update => {
            let bytes = match &op.content {
                Some(b) => b.clone(),
                None => {
                    // Directory or unreadable file — forward as-is.
                    emit_op(events, root, &op);
                    return None;
                }
            };
            // Normalize line endings so CRLF-on-disk vs LF-from-Studio don't
            // show up as divergent content.
            let normalized = fs_map::normalize_line_endings(&bytes).into_owned();
            let mtime = fs_mtime(&op.path);
            match conflicts.on_fs_change(&op.path, &normalized, mtime) {
                FsDecision::NoChange => {}
                FsDecision::Propagate => emit_op(events, root, &op),
                FsDecision::Conflict => emit_conflict(events, &op.path),
            }
            None
        }
        OpKind::Delete => match conflicts.finish_fs_destructive(&op.path) {
            FsDecision::Conflict => {
                emit_conflict(events, &op.path);
                Some(BlockedFsDestructive {
                    kind: "delete",
                    path: op.path,
                    from: None,
                })
            }
            FsDecision::NoChange | FsDecision::Propagate => {
                emit_op(events, root, &op);
                conflicts.commit_fs_delete(&op.path);
                None
            }
        },
        OpKind::Rename => {
            let source = op.from.as_deref().unwrap_or(op.path.as_path());
            match conflicts.finish_fs_destructive(source) {
                FsDecision::Conflict => {
                    emit_conflict(events, source);
                    Some(BlockedFsDestructive {
                        kind: "rename",
                        path: op.path,
                        from: op.from,
                    })
                }
                FsDecision::NoChange | FsDecision::Propagate => {
                    emit_op(events, root, &op);
                    if let Some(from) = &op.from {
                        conflicts.commit_fs_rename(from, &op.path);
                    }
                    None
                }
            }
        }
    }
}

pub(super) fn emit_op(events: &broadcast::Sender<String>, root: &Path, op: &Op) {
    let Some(plugin_op) = http::fs_op_to_plugin_op(root, op) else {
        return;
    };
    // Convert once at the watcher boundary. Keeping file contents as raw byte
    // arrays in the broadcast channel multiplied allocation and JSON parsing
    // work for every subscriber before the WebSocket layer converted them.
    if let Some(payload) = crate::ws::journal_op_event(&plugin_op) {
        let _ = events.send(payload);
    }
}

pub(super) fn emit_conflict(events: &broadcast::Sender<String>, path: &std::path::Path) {
    let payload = serde_json::json!({ "type": "conflict", "path": path });
    if let Ok(s) = serde_json::to_string(&payload) {
        let _ = events.send(s);
    }
}

pub(super) fn emit_sync_error(
    events: &broadcast::Sender<String>,
    path: &std::path::Path,
    error: &str,
) {
    let payload = serde_json::json!({
        "type": "sync-error",
        "path": path,
        "error": error,
    });
    if let Ok(serialized) = serde_json::to_string(&payload) {
        let _ = events.send(serialized);
    }
}

pub(super) fn fs_mtime(path: &std::path::Path) -> u64 {
    fs_safety::metadata_no_follow(path)
        .ok()
        .flatten()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| {
            duration
                .as_nanos()
                .min(u128::from(u64::MAX))
                .try_into()
                .unwrap_or(u64::MAX)
        })
        .unwrap_or(0)
}
