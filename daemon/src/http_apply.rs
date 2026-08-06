#[derive(Debug)]
enum PrunableStreamBackupEntry {
    File {
        path: PathBuf,
        generation: crate::fs_safety::FileGeneration,
    },
    Directory {
        path: PathBuf,
        identity: crate::fs_safety::FileIdentity,
    },
}

impl PrunableStreamBackupEntry {
    fn path(&self) -> &Path {
        match self {
            Self::File { path, .. } | Self::Directory { path, .. } => path,
        }
    }

    fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }
}

fn capture_prunable_stream_backup(
    root: &Path,
    transaction: &Path,
    discovered_generation: &crate::fs_safety::FileGeneration,
) -> Result<Vec<PrunableStreamBackupEntry>, String> {
    let backup_root = root.join(".rosync-backups");
    if transaction.parent() != Some(backup_root.as_path())
        || transaction
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| successful_stream_backup_stamp(name).is_none())
    {
        return Err(format!(
            "refusing to prune an unclassified stream backup {}",
            transaction.display()
        ));
    }
    let mut entries = Vec::new();
    let mut pending = vec![(transaction.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > crate::fs_safety::MAX_SERVICE_TREE_DEPTH {
            return Err(format!(
                "successful stream backup exceeds the safe depth limit: {}",
                transaction.display()
            ));
        }
        let directory_guard = crate::fs_safety::guard_descendant_directory_chain(root, &directory)
            .map_err(|error| {
                format!(
                    "guard successful stream backup directory {}: {error}",
                    directory.display()
                )
            })?;
        directory_guard.verify().map_err(|error| {
            format!(
                "verify successful stream backup directory {}: {error}",
                directory.display()
            )
        })?;
        let before = crate::fs_safety::directory_generation_no_follow(&directory)
            .map_err(|error| format!("inspect stream backup {}: {error}", directory.display()))?;
        if depth == 0 && &before != discovered_generation {
            return Err(format!(
                "refusing to prune stream backup replaced after discovery: {}",
                transaction.display()
            ));
        }
        let children = std::fs::read_dir(&directory)
            .map_err(|error| format!("scan stream backup {}: {error}", directory.display()))?;
        for child in children {
            let child = child
                .map_err(|error| format!("scan stream backup {}: {error}", directory.display()))?;
            let path = child.path();
            let metadata =
                crate::fs_safety::require_metadata_no_follow(&path).map_err(|error| {
                    format!("inspect stream backup entry {}: {error}", path.display())
                })?;
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else if metadata.is_file() {
                let generation = crate::fs_safety::file_generation_no_follow(&path)?;
                entries.push(PrunableStreamBackupEntry::File { path, generation });
            } else {
                return Err(format!(
                    "refusing unsupported stream backup entry {}",
                    path.display()
                ));
            }
            if entries.len() + pending.len() > MAX_BOOTSTRAP_NODES {
                return Err(format!(
                    "successful stream backup exceeds the safe entry limit of {MAX_BOOTSTRAP_NODES}"
                ));
            }
        }
        let after = crate::fs_safety::directory_generation_no_follow(&directory)
            .map_err(|error| format!("reinspect stream backup {}: {error}", directory.display()))?;
        if before != after {
            return Err(format!(
                "stream backup changed while it was scanned: {}",
                directory.display()
            ));
        }
        if depth == 0 && &after != discovered_generation {
            return Err(format!(
                "refusing to prune stream backup changed after discovery: {}",
                transaction.display()
            ));
        }
        directory_guard.verify().map_err(|error| {
            format!(
                "stream backup parent changed while scanning {}: {error}",
                directory.display()
            )
        })?;
        entries.push(PrunableStreamBackupEntry::Directory {
            path: directory,
            identity: before.identity,
        });
    }
    entries.sort_by(|left, right| {
        right
            .path()
            .components()
            .count()
            .cmp(&left.path().components().count())
            .then_with(|| right.is_file().cmp(&left.is_file()))
            .then_with(|| right.path().cmp(left.path()))
    });
    Ok(entries)
}

fn remove_successful_stream_backup(
    root: &Path,
    transaction: &Path,
    discovered_generation: &crate::fs_safety::FileGeneration,
) -> Result<(), String> {
    let backup_root = root.join(".rosync-backups");
    if transaction.parent() != Some(backup_root.as_path())
        || transaction
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| successful_stream_backup_stamp(name).is_none())
    {
        return Err(format!(
            "refusing to prune an unclassified stream backup {}",
            transaction.display()
        ));
    }
    validate_successful_stream_backup_marker(root, transaction)?;
    if crate::fs_safety::directory_generation_no_follow(transaction).map_err(|error| {
        format!(
            "reinspect discovered stream backup {}: {error}",
            transaction.display()
        )
    })? != *discovered_generation
    {
        return Err(format!(
            "refusing to prune stream backup replaced after discovery: {}",
            transaction.display()
        ));
    }
    let entries = capture_prunable_stream_backup(root, transaction, discovered_generation)?;
    for entry in entries {
        let path = entry.path().to_path_buf();
        let guard = crate::fs_safety::guard_descendant_parent_chain(root, &path, false)
            .map_err(|error| format!("guard stream backup removal {}: {error}", path.display()))?;
        guard.verify().map_err(|error| {
            format!(
                "verify stream backup removal parent {}: {error}",
                path.display()
            )
        })?;
        match entry {
            PrunableStreamBackupEntry::File { path, generation } => {
                if crate::fs_safety::file_generation_no_follow(&path)? != generation {
                    return Err(format!(
                        "refusing to prune changed stream backup file {}",
                        path.display()
                    ));
                }
                std::fs::remove_file(&path).map_err(|error| {
                    format!("remove stream backup file {}: {error}", path.display())
                })?;
            }
            PrunableStreamBackupEntry::Directory { path, identity } => {
                if crate::fs_safety::directory_generation_no_follow(&path)
                    .map_err(|error| {
                        format!(
                            "reinspect stream backup directory {}: {error}",
                            path.display()
                        )
                    })?
                    .identity
                    != identity
                {
                    return Err(format!(
                        "refusing to prune replaced stream backup directory {}",
                        path.display()
                    ));
                }
                if std::fs::read_dir(&path)
                    .map_err(|error| {
                        format!("verify empty stream backup {}: {error}", path.display())
                    })?
                    .next()
                    .transpose()
                    .map_err(|error| {
                        format!("verify empty stream backup {}: {error}", path.display())
                    })?
                    .is_some()
                {
                    return Err(format!(
                        "refusing to prune stream backup directory that gained entries: {}",
                        path.display()
                    ));
                }
                std::fs::remove_dir(&path).map_err(|error| {
                    format!("remove stream backup directory {}: {error}", path.display())
                })?;
            }
        }
        guard.verify().map_err(|error| {
            format!(
                "stream backup parent changed during removal {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn stream_backup_name_parts(name: &str, prefix: &str) -> Option<(u128, u64)> {
    if name.len() > 96 {
        return None;
    }
    let mut parts = name.strip_prefix(prefix)?.split('-');
    let stamp_text = parts.next()?;
    let sequence_text = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let stamp = stamp_text.parse::<u128>().ok()?;
    let sequence = sequence_text.parse::<u64>().ok()?;
    if sequence == 0 || stamp.to_string() != stamp_text || sequence.to_string() != sequence_text {
        return None;
    }
    Some((stamp, sequence))
}

fn successful_stream_backup_stamp(name: &str) -> Option<u128> {
    stream_backup_name_parts(name, "stream-success-").map(|(stamp, _)| stamp)
}

fn successful_stream_backup_marker(
    transaction: &Path,
    stream_id: &str,
) -> Result<SuccessfulStreamBackupMarker, String> {
    if stream_id.is_empty() || stream_id.len() > 128 {
        return Err("successful stream backup has an invalid stream ID".into());
    }
    let transaction_name = transaction
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("stream backup name is not UTF-8: {}", transaction.display()))?;
    if successful_stream_backup_stamp(transaction_name).is_none() {
        return Err(format!(
            "successful stream backup name is not canonical: {}",
            transaction.display()
        ));
    }
    Ok(SuccessfulStreamBackupMarker {
        version: 1,
        kind: "completed-stream".into(),
        stream_id: stream_id.to_string(),
        completed_services: snapshot::SYNCED_SERVICES.len(),
        transaction: transaction_name.to_string(),
    })
}

fn validate_successful_stream_backup_marker(
    root: &Path,
    transaction: &Path,
) -> Result<SuccessfulStreamBackupMarker, String> {
    let transaction_name = transaction
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("stream backup name is not UTF-8: {}", transaction.display()))?;
    if successful_stream_backup_stamp(transaction_name).is_none() {
        return Err(format!(
            "successful stream backup name is not canonical: {}",
            transaction.display()
        ));
    }
    let marker_path = transaction.join(SUCCESSFUL_STREAM_BACKUP_MARKER);
    let transaction_guard = crate::fs_safety::guard_descendant_directory_chain(root, transaction)
        .map_err(|error| {
        format!(
            "guard successful stream backup marker parent {}: {error}",
            transaction.display()
        )
    })?;
    let marker_guard = crate::fs_safety::guard_descendant_parent_chain(root, &marker_path, false)
        .map_err(|error| {
        format!(
            "guard successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    transaction_guard.verify().map_err(|error| {
        format!(
            "verify successful stream backup marker parent {}: {error}",
            transaction.display()
        )
    })?;
    marker_guard.verify().map_err(|error| {
        format!(
            "verify successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    let metadata = crate::fs_safety::require_metadata_no_follow(&marker_path).map_err(|error| {
        format!(
            "successful stream backup marker is missing or unsafe {}: {error}",
            marker_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_SUCCESSFUL_STREAM_BACKUP_MARKER_BYTES {
        return Err(format!(
            "successful stream backup marker is not a bounded regular file: {}",
            marker_path.display()
        ));
    }
    let before = crate::fs_safety::file_generation_no_follow(&marker_path)?;
    let bytes = crate::fs_safety::read_file_no_follow(&marker_path).map_err(|error| {
        format!(
            "read successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    if crate::fs_safety::file_generation_no_follow(&marker_path)? != before {
        return Err(format!(
            "successful stream backup marker changed while reading: {}",
            marker_path.display()
        ));
    }
    transaction_guard.verify().map_err(|error| {
        format!(
            "successful stream backup marker parent changed {}: {error}",
            transaction.display()
        )
    })?;
    marker_guard.verify().map_err(|error| {
        format!(
            "successful stream backup marker parent changed {}: {error}",
            marker_path.display()
        )
    })?;
    let marker: SuccessfulStreamBackupMarker = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "parse successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    if marker.version != 1
        || marker.kind != "completed-stream"
        || marker.stream_id.is_empty()
        || marker.stream_id.len() > 128
        || marker.completed_services != snapshot::SYNCED_SERVICES.len()
        || marker.transaction != transaction_name
    {
        return Err(format!(
            "successful stream backup marker has invalid provenance: {}",
            marker_path.display()
        ));
    }
    Ok(marker)
}

fn write_successful_stream_backup_marker(
    root: &Path,
    transaction: &Path,
    stream_id: &str,
) -> Result<(), String> {
    use std::io::Write as _;

    let marker = successful_stream_backup_marker(transaction, stream_id)?;
    let bytes = serde_json::to_vec(&marker)
        .map_err(|error| format!("encode successful stream backup marker: {error}"))?;
    if bytes.len() as u64 > MAX_SUCCESSFUL_STREAM_BACKUP_MARKER_BYTES {
        return Err("successful stream backup marker exceeded its byte limit".into());
    }
    let marker_path = transaction.join(SUCCESSFUL_STREAM_BACKUP_MARKER);
    let transaction_guard = crate::fs_safety::guard_descendant_directory_chain(root, transaction)
        .map_err(|error| {
        format!(
            "guard successful stream backup marker parent {}: {error}",
            transaction.display()
        )
    })?;
    let marker_guard = crate::fs_safety::guard_descendant_parent_chain(root, &marker_path, true)
        .map_err(|error| {
            format!(
                "guard successful stream backup marker {}: {error}",
                marker_path.display()
            )
        })?;
    transaction_guard.verify().map_err(|error| {
        format!(
            "verify successful stream backup marker parent {}: {error}",
            transaction.display()
        )
    })?;
    marker_guard.verify().map_err(|error| {
        format!(
            "verify successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    if crate::fs_safety::metadata_no_follow(&marker_path)
        .map_err(|error| format!("inspect successful stream backup marker target: {error}"))?
        .is_some()
    {
        return Err(format!(
            "successful stream backup marker already exists: {}",
            marker_path.display()
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_path)
        .map_err(|error| {
            format!(
                "create successful stream backup marker {}: {error}",
                marker_path.display()
            )
        })?;
    file.write_all(&bytes).map_err(|error| {
        format!(
            "write successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    file.sync_all().map_err(|error| {
        format!(
            "sync successful stream backup marker {}: {error}",
            marker_path.display()
        )
    })?;
    drop(file);
    transaction_guard.verify().map_err(|error| {
        format!(
            "successful stream backup marker parent changed {}: {error}",
            transaction.display()
        )
    })?;
    marker_guard.verify().map_err(|error| {
        format!(
            "successful stream backup marker parent changed {}: {error}",
            marker_path.display()
        )
    })?;
    let validated = validate_successful_stream_backup_marker(root, transaction)?;
    if validated != marker {
        return Err(format!(
            "successful stream backup marker changed after creation: {}",
            marker_path.display()
        ));
    }
    Ok(())
}

fn promote_successful_stream_backup(
    root: &Path,
    transaction: &Path,
) -> Result<(PathBuf, Option<String>), String> {
    let backup_root = root.join(".rosync-backups");
    if transaction.parent() != Some(backup_root.as_path()) {
        return Err(format!(
            "stream backup is outside the project backup root: {}",
            transaction.display()
        ));
    }
    let name = transaction
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("stream backup name is not UTF-8: {}", transaction.display()))?;
    if successful_stream_backup_stamp(name).is_some() {
        return Ok((transaction.to_path_buf(), None));
    }
    let (stamp, sequence) = stream_backup_name_parts(name, "stream-").ok_or_else(|| {
        format!(
            "stream backup has an unexpected transaction name: {}",
            transaction.display()
        )
    })?;
    let promoted = backup_root.join(format!("stream-success-{stamp}-{sequence}"));
    let source_guard = crate::fs_safety::guard_descendant_parent_chain(root, transaction, false)
        .map_err(|error| format!("guard successful stream backup: {error}"))?;
    let target_guard = crate::fs_safety::guard_descendant_parent_chain(root, &promoted, true)
        .map_err(|error| format!("guard promoted stream backup: {error}"))?;
    source_guard
        .verify()
        .map_err(|error| format!("verify successful stream backup parent: {error}"))?;
    target_guard
        .verify()
        .map_err(|error| format!("verify promoted stream backup parent: {error}"))?;
    let source_generation = crate::fs_safety::directory_generation_no_follow(transaction)
        .map_err(|error| format!("inspect successful stream backup: {error}"))?;
    if crate::fs_safety::metadata_no_follow(&promoted)
        .map_err(|error| format!("inspect promoted stream backup target: {error}"))?
        .is_some()
    {
        return Err(format!(
            "promoted stream backup target already exists: {}",
            promoted.display()
        ));
    }
    source_guard
        .verify()
        .map_err(|error| format!("successful stream backup parent changed: {error}"))?;
    target_guard
        .verify()
        .map_err(|error| format!("promoted stream backup parent changed: {error}"))?;
    std::fs::rename(transaction, &promoted)
        .map_err(|error| format!("promote successful stream backup: {error}"))?;

    let warning = source_guard
        .verify()
        .err()
        .map(|error| format!("successful backup parent changed after promotion: {error}"))
        .or_else(|| {
            target_guard
                .verify()
                .err()
                .map(|error| format!("promoted backup parent changed: {error}"))
        })
        .or_else(|| {
            crate::fs_safety::directory_generation_no_follow(&promoted)
                .err()
                .map(|error| format!("reinspect promoted backup: {error}"))
        })
        .or_else(|| {
            crate::fs_safety::directory_generation_no_follow(&promoted)
                .ok()
                .filter(|generation| generation.identity != source_generation.identity)
                .map(|_| "promoted backup identity changed after rename".to_string())
        });
    Ok((promoted, warning))
}

fn prune_successful_stream_backups(root: &Path) -> Vec<String> {
    let backup_root = root.join(".rosync-backups");
    let metadata = match crate::fs_safety::metadata_no_follow(&backup_root) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Vec::new(),
        Err(error) => {
            return vec![format!(
                "inspect successful stream backup root {}: {error}",
                backup_root.display()
            )];
        }
    };
    if !metadata.is_dir() {
        return vec![format!(
            "backup root is not a physical directory: {}",
            backup_root.display()
        )];
    }
    let backup_root_guard =
        match crate::fs_safety::guard_descendant_directory_chain(root, &backup_root) {
            Ok(guard) => guard,
            Err(error) => {
                return vec![format!(
                    "guard successful stream backup root {}: {error}",
                    backup_root.display()
                )];
            }
        };
    if let Err(error) = backup_root_guard.verify() {
        return vec![format!(
            "verify successful stream backup root {}: {error}",
            backup_root.display()
        )];
    }
    let mut warnings = Vec::new();
    let mut candidates = Vec::<(u128, PathBuf, crate::fs_safety::FileGeneration)>::new();
    let children = match std::fs::read_dir(&backup_root) {
        Ok(children) => children,
        Err(error) => {
            return vec![format!(
                "scan successful stream backups {}: {error}",
                backup_root.display()
            )];
        }
    };
    for child in children {
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                warnings.push(format!("scan successful stream backup: {error}"));
                continue;
            }
        };
        let Some(name) = child.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stamp) = successful_stream_backup_stamp(&name) else {
            continue;
        };
        let path = child.path();
        if let Err(error) = validate_successful_stream_backup_marker(root, &path) {
            warnings.push(format!(
                "skip unproven successful stream backup {}: {error}",
                path.display()
            ));
            continue;
        }
        match crate::fs_safety::directory_generation_no_follow(&path) {
            Ok(generation) => candidates.push((stamp, path, generation)),
            Err(error) => warnings.push(format!(
                "inspect successful stream backup {}: {error}",
                path.display()
            )),
        }
    }
    if let Err(error) = backup_root_guard.verify() {
        warnings.push(format!(
            "successful stream backup root changed while scanning {}: {error}",
            backup_root.display()
        ));
        return warnings;
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for (index, (stamp, path, generation)) in candidates.into_iter().enumerate() {
        let expired = now.saturating_sub(stamp) > SUCCESSFUL_STREAM_BACKUP_RETENTION.as_nanos();
        if index >= MAX_SUCCESSFUL_STREAM_BACKUPS || expired {
            if let Err(error) = remove_successful_stream_backup(root, &path, &generation) {
                warnings.push(error);
            }
        }
    }
    warnings
}

fn run_stream_commit_hook(
    control: &StreamCommitControl,
    point: StreamCommitHookPoint,
    backup_service: &Path,
    live_service: &Path,
    stage_service: &Path,
) -> Result<(), String> {
    #[cfg(test)]
    if let Some(hook) = control.test_hook.as_ref() {
        return hook(point, backup_service, live_service, stage_service);
    }
    #[cfg(not(test))]
    let _ = (control, point, backup_service, live_service, stage_service);
    Ok(())
}

fn restore_stream_backup_before_install(
    root: &Path,
    service: &str,
    live_service: &Path,
    backup_service: &Path,
    backup_transaction: &Path,
    live_parent_guard: &crate::fs_safety::PathParentGuard,
    backup_parent_guard: &crate::fs_safety::PathParentGuard,
) -> Result<Option<String>, String> {
    let backup_fingerprint = capture_exact_tree_fingerprint(backup_transaction, service)
        .map_err(|error| format!("capture retained backup before rollback: {error}"))?;
    if crate::fs_safety::metadata_no_follow(live_service)
        .map_err(|error| {
            format!(
                "inspect live rollback target {}: {error}",
                live_service.display()
            )
        })?
        .is_some()
    {
        return Err(format!(
            "refusing rollback because live service target appeared: {}",
            live_service.display()
        ));
    }
    live_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because live service parent changed {}: {error}",
            live_service.display()
        )
    })?;
    backup_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because backup parent changed {}: {error}",
            backup_service.display()
        )
    })?;
    std::fs::rename(backup_service, live_service).map_err(|error| {
        format!(
            "restore backup {} -> {}: {error}",
            backup_service.display(),
            live_service.display()
        )
    })?;

    // Once the exact backup has been atomically restored, keep the live tree
    // even if a concurrent writer changes it during post-rename verification.
    // Returning it to the user is safer than attempting another destructive
    // move with no remaining backup source.
    let _ = live_parent_guard.verify();
    let backup_parent_check = backup_parent_guard.verify();
    let _ = capture_exact_tree_fingerprint(root, service)
        .map(|current| relocated_fingerprint_matches(&backup_fingerprint, &current));
    Ok(match backup_parent_check {
        Err(error) => Some(format!(
            "restored live files but refused backup transaction cleanup after its parent changed {}: {error}",
            backup_transaction.display()
        )),
        Ok(()) => cleanup_empty_stream_backup_transaction(root, backup_transaction)
            .err()
            .map(|error| {
                format!(
                    "restored live files but could not clean backup transaction {}: {error}",
                    backup_transaction.display()
                )
            }),
    })
}

struct InstalledStreamRollback<'a> {
    root: &'a Path,
    service: &'a str,
    live_service: &'a Path,
    backup_service: &'a Path,
    backup_transaction: &'a Path,
    stage_service: &'a Path,
    staged_fingerprint: &'a ExactTreeFingerprint,
    live_parent_guard: &'a crate::fs_safety::PathParentGuard,
    backup_parent_guard: &'a crate::fs_safety::PathParentGuard,
    stage_parent_guard: &'a crate::fs_safety::PathParentGuard,
}

fn restore_stream_backup_after_install(
    rollback: InstalledStreamRollback<'_>,
) -> Result<Option<String>, String> {
    let InstalledStreamRollback {
        root,
        service,
        live_service,
        backup_service,
        backup_transaction,
        stage_service,
        staged_fingerprint,
        live_parent_guard,
        backup_parent_guard,
        stage_parent_guard,
    } = rollback;
    let current_live = capture_exact_tree_fingerprint(root, service)
        .map_err(|error| format!("capture installed service before rollback: {error}"))?;
    if !relocated_fingerprint_matches(staged_fingerprint, &current_live) {
        return Err(format!(
            "refusing rollback because installed service changed: {}",
            live_service.display()
        ));
    }
    let backup_fingerprint = capture_exact_tree_fingerprint(backup_transaction, service)
        .map_err(|error| format!("capture retained backup before rollback: {error}"))?;
    if crate::fs_safety::metadata_no_follow(stage_service)
        .map_err(|error| {
            format!(
                "inspect staged rollback target {}: {error}",
                stage_service.display()
            )
        })?
        .is_some()
    {
        return Err(format!(
            "refusing rollback because staged target reappeared: {}",
            stage_service.display()
        ));
    }
    live_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because live service parent changed {}: {error}",
            live_service.display()
        )
    })?;
    backup_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because backup parent changed {}: {error}",
            backup_service.display()
        )
    })?;
    stage_parent_guard.verify().map_err(|error| {
        format!(
            "refusing rollback because stage parent changed {}: {error}",
            stage_service.display()
        )
    })?;
    std::fs::rename(live_service, stage_service).map_err(|error| {
        format!(
            "move installed service aside {} -> {}: {error}",
            live_service.display(),
            stage_service.display()
        )
    })?;
    if let Err(error) = std::fs::rename(backup_service, live_service) {
        let reinstall = std::fs::rename(stage_service, live_service);
        return Err(format!(
            "restore backup {} -> {}: {error}; reinstall staged service: {}",
            backup_service.display(),
            live_service.display(),
            reinstall
                .map(|_| "ok".to_string())
                .unwrap_or_else(|reinstall| reinstall.to_string())
        ));
    }

    // As above, the original disk tree is now back at the live path. Do not
    // risk a second destructive swap merely because post-rename observation
    // races another local writer.
    let _ = live_parent_guard.verify();
    let backup_parent_check = backup_parent_guard.verify();
    let _ = stage_parent_guard.verify();
    let _ = capture_exact_tree_fingerprint(root, service)
        .map(|current| relocated_fingerprint_matches(&backup_fingerprint, &current));
    Ok(match backup_parent_check {
        Err(error) => Some(format!(
            "restored live files but refused backup transaction cleanup after its parent changed {}: {error}",
            backup_transaction.display()
        )),
        Ok(()) => cleanup_empty_stream_backup_transaction(root, backup_transaction)
            .err()
            .map(|error| {
                format!(
                    "restored live files but could not clean backup transaction {}: {error}",
                    backup_transaction.display()
                )
            }),
    })
}

fn retain_stream_commit_backup(
    state: &AppState,
    control: &mut StreamCommitControl,
    service: &str,
    live_service: &Path,
    backup_transaction: Option<&Path>,
    failure: &str,
    rollback: &str,
) {
    control.partial_failure = true;
    control.retained_backup = backup_transaction.map(Path::to_path_buf);
    let event = json!({
        "type": "stream-commit-partial",
        "service": service,
        "livePath": live_service,
        "backup": backup_transaction,
        "error": failure,
        "rollbackError": rollback,
    });
    if let Ok(serialized) = serde_json::to_string(&event) {
        let _ = state.events.send(serialized);
    }
    #[cfg(not(test))]
    {
        let _ = write_log_entry(Json(json!({
            "action": "stream-commit-partial",
            "service": service,
            "livePath": live_service,
            "backup": backup_transaction,
            "error": failure,
            "rollbackError": rollback,
        })));
    }
}

fn prepare_stream_baselines(
    stage_service: &Path,
    live_service: &Path,
    staged_generation: &crate::fs_safety::TreeGeneration,
) -> Result<Vec<PreparedStreamBaseline>, String> {
    let mut baselines = Vec::new();
    for entry in staged_generation.entries() {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if classify_script_file(name).is_none() && !is_init_file(name) {
            continue;
        }
        let relative = entry.path.strip_prefix(stage_service).map_err(|error| {
            format!(
                "prepare streamed baseline path {}: {error}",
                entry.path.display()
            )
        })?;
        // `staged_generation` was captured immediately before this pass from
        // a randomly named, process-owned temporary tree. Re-validating every
        // ancestor for every file here made cold bootstrap quadratic in path
        // depth on Windows. The live tree still has its independent transfer
        // fence and is atomically replaced below.
        let bytes = std::fs::read(&entry.path)
            .map_err(|error| format!("read staged baseline {}: {error}", entry.path.display()))?;
        baselines.push(PreparedStreamBaseline {
            path: live_service.join(relative),
            source_hash: hash(&normalize_line_endings(&bytes)),
            fs_mtime: entry
                .generation
                .modified_ns
                .map(|value| u64::try_from(value).unwrap_or(u64::MAX))
                .unwrap_or(0),
        });
    }
    Ok(baselines)
}

fn commit_streamed_service(input: StreamCommitInput) -> Result<StreamCommitResult, String> {
    let StreamCommitInput {
        state,
        service,
        service_node,
        mut source_bytes,
        initial_fingerprint,
        strict,
        force_prune,
        commit_control,
    } = input;
    let root = state.canonical_project.as_path();
    let created = !initial_fingerprint.metadata.present;
    crate::fs_safety::validate_service_tree_no_follow(root, &service)?;
    let stage_parent = root.parent().ok_or_else(|| {
        format!(
            "project root has no same-volume staging parent: {}",
            root.display()
        )
    })?;
    let stage_parent_metadata = crate::fs_safety::require_metadata_no_follow(stage_parent)
        .map_err(|error| format!("inspect staging parent {}: {error}", stage_parent.display()))?;
    if !stage_parent_metadata.is_dir() {
        return Err(format!(
            "same-volume staging parent is not a directory: {}",
            stage_parent.display()
        ));
    }
    let stage = tempfile::Builder::new()
        .prefix(".rosync-stage-")
        .tempdir_in(stage_parent)
        .map_err(|error| format!("create same-volume service stage: {error}"))?;
    let staged_hash =
        copy_fenced_service_to_stage(root, &initial_fingerprint.metadata, stage.path())?;
    if staged_hash != initial_fingerprint.content_hash
        || crate::fs_safety::capture_tree_metadata(root, &service)? != initial_fingerprint.metadata
    {
        return Err(format!(
            "disk service {service} changed during streamed upload; no files were replaced"
        ));
    }

    let stage_service = stage.path().join(&service);
    let stage_quiet = Mutex::new(HashMap::new());
    let stage_conflicts = crate::conflict::ConflictEngine::new();
    let stage_ctx = PushCtx {
        conflicts: &stage_conflicts,
        push_quiet: &stage_quiet,
        force_overwrite: true,
        strict,
        force_prune,
        project_root: stage.path(),
        backup_forced_removals: false,
        private_stage: true,
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
    };
    let mut source_provider = |node: &Value| {
        let id = node
            .get("streamId")
            .and_then(Value::as_u64)
            .ok_or("streamed script is missing its source ID")?;
        source_bytes
            .remove(&id)
            .map(Some)
            .ok_or_else(|| format!("streamed Source bytes are missing for ID {id}"))
    };
    let applied = match apply_service_node_with_sources(
        stage.path(),
        &service_node,
        &stage_ctx,
        &mut source_provider,
    ) {
        Ok(applied) => applied,
        Err(error) => return Err(format!("apply staged service {service}: {error}")),
    };

    let staged_fingerprint = capture_exact_tree_fingerprint(stage.path(), &service)?;
    let live_service = root.join(&service);
    let prepared_baselines = prepare_stream_baselines(
        &stage_service,
        &live_service,
        &staged_fingerprint.metadata,
    )?;
    // The staged copy already re-read and content-verified the live service;
    // a metadata generation fence is sufficient here and avoids hashing it a
    // second time immediately before commit.
    if crate::fs_safety::capture_tree_metadata(root, &service)? != initial_fingerprint.metadata {
        return Err(format!(
            "disk service {service} changed before atomic commit; no files were replaced"
        ));
    }
    let mut commit_control = commit_control.lock().unwrap();
    if commit_control.cancelled {
        return Err("streamed service commit was cancelled before disk replacement".into());
    }
    let mut backup_transaction = None;
    if initial_fingerprint.metadata.present {
        let (destination, transaction) = create_stream_backup_destination(root, &service)?;
        let backup_parent = transaction.as_path();
        let prepare = (|| {
            let live_parent_guard =
                crate::fs_safety::guard_synced_parent_chain(root, &live_service, false).map_err(
                    |error| format!("guard live service {}: {error}", live_service.display()),
                )?;
            let backup_parent_guard =
                crate::fs_safety::guard_descendant_parent_chain(root, &destination, true).map_err(
                    |error| {
                        format!(
                            "guard stream backup destination {}: {error}",
                            destination.display()
                        )
                    },
                )?;
            let stage_parent_guard = crate::fs_safety::guard_descendant_parent_chain(
                stage.path(),
                &stage_service,
                false,
            )
            .map_err(|error| {
                format!("guard staged service {}: {error}", stage_service.display())
            })?;
            live_parent_guard.verify().map_err(|error| {
                format!(
                    "verify live service parent {}: {error}",
                    live_service.display()
                )
            })?;
            backup_parent_guard.verify().map_err(|error| {
                format!(
                    "verify stream backup parent {}: {error}",
                    backup_parent.display()
                )
            })?;
            run_stream_commit_hook(
                &commit_control,
                StreamCommitHookPoint::BeforeBackupRename,
                &destination,
                &live_service,
                &stage_service,
            )?;
            std::fs::rename(&live_service, &destination).map_err(|error| {
                format!(
                    "move live service {} to backup {}: {error}",
                    live_service.display(),
                    destination.display()
                )
            })?;
            Ok::<_, String>((live_parent_guard, backup_parent_guard, stage_parent_guard))
        })();
        let (live_parent_guard, backup_parent_guard, stage_parent_guard) = match prepare {
            Ok(guards) => guards,
            Err(error) => {
                return Err(
                    match cleanup_empty_stream_backup_transaction(root, &transaction) {
                        Ok(()) => error,
                        Err(cleanup) => {
                            format!("{error}; empty backup transaction cleanup failed: {cleanup}")
                        }
                    },
                );
            }
        };
        let mut stage_installed = false;
        let install = (|| -> Result<(), String> {
            live_parent_guard.verify().map_err(|error| {
                format!(
                    "live service parent changed during backup rename {}: {error}",
                    live_service.display()
                )
            })?;
            backup_parent_guard.verify().map_err(|error| {
                format!(
                    "stream backup parent changed during rename {}: {error}",
                    backup_parent.display()
                )
            })?;
            run_stream_commit_hook(
                &commit_control,
                StreamCommitHookPoint::AfterBackupRename,
                &destination,
                &live_service,
                &stage_service,
            )?;
            let moved_fingerprint = capture_exact_tree_fingerprint(&transaction, &service)?;
            if !relocated_fingerprint_matches(&initial_fingerprint, &moved_fingerprint) {
                return Err(
                    "the moved tree no longer matched its transfer fence after backup rename"
                        .into(),
                );
            }
            stage_parent_guard.verify().map_err(|error| {
                format!(
                    "verify staged service parent {}: {error}",
                    stage_service.display()
                )
            })?;
            live_parent_guard.verify().map_err(|error| {
                format!(
                    "verify live service parent {}: {error}",
                    live_service.display()
                )
            })?;
            run_stream_commit_hook(
                &commit_control,
                StreamCommitHookPoint::BeforeStageInstall,
                &destination,
                &live_service,
                &stage_service,
            )?;
            std::fs::rename(&stage_service, &live_service).map_err(|error| {
                format!("commit staged service {}: {error}", live_service.display())
            })?;
            stage_installed = true;
            run_stream_commit_hook(
                &commit_control,
                StreamCommitHookPoint::AfterStageInstall,
                &destination,
                &live_service,
                &stage_service,
            )?;
            stage_parent_guard.verify().map_err(|error| {
                format!(
                    "staged service parent changed during commit {}: {error}",
                    stage_service.display()
                )
            })?;
            live_parent_guard.verify().map_err(|error| {
                format!(
                    "live service parent changed during commit {}: {error}",
                    live_service.display()
                )
            })?;
            Ok(())
        })();
        if let Err(failure) = install {
            let rollback = if stage_installed {
                restore_stream_backup_after_install(InstalledStreamRollback {
                    root,
                    service: &service,
                    live_service: &live_service,
                    backup_service: &destination,
                    backup_transaction: &transaction,
                    stage_service: &stage_service,
                    staged_fingerprint: &staged_fingerprint,
                    live_parent_guard: &live_parent_guard,
                    backup_parent_guard: &backup_parent_guard,
                    stage_parent_guard: &stage_parent_guard,
                })
            } else {
                restore_stream_backup_before_install(
                    root,
                    &service,
                    &live_service,
                    &destination,
                    &transaction,
                    &live_parent_guard,
                    &backup_parent_guard,
                )
            };
            return Err(match rollback {
                Ok(cleanup_warning) => format!(
                    "streamed service commit failed: {failure}; live files were restored{}",
                    cleanup_warning
                        .map(|warning| format!("; cleanup warning: {warning}"))
                        .unwrap_or_default()
                ),
                Err(rollback) => {
                    let retained = crate::fs_safety::metadata_no_follow(&transaction)
                        .ok()
                        .flatten()
                        .is_some_and(|metadata| metadata.is_dir())
                        .then_some(transaction.as_path());
                    retain_stream_commit_backup(
                        &state,
                        &mut commit_control,
                        &service,
                        &live_service,
                        retained,
                        &failure,
                        &rollback,
                    );
                    format!(
                        "streamed service commit is partial: {failure}; rollback refused or failed: {rollback}; recovery backup: {}",
                        retained
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "not retained; inspect the live service".into())
                    )
                }
            });
        }
        backup_transaction = Some(transaction);
    } else {
        let stage_parent_guard =
            crate::fs_safety::guard_descendant_parent_chain(stage.path(), &stage_service, false)
                .map_err(|error| {
                    format!("guard staged service {}: {error}", stage_service.display())
                })?;
        let live_parent_guard =
            crate::fs_safety::guard_synced_parent_chain(root, &live_service, true).map_err(
                |error| {
                    format!(
                        "guard live service target {}: {error}",
                        live_service.display()
                    )
                },
            )?;
        stage_parent_guard.verify().map_err(|error| {
            format!(
                "verify staged service parent {}: {error}",
                stage_service.display()
            )
        })?;
        live_parent_guard.verify().map_err(|error| {
            format!(
                "verify live service parent {}: {error}",
                live_service.display()
            )
        })?;
        std::fs::rename(&stage_service, &live_service).map_err(|error| {
            format!("commit staged service {}: {error}", live_service.display())
        })?;
        // There was no prior disk tree to lose. Once the staged service is
        // installed, publish the commit even if a post-rename observation
        // races another local writer.
        let _ = stage_parent_guard.verify();
        let _ = live_parent_guard.verify();
    }

    commit_control.committed = true;
    drop(commit_control);
    let live_ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: true,
        strict,
        force_prune,
        project_root: root,
        backup_forced_removals: true,
        private_stage: false,
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
    };
    live_ctx.mark_quiet(&live_service);
    Ok(StreamCommitResult {
        applied,
        backup: backup_transaction,
        created,
        installed_fingerprint: staged_fingerprint,
        baselines: prepared_baselines,
    })
}

fn new_push_service_stream(service: &str) -> PushServiceStream {
    PushServiceStream {
        service: service.to_string(),
        phase: PushStreamPhase::Structure,
        next_chunk: 0,
        records: Vec::new(),
        accepted_structure_bytes: 0,
        accepted_source_bytes: 0,
        service_node: None,
        script_ids: Vec::new(),
        next_script: 0,
        receiving_source: None,
        source_bytes: HashMap::new(),
        initial_fingerprint: None,
        fence_result: None,
        commit_result: None,
        commit_control: None,
    }
}

fn push_stream_expired(session: &PushStreamAccumulator) -> bool {
    match session.completed_at {
        Some(completed) => completed.elapsed() >= STREAM_COMPLETED_TTL,
        None => session.last_activity.elapsed() >= STREAM_SESSION_TTL,
    }
}

fn prune_push_stream_sessions(sessions: &mut HashMap<PathBuf, Arc<Mutex<PushStreamAccumulator>>>) {
    sessions.retain(|_, session| {
        session
            .try_lock()
            .map(|session| !push_stream_expired(&session))
            .unwrap_or(true)
    });
}

fn schedule_push_stream_cleanup(
    project: PathBuf,
    session: &Arc<Mutex<PushStreamAccumulator>>,
    wake_after: Duration,
) {
    let session = Arc::downgrade(session);
    tokio::spawn(async move {
        tokio::time::sleep(wake_after).await;
        loop {
            let Some(session) = session.upgrade() else {
                return;
            };
            let remaining = {
                let session = session.lock().unwrap();
                match session.completed_at {
                    Some(completed) => STREAM_COMPLETED_TTL.saturating_sub(completed.elapsed()),
                    None => STREAM_SESSION_TTL.saturating_sub(session.last_activity.elapsed()),
                }
            };
            if !remaining.is_zero() {
                drop(session);
                tokio::time::sleep(remaining).await;
                continue;
            }
            let attempt = {
                let sessions = PUSH_STREAM_ACCUMULATORS.get_or_init(|| Mutex::new(HashMap::new()));
                let mut sessions = sessions.lock().unwrap();
                try_remove_expired_stream_session(
                    &mut sessions,
                    &project,
                    &session,
                    push_stream_expired,
                )
            };
            match attempt {
                StreamCleanupAttempt::Removed | StreamCleanupAttempt::Superseded => return,
                StreamCleanupAttempt::Retry => {
                    tokio::time::sleep(STREAM_CLEANUP_RETRY_DELAY).await;
                }
            }
        }
    });
}

fn push_stream_request_hash(body: &PushBody) -> Result<crate::conflict::Hash, String> {
    serde_json::to_vec(body)
        .map(|encoded| hash(&encoded))
        .map_err(|error| error.to_string())
}

fn push_stream_response(
    session: &PushStreamAccumulator,
    service: &str,
    phase: &str,
    next_chunk: u64,
) -> Value {
    json!({
        "ok": true,
        "streamId": session.stream_id,
        "nextService": service,
        "phase": phase,
        "nextChunk": next_chunk,
    })
}

fn retain_stream_commit_result(
    session: &mut PushStreamAccumulator,
    service: &str,
    result: StreamCommitResult,
) {
    session.applied += result.applied;
    if let Some(backup) = result.backup.as_ref() {
        session.backups.push(backup.clone());
    }
    session.prepared_baselines.extend(result.baselines);
    session.committed_services.push(CommittedStreamService {
        service: service.to_string(),
        created: result.created,
        backup: result.backup,
        recovery_action: if result.created {
            StreamRecoveryAction::RemoveCreatedService
        } else {
            StreamRecoveryAction::RestoreBackup
        },
        installed_fingerprint: result.installed_fingerprint,
    });
}

fn append_source_parts_atomically(
    service: &mut PushServiceStream,
    session_source_bytes: &mut u64,
    parts: &[StreamSourcePart],
    final_chunk: bool,
) -> Result<(), String> {
    let encoded =
        serde_json::to_vec(parts).map_err(|error| format!("encode streamed Sources: {error}"))?;
    if encoded.len() > STREAM_SOURCE_CHUNK_BYTES {
        return Err(format!(
            "encoded Source chunks are limited to {STREAM_SOURCE_CHUNK_BYTES} bytes"
        ));
    }
    if parts.len() > STREAM_SOURCE_PART_CHUNK_NODES {
        return Err(format!(
            "Source chunks are limited to {STREAM_SOURCE_PART_CHUNK_NODES} parts"
        ));
    }

    let mut next_script = service.next_script;
    let mut receiving = service.receiving_source.clone();
    let mut accepted_service_bytes = service.accepted_source_bytes;
    let mut accepted_session_bytes = *session_source_bytes;
    let mut writes = Vec::<(u64, Vec<u8>)>::with_capacity(parts.len());
    for part in parts {
        let bytes = part.data.as_bytes();
        if bytes.len() > STREAM_SOURCE_PART_BYTES {
            return Err(format!(
                "Source part {} for stream ID {} exceeds {STREAM_SOURCE_PART_BYTES} bytes",
                part.part_index, part.id
            ));
        }
        if part.total_bytes > MAX_STREAM_SOURCE_BYTES {
            return Err(format!(
                "Source for stream ID {} exceeds {MAX_STREAM_SOURCE_BYTES} bytes",
                part.id
            ));
        }
        let expected_id = service
            .script_ids
            .get(next_script)
            .copied()
            .ok_or_else(|| format!("unexpected Source for stream ID {}", part.id))?;
        if part.id != expected_id {
            return Err(format!(
                "Source stream expected script ID {expected_id}, received {}",
                part.id
            ));
        }
        let digest = parse_sha256_hex(&part.sha256)?;
        if receiving.is_none() {
            if part.part_index != 0 || part.offset != 0 {
                return Err(format!(
                    "Source for stream ID {} must begin at part 0, offset 0",
                    part.id
                ));
            }
            (accepted_service_bytes, accepted_session_bytes) = charge_stream_source_bytes(
                accepted_service_bytes,
                accepted_session_bytes,
                part.total_bytes,
            )?;
            receiving = Some(ReceivingSource {
                id: part.id,
                next_part: 0,
                offset: 0,
                total_bytes: part.total_bytes,
                sha256: digest,
                hasher: Sha256::new(),
            });
        }
        let current = receiving
            .as_mut()
            .expect("receiving source was initialized");
        if current.id != part.id
            || current.next_part != part.part_index
            || current.offset != part.offset
            || current.total_bytes != part.total_bytes
            || current.sha256 != digest
        {
            return Err(format!(
                "Source part {} for stream ID {} is stale or out of order",
                part.part_index, part.id
            ));
        }
        let new_offset = current
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or("Source byte offset overflowed")?;
        if new_offset > current.total_bytes {
            return Err(format!(
                "Source part {} for stream ID {} exceeds its declared total",
                part.part_index, part.id
            ));
        }
        if part.final_part != (new_offset == current.total_bytes) {
            return Err(format!(
                "Source part {} for stream ID {} has an inconsistent finalPart",
                part.part_index, part.id
            ));
        }
        current.hasher.update(bytes);
        current.offset = new_offset;
        current.next_part += 1;
        writes.push((part.id, bytes.to_vec()));
        if part.final_part {
            let actual: crate::conflict::Hash = current.hasher.clone().finalize().into();
            if actual != current.sha256 {
                return Err(format!("Source SHA-256 mismatch for stream ID {}", part.id));
            }
            receiving = None;
            next_script += 1;
        }
    }

    let complete = next_script == service.script_ids.len() && receiving.is_none();
    if final_chunk && !complete {
        return Err(format!(
            "Source stream ended after {next_script}/{} scripts",
            service.script_ids.len()
        ));
    }

    // Keep the bounded transfer in memory until the service worker writes the
    // final staged projection. The old implementation created one temporary
    // file per script and immediately reread it, doubling file creation and
    // metadata scans on the hottest Windows cold-connect path.
    for (id, bytes) in writes {
        service.source_bytes.entry(id).or_default().extend(bytes);
    }
    service.next_script = next_script;
    service.receiving_source = receiving;
    service.accepted_source_bytes = accepted_service_bytes;
    *session_source_bytes = accepted_session_bytes;
    Ok(())
}

fn spawn_exact_fingerprint(
    root: PathBuf,
    service: String,
) -> std::sync::mpsc::Receiver<Result<ExactTreeFingerprint, String>> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = send.send(capture_exact_tree_fingerprint(&root, &service));
    });
    receive
}

fn spawn_stream_commit(
    input: StreamCommitInput,
) -> std::sync::mpsc::Receiver<Result<StreamCommitResult, String>> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = commit_streamed_service(input);
        let _ = send.send(result);
    });
    receive
}

fn process_streamed_push_chunk(
    state: &AppState,
    session: &mut PushStreamAccumulator,
    body: &PushBody,
) -> Result<Value, String> {
    let service = body
        .service
        .as_deref()
        .ok_or("streamed push is missing service")?;
    let phase = body
        .phase
        .as_deref()
        .ok_or("streamed push is missing phase")?;
    let chunk_index = body
        .chunk_index
        .ok_or("streamed push is missing chunkIndex")?;
    if service != session.service_stream.service {
        return Err(format!(
            "streamed push expected service {}, received {service}",
            session.service_stream.service
        ));
    }
    if chunk_index != session.service_stream.next_chunk {
        return Err(format!(
            "streamed push {service} {phase} expected chunk {}, received {chunk_index}",
            session.service_stream.next_chunk
        ));
    }

    match session.service_stream.phase {
        PushStreamPhase::Structure => {
            if phase != "structure" || !body.sources.is_empty() {
                return Err("service structure phase accepts only flat records".into());
            }
            validate_stream_record_chunk_fields(&body.records)?;
            let chunk_bytes = encoded_stream_record_chunk_bytes(&body.records)?;
            if body.records.len() > STREAM_STRUCTURE_CHUNK_NODES {
                return Err(format!(
                    "structure chunks are limited to {STREAM_STRUCTURE_CHUNK_NODES} records"
                ));
            }
            if session
                .service_stream
                .records
                .len()
                .checked_add(body.records.len())
                .is_none_or(|count| count > MAX_BOOTSTRAP_NODES)
            {
                return Err(format!(
                    "streamed service exceeds {MAX_BOOTSTRAP_NODES} records"
                ));
            }
            for (offset, record) in body.records.iter().enumerate() {
                let expected = (session.service_stream.records.len() + offset) as u64;
                if record.id != expected {
                    return Err(format!(
                        "streamed structure IDs must be dense; expected {expected}, received {}",
                        record.id
                    ));
                }
            }
            let (service_bytes, session_bytes) = charge_stream_structure_bytes(
                session.service_stream.accepted_structure_bytes,
                session.accepted_stream_bytes,
                chunk_bytes,
            )?;
            session.service_stream.accepted_structure_bytes = service_bytes;
            session.accepted_stream_bytes = session_bytes;
            session
                .service_stream
                .records
                .extend(body.records.iter().cloned());
            session.service_stream.next_chunk += 1;
            if !body.final_chunk {
                return Ok(push_stream_response(
                    session,
                    service,
                    "structure",
                    session.service_stream.next_chunk,
                ));
            }
            let validated =
                validate_flat_snapshot(&session.service_stream.records, service, false)?;
            preflight_streamed_service_fragments(&validated.service)?;
            session.service_stream.records.clear();
            session.service_stream.records.shrink_to_fit();
            session.service_stream.service_node = Some(validated.service);
            session.service_stream.script_ids = validated.script_ids;
            session.service_stream.source_bytes.clear();
            session.service_stream.fence_result = Some(spawn_exact_fingerprint(
                state.canonical_project.as_ref().clone(),
                service.to_string(),
            ));
            session.service_stream.phase = PushStreamPhase::DiskFence;
            session.service_stream.next_chunk = 0;
            Ok(push_stream_response(session, service, "diskFence", 0))
        }
        PushStreamPhase::DiskFence => {
            if phase != "diskFence"
                || !body.records.is_empty()
                || !body.sources.is_empty()
                || body.final_chunk
            {
                return Err("diskFence accepts only empty continuation ticks".into());
            }
            let result = session
                .service_stream
                .fence_result
                .as_ref()
                .ok_or("diskFence worker is missing")?
                .try_recv();
            match result {
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    session.service_stream.next_chunk += 1;
                    Ok(push_stream_response(
                        session,
                        service,
                        "diskFence",
                        session.service_stream.next_chunk,
                    ))
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err("diskFence worker disconnected".into())
                }
                Ok(Err(error)) => Err(error),
                Ok(Ok(fingerprint)) => {
                    if session.strict {
                        let expected = session
                            .expected_service_generations
                            .get(session.next_service)
                            .ok_or("strict streamed push disk fence is incomplete")?;
                        if fingerprint.metadata != *expected {
                            return Err(format!(
                                "disk service {service} changed after the initial Studio choice; no files were replaced"
                            ));
                        }
                    }
                    session.service_stream.initial_fingerprint = Some(fingerprint);
                    session.service_stream.fence_result = None;
                    session.service_stream.phase = PushStreamPhase::Sources;
                    session.service_stream.next_chunk = 0;
                    Ok(push_stream_response(session, service, "sources", 0))
                }
            }
        }
        PushStreamPhase::Sources => {
            if phase != "sources" || !body.records.is_empty() {
                return Err("service Source phase accepts only Source parts".into());
            }
            append_source_parts_atomically(
                &mut session.service_stream,
                &mut session.accepted_source_bytes,
                &body.sources,
                body.final_chunk,
            )?;
            session.service_stream.next_chunk += 1;
            if !body.final_chunk {
                return Ok(push_stream_response(
                    session,
                    service,
                    "sources",
                    session.service_stream.next_chunk,
                ));
            }
            let service_node = session
                .service_stream
                .service_node
                .take()
                .ok_or("streamed service structure is missing")?;
            let source_bytes = std::mem::take(
                &mut session
                .service_stream
                .source_bytes,
            );
            let initial_fingerprint = session
                .service_stream
                .initial_fingerprint
                .take()
                .ok_or("streamed disk fence is missing")?;
            let commit_control = Arc::new(Mutex::new(StreamCommitControl::default()));
            session.service_stream.commit_result = Some(spawn_stream_commit(StreamCommitInput {
                state: state.clone(),
                service: service.to_string(),
                service_node,
                source_bytes,
                initial_fingerprint,
                strict: session.strict,
                force_prune: session.force_prune,
                commit_control: commit_control.clone(),
            }));
            session.service_stream.commit_control = Some(commit_control);
            session.service_stream.phase = PushStreamPhase::DiskRevalidate;
            session.service_stream.next_chunk = 0;
            Ok(push_stream_response(session, service, "diskRevalidate", 0))
        }
        PushStreamPhase::DiskRevalidate => {
            if phase != "diskRevalidate"
                || !body.records.is_empty()
                || !body.sources.is_empty()
                || body.final_chunk
            {
                return Err("diskRevalidate accepts only empty continuation ticks".into());
            }
            let result = session
                .service_stream
                .commit_result
                .as_ref()
                .ok_or("diskRevalidate worker is missing")?
                .try_recv();
            match result {
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    session.service_stream.next_chunk += 1;
                    Ok(push_stream_response(
                        session,
                        service,
                        "diskRevalidate",
                        session.service_stream.next_chunk,
                    ))
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err("diskRevalidate worker disconnected".into())
                }
                Ok(Err(error)) => Err(error),
                Ok(Ok(result)) => {
                    retain_stream_commit_result(session, service, result);
                    session.next_service += 1;
                    if let Some(next_service) =
                        snapshot::SYNCED_SERVICES.get(session.next_service).copied()
                    {
                        session.service_stream = new_push_service_stream(next_service);
                        Ok(push_stream_response(session, next_service, "structure", 0))
                    } else {
                        Ok(json!({
                            "ok": true,
                            "action": "complete",
                            "streamId": session.stream_id,
                            "applied": session.applied,
                            "backups": session.backups,
                            "committedServices": session.committed_services,
                        }))
                    }
                }
            }
        }
    }
}

fn finalize_successful_stream_backups(
    root: &Path,
    session: &mut PushStreamAccumulator,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if session.committed_services.len() != snapshot::SYNCED_SERVICES.len()
        || !session
            .committed_services
            .iter()
            .zip(snapshot::SYNCED_SERVICES)
            .all(|(committed, expected)| committed.service == *expected)
    {
        return vec![
            "refusing to classify backups before every synced service committed in order".into(),
        ];
    }
    let stream_id = session.stream_id.clone();
    for committed in &mut session.committed_services {
        let Some(backup) = committed.backup.clone() else {
            continue;
        };
        match promote_successful_stream_backup(root, &backup) {
            Ok((promoted, warning)) => {
                committed.backup = Some(promoted.clone());
                if let Some(warning) = warning {
                    warnings.push(warning);
                }
                if let Err(error) =
                    write_successful_stream_backup_marker(root, &promoted, &stream_id)
                {
                    warnings.push(format!(
                        "successful backup {} remains ineligible for automatic pruning: {error}",
                        promoted.display()
                    ));
                }
            }
            Err(error) => warnings.push(format!(
                "retain successful backup {} without automatic pruning: {error}",
                backup.display()
            )),
        }
    }
    session.backups = session
        .committed_services
        .iter()
        .filter_map(|committed| committed.backup.clone())
        .collect();
    warnings.extend(prune_successful_stream_backups(root));
    warnings
}

#[derive(Default)]
struct StreamGenerationRollback {
    rolled_back_services: Vec<String>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

fn rollback_created_stream_service(
    state: &AppState,
    committed: &CommittedStreamService,
) -> Result<(), String> {
    let root = state.canonical_project.as_path();
    let live_service = root.join(&committed.service);
    if crate::fs_safety::metadata_no_follow(&live_service)
        .map_err(|error| format!("inspect created rollback target: {error}"))?
        .is_none()
    {
        return Ok(());
    }
    let current = capture_exact_tree_fingerprint(root, &committed.service)?;
    if !relocated_fingerprint_matches(&committed.installed_fingerprint, &current) {
        return Err(format!(
            "refusing to remove created service because it changed after commit: {}",
            live_service.display()
        ));
    }
    let expected = capture_synced_subtree(root, &live_service)?
        .ok_or_else(|| format!("created service disappeared: {}", live_service.display()))?;
    let revalidated = capture_exact_tree_fingerprint(root, &committed.service)?;
    if !relocated_fingerprint_matches(&committed.installed_fingerprint, &revalidated) {
        return Err(format!(
            "refusing to remove created service because it changed during rollback: {}",
            live_service.display()
        ));
    }
    let ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: true,
        strict: false,
        force_prune: false,
        project_root: root,
        backup_forced_removals: false,
        private_stage: false,
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
    };
    if !remove_synced_subtree(&live_service, &ctx, Some(&expected))? {
        return Err(format!(
            "created service disappeared before rollback: {}",
            live_service.display()
        ));
    }
    Ok(())
}

fn rollback_replaced_stream_service(
    state: &AppState,
    committed: &CommittedStreamService,
) -> Result<Option<String>, String> {
    let root = state.canonical_project.as_path();
    let transaction = committed.backup.as_ref().ok_or_else(|| {
        format!(
            "committed service {} is missing its recovery backup",
            committed.service
        )
    })?;
    let backup_service = transaction.join(&committed.service);
    let live_service = root.join(&committed.service);
    let stage_parent = root.parent().ok_or_else(|| {
        format!(
            "project root has no same-volume rollback parent: {}",
            root.display()
        )
    })?;
    let rollback_stage = tempfile::Builder::new()
        .prefix(".rosync-generation-rollback-")
        .tempdir_in(stage_parent)
        .map_err(|error| format!("create same-volume rollback stage: {error}"))?;
    let stage_service = rollback_stage.path().join(&committed.service);
    let live_parent_guard = crate::fs_safety::guard_synced_parent_chain(root, &live_service, false)
        .map_err(|error| format!("guard live rollback service: {error}"))?;
    let backup_parent_guard =
        crate::fs_safety::guard_descendant_parent_chain(root, &backup_service, false)
            .map_err(|error| format!("guard generation backup: {error}"))?;
    let stage_parent_guard = crate::fs_safety::guard_descendant_parent_chain(
        rollback_stage.path(),
        &stage_service,
        true,
    )
    .map_err(|error| format!("guard generation rollback stage: {error}"))?;
    let result = restore_stream_backup_after_install(InstalledStreamRollback {
        root,
        service: &committed.service,
        live_service: &live_service,
        backup_service: &backup_service,
        backup_transaction: transaction,
        stage_service: &stage_service,
        staged_fingerprint: &committed.installed_fingerprint,
        live_parent_guard: &live_parent_guard,
        backup_parent_guard: &backup_parent_guard,
        stage_parent_guard: &stage_parent_guard,
    })?;
    PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: true,
        strict: false,
        force_prune: false,
        project_root: root,
        backup_forced_removals: false,
        private_stage: false,
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
    }
    .mark_quiet(&live_service);
    Ok(result)
}

fn rollback_stream_generation(
    state: &AppState,
    session: &mut PushStreamAccumulator,
) -> StreamGenerationRollback {
    let mut report = StreamGenerationRollback::default();
    let mut still_committed = Vec::new();
    for committed in std::mem::take(&mut session.committed_services)
        .into_iter()
        .rev()
    {
        let result = if committed.created {
            rollback_created_stream_service(state, &committed).map(|()| None)
        } else {
            rollback_replaced_stream_service(state, &committed)
        };
        match result {
            Ok(warning) => {
                report.rolled_back_services.push(committed.service.clone());
                if let Some(warning) = warning {
                    report.warnings.push(warning);
                }
            }
            Err(error) => {
                report
                    .errors
                    .push(format!("rollback {}: {error}", committed.service));
                still_committed.push(committed);
            }
        }
    }
    still_committed.reverse();
    report.rolled_back_services.reverse();
    session.committed_services = still_committed;
    session.backups = session
        .committed_services
        .iter()
        .filter_map(|committed| committed.backup.clone())
        .collect();
    if session.committed_services.is_empty() {
        session.applied = 0;
        session.prepared_baselines.clear();
    }
    if let Some(checkpoint) = session.conflict_checkpoint.take() {
        state.conflict.restore_checkpoint(checkpoint);
    }
    report
}

fn audit_stream_push_partial(
    state: &AppState,
    session: &PushStreamAccumulator,
    failed_service: &str,
    error: &str,
) {
    let event = json!({
        "type": "stream-push-partial",
        "streamId": session.stream_id,
        "failedService": failed_service,
        "error": error,
        "applied": session.applied,
        "backups": session.backups,
        "committedServices": session.committed_services,
        "recoveryRequired": true,
    });
    if let Ok(serialized) = serde_json::to_string(&event) {
        let _ = state.events.send(serialized);
    }
    #[cfg(not(test))]
    {
        let _ = write_log_entry(Json(json!({
            "action": "stream-push-partial",
            "streamId": session.stream_id,
            "failedService": failed_service,
            "error": error,
            "applied": session.applied,
            "backups": session.backups,
            "committedServices": session.committed_services,
            "recoveryRequired": true,
        })));
    }
}

fn audit_stream_push_rolled_back(
    state: &AppState,
    session: &PushStreamAccumulator,
    failed_service: &str,
    error: &str,
    rollback: &StreamGenerationRollback,
) {
    let event = json!({
        "type": "stream-push-rolled-back",
        "streamId": session.stream_id,
        "failedService": failed_service,
        "error": error,
        "rolledBackServices": rollback.rolled_back_services,
        "rollbackWarnings": rollback.warnings,
    });
    if let Ok(serialized) = serde_json::to_string(&event) {
        let _ = state.events.send(serialized);
    }
    #[cfg(not(test))]
    {
        let _ = write_log_entry(Json(json!({
            "action": "stream-push-rolled-back",
            "streamId": session.stream_id,
            "failedService": failed_service,
            "error": error,
            "rolledBackServices": rollback.rolled_back_services,
            "rollbackWarnings": rollback.warnings,
        })));
    }
}

fn audit_stream_push_complete(
    state: &AppState,
    session: &PushStreamAccumulator,
    retention_warnings: &[String],
) {
    let event = json!({
        "type": "stream-push-complete",
        "streamId": session.stream_id,
        "applied": session.applied,
        "backups": session.backups,
        "committedServices": session.committed_services,
        "backupRetentionWarnings": retention_warnings,
    });
    if let Ok(serialized) = serde_json::to_string(&event) {
        let _ = state.events.send(serialized);
    }
    #[cfg(not(test))]
    {
        let _ = write_log_entry(Json(json!({
            "action": "stream-push-complete",
            "streamId": session.stream_id,
            "applied": session.applied,
            "backups": session.backups,
            "committedServices": session.committed_services,
            "backupRetentionWarnings": retention_warnings,
        })));
    }
}

fn consume_studio_transfer_grant(
    project: &Path,
    choice_id: &str,
) -> Result<StudioTransferGrant, String> {
    if !valid_initial_choice_token(choice_id) {
        return Err("strict streamed push has an invalid choiceId".into());
    }
    let key = (project.to_path_buf(), choice_id.to_string());
    let grant = {
        let grants = STUDIO_TRANSFER_GRANTS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut grants = grants.lock().unwrap();
        grants.retain(|_, grant| grant.created_at.elapsed() < STUDIO_TRANSFER_GRANT_TTL);
        grants.remove(&key)
    }
    .ok_or("strict streamed push choiceId is stale, consumed, or unauthorized")?;
    revalidate_initial_service_generations(project, &grant.service_generations)?;
    Ok(grant)
}

fn streamed_push(state: &AppState, body: PushBody) -> Json<Value> {
    if body.plugin_protocol != Some(crate::ws::PLUGIN_PROTOCOL_VERSION) {
        return Json(json!({
            "ok": false,
            "error": format!(
                "incompatible Studio plugin protocol; expected {}",
                crate::ws::PLUGIN_PROTOCOL_VERSION
            ),
        }));
    }
    let Some(stream_id) = body.stream_id.as_deref() else {
        return Json(json!({ "ok": false, "error": "streamed push requires streamId" }));
    };
    if stream_id.is_empty() || stream_id.len() > 128 {
        return Json(json!({ "ok": false, "error": "invalid streamed push streamId" }));
    }
    if body.strict && body.choice_id.is_none() {
        return Json(json!({
            "ok": false,
            "error": "strict streamed push requires the initial Studio choiceId",
        }));
    }
    if !body.strict && body.choice_id.is_some() {
        return Json(json!({
            "ok": false,
            "error": "non-strict streamed push cannot include an initial choiceId",
        }));
    }
    let is_start = body.service.as_deref() == snapshot::SYNCED_SERVICES.first().copied()
        && body.phase.as_deref() == Some("structure")
        && body.chunk_index == Some(0);
    let request_hash = match push_stream_request_hash(&body) {
        Ok(request_hash) => request_hash,
        Err(error) => {
            return Json(json!({ "ok": false, "error": format!("encode push chunk: {error}") }));
        }
    };
    let project = state.canonical_project.as_ref().clone();
    let sessions = PUSH_STREAM_ACCUMULATORS.get_or_init(|| Mutex::new(HashMap::new()));
    let session_handle = {
        let mut sessions = sessions.lock().unwrap();
        prune_push_stream_sessions(&mut sessions);
        match sessions.get(&project).cloned() {
            Some(session) if session.lock().unwrap().stream_id == stream_id => session,
            Some(_) if !is_start => {
                return Json(json!({
                    "ok": false,
                    "error": "streamed push session is stale; restart from structure chunk 0",
                }));
            }
            _ if !is_start => {
                return Json(json!({
                    "ok": false,
                    "error": "streamed push session is missing; restart from structure chunk 0",
                }));
            }
            _ => {
                if sessions.len() >= MAX_STREAM_SESSIONS && !sessions.contains_key(&project) {
                    return Json(json!({
                        "ok": false,
                        "error": "too many active streamed transfer sessions",
                    }));
                }
                let expected_service_generations = if body.strict {
                    match consume_studio_transfer_grant(
                        &project,
                        body.choice_id
                            .as_deref()
                            .expect("strict push choiceId was validated above"),
                    ) {
                        Ok(grant) => grant.service_generations,
                        Err(error) => {
                            return Json(json!({
                                "ok": false,
                                "stale": true,
                                "error": error,
                            }));
                        }
                    }
                } else {
                    Vec::new()
                };
                let session = Arc::new(Mutex::new(PushStreamAccumulator {
                    rollback_state: state.clone(),
                    stream_id: stream_id.to_string(),
                    choice_id: body.choice_id.clone(),
                    expected_service_generations,
                    strict: body.strict,
                    force_prune: body.force_prune || body.strict,
                    next_service: 0,
                    service_stream: new_push_service_stream(snapshot::SYNCED_SERVICES[0]),
                    applied: 0,
                    backups: Vec::new(),
                    committed_services: Vec::new(),
                    prepared_baselines: Vec::new(),
                    conflict_checkpoint: Some(state.conflict.checkpoint()),
                    accepted_stream_bytes: 0,
                    accepted_source_bytes: 0,
                    last_request_hash: None,
                    last_response: None,
                    last_activity: Instant::now(),
                    completed_at: None,
                }));
                sessions.insert(project.clone(), session.clone());
                schedule_push_stream_cleanup(project.clone(), &session, STREAM_SESSION_TTL);
                session
            }
        }
    };

    let mut session = session_handle.lock().unwrap();
    if session.strict != body.strict || session.choice_id != body.choice_id {
        return Json(json!({
            "ok": false,
            "error": "streamed push authorization changed after stream start",
        }));
    }
    if push_stream_expired(&session) {
        return Json(json!({
            "ok": false,
            "error": "streamed push session expired; restart from structure chunk 0",
        }));
    }
    if session.last_request_hash == Some(request_hash) {
        if let Some(response) = session.last_response.clone() {
            return Json(response);
        }
    }
    if session.completed_at.is_some() {
        return Json(json!({
            "ok": false,
            "error": "streamed push already completed",
        }));
    }
    let mut response = match process_streamed_push_chunk(state, &mut session, &body) {
        Ok(response) => response,
        Err(error) => {
            let failed_service = session.service_stream.service.clone();
            let (commit_already_finished, partial_failure, retained_backup) = session
                .service_stream
                .commit_control
                .as_ref()
                .map(|control| {
                    let mut control = control.lock().unwrap();
                    if !control.committed && !control.partial_failure {
                        control.cancelled = true;
                    }
                    (
                        control.committed,
                        control.partial_failure,
                        control.retained_backup.clone(),
                    )
                })
                .unwrap_or((false, false, None));
            if commit_already_finished {
                // The atomic rename finished before this malformed poll
                // arrived. Retain the session/result so a corrected cursor can
                // still observe the committed outcome.
                return Json(json!({ "ok": false, "error": error }));
            }
            let had_prior_commits = !session.committed_services.is_empty();
            let rollback = if had_prior_commits {
                rollback_stream_generation(state, &mut session)
            } else {
                if let Some(checkpoint) = session.conflict_checkpoint.take() {
                    state.conflict.restore_checkpoint(checkpoint);
                }
                StreamGenerationRollback::default()
            };
            if partial_failure {
                if let Some(backup) = retained_backup.as_ref() {
                    if !session.backups.contains(backup) {
                        session.backups.push(backup.clone());
                    }
                }
            }
            if partial_failure || !rollback.errors.is_empty() {
                let response = json!({
                    "ok": false,
                    "action": "partial",
                    "streamId": session.stream_id,
                    "error": error,
                    "failedService": failed_service,
                    "recoveryRequired": true,
                    "backups": session.backups,
                    "committedServices": session.committed_services,
                    "rolledBackServices": rollback.rolled_back_services,
                    "rollbackWarnings": rollback.warnings,
                    "rollbackErrors": rollback.errors,
                });
                session.last_request_hash = Some(request_hash);
                session.last_response = Some(response.clone());
                session.last_activity = Instant::now();
                session.completed_at = Some(Instant::now());
                audit_stream_push_partial(state, &session, &failed_service, &error);
                schedule_push_stream_cleanup(
                    project.clone(),
                    &session_handle,
                    STREAM_COMPLETED_TTL,
                );
                return Json(response);
            }
            if had_prior_commits {
                let response = json!({
                    "ok": false,
                    "action": "rolled-back",
                    "streamId": session.stream_id,
                    "error": error,
                    "failedService": failed_service,
                    "recoveryRequired": false,
                    "backups": session.backups,
                    "committedServices": session.committed_services,
                    "rolledBackServices": rollback.rolled_back_services,
                    "rollbackWarnings": rollback.warnings,
                });
                session.last_request_hash = Some(request_hash);
                session.last_response = Some(response.clone());
                session.last_activity = Instant::now();
                session.completed_at = Some(Instant::now());
                audit_stream_push_rolled_back(state, &session, &failed_service, &error, &rollback);
                schedule_push_stream_cleanup(
                    project.clone(),
                    &session_handle,
                    STREAM_COMPLETED_TTL,
                );
                return Json(response);
            }
            drop(session);
            let mut sessions = sessions.lock().unwrap();
            if sessions
                .get(&project)
                .is_some_and(|current| Arc::ptr_eq(current, &session_handle))
            {
                sessions.remove(&project);
            }
            return Json(json!({ "ok": false, "error": error }));
        }
    };
    let complete = response.get("action").and_then(Value::as_str) == Some("complete");
    if complete {
        let roots = snapshot::SYNCED_SERVICES
            .iter()
            .map(|service| state.canonical_project.join(service))
            .collect::<Vec<_>>();
        let baselines = std::mem::take(&mut session.prepared_baselines)
            .into_iter()
            .map(|baseline| (baseline.path, baseline.source_hash, baseline.fs_mtime));
        state.conflict.commit_generation(&roots, baselines);
        session.conflict_checkpoint = None;
        let retention_warnings =
            finalize_successful_stream_backups(state.canonical_project.as_path(), &mut session);
        if let Some(object) = response.as_object_mut() {
            object.insert("backups".into(), json!(session.backups));
            object.insert(
                "committedServices".into(),
                json!(session.committed_services),
            );
            if !retention_warnings.is_empty() {
                object.insert("backupRetentionWarnings".into(), json!(retention_warnings));
            }
        }
        audit_stream_push_complete(state, &session, &retention_warnings);
    }
    session.last_request_hash = Some(request_hash);
    session.last_response = Some(response.clone());
    session.last_activity = Instant::now();
    if complete {
        session.completed_at = Some(Instant::now());
        schedule_push_stream_cleanup(project, &session_handle, STREAM_COMPLETED_TTL);
    }
    Json(response)
}

fn validate_bootstrap_services(services: &[Value]) -> Result<(), String> {
    validate_bootstrap_services_with_limits(
        services,
        MAX_BOOTSTRAP_INSTANCE_DEPTH,
        MAX_BOOTSTRAP_NODES,
    )
}

fn validate_bootstrap_service_roots(
    services: &[Value],
    require_exactly_one: bool,
) -> Result<(), String> {
    if require_exactly_one && services.len() != 1 {
        return Err(format!(
            "protocol {} bootstrap requires exactly one synced service per request",
            crate::ws::PLUGIN_PROTOCOL_VERSION
        ));
    }
    let mut seen = HashSet::with_capacity(services.len());
    for service in services {
        let name = service
            .get("name")
            .and_then(Value::as_str)
            .ok_or("bootstrap service root is missing a string name")?;
        let class = service
            .get("class")
            .and_then(Value::as_str)
            .ok_or("bootstrap service root is missing a string class")?;
        if !snapshot::SYNCED_SERVICES.contains(&name) || class != name {
            return Err(format!(
                "bootstrap root {name:?} is not an allowed synced service"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("bootstrap repeats synced service {name}"));
        }
    }
    Ok(())
}

fn validate_full_tree_value(tree: &Value) -> Result<(), String> {
    match tree {
        Value::Array(nodes) => validate_bootstrap_services(nodes),
        node => validate_bootstrap_services(std::slice::from_ref(node)),
    }
}

fn validate_bootstrap_services_with_limits(
    services: &[Value],
    max_depth: usize,
    max_nodes: usize,
) -> Result<(), String> {
    let mut pending = services
        .iter()
        .rev()
        // The service itself is the traversal root. Match the disk emitter's
        // depth accounting by counting its direct children as depth 1.
        .map(|service| (service, 0usize))
        .collect::<Vec<_>>();
    let mut node_count = 0usize;

    while let Some((node, depth)) = pending.pop() {
        if depth > max_depth {
            return Err(format!(
                "Studio tree depth exceeds the supported limit of {max_depth} instances"
            ));
        }
        node_count = node_count
            .checked_add(1)
            .ok_or_else(|| "Studio tree node count overflowed".to_string())?;
        if node_count > max_nodes {
            return Err(format!(
                "Studio tree contains more than the supported limit of {max_nodes} instances"
            ));
        }

        let object = node
            .as_object()
            .ok_or_else(|| "Studio tree node must be an object".to_string())?;
        if object.get("name").and_then(Value::as_str).is_none() {
            return Err("Studio tree node is missing a string name".to_string());
        }
        if object.get("class").and_then(Value::as_str).is_none() {
            return Err("Studio tree node is missing a string class".to_string());
        }
        match object.get("children") {
            None | Some(Value::Null) => {}
            Some(Value::Array(children)) => {
                pending.extend(children.iter().rev().map(|child| (child, depth + 1)));
            }
            Some(_) => return Err("Studio tree node children must be an array".to_string()),
        }
    }
    Ok(())
}

async fn push(State(state): State<AppState>, Json(body): Json<PushBody>) -> Json<Value> {
    match run_handler_blocking(move || push_blocking(&state, body)).await {
        Ok(response) => response,
        Err(error) => Json(json!({
            "ok": false,
            "applied": 0,
            "skipped": 0,
            "conflicts": [],
            "errors": [error],
        })),
    }
}

fn push_blocking(state: &AppState, body: PushBody) -> Json<Value> {
    if body.stream_id.is_some() || body.phase.is_some() {
        return streamed_push(state, body);
    }
    if body.initial_delta {
        return initial_studio_delta(state, body);
    }
    if body.bootstrap && body.plugin_protocol != Some(crate::ws::PLUGIN_PROTOCOL_VERSION) {
        return Json(json!({
            "ok": false,
            "applied": 0,
            "skipped": 0,
            "conflicts": [],
            "errors": [format!(
                "incompatible Studio plugin protocol; expected {}. Reinstall the Studio plugin.",
                crate::ws::PLUGIN_PROTOCOL_VERSION
            )],
        }));
    }
    if body.bootstrap {
        if let Err(error) = validate_bootstrap_service_roots(&body.services, true)
            .and_then(|_| validate_bootstrap_services(&body.services))
        {
            return Json(json!({
                "ok": false,
                "applied": 0,
                "skipped": 0,
                "conflicts": [],
                "errors": [format!("bootstrap: {error}")],
            }));
        }
    }
    let root = state.canonical_project.as_path();
    let ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: false,
        strict: false,
        force_prune: false,
        project_root: root,
        backup_forced_removals: true,
        private_stage: false,
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
    };
    let mut res = PushApplyResult::default();

    if body.bootstrap {
        let bootstrap_ctx = PushCtx {
            conflicts: state.conflict.as_ref(),
            push_quiet: state.push_quiet.as_ref(),
            force_overwrite: true,
            strict: body.strict,
            force_prune: body.force_prune,
            project_root: root,
            backup_forced_removals: true,
            private_stage: false,
            dirty_parents: Mutex::new(std::collections::HashSet::new()),
        };
        for svc in &body.services {
            match apply_service_node(root, svc, &bootstrap_ctx) {
                Ok(n) => res.applied += n,
                Err(e) => res.errors.push(format!("bootstrap: {e}")),
            }
        }
    }

    apply_ops_into(root, &body.ops, &ctx, &mut res);

    Json(json!({
        "ok": res.errors.is_empty(),
        "applied": res.applied,
        "skipped": res.skipped,
        "conflicts": res.conflicts,
        "errors": res.errors,
    }))
}

fn initial_studio_delta(state: &AppState, body: PushBody) -> Json<Value> {
    if body.bootstrap
        || body.strict
        || body.force_prune
        || !body.services.is_empty()
        || body.plugin_protocol != Some(crate::ws::PLUGIN_PROTOCOL_VERSION)
    {
        return Json(json!({
            "ok": false,
            "stale": false,
            "error": "initial Studio delta has incompatible transfer options",
        }));
    }
    let Some(choice_id) = body.choice_id.as_deref() else {
        return Json(json!({
            "ok": false,
            "stale": false,
            "error": "initial Studio delta requires choiceId",
        }));
    };
    let grant = match consume_studio_transfer_grant(state.canonical_project.as_path(), choice_id) {
        Ok(grant) => grant,
        Err(error) => {
            return Json(json!({ "ok": false, "stale": true, "error": error }));
        }
    };
    if grant.delta_items.is_empty() {
        return Json(json!({
            "ok": false,
            "stale": true,
            "error": "initial choice has no bounded delta comparison rows",
        }));
    }

    let mut received = std::collections::HashSet::with_capacity(body.ops.len());
    for op in &body.ops {
        let op_kind = op.get("op").and_then(Value::as_str).unwrap_or("");
        let Some(path) = op.get("path").and_then(Value::as_array) else {
            return Json(json!({ "ok": false, "stale": true, "error": "delta operation is missing path" }));
        };
        let mut segments = Vec::with_capacity(path.len() + 1);
        for segment in path {
            let Some(segment) = segment.as_str() else {
                return Json(json!({ "ok": false, "stale": true, "error": "delta path contains a non-string segment" }));
            };
            segments.push(segment);
        }
        if op_kind == "set" {
            let Some(name) = op
                .get("node")
                .and_then(Value::as_object)
                .and_then(|node| node.get("name"))
                .and_then(Value::as_str)
            else {
                return Json(json!({ "ok": false, "stale": true, "error": "delta set is missing its node name" }));
            };
            segments.push(name);
        }
        let generated_path = segments.join("/");
        let Some(expected) = grant.delta_items.get(&generated_path) else {
            return Json(json!({
                "ok": false,
                "stale": true,
                "error": "delta operation is outside the authorized comparison set",
            }));
        };
        let source_only = op
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| {
                properties.len() == 1 && properties.get("Source").and_then(Value::as_str).is_some()
            });
        let action_matches = match expected.action {
            InitialChoiceAction::Create => op_kind == "delete",
            InitialChoiceAction::Remove => op_kind == "set",
            InitialChoiceAction::Overwrite => {
                op_kind == "set"
                    || (op_kind == "update"
                        && expected.kind == "script"
                        && !expected.class_changed
                        && expected.source_changed
                        && source_only)
            }
        };
        let expected_class = match expected.action {
            InitialChoiceAction::Remove => expected.class.as_deref(),
            InitialChoiceAction::Overwrite => expected.studio_class.as_deref(),
            InitialChoiceAction::Create => None,
        };
        let class_matches = op_kind != "set"
            || expected_class.is_none()
            || op
                .get("node")
                .and_then(Value::as_object)
                .and_then(|node| node.get("class"))
                .and_then(Value::as_str)
                == expected_class;
        if !action_matches || !class_matches || !received.insert(generated_path) {
            return Json(json!({
                "ok": false,
                "stale": true,
                "error": "delta operation does not match its authorized comparison row",
            }));
        }
    }
    if received.len() != grant.delta_items.len()
        || !grant.delta_items.keys().all(|path| received.contains(path))
    {
        return Json(json!({
            "ok": false,
            "stale": true,
            "error": "initial Studio delta did not include the complete comparison set",
        }));
    }

    let root = state.canonical_project.as_path();
    let ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: true,
        strict: false,
        force_prune: false,
        project_root: root,
        backup_forced_removals: true,
        private_stage: false,
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
    };
    let mut res = PushApplyResult::default();
    apply_ops_into(root, &body.ops, &ctx, &mut res);
    Json(json!({
        "ok": res.errors.is_empty() && res.conflicts.is_empty(),
        "deltaApplied": res.applied,
        "skipped": res.skipped,
        "conflicts": res.conflicts,
        "errors": res.errors,
    }))
}

/// Aggregate result of applying a batch of plugin push ops.
#[derive(Default, Debug)]
pub(crate) struct PushApplyResult {
    pub applied: usize,
    pub skipped: usize,
    pub conflicts: Vec<String>,
    pub errors: Vec<String>,
}

/// Apply a slice of plugin-shape ops against the project root, folding each
/// outcome into `out`. Shared between the HTTP `/push` handler and the
/// WebSocket `push` frame handler.
pub(crate) fn apply_ops_into(
    root: &Path,
    ops: &[Value],
    ctx: &PushCtx<'_>,
    out: &mut PushApplyResult,
) {
    for op in ops {
        match apply_op(root, op, ctx) {
            Ok(ApplyOutcome::Applied(n)) => out.applied += n,
            Ok(ApplyOutcome::Skipped) => out.skipped += 1,
            Ok(ApplyOutcome::Conflict(p)) => out.conflicts.push(p.display().to_string()),
            Err(e) => out.errors.push(e),
        }
    }
}

/// Apply a batch of plugin push ops using `state`. Used by the WebSocket
/// handler; constructs a `PushCtx` internally so callers don't have to touch
/// the conflict/quiet machinery.
pub(crate) fn apply_push_ops(state: &AppState, ops: &[Value]) -> PushApplyResult {
    let root = state.canonical_project.as_path();
    let ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: false,
        strict: false,
        force_prune: false,
        project_root: root,
        backup_forced_removals: true,
        private_stage: false,
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
    };
    let mut out = PushApplyResult::default();
    apply_ops_into(root, ops, &ctx, &mut out);
    out
}

/// Handles wired into every /push sub-handler so writes can (a) consult the
/// conflict engine and (b) mark paths as "we just wrote this" to suppress the
/// watcher's echo (Argon `SYNCBACK_DEBOUNCE_TIME`).
pub(crate) struct PushCtx<'a> {
    pub conflicts: &'a crate::conflict::ConflictEngine,
    pub push_quiet: &'a Mutex<HashMap<PathBuf, Instant>>,
    pub force_overwrite: bool,
    pub strict: bool,
    pub force_prune: bool,
    pub project_root: &'a Path,
    pub backup_forced_removals: bool,
    /// This is a fresh, randomly named, process-owned staging tree. Its files
    /// can be written directly because the live service remains protected by
    /// the outer generation fence and atomic service-directory rename.
    pub private_stage: bool,
    /// Parents that received an atomic file replacement in this batch.
    pub dirty_parents: Mutex<std::collections::HashSet<PathBuf>>,
}

impl<'a> Drop for PushCtx<'a> {
    fn drop(&mut self) {
        let parents = std::mem::take(&mut *self.dirty_parents.lock().unwrap());
        for parent in parents {
            sync_directory_best_effort(&parent);
        }
    }
}

impl<'a> PushCtx<'a> {
    fn note_dirty_parent(&self, parent: &Path) {
        self.dirty_parents
            .lock()
            .unwrap()
            .insert(parent.to_path_buf());
    }

    fn mark_quiet(&self, path: &Path) {
        // The watcher cannot observe the process-owned staging tree. Quiet
        // keys are published for the live service after the atomic install.
        if self.private_stage {
            return;
        }
        // Every production path is constructed below the already-canonical
        // project root. Keep the watcher key lexical: canonicalizing a path
        // here would follow a link/reparse point that appeared concurrently
        // and could alias an unrelated external tree.
        let canon = path
            .strip_prefix(self.project_root)
            .map(|relative| self.project_root.join(relative))
            .unwrap_or_else(|_| path.to_path_buf());
        let deadline = Instant::now() + Duration::from_millis(PUSH_QUIET_MS);
        let mut guard = self.push_quiet.lock().unwrap();
        guard.insert(canon, deadline);
    }
}

// ---------------------------------------------------------------------------
// /poll — long-poll filesystem → plugin
// ---------------------------------------------------------------------------
#[derive(Deserialize)]
struct PollParams {
    #[serde(default)]
    #[allow(dead_code)]
    since: Option<u64>,
}

async fn poll(State(state): State<AppState>, Query(_params): Query<PollParams>) -> Json<Value> {
    let mut rx = state.events.subscribe();
    let root = state.canonical_project.as_path();
    let mut out: Vec<Value> = Vec::new();

    // Wait up to 30s for the first conflict-filtered op, then drain anything
    // else that arrived within a brief coalesce window so bursts go together.
    let first = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let ops = event_to_plugin_ops(root, &event);
                    if !ops.is_empty() {
                        return Some(ops);
                    }
                }
                Err(_) => return None,
            }
        }
    })
    .await;
    match first {
        Ok(Some(ops)) => out.extend(ops),
        Ok(None) => {}
        Err(_) => {
            // Timeout — return empty, plugin re-polls immediately.
            return Json(json!({ "ok": true, "ops": out }));
        }
    }

    // Brief drain window.
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        out.extend(event_to_plugin_ops(root, &event));
    }

    Json(json!({ "ok": true, "ops": out }))
}

fn flatten_plugin_op(op: Value) -> Vec<Value> {
    if op.get("op").and_then(Value::as_str) != Some("batch") {
        return vec![op];
    }
    op.get("ops")
        .and_then(Value::as_array)
        .filter(|ops| !ops.is_empty() && ops.len() <= 8)
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn event_to_plugin_ops(root: &Path, event: &str) -> Vec<Value> {
    let Ok(value) = serde_json::from_str::<Value>(event) else {
        return Vec::new();
    };
    if value.get("type").and_then(Value::as_str) == Some("plugin-op") {
        return value
            .get("op")
            .cloned()
            .map(flatten_plugin_op)
            .unwrap_or_default();
    }
    if value.get("type").and_then(Value::as_str) != Some("op") {
        return Vec::new();
    }
    let Some(raw_op) = value.get("op").cloned() else {
        return Vec::new();
    };
    // Journaled protocol-6 events already contain plugin-shaped operations.
    // Legacy filesystem events contain the daemon's typed Op and still need
    // conversion at this compatibility boundary.
    if raw_op.get("op").and_then(Value::as_str).is_some() {
        return flatten_plugin_op(raw_op);
    }
    let Ok(op) = serde_json::from_value::<Op>(raw_op) else {
        return Vec::new();
    };
    fs_op_to_plugin_op(root, &op)
        .map(flatten_plugin_op)
        .unwrap_or_default()
}

fn broadcast_filtered_op(
    events: &broadcast::Sender<String>,
    root: &Path,
    op: &Op,
) -> Result<(), String> {
    let plugin_op = fs_op_to_plugin_op(root, op)
        .ok_or_else(|| "resolved operation could not be projected for Studio".to_string())?;
    let payload = crate::ws::journal_op_event(&plugin_op)
        .ok_or_else(|| "serialize journaled operation".to_string())?;
    events
        .send(payload)
        .map(|_| ())
        .map_err(|_| "no connected client can receive the resolved conflict".to_string())
}

fn broadcast_plugin_op(events: &broadcast::Sender<String>, op: Value) -> Result<(), String> {
    for nested in flatten_plugin_op(op) {
        let payload = crate::ws::journal_op_event(&nested)
            .ok_or_else(|| "serialize journaled plugin operation".to_string())?;
        events
            .send(payload)
            .map_err(|_| "no connected client can receive the resolved conflict".to_string())?;
    }
    Ok(())
}

fn deliver_prepared_rename(
    events: &broadcast::Sender<String>,
    rename: &Value,
    retained: &[Value],
) -> Result<usize, (String, usize)> {
    deliver_prepared_rename_with(rename, retained, |op| {
        broadcast_plugin_op(events, op.clone())
    })
}

fn deliver_prepared_rename_with<F>(
    rename: &Value,
    retained: &[Value],
    mut deliver: F,
) -> Result<usize, (String, usize)>
where
    F: FnMut(&Value) -> Result<(), String>,
{
    let mut delivered = 0usize;
    for op in std::iter::once(rename).chain(retained.iter()) {
        if let Err(error) = deliver(op) {
            return Err((error, delivered));
        }
        delivered += 1;
    }
    Ok(delivered)
}

fn compensate_studio_rename(
    events: &broadcast::Sender<String>,
    root: &Path,
    applied_rename: &Value,
    from: &Path,
    to: &Path,
    conflict_path: &Path,
    studio_bytes: &[u8],
) -> bool {
    if applied_rename.get("op").and_then(Value::as_str) != Some("rename") {
        // Class-changing renames require reconstructing the original class and
        // cannot be safely guessed after a partial apply.
        return false;
    }
    let Some(rename_from) = applied_rename.get("from").cloned() else {
        return false;
    };
    let Some(rename_to) = applied_rename.get("to").cloned() else {
        return false;
    };

    let destination_conflict = if conflict_path == from {
        to.to_path_buf()
    } else if let Ok(suffix) = conflict_path.strip_prefix(from) {
        to.join(suffix)
    } else {
        return false;
    };
    let Ok(relative) = destination_conflict.strip_prefix(root) else {
        return false;
    };
    let segments = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(String::from))
        .collect::<Vec<_>>();
    let Some(destination_lookup) = segs_to_lookup_path(&segments) else {
        return false;
    };
    let Ok(studio_source) = std::str::from_utf8(studio_bytes) else {
        return false;
    };

    let restore_source = json!({
        "op": "update",
        "path": destination_lookup,
        "properties": { "Source": studio_source },
    });
    let reverse_rename = json!({
        "op": "rename",
        "from": rename_to,
        "to": rename_from,
    });
    broadcast_plugin_op(events, restore_source).is_ok()
        && broadcast_plugin_op(events, reverse_rename).is_ok()
}

fn mark_conflict_resolution_quiet(state: &AppState, path: &Path) {
    let canon = path
        .strip_prefix(state.canonical_project.as_path())
        .map(|relative| state.canonical_project.join(relative))
        .unwrap_or_else(|_| path.to_path_buf());
    let deadline = Instant::now() + Duration::from_millis(PUSH_QUIET_MS);
    state.push_quiet.lock().unwrap().insert(canon, deadline);
}

fn audit_conflict_resolution(action: &str, fields: Value) {
    let mut entry = json!({
        "source": "filesystem-sync-conflict",
        "action": action,
        "outcome": "resolved",
    });
    if let (Some(entry), Some(fields)) = (entry.as_object_mut(), fields.as_object()) {
        entry.extend(fields.clone());
    }
    let _ = write_log_entry(Json(entry));
}

#[cfg(test)]
fn conflict_swap_path(parent: &Path, label: &str) -> Result<PathBuf, String> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    use std::sync::atomic::Ordering;
    for _ in 0..64 {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".rosync-conflict-{label}-{}-{sequence}.swp",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not allocate conflict rollback path in {}",
        parent.display()
    ))
}

#[cfg(test)]
fn write_conflict_temp(parent: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write as _;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create conflict temp parent {}: {error}", parent.display()))?;
    for _ in 0..64 {
        let path = conflict_swap_path(parent, "write")?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("write conflict temp {}: {error}", path.display()));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("create conflict temp {}: {error}", path.display()));
            }
        }
    }
    Err(format!(
        "could not create conflict temp file in {}",
        parent.display()
    ))
}

fn restore_fs_rename_transactional(
    from: &Path,
    to: &Path,
    conflict_path: &Path,
    studio_bytes: &[u8],
    ctx: &PushCtx<'_>,
) -> Result<(), String> {
    let from_metadata = crate::fs_safety::metadata_no_follow(from)
        .map_err(|error| format!("inspect restore source {}: {error}", from.display()))?;
    let to_metadata = crate::fs_safety::metadata_no_follow(to)
        .map_err(|error| format!("inspect retained destination {}: {error}", to.display()))?;
    if from_metadata.is_some() || to_metadata.is_none() {
        return Err(format!(
            "restore rename requires only the retained destination to exist (from={}, to={})",
            from_metadata.is_some(),
            to_metadata.is_some()
        ));
    }
    let retained_fence = capture_synced_subtree(ctx.project_root, to)?
        .ok_or_else(|| format!("retained rename destination disappeared: {}", to.display()))?;
    let from_parent = from
        .parent()
        .ok_or_else(|| format!("restore rename has no source parent: {}", from.display()))?;
    ensure_synced_directory_chain(ctx.project_root, from_parent)?;
    let to_parent = to
        .parent()
        .ok_or_else(|| format!("restore rename has no destination parent: {}", to.display()))?;
    let from_guard = crate::fs_safety::guard_synced_directory_chain(ctx.project_root, from_parent)
        .map_err(|error| {
            format!(
                "guard restore source parent {}: {error}",
                from_parent.display()
            )
        })?;
    let to_guard = crate::fs_safety::guard_synced_directory_chain(ctx.project_root, to_parent)
        .map_err(|error| {
            format!(
                "guard retained destination parent {}: {error}",
                to_parent.display()
            )
        })?;
    from_guard.verify().map_err(|error| {
        format!(
            "verify restore source parent {}: {error}",
            from_parent.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "verify retained destination parent {}: {error}",
            to_parent.display()
        )
    })?;
    std::fs::rename(to, from).map_err(|error| {
        format!(
            "restore rename {} -> {}: {error}",
            to.display(),
            from.display()
        )
    })?;
    from_guard.verify().map_err(|error| {
        format!(
            "restore source parent changed during rename {}: {error}",
            from_parent.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "retained destination parent changed during rename {}: {error}",
            to_parent.display()
        )
    })?;
    let restored_fence = capture_synced_subtree(ctx.project_root, from)?
        .ok_or_else(|| format!("restored rename source disappeared: {}", from.display()))?;
    if !relocated_subtree_matches(&retained_fence, &restored_fence) {
        return Err(format!(
            "retained subtree changed during restore rename: {}",
            from.display()
        ));
    }
    if let Some(parent) = conflict_path.parent() {
        ensure_synced_directory_chain(ctx.project_root, parent)?;
    }
    if let Err(error) = write_synced_file_atomic(conflict_path, studio_bytes, ctx) {
        let rollback = rollback_restored_rename_if_unchanged(
            ctx.project_root,
            from,
            to,
            &restored_fence,
            &from_guard,
            &to_guard,
        );
        return Err(format!(
            "install restored Studio source {}: {error}; directory rollback: {}",
            conflict_path.display(),
            rollback
                .map(|_| "ok".to_string())
                .unwrap_or_else(|rollback| rollback.to_string())
        ));
    }
    Ok(())
}

fn rollback_restored_rename_if_unchanged(
    project_root: &Path,
    from: &Path,
    to: &Path,
    restored_fence: &SafeSubtreeFence,
    from_guard: &crate::fs_safety::PathParentGuard,
    to_guard: &crate::fs_safety::PathParentGuard,
) -> Result<(), String> {
    from_guard.verify().map_err(|error| {
        format!(
            "refusing directory rollback because source parent changed {}: {error}",
            from.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "refusing directory rollback because destination parent changed {}: {error}",
            to.display()
        )
    })?;
    let current = capture_synced_subtree(project_root, from)?.ok_or_else(|| {
        format!(
            "refusing directory rollback because restored source disappeared: {}",
            from.display()
        )
    })?;
    if !relocated_subtree_matches(restored_fence, &current) {
        return Err(format!(
            "refusing directory rollback because restored source changed: {}",
            from.display()
        ));
    }
    if crate::fs_safety::metadata_no_follow(to)
        .map_err(|error| format!("inspect rollback destination {}: {error}", to.display()))?
        .is_some()
    {
        return Err(format!(
            "refusing directory rollback because destination appeared: {}",
            to.display()
        ));
    }
    from_guard.verify().map_err(|error| {
        format!(
            "refusing directory rollback because source parent changed {}: {error}",
            from.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "refusing directory rollback because destination parent changed {}: {error}",
            to.display()
        )
    })?;
    std::fs::rename(from, to).map_err(|error| {
        format!(
            "restore rollback {} -> {}: {error}",
            from.display(),
            to.display()
        )
    })?;
    from_guard.verify().map_err(|error| {
        format!(
            "source parent changed during directory rollback {}: {error}",
            from.display()
        )
    })?;
    to_guard.verify().map_err(|error| {
        format!(
            "destination parent changed during directory rollback {}: {error}",
            to.display()
        )
    })?;
    Ok(())
}

fn restore_fs_deleted_source(
    path: &Path,
    studio_bytes: &[u8],
    ctx: &PushCtx<'_>,
) -> Result<(), String> {
    if crate::fs_safety::metadata_no_follow(path)
        .map_err(|error| format!("inspect restored source {}: {error}", path.display()))?
        .is_some()
    {
        return Err(format!(
            "refusing to restore deleted source because {} already exists",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("restored source has no parent: {}", path.display()))?;
    ensure_synced_directory_chain(ctx.project_root, parent)?;
    write_synced_file_atomic(path, studio_bytes, ctx)
}

const DIRECTORY_DELETE_RESTORE_ERROR: &str =
    "cannot safely restore a directory deleted from disk from one conflicted source; no files were written and the conflict remains parked. Restore the full subtree from Studio before resolving it";

fn validate_fs_delete_restore(is_dir: bool) -> Result<(), &'static str> {
    if is_dir {
        Err(DIRECTORY_DELETE_RESTORE_ERROR)
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn restore_fs_deleted_source_with<R>(
    path: &Path,
    studio_bytes: &[u8],
    mut rename: R,
) -> Result<(), String>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    if path.exists() {
        return Err(format!(
            "refusing to restore deleted source because {} already exists",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("restored source has no parent: {}", path.display()))?;
    let temporary = write_conflict_temp(parent, studio_bytes)?;
    if path.exists() {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "refusing to restore deleted source because {} appeared during restore",
            path.display()
        ));
    }
    if let Err(error) = rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "install restored source {}: {error}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
fn restore_fs_rename_transactional_with<R>(
    from: &Path,
    to: &Path,
    conflict_path: &Path,
    studio_bytes: &[u8],
    mut rename: R,
) -> Result<(), String>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    if from.exists() || !to.exists() {
        return Err(format!(
            "restore rename requires only the retained destination to exist (from={}, to={})",
            from.exists(),
            to.exists()
        ));
    }
    let temp_parent = from.parent().ok_or_else(|| {
        format!(
            "restore rename has no parent for original path {}",
            from.display()
        )
    })?;
    let write_temp = write_conflict_temp(temp_parent, studio_bytes)?;

    if let Err(error) = rename(to, from) {
        let _ = std::fs::remove_file(&write_temp);
        return Err(format!(
            "restore rename {} -> {}: {error}",
            to.display(),
            from.display()
        ));
    }

    if let Some(parent) = conflict_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            let rollback = rename(from, to);
            let _ = std::fs::remove_file(&write_temp);
            return Err(format!(
                "create restored source parent {}: {error}; directory rollback: {}",
                parent.display(),
                rollback
                    .map(|_| "ok".to_string())
                    .unwrap_or_else(|rollback| rollback.to_string())
            ));
        }
    }

    let backup = if conflict_path.exists() {
        let parent = conflict_path.parent().unwrap_or(temp_parent);
        let backup = match conflict_swap_path(parent, "backup") {
            Ok(backup) => backup,
            Err(error) => {
                let rollback = rename(from, to);
                let _ = std::fs::remove_file(&write_temp);
                return Err(format!(
                    "{error}; directory rollback: {}",
                    rollback
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|rollback| rollback.to_string())
                ));
            }
        };
        if let Err(error) = rename(conflict_path, &backup) {
            let rollback = rename(from, to);
            let _ = std::fs::remove_file(&write_temp);
            return Err(format!(
                "backup restored source {}: {error}; directory rollback: {}",
                conflict_path.display(),
                rollback
                    .map(|_| "ok".to_string())
                    .unwrap_or_else(|rollback| rollback.to_string())
            ));
        }
        Some(backup)
    } else {
        None
    };

    if let Err(error) = rename(&write_temp, conflict_path) {
        let source_rollback = backup.as_ref().map(|backup| rename(backup, conflict_path));
        let directory_rollback = rename(from, to);
        let _ = std::fs::remove_file(&write_temp);
        return Err(format!(
            "install restored Studio source {}: {error}; source rollback: {}; directory rollback: {}",
            conflict_path.display(),
            source_rollback
                .map(|result| result.map(|_| "ok".to_string()).unwrap_or_else(|error| error.to_string()))
                .unwrap_or_else(|| "not-needed".to_string()),
            directory_rollback
                .map(|_| "ok".to_string())
                .unwrap_or_else(|error| error.to_string())
        ));
    }

    if let Some(backup) = backup {
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

fn collect_tree_update_ops(
    project_root: &Path,
    path: &Path,
    out: &mut Vec<Op>,
) -> Result<(), String> {
    let fence = capture_synced_subtree(project_root, path)?
        .ok_or_else(|| format!("resolved local tree disappeared: {}", path.display()))?;
    for entry in &fence.entries {
        let is_dir = entry.kind == crate::fs_safety::SafeEntryKind::Directory;
        let content = if is_dir {
            None
        } else {
            let Some(name) = entry.path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if classify_script_file(name).is_none() && !is_init_file(name) {
                continue;
            }
            Some(read_synced_file(project_root, &entry.path)?)
        };
        out.push(Op {
            kind: OpKind::Update,
            path: entry.path.clone(),
            from: None,
            content,
            is_dir: Some(is_dir),
        });
    }
    Ok(())
}

async fn wait_for_source_acks(
    conflicts: &crate::conflict::ConflictEngine,
    ops: &[Op],
    timeout: Duration,
) -> bool {
    let expected: Vec<(&Path, Vec<u8>)> = ops
        .iter()
        .filter_map(|op| {
            op.content.as_deref().map(|content| {
                (
                    op.path.as_path(),
                    normalize_line_endings(content).into_owned(),
                )
            })
        })
        .collect();
    if expected.is_empty() {
        return true;
    }

    let deadline = Instant::now() + timeout;
    loop {
        if expected
            .iter()
            .all(|(path, content)| conflicts.matches_baseline(path, content))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn restore_resolved_conflict(
    state: &AppState,
    target: &Path,
    bytes: Vec<u8>,
    is_dir: bool,
    rejected_studio: Option<Vec<u8>>,
) {
    if let Some(studio_bytes) = rejected_studio {
        state
            .conflict
            .park_studio_update(target, bytes, studio_bytes, fs_mtime(target));
    } else {
        state
            .conflict
            .park_studio_delete(target, bytes, fs_mtime(target), is_dir);
    }
}

// ---------------------------------------------------------------------------
// /events — SSE stream
// ---------------------------------------------------------------------------

async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(msg) => Some(Ok(Event::default().data(msg))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// /resolve
// ---------------------------------------------------------------------------

async fn resolve_list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "conflicts": state.conflict.list(),
    }))
}

#[derive(Deserialize)]
struct ResolveBody {
    path: String,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    choice: Option<String>,
}

fn parse_resolution(raw: &str) -> Result<Resolution, String> {
    match raw {
        "keep-local" | "keep-disk" | "keep_disk" | "keep_fs" | "fs" | "local" | "disk" => {
            Ok(Resolution::KeepLocal)
        }
        "keep-studio" | "keep_studio" | "studio" => Ok(Resolution::KeepStudio),
        other => Err(format!("unknown resolution: {other}")),
    }
}

fn resolve_conflict_target(project: &Path, raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    let candidate = if path.is_absolute() {
        path
    } else {
        project.join(path)
    };
    crate::fs_safety::validate_synced_path(project, &candidate, true)
        .map(|_| candidate.clone())
        .map_err(|error| format!("unsafe conflict path {}: {error}", candidate.display()))
}

async fn resolve(
    State(state): State<AppState>,
    Json(body): Json<ResolveBody>,
) -> impl IntoResponse {
    let raw = body.resolution.or(body.choice).unwrap_or_default();
    let resolution = match parse_resolution(&raw) {
        Ok(resolution) => resolution,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": error,
            }));
        }
    };

    let target = match resolve_conflict_target(&state.canonical_project, &body.path) {
        Ok(target) => target,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "error": error,
                "path": body.path,
            }));
        }
    };
    if resolution == Resolution::KeepLocal && state.active_plugin.lock().unwrap().is_none() {
        return Json(json!({
            "ok": false,
            "error": "cannot keep local while the Studio plugin is disconnected",
            "path": body.path,
        }));
    }
    let Some(decision) = state.conflict.resolve(&target, resolution) else {
        return Json(json!({
            "ok": false,
            "error": "no parked conflict for that path",
            "path": body.path,
        }));
    };
    let resolution_ctx = PushCtx {
        conflicts: state.conflict.as_ref(),
        push_quiet: state.push_quiet.as_ref(),
        force_overwrite: true,
        strict: false,
        force_prune: false,
        project_root: state.canonical_project.as_path(),
        backup_forced_removals: false,
        private_stage: false,
        dirty_parents: Mutex::new(std::collections::HashSet::new()),
    };

    match decision {
        Resolved::WriteFs(bytes) => {
            if let Some(parent) = target.parent() {
                if let Err(error) =
                    ensure_synced_directory_chain(state.canonical_project.as_path(), parent)
                {
                    return Json(json!({ "ok": false, "error": error }));
                }
            }
            if let Err(error) = write_synced_file_atomic(&target, &bytes, &resolution_ctx) {
                return Json(json!({ "ok": false, "error": error }));
            }
            state
                .conflict
                .record_sync(&target, hash(&bytes), fs_mtime(&target));
            Json(json!({ "ok": true, "action": "wrote-fs", "path": body.path }))
        }
        Resolved::PushStudio {
            bytes,
            is_dir,
            rejected_studio,
        } => {
            let ops = if is_dir {
                let mut ops = Vec::new();
                if let Err(error) =
                    collect_tree_update_ops(state.canonical_project.as_path(), &target, &mut ops)
                {
                    state
                        .conflict
                        .park_studio_delete(&target, bytes, fs_mtime(&target), true);
                    return Json(json!({ "ok": false, "error": error }));
                }
                ops
            } else {
                vec![Op {
                    kind: OpKind::Update,
                    path: target.clone(),
                    from: None,
                    content: Some(bytes.clone()),
                    is_dir: Some(false),
                }]
            };
            let delivery = ops
                .iter()
                .try_for_each(|op| {
                    broadcast_filtered_op(&state.events, state.canonical_project.as_path(), op)
                });
            if let Err(error) = delivery {
                restore_resolved_conflict(&state, &target, bytes, is_dir, rejected_studio);
                return Json(json!({ "ok": false, "error": error }));
            }
            if !wait_for_source_acks(state.conflict.as_ref(), &ops, Duration::from_secs(5)).await {
                restore_resolved_conflict(&state, &target, bytes, is_dir, rejected_studio);
                return Json(json!({
                    "ok": false,
                    "error": "Studio did not acknowledge the resolved source; conflict remains parked",
                }));
            }
            Json(json!({ "ok": true, "action": "pushed-studio", "path": body.path }))
        }
        Resolved::DeleteFs { bytes, is_dir } => {
            if let Err(error) = remove_synced_subtree(&target, &resolution_ctx, None) {
                state
                    .conflict
                    .park_studio_delete(&target, bytes, fs_mtime(&target), is_dir);
                return Json(json!({ "ok": false, "error": error }));
            }
            state.conflict.forget_path(&target);
            Json(json!({ "ok": true, "action": "deleted-fs", "path": body.path }))
        }
        Resolved::DeleteStudio {
            path,
            conflict_path,
            studio_bytes,
            is_dir,
        } => {
            let op = Op {
                kind: OpKind::Delete,
                path: path.clone(),
                from: None,
                content: None,
                is_dir: Some(is_dir),
            };
            if fs_op_to_plugin_op(state.canonical_project.as_path(), &op).is_none() {
                state
                    .conflict
                    .park_fs_delete_conflict(&conflict_path, &path, studio_bytes, is_dir);
                return Json(json!({
                    "ok": false,
                    "error": format!("cannot map disk delete {} to a Studio path", path.display()),
                }));
            }
            if let Err(error) = broadcast_filtered_op(
                &state.events,
                state.canonical_project.as_path(),
                &op,
            ) {
                state
                    .conflict
                    .park_fs_delete_conflict(&conflict_path, &path, studio_bytes, is_dir);
                return Json(json!({ "ok": false, "error": error }));
            }
            state.conflict.commit_fs_delete(&path);
            audit_conflict_resolution(
                "delete-studio",
                json!({ "path": path, "resolution": "keep-disk" }),
            );
            Json(json!({
                "ok": true,
                "action": "deleted-studio",
                "path": body.path,
            }))
        }
        Resolved::RenameStudio {
            from,
            to,
            is_dir,
            conflict_path,
            studio_bytes,
            local_bytes,
        } => {
            let rename = Op {
                kind: OpKind::Rename,
                path: to.clone(),
                from: Some(from.clone()),
                content: None,
                is_dir: Some(is_dir),
            };
            let mut ops = Vec::new();
            if let Err(error) =
                collect_tree_update_ops(state.canonical_project.as_path(), &to, &mut ops)
            {
                state.conflict.park_fs_rename_conflict(
                    &conflict_path,
                    &from,
                    &to,
                    local_bytes,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({ "ok": false, "error": error }));
            }
            let mut retained_plugin_ops = Vec::with_capacity(ops.len());
            for op in &ops {
                let Some(plugin_op) = fs_op_to_plugin_op(state.canonical_project.as_path(), op)
                else {
                    state.conflict.park_fs_rename_conflict(
                        &conflict_path,
                        &from,
                        &to,
                        local_bytes,
                        studio_bytes,
                        is_dir,
                    );
                    return Json(json!({
                        "ok": false,
                        "error": format!(
                            "cannot map retained rename source {} to Studio",
                            op.path.display()
                        ),
                    }));
                };
                retained_plugin_ops.push(plugin_op);
            }
            let Some(plugin_op) = fs_op_to_plugin_op(state.canonical_project.as_path(), &rename)
            else {
                state.conflict.park_fs_rename_conflict(
                    &conflict_path,
                    &from,
                    &to,
                    local_bytes,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({
                    "ok": false,
                    "error": format!(
                        "cannot map disk rename {} -> {} to Studio paths",
                        from.display(),
                        to.display()
                    ),
                }));
            };
            // Every retained source was read and translated before the first
            // Studio mutation. Rename first, then re-apply the destination
            // tree so Keep Disk means both name and source win.
            if let Err((error, delivered)) =
                deliver_prepared_rename(&state.events, &plugin_op, &retained_plugin_ops)
            {
                let compensated = delivered > 0
                    && compensate_studio_rename(
                        &state.events,
                        state.canonical_project.as_path(),
                        &plugin_op,
                        &from,
                        &to,
                        &conflict_path,
                        &studio_bytes,
                    );
                state.conflict.park_fs_rename_conflict(
                    &conflict_path,
                    &from,
                    &to,
                    local_bytes,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({
                    "ok": false,
                    "error": format!(
                        "{error} after {delivered} queued op(s); Studio rename compensation {}",
                        if compensated { "was queued" } else { "was unavailable" }
                    ),
                }));
            }
            if !wait_for_source_acks(state.conflict.as_ref(), &ops, Duration::from_secs(5)).await {
                let compensated = compensate_studio_rename(
                    &state.events,
                    state.canonical_project.as_path(),
                    &plugin_op,
                    &from,
                    &to,
                    &conflict_path,
                    &studio_bytes,
                );
                state.conflict.park_fs_rename_conflict(
                    &conflict_path,
                    &from,
                    &to,
                    local_bytes,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({
                    "ok": false,
                    "error": format!(
                        "Studio did not acknowledge retained disk source after rename; compensation {}",
                        if compensated { "was queued" } else { "was unavailable" }
                    ),
                }));
            }
            state.conflict.forget_path(&from);
            audit_conflict_resolution(
                "rename-studio",
                json!({
                    "from": from,
                    "to": to,
                    "isDirectory": is_dir,
                    "resolution": "keep-disk",
                }),
            );
            Json(json!({
                "ok": true,
                "action": "renamed-studio",
                "path": body.path,
            }))
        }
        Resolved::RestoreFsDelete {
            delete_root,
            conflict_path,
            studio_bytes,
            is_dir,
        } => {
            if let Err(error) = validate_fs_delete_restore(is_dir) {
                let _ = write_log_entry(Json(json!({
                    "source": "filesystem-sync-conflict",
                    "action": "restore-disk-delete",
                    "deleteRoot": &delete_root,
                    "path": &conflict_path,
                    "resolution": "keep-studio",
                    "outcome": "blocked-directory-restore",
                    "error": error,
                })));
                state.conflict.park_fs_delete_conflict(
                    &conflict_path,
                    &delete_root,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({
                    "ok": false,
                    "code": "DIRECTORY_DELETE_RESTORE_REQUIRES_STUDIO_PULL",
                    "error": error,
                    "conflictRemains": true,
                }));
            }
            mark_conflict_resolution_quiet(&state, &conflict_path);
            if let Err(error) =
                restore_fs_deleted_source(&conflict_path, &studio_bytes, &resolution_ctx)
            {
                state.conflict.park_fs_delete_conflict(
                    &conflict_path,
                    &delete_root,
                    studio_bytes,
                    is_dir,
                );
                return Json(json!({ "ok": false, "error": error }));
            }
            state.conflict.record_sync(
                &conflict_path,
                hash(&studio_bytes),
                fs_mtime(&conflict_path),
            );
            mark_conflict_resolution_quiet(&state, &conflict_path);
            audit_conflict_resolution(
                "restore-disk-delete",
                json!({
                    "deleteRoot": delete_root,
                    "path": conflict_path,
                    "resolution": "keep-studio",
                }),
            );
            Json(json!({
                "ok": true,
                "action": "restored-fs",
                "path": body.path,
            }))
        }
        Resolved::RestoreFsRename {
            from,
            to,
            conflict_path,
            studio_bytes,
            is_dir,
            local_bytes,
        } => {
            let repark = || {
                if from.exists() && !to.exists() {
                    state.conflict.park_studio_update(
                        &conflict_path,
                        local_bytes.clone(),
                        studio_bytes.clone(),
                        fs_mtime(&conflict_path),
                    );
                } else {
                    state.conflict.park_fs_rename_conflict(
                        &conflict_path,
                        &from,
                        &to,
                        local_bytes.clone(),
                        studio_bytes.clone(),
                        is_dir,
                    );
                }
            };
            mark_conflict_resolution_quiet(&state, &from);
            mark_conflict_resolution_quiet(&state, &to);
            if let Err(error) = restore_fs_rename_transactional(
                &from,
                &to,
                &conflict_path,
                &studio_bytes,
                &resolution_ctx,
            ) {
                repark();
                return Json(json!({ "ok": false, "error": error }));
            }
            state.conflict.record_sync(
                &conflict_path,
                hash(&studio_bytes),
                fs_mtime(&conflict_path),
            );
            mark_conflict_resolution_quiet(&state, &from);
            mark_conflict_resolution_quiet(&state, &to);
            mark_conflict_resolution_quiet(&state, &conflict_path);
            audit_conflict_resolution(
                "restore-disk-rename",
                json!({
                    "from": from,
                    "to": to,
                    "path": conflict_path,
                    "resolution": "keep-studio",
                }),
            );
            Json(json!({
                "ok": true,
                "action": "restored-fs-rename",
                "path": body.path,
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin op → filesystem
// ---------------------------------------------------------------------------

enum ApplyOutcome {
    Applied(usize),
    Skipped,
    Conflict(PathBuf),
}

struct ChildAssignment<'a> {
    node: &'a Value,
    fragment: String,
    fallback_by_name: bool,
    projection_class: &'a str,
    projection_has_children: bool,
    action: ChildAction,
}

struct AppliedChildren {
    applied: usize,
    wanted_fragments: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChildAction {
    Materialize,
    PruneCarrier,
    ReserveOnly,
}

fn op_kind(op: &Value) -> &str {
    op.get("op")
        .and_then(|v| v.as_str())
        .or_else(|| op.get("type").and_then(|v| v.as_str()))
        .unwrap_or("")
}

fn path_segments(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn exact_disk_path_from_op(
    root: &Path,
    op: &Value,
    field: &str,
) -> Result<Option<PathBuf>, String> {
    let Some(raw) = op.get(field) else {
        return Ok(None);
    };
    let array = raw
        .as_array()
        .ok_or_else(|| format!("{field} must be an array of filesystem fragments"))?;
    let mut fragments = Vec::with_capacity(array.len());
    for value in array {
        let fragment = value
            .as_str()
            .ok_or_else(|| format!("{field} entries must be strings"))?;
        if fragment.is_empty()
            || fragment == "."
            || fragment == ".."
            || fragment.contains(['/', '\\', '\0', ':'])
            || Path::new(fragment).is_absolute()
        {
            return Err(format!("unsafe {field} fragment: {fragment:?}"));
        }
        fragments.push(fragment.to_string());
    }
    let Some(service) = fragments.first() else {
        return Err(format!("{field} must include a synced service"));
    };
    if !snapshot::SYNCED_SERVICES.contains(&service.as_str()) {
        return Err(format!("{field} is outside a synced service: {service}"));
    }

    let path = fragments
        .iter()
        .fold(root.to_path_buf(), |path, fragment| path.join(fragment));
    match crate::fs_safety::validate_synced_path(root, &path, true) {
        Ok(_) => Ok(Some(path)),
        Err(error) => {
            // A canonical update path can alias the already-existing legacy
            // parent marker on a case-insensitive or Unicode-normalizing
            // filesystem. Preserve exact-path validation everywhere else,
            // and resolve this one ambiguity through the parent's unique,
            // class/name-checked script source.
            if field == "diskPath" && op_kind(op) == "update" {
                if let Some(existing) =
                    equivalent_init_update_target(root, &fragments).map_err(|reason| {
                        format!(
                            "unsafe {field} path {}: {error}; reconcile init marker: {reason}",
                            path.display()
                        )
                    })?
                {
                    return Ok(Some(existing));
                }
            }
            Err(format!("unsafe {field} path {}: {error}", path.display()))
        }
    }
}

fn equivalent_init_update_target(
    root: &Path,
    fragments: &[String],
) -> Result<Option<PathBuf>, String> {
    let Some(requested_fragment) = fragments.last() else {
        return Ok(None);
    };
    let requested = parse_init_file(requested_fragment)
        .map(|(class, name)| (class, Some(name)))
        .or_else(|| parse_plain_init_file(requested_fragment).map(|class| (class, None)));
    let Some((requested_class, requested_name)) = requested else {
        return Ok(None);
    };
    if fragments.len() < 2 {
        return Ok(None);
    }
    let parent = fragments[..fragments.len() - 1]
        .iter()
        .fold(root.to_path_buf(), |path, fragment| path.join(fragment));
    crate::fs_safety::validate_synced_path(root, &parent, false)
        .map_err(|error| format!("validate parent {}: {error}", parent.display()))?;
    let Some((actual_class, actual_name, actual_path)) = script_with_children_source(&parent)
        .map_err(|error| format!("inspect parent {}: {error}", parent.display()))?
    else {
        return Ok(None);
    };
    if actual_class != requested_class
        || requested_name
            .as_ref()
            .is_some_and(|name| !logical_names_equivalent(name, &actual_name))
    {
        return Ok(None);
    }
    Ok(Some(actual_path))
}

fn disk_fragment_matches_node(fragment: &str, node: &Value) -> bool {
    let Some(name) = node.get("name").and_then(Value::as_str) else {
        return false;
    };
    let Some(class) = node.get("class").and_then(Value::as_str) else {
        return false;
    };
    let is_dir = node
        .get("diskFragmentIsDir")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| node_projection_has_children(node) || class == "Folder");
    disk_fragment_matches_identity(fragment, name, class, is_dir)
}

fn disk_fragment_matches_identity(fragment: &str, name: &str, class: &str, is_dir: bool) -> bool {
    let (fragment_class, stem) = if is_dir {
        (None, fragment.to_string())
    } else {
        let Some((script_class, stem)) = classify_script_file(fragment) else {
            return false;
        };
        (Some(script_class.class_name()), stem)
    };
    if fragment_class.is_some_and(|fragment_class| fragment_class != class) {
        return false;
    }
    let encoded_name = parse_disambiguated(&stem)
        .map(|(base, _)| base)
        .unwrap_or(stem);
    crate::fs_map::decode_name(&encoded_name) == name
}

fn paths_refer_to_same_entry(left: &Path, right: &Path) -> bool {
    left == right || crate::fs_safety::same_physical_object_no_follow(left, right).unwrap_or(false)
}

fn validate_exact_set_target(target: &Path, node: &Value) -> Result<(), String> {
    let fragment = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("set: non-UTF-8 disk fragment {}", target.display()))?;
    let class = node
        .get("class")
        .and_then(Value::as_str)
        .ok_or("set: node missing class")?;
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .ok_or("set: node missing name")?;
    let expected_is_dir = class == "Folder" || node_projection_has_children(node);
    if node
        .get("diskFragmentIsDir")
        .and_then(Value::as_bool)
        .is_some_and(|declared| declared != expected_is_dir)
    {
        return Err(format!(
            "set: diskFragmentIsDir does not match the node representation for {fragment:?}"
        ));
    }
    if node
        .get("diskFragment")
        .and_then(Value::as_str)
        .is_some_and(|declared| declared != fragment)
        || !disk_fragment_matches_node(fragment, node)
    {
        return Err(format!(
            "set: disk fragment {fragment:?} does not match node identity"
        ));
    }
    if target.exists() {
        let existing = path_to_instance_meta(target)
            .map_err(|error| format!("set: inspect {}: {error}", target.display()))?
            .ok_or_else(|| {
                format!(
                    "set: existing target is not a synced instance: {}",
                    target.display()
                )
            })?;
        if existing.name != name || existing.class != class || existing.is_dir != expected_is_dir {
            return Err(format!(
                "set: existing target does not match {class} {name:?}: {}",
                target.display()
            ));
        }
    }
    Ok(())
}

fn apply_exact_set(
    target: PathBuf,
    transition_from: Option<PathBuf>,
    node: &Value,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    validate_exact_set_target(&target, node)?;
    let parent = target
        .parent()
        .ok_or_else(|| format!("set: no parent for {}", target.display()))?;
    let fragment = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("set: non-UTF-8 disk fragment {}", target.display()))?;

    let Some(source) = transition_from else {
        return apply_set_in_dir(parent, node, ctx, Some((fragment, false)));
    };
    if paths_refer_to_same_entry(&source, &target) {
        return apply_set_in_dir(parent, node, ctx, Some((fragment, false)));
    }

    let source_parent = source
        .parent()
        .ok_or_else(|| format!("set: no parent for {}", source.display()))?;
    if !paths_refer_to_same_entry(source_parent, parent) {
        return Err(format!(
            "set: representation transition must stay in one directory: {} -> {}",
            source.display(),
            target.display()
        ));
    }
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .ok_or("set: node missing name")?;
    let class = node
        .get("class")
        .and_then(Value::as_str)
        .ok_or("set: node missing class")?;
    let new_is_dir = class == "Folder" || node_projection_has_children(node);

    if !source.exists() {
        // A replay after a successful transition is idempotent: the old
        // representation is gone and the new one is updated in place.
        if target.exists() {
            if !existing_fragment_compatible(&target, class, new_is_dir) {
                return Err(format!(
                    "set: transition destination is not the requested instance: {}",
                    target.display()
                ));
            }
            return apply_set_in_dir(parent, node, ctx, Some((fragment, false)));
        }
        return Err(format!(
            "set: transition source does not exist: {}",
            source.display()
        ));
    }
    if target.exists() {
        return Err(format!(
            "set: transition destination already exists: {}",
            target.display()
        ));
    }

    let old = path_to_instance_meta(&source)
        .map_err(|error| format!("set: inspect {}: {error}", source.display()))?
        .ok_or_else(|| {
            format!(
                "set: transition source is not a synced instance: {}",
                source.display()
            )
        })?;
    let old_fragment = source
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("set: non-UTF-8 transition source {}", source.display()))?;
    if ScriptClass::from_class(class).is_none()
        || old.name != name
        || old.class != class
        || old.is_dir == new_is_dir
        || !disk_fragment_matches_identity(old_fragment, name, class, old.is_dir)
    {
        return Err(format!(
            "set: {} is not the opposite representation of {class} {name:?}",
            source.display()
        ));
    }

    // A representation change destroys the old physical tree. Require every
    // synced source in that tree to still match its baseline before creating
    // the replacement, so a local edit is never obscured by a parallel path.
    if !ctx.force_overwrite
        && !transition_tree_matches_baselines(&source, ctx.conflicts, ctx.project_root)?
    {
        if source.is_file() {
            let local = read_synced_file(ctx.project_root, &source)?;
            let studio = node
                .get("properties")
                .and_then(|properties| properties.get("Source"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            ctx.conflicts
                .park_studio_update(&source, local, studio, fs_mtime(&source));
        } else {
            ctx.conflicts.park_studio_delete(
                &source,
                format!("[directory retained on disk: {}]", source.display()).into_bytes(),
                fs_mtime(&source),
                true,
            );
        }
        return Ok(ApplyOutcome::Conflict(source));
    }

    let outcome = apply_set_in_dir(parent, node, ctx, Some((fragment, false)))?;
    let ApplyOutcome::Applied(applied) = outcome else {
        return Ok(outcome);
    };

    // Only remove the clean old representation after the new tree has been
    // materialized successfully. Forced startup reconciliation keeps its
    // normal recoverable backup behavior.
    remove_path_for_replace(&source, ctx)?;
    ctx.conflicts.forget_path(&source);
    ctx.mark_quiet(&source);
    ctx.mark_quiet(&target);
    Ok(ApplyOutcome::Applied(applied + 1))
}

fn apply_op(root: &Path, op: &Value, ctx: &PushCtx<'_>) -> Result<ApplyOutcome, String> {
    match op_kind(op) {
        "set" | "replace" => {
            let parent_segs = op.get("path").map(path_segments).unwrap_or_default();
            let node = op.get("node").ok_or("set: missing node")?;
            if let Some(target) = exact_disk_path_from_op(root, op, "diskPath")? {
                let transition_from = exact_disk_path_from_op(root, op, "fromDiskPath")?;
                apply_exact_set(target, transition_from, node, ctx)
            } else if op.get("fromDiskPath").is_some() {
                Err("set: fromDiskPath requires diskPath".into())
            } else {
                apply_set(root, &parent_segs, node, ctx)
            }
        }
        "delete" | "remove" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            if let Some(target) = exact_disk_path_from_op(root, op, "diskPath")? {
                apply_delete_target(target, ctx)
            } else {
                apply_delete(root, &segs, ctx)
            }
        }
        "update" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            let props = op.get("properties").cloned();
            let name = op.get("name").and_then(|v| v.as_str()).map(str::to_string);
            if let Some(target) = exact_disk_path_from_op(root, op, "diskPath")? {
                apply_update_target(target, props, ctx)
            } else {
                apply_update(root, &segs, props, name, ctx)
            }
        }
        "rename" => {
            let segs = op.get("path").map(path_segments).unwrap_or_default();
            let new_name = op
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("rename: missing name")?;
            let legacy_exact = exact_disk_path_from_op(root, op, "diskPath")?;
            let exact_from = exact_disk_path_from_op(root, op, "fromDiskPath")?;
            let exact_to = exact_disk_path_from_op(root, op, "toDiskPath")?;
            if let (Some(legacy), Some(from)) = (&legacy_exact, &exact_from) {
                if !paths_refer_to_same_entry(legacy, from) {
                    return Err(
                        "rename: diskPath and fromDiskPath identify different sources".into(),
                    );
                }
            }
            let exact_source = exact_from.or(legacy_exact);
            if exact_source.is_some() || exact_to.is_some() {
                let source = match exact_source {
                    Some(path) => path,
                    None => match resolve_segments_to_path(root, &segs)? {
                        Some(path) => path,
                        None => return Ok(ApplyOutcome::Applied(0)),
                    },
                };
                apply_rename_target(source, new_name, exact_to, ctx).map(ApplyOutcome::Applied)
            } else {
                apply_rename(root, &segs, new_name, ctx).map(ApplyOutcome::Applied)
            }
        }
        "move" => {
            let from_segs = op.get("from").map(path_segments).unwrap_or_default();
            let to_segs = op.get("to").map(path_segments).unwrap_or_default();
            let exact_from = exact_disk_path_from_op(root, op, "fromDiskPath")?;
            let exact_to = exact_disk_path_from_op(root, op, "toDiskPath")?;
            if exact_from.is_some() || exact_to.is_some() {
                let source = match exact_from {
                    Some(path) => path,
                    None => match resolve_segments_to_path(root, &from_segs)? {
                        Some(path) => path,
                        None => return Ok(ApplyOutcome::Applied(0)),
                    },
                };
                let new_name = to_segs.last().ok_or("move: empty 'to' path")?;
                apply_move_target(source, exact_to, new_name, ctx).map(ApplyOutcome::Applied)
            } else {
                apply_move(root, &from_segs, &to_segs, ctx).map(ApplyOutcome::Applied)
            }
        }
        "" => Err("op missing kind".to_string()),
        other => Err(format!("unknown op: {other}")),
    }
}

type StreamSourceProvider<'a> = dyn FnMut(&Value) -> Result<Option<Vec<u8>>, String> + 'a;

fn ensure_push_directory(ctx: &PushCtx<'_>, target: &Path) -> Result<PathBuf, String> {
    if ctx.private_stage {
        if !target.starts_with(ctx.project_root) {
            return Err(format!(
                "private stage directory escapes its root: {}",
                target.display()
            ));
        }
        std::fs::create_dir_all(target)
            .map_err(|error| format!("create private stage {}: {error}", target.display()))?;
        Ok(target.to_path_buf())
    } else {
        ensure_synced_directory_chain(ctx.project_root, target)
    }
}

fn apply_service_node(root: &Path, node: &Value, ctx: &PushCtx<'_>) -> Result<usize, String> {
    let mut source_provider = |node: &Value| {
        Ok(node
            .get("properties")
            .and_then(|properties| properties.get("Source"))
            .and_then(Value::as_str)
            .map(|source| source.as_bytes().to_vec()))
    };
    apply_service_node_with_sources(root, node, ctx, &mut source_provider)
}

fn apply_service_node_with_sources(
    root: &Path,
    node: &Value,
    ctx: &PushCtx<'_>,
    source_provider: &mut StreamSourceProvider<'_>,
) -> Result<usize, String> {
    let name = node
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("service: missing name")?;
    let svc_dir = root.join(encode_name(name));
    ensure_push_directory(ctx, &svc_dir)?;
    ctx.mark_quiet(&svc_dir);
    // Materialize children of the service node.
    let mut n = 0usize;
    let children = node
        .get("children")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let child_result =
        apply_children_in_dir_with_sources(&svc_dir, children, ctx, source_provider)?;
    n += child_result.applied;
    if ctx.strict && ctx.force_prune {
        n += prune_dir_to_fragments(&svc_dir, &child_result.wanted_fragments, false, ctx)?;
    }
    Ok(n)
}

fn apply_set(
    root: &Path,
    parent_segs: &[String],
    node: &Value,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    let parent_dir = resolve_segments_to_dir(root, parent_segs)?;
    apply_set_in_dir(&parent_dir, node, ctx, None)
}

fn apply_set_in_dir(
    parent_dir: &Path,
    node: &Value,
    ctx: &PushCtx<'_>,
    preferred_fragment: Option<(&str, bool)>,
) -> Result<ApplyOutcome, String> {
    let mut source_provider = |node: &Value| {
        Ok(node
            .get("properties")
            .and_then(|properties| properties.get("Source"))
            .and_then(Value::as_str)
            .map(|source| source.as_bytes().to_vec()))
    };
    apply_set_in_dir_with_sources(
        parent_dir,
        node,
        ctx,
        preferred_fragment,
        &mut source_provider,
    )
}

fn apply_set_in_dir_with_sources(
    parent_dir: &Path,
    node: &Value,
    ctx: &PushCtx<'_>,
    preferred_fragment: Option<(&str, bool)>,
    source_provider: &mut StreamSourceProvider<'_>,
) -> Result<ApplyOutcome, String> {
    if node_is_avoid_sync_boundary(node) || node_is_avoid_sync_carrier(node) {
        return Ok(ApplyOutcome::Skipped);
    }
    let name = node
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("set: node missing name")?;
    let class = node
        .get("class")
        .and_then(|v| v.as_str())
        .ok_or("set: node missing class")?;
    // Scope: daemon only materializes scripts + folders. Anything else is
    // Studio-authoritative and silently skipped (not errored).
    if !is_scoped_class(class) {
        return Ok(ApplyOutcome::Skipped);
    }
    let children = node
        .get("children")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let has_children = !children.is_empty();
    if class == "Folder" && !has_children {
        return Ok(ApplyOutcome::Skipped);
    }
    ensure_push_directory(ctx, parent_dir)?;

    // If a node with this name already exists on disk, reuse its path; otherwise
    // compute a fresh fragment.
    let mut existing = match preferred_fragment {
        Some((fragment, fallback_by_name)) => {
            if parent_dir.join(fragment).exists() {
                Some(fragment.to_string())
            } else if fallback_by_name {
                find_child_fragment_by_name(parent_dir, name).map_err(|e| e.to_string())?
            } else {
                None
            }
        }
        None => find_child_fragment_by_name(parent_dir, name).map_err(|e| e.to_string())?,
    };
    if let Some(fragment) = existing.as_deref() {
        let existing_path = parent_dir.join(fragment);
        if !existing_fragment_compatible(&existing_path, class, has_children) {
            if ctx.force_overwrite {
                remove_path_for_replace(&existing_path, ctx)?;
                existing = None;
            } else {
                return Ok(ApplyOutcome::Skipped);
            }
        }
    }
    let frag = match &existing {
        Some(f) => {
            let p = parent_dir.join(f);
            let is_dir = p.is_dir();
            crate::fs_map::PathFragment {
                fragment: f.clone(),
                is_dir,
            }
        }
        None => match preferred_fragment {
            Some((fragment, _)) => crate::fs_map::PathFragment {
                fragment: fragment.to_string(),
                is_dir: class == "Folder" || has_children,
            },
            None => {
                let taken = siblings_except(parent_dir, None)?;
                instance_to_path(
                    &InstanceDescriptor {
                        class,
                        name,
                        has_children,
                    },
                    &taken,
                )
            }
        },
    };

    let target = parent_dir.join(&frag.fragment);

    let sc = ScriptClass::from_class(class);
    let mut applied = 0usize;

    match (sc, has_children) {
        (Some(_), false) => {
            // Leaf script file. Normalize CRLF→LF so comparisons against FS
            // bytes and cached hashes line up regardless of checkout style.
            let raw_bytes = source_provider(node)?.unwrap_or_default();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            match apply_source_bytes(&target, &bytes, ctx)? {
                SourceWriteOutcome::Applied => applied += 1,
                SourceWriteOutcome::Skipped => {}
                SourceWriteOutcome::Conflict(path) => return Ok(ApplyOutcome::Conflict(path)),
            }
        }
        (Some(sc), true) => {
            // Script-with-children directory.
            ensure_push_directory(ctx, &target)?;
            ctx.mark_quiet(&target);
            let init_name = portable_init_file_name(name, sc);
            let preferred_init_path = target.join(&init_name);
            let init_path = if preferred_init_path.exists() {
                preferred_init_path
            } else {
                find_existing_init_source(&target, name, sc)?.unwrap_or(preferred_init_path)
            };
            let raw_bytes = source_provider(node)?.unwrap_or_default();
            let bytes = normalize_line_endings(&raw_bytes).into_owned();
            match apply_source_bytes(&init_path, &bytes, ctx)? {
                SourceWriteOutcome::Applied => applied += 1,
                SourceWriteOutcome::Skipped => {}
                SourceWriteOutcome::Conflict(path) => return Ok(ApplyOutcome::Conflict(path)),
            }
            let child_result =
                apply_children_in_dir_with_sources(&target, children, ctx, source_provider)?;
            applied += child_result.applied;
            if ctx.strict && ctx.force_prune {
                applied +=
                    prune_dir_to_fragments(&target, &child_result.wanted_fragments, true, ctx)?;
            }
        }
        (None, _) => {
            // Folder (the only surviving non-script whitelisted class).
            ensure_push_directory(ctx, &target)?;
            ctx.mark_quiet(&target);
            let child_result =
                apply_children_in_dir_with_sources(&target, children, ctx, source_provider)?;
            applied += child_result.applied;
            if ctx.strict && ctx.force_prune {
                applied +=
                    prune_dir_to_fragments(&target, &child_result.wanted_fragments, false, ctx)?;
            }
            applied += 1;
        }
    }
    Ok(ApplyOutcome::Applied(applied))
}

/// Apply a complete sibling batch after indexing the existing directory once.
///
/// Bootstrap snapshots commonly contain thousands of children under one
/// service. Looking up a legacy/case-disambiguated fragment separately for
/// every child turns that workload into O(children * directory entries).
/// Reusing this index keeps each directory level linear while preserving the
/// exact-fragment-first and best-compatible legacy fallback behavior.
fn apply_children_in_dir_with_sources(
    parent_dir: &Path,
    children: &[Value],
    ctx: &PushCtx<'_>,
    source_provider: &mut StreamSourceProvider<'_>,
) -> Result<AppliedChildren, String> {
    let existing_index = index_child_fragments(parent_dir)
        .map_err(|error| format!("scan {}: {error}", parent_dir.display()))?;
    let assignments = child_fragment_assignments(children);
    let mut applied = 0usize;
    let mut wanted_fragments = HashSet::new();
    let mut consumed_existing = HashSet::new();
    for child in assignments {
        let fragment = resolve_child_assignment_fragment(
            parent_dir,
            &child,
            &existing_index,
            &mut consumed_existing,
        )?;
        wanted_fragments.insert(fragment.to_ascii_lowercase());
        if child.action == ChildAction::ReserveOnly {
            continue;
        }
        if child.action == ChildAction::PruneCarrier {
            applied +=
                prune_existing_avoid_sync_carrier(parent_dir, child.node, ctx, &fragment, false)?;
            continue;
        }
        if let ApplyOutcome::Applied(count) = apply_set_in_dir_with_sources(
            parent_dir,
            child.node,
            ctx,
            Some((&fragment, false)),
            source_provider,
        )? {
            applied += count;
        }
    }
    Ok(AppliedChildren {
        applied,
        wanted_fragments,
    })
}

/// Strict Studio-wins must prune stale sync-owned entries around an ignored
/// branch without ever creating the Studio-only carrier on disk. Descend only
/// when the carrier already has a filesystem directory; an AvoidSync boundary
/// below it is retained wholesale, while unrelated siblings are removed.
fn prune_existing_avoid_sync_carrier(
    parent_dir: &Path,
    node: &Value,
    ctx: &PushCtx<'_>,
    preferred_fragment: &str,
    fallback_by_name: bool,
) -> Result<usize, String> {
    if !ctx.strict || !ctx.force_prune {
        return Ok(0);
    }
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .ok_or("carrier: node missing name")?;
    let target = if parent_dir.join(preferred_fragment).exists() {
        parent_dir.join(preferred_fragment)
    } else if fallback_by_name {
        let Some(existing) =
            find_child_fragment_by_name(parent_dir, name).map_err(|error| error.to_string())?
        else {
            return Ok(0);
        };
        parent_dir.join(existing)
    } else {
        return Ok(0);
    };

    if !target.is_dir() {
        if target.exists() && disk_path_is_sync_owned(&target) {
            remove_path_for_replace(&target, ctx)?;
            return Ok(1);
        }
        return Ok(0);
    }

    let children = node
        .get("children")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let existing_index = index_child_fragments(&target)
        .map_err(|error| format!("scan {}: {error}", target.display()))?;
    let mut applied = 0usize;
    let mut wanted_fragments = HashSet::new();
    let mut consumed_existing = HashSet::new();
    for child in child_fragment_assignments(children) {
        let fragment = resolve_child_assignment_fragment(
            &target,
            &child,
            &existing_index,
            &mut consumed_existing,
        )?;
        wanted_fragments.insert(fragment.to_ascii_lowercase());
        if child.action == ChildAction::PruneCarrier {
            applied +=
                prune_existing_avoid_sync_carrier(&target, child.node, ctx, &fragment, false)?;
        }
    }
    applied += prune_dir_to_fragments(&target, &wanted_fragments, false, ctx)?;
    Ok(applied)
}

/// Resolve one logical snapshot child to at most one existing filesystem
/// fragment. AvoidSync reservations are processed before materialized
/// siblings, so consuming candidates here prevents an ignored branch and a
/// same-name live sibling from ever claiming the same path. Exact canonical
/// fragments always win; the legacy-name fallback is intentionally limited to
/// an undisambiguated compatible fragment so a missing ignored branch cannot
/// steal an existing live `[N]` sibling on a later bootstrap.
fn resolve_child_assignment_fragment(
    parent_dir: &Path,
    assignment: &ChildAssignment<'_>,
    existing: &ExistingChildFragmentIndex,
    consumed_existing: &mut HashSet<String>,
) -> Result<String, String> {
    let name = assignment
        .node
        .get("name")
        .and_then(Value::as_str)
        .ok_or("set: node missing name")?;
    let canonical_key = assignment.fragment.to_ascii_lowercase();
    let canonical_path = parent_dir.join(&assignment.fragment);
    if canonical_path.exists() && !consumed_existing.contains(&canonical_key) {
        consumed_existing.insert(canonical_key);
        return Ok(assignment.fragment.clone());
    }

    let may_use_legacy_name =
        assignment.action != ChildAction::Materialize || assignment.fallback_by_name;
    if may_use_legacy_name {
        if let Some(candidates) = existing.all_by_name.get(name) {
            for candidate in candidates {
                let candidate_key = candidate.to_ascii_lowercase();
                if consumed_existing.contains(&candidate_key)
                    || fragment_disambiguation_ordinal(candidate) != 0
                {
                    continue;
                }
                let candidate_path = parent_dir.join(candidate);
                let compatible = match assignment.action {
                    ChildAction::Materialize | ChildAction::ReserveOnly => {
                        if is_scoped_class(assignment.projection_class) {
                            existing_fragment_compatible(
                                &candidate_path,
                                assignment.projection_class,
                                assignment.projection_has_children,
                            )
                        } else {
                            candidate_path.is_dir()
                        }
                    }
                    ChildAction::PruneCarrier => candidate_path.is_dir(),
                };
                if compatible {
                    consumed_existing.insert(candidate_key);
                    return Ok(candidate.clone());
                }
            }
        }
    }

    // Reserving a nonexistent canonical fragment still consumes its logical
    // slot for this batch. PathFragmentAllocator already guarantees distinct
    // canonical fragments, and this makes accidental duplicate assignments
    // fail closed instead of aliasing one target.
    if !consumed_existing.insert(canonical_key) {
        return Err(format!(
            "ambiguous snapshot children resolve to the same fragment {}",
            parent_dir.join(&assignment.fragment).display()
        ));
    }
    Ok(assignment.fragment.clone())
}

/// Find the source file for an existing script-with-children without assuming
/// it already uses the latest portable filename encoding. Older projects may
/// have a literal-Unicode `init (<Name>)` file; reuse it instead of creating a
/// second encoded init file beside it.
fn find_existing_init_source(
    dir: &Path,
    expected_name: &str,
    expected_class: ScriptClass,
) -> Result<Option<PathBuf>, String> {
    let index = crate::fs_safety::PortableDirectoryIndex::read(dir)
        .map_err(|error| format!("scan {}: {error}", dir.display()))?;
    let mut named_matches = Vec::new();
    let mut plain_match = None;
    for entry in index.entries() {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        if let Some((class, name)) = parse_init_file(&entry.fragment) {
            if class == expected_class && logical_names_equivalent(&name, expected_name) {
                named_matches.push(entry.path.clone());
            }
            continue;
        }
        if parse_plain_init_file(&entry.fragment) == Some(expected_class) {
            plain_match = Some(entry.path.clone());
        }
    }
    if named_matches.len() > 1 {
        return Err(format!(
            "multiple init sources in {} map to {}",
            dir.display(),
            expected_name
        ));
    }
    Ok(named_matches.pop().or(plain_match))
}

fn child_fragment_assignments(children: &[Value]) -> Vec<ChildAssignment<'_>> {
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for child in children {
        if !node_should_reserve_path(child) {
            continue;
        }
        if let Some(name) = child.get("name").and_then(|v| v.as_str()) {
            *name_counts.entry(name.to_string()).or_insert(0) += 1;
        }
    }

    let mut allocator = PathFragmentAllocator::new();
    let mut out = Vec::new();
    for index in diff::snapshot_sibling_order(children) {
        let child = &children[index];
        if !node_should_reserve_path(child) {
            continue;
        }
        let Some(name) = child.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(class) = child.get("class").and_then(|v| v.as_str()) else {
            continue;
        };
        let has_children = node_projection_has_children(child);
        let fragment = allocator.allocate(&InstanceDescriptor {
            class,
            name,
            has_children,
        });
        let action = if node_is_avoid_sync_boundary(child) {
            ChildAction::ReserveOnly
        } else if node_is_avoid_sync_carrier(child) {
            ChildAction::PruneCarrier
        } else {
            ChildAction::Materialize
        };
        out.push(ChildAssignment {
            node: child,
            fragment: fragment.fragment,
            fallback_by_name: name_counts.get(name).copied().unwrap_or(0) == 1,
            projection_class: class,
            projection_has_children: has_children,
            action,
        });
        // An AvoidSync marker deliberately omits its descendants, so the
        // daemon cannot know whether an ignored script currently projects as a
        // leaf file or a script-with-children directory. Reserve both portable
        // shapes; safety takes precedence over packing a same-name live sibling
        // into either bare fragment.
        if node_is_avoid_sync_boundary(child) && ScriptClass::from_class(class).is_some() {
            let alternate = allocator.allocate(&InstanceDescriptor {
                class,
                name,
                has_children: !has_children,
            });
            out.push(ChildAssignment {
                node: child,
                fragment: alternate.fragment,
                fallback_by_name: name_counts.get(name).copied().unwrap_or(0) == 1,
                projection_class: class,
                projection_has_children: !has_children,
                action: ChildAction::ReserveOnly,
            });
        }
    }
    out
}

fn node_should_reserve_path(node: &Value) -> bool {
    node_should_materialize(node)
        || node_is_avoid_sync_boundary(node)
        || node_is_avoid_sync_carrier(node)
}

fn node_projection_has_children(node: &Value) -> bool {
    node.get("children")
        .and_then(Value::as_array)
        .is_some_and(|children| !children.is_empty())
        || node.get("hasChildren").and_then(Value::as_bool) == Some(true)
}

fn node_should_materialize(node: &Value) -> bool {
    if node_is_avoid_sync_boundary(node) || node_is_avoid_sync_carrier(node) {
        return false;
    }

    let class = node.get("class").and_then(|v| v.as_str()).unwrap_or("");
    if !is_scoped_class(class) {
        return false;
    }

    let has_children = node
        .get("children")
        .and_then(|v| v.as_array())
        .map(|children| !children.is_empty())
        .unwrap_or(false);
    class != "Folder" || has_children
}

fn node_is_avoid_sync_boundary(node: &Value) -> bool {
    node.get("avoidSync").and_then(Value::as_bool) == Some(true)
}

fn node_is_avoid_sync_carrier(node: &Value) -> bool {
    node.get("avoidSyncCarrier").and_then(Value::as_bool) == Some(true)
}

fn existing_fragment_compatible(path: &Path, class: &str, has_children: bool) -> bool {
    let Ok(Some(inst)) = path_to_instance_meta(path) else {
        return false;
    };
    if class == "Folder" {
        return inst.class == "Folder" && !inst.is_script_with_children;
    }
    if ScriptClass::from_class(class).is_some() {
        if has_children {
            return inst.is_dir && inst.is_script_with_children && inst.class == class;
        }
        return !inst.is_dir && inst.class == class;
    }
    false
}

fn prune_dir_to_fragments(
    dir: &Path,
    wanted_fragments: &HashSet<String>,
    keep_init_files: bool,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    let validated = crate::fs_safety::validate_synced_path(ctx.project_root, dir, true)
        .map_err(|error| format!("validate prune directory {}: {error}", dir.display()))?;
    let Some(metadata) = crate::fs_safety::metadata_no_follow(&validated)
        .map_err(|error| format!("inspect prune directory {}: {error}", dir.display()))?
    else {
        return Ok(0);
    };
    if !metadata.is_dir() {
        return Err(format!(
            "prune target is not a directory: {}",
            dir.display()
        ));
    }
    let mut removed = 0usize;
    let index = crate::fs_safety::PortableDirectoryIndex::read(&validated)
        .map_err(|error| format!("scan prune directory {}: {error}", dir.display()))?;
    let parent_source = index.unique_init_source().map(|entry| entry.path.as_path());
    for entry in index.entries() {
        let path = entry.path.clone();
        let file_name = entry.fragment.as_str();
        if file_name == META_FILE || file_name == ".DS_Store" {
            continue;
        }
        if wanted_fragments.contains(&file_name.to_ascii_lowercase()) {
            continue;
        }
        if parent_source == Some(path.as_path()) {
            if keep_init_files {
                continue;
            }
            remove_path_for_replace(&path, ctx)?;
            removed += 1;
            continue;
        }
        if !disk_path_is_sync_owned(&path) {
            continue;
        }
        remove_path_for_replace(&path, ctx)?;
        removed += 1;
    }
    Ok(removed)
}

fn disk_path_is_sync_owned(path: &Path) -> bool {
    let Ok(Some(inst)) = path_to_instance_meta(path) else {
        return false;
    };
    if inst.script_class.is_some() {
        return true;
    }
    if inst.class == "Folder" && inst.is_dir {
        return folder_contains_sync_owned_path(path);
    }
    false
}

fn folder_contains_sync_owned_path(dir: &Path) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = stack.pop() {
        let Ok(index) = crate::fs_safety::PortableDirectoryIndex::read(&directory) else {
            return false;
        };
        for entry in index.entries() {
            visited = visited.saturating_add(1);
            if visited > crate::fs_safety::MAX_SERVICE_TREE_NODES {
                return false;
            }
            if entry.fragment == META_FILE || entry.fragment == ".DS_Store" {
                continue;
            }
            if is_init_file(&entry.fragment) {
                return true;
            }
            if entry.kind == crate::fs_safety::SafeEntryKind::File {
                if classify_script_file(&entry.fragment).is_some() {
                    return true;
                }
            } else {
                stack.push(entry.path.clone());
            }
        }
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SafeSubtreeEntry {
    path: PathBuf,
    relative: PathBuf,
    kind: crate::fs_safety::SafeEntryKind,
    file_generation: Option<crate::fs_safety::FileGeneration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SafeSubtreeFence {
    root: PathBuf,
    entries: Vec<SafeSubtreeEntry>,
}

fn relocated_subtree_matches(left: &SafeSubtreeFence, right: &SafeSubtreeFence) -> bool {
    left.entries.len() == right.entries.len()
        && left
            .entries
            .iter()
            .zip(&right.entries)
            .all(|(left, right)| {
                left.relative == right.relative
                    && left.kind == right.kind
                    && left.file_generation == right.file_generation
            })
}

#[derive(Debug)]
struct BackupReceipt {
    #[cfg_attr(not(test), allow(dead_code))]
    destination: PathBuf,
    source_fence: SafeSubtreeFence,
}

fn capture_synced_subtree(
    project_root: &Path,
    path: &Path,
) -> Result<Option<SafeSubtreeFence>, String> {
    let lexical_root = path.to_path_buf();
    let validated = crate::fs_safety::validate_synced_path(project_root, path, true)
        .map_err(|error| format!("validate synced subtree {}: {error}", path.display()))?;
    let parent_guard = crate::fs_safety::guard_synced_parent_chain(project_root, &validated, true)
        .map_err(|error| format!("guard synced subtree {}: {error}", path.display()))?;
    parent_guard
        .verify()
        .map_err(|error| format!("verify synced subtree parent {}: {error}", path.display()))?;
    let Some(root_metadata) = crate::fs_safety::metadata_no_follow(&validated)
        .map_err(|error| format!("inspect synced subtree {}: {error}", path.display()))?
    else {
        return Ok(None);
    };
    let root_kind = if root_metadata.is_dir() {
        crate::fs_safety::SafeEntryKind::Directory
    } else if root_metadata.is_file() {
        crate::fs_safety::SafeEntryKind::File
    } else {
        return Err(format!(
            "unsupported object in synced subtree: {}",
            validated.display()
        ));
    };
    let root_generation = if root_kind == crate::fs_safety::SafeEntryKind::File {
        Some(crate::fs_safety::file_generation_no_follow(&validated)?)
    } else {
        None
    };
    let mut entries = vec![SafeSubtreeEntry {
        path: lexical_root.clone(),
        relative: PathBuf::new(),
        kind: root_kind,
        file_generation: root_generation,
    }];
    let mut stack = Vec::new();
    if root_kind == crate::fs_safety::SafeEntryKind::Directory {
        stack.push((validated.clone(), PathBuf::new(), 0usize));
    }
    while let Some((directory, relative, depth)) = stack.pop() {
        if depth > crate::fs_safety::MAX_SERVICE_TREE_DEPTH {
            return Err(format!(
                "synced subtree exceeds depth {} at {}",
                crate::fs_safety::MAX_SERVICE_TREE_DEPTH,
                directory.display()
            ));
        }
        let index = crate::fs_safety::PortableDirectoryIndex::read(&directory)
            .map_err(|error| format!("scan synced subtree {}: {error}", directory.display()))?;
        for entry in index.entries() {
            if entries.len() >= crate::fs_safety::MAX_SERVICE_TREE_NODES {
                return Err(format!(
                    "synced subtree exceeds node limit {}",
                    crate::fs_safety::MAX_SERVICE_TREE_NODES
                ));
            }
            let entry_relative = relative.join(&entry.fragment);
            let file_generation = if entry.kind == crate::fs_safety::SafeEntryKind::File {
                Some(crate::fs_safety::file_generation_no_follow(&entry.path)?)
            } else {
                None
            };
            entries.push(SafeSubtreeEntry {
                path: lexical_root.join(&entry_relative),
                relative: entry_relative.clone(),
                kind: entry.kind,
                file_generation,
            });
        }
        for entry in index.entries().iter().rev() {
            if entry.kind == crate::fs_safety::SafeEntryKind::Directory {
                stack.push((
                    entry.path.clone(),
                    relative.join(&entry.fragment),
                    depth + 1,
                ));
            }
        }
    }
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    parent_guard
        .verify()
        .map_err(|error| format!("synced subtree parent changed {}: {error}", path.display()))?;
    Ok(Some(SafeSubtreeFence {
        root: lexical_root,
        entries,
    }))
}

fn ensure_descendant_directory_chain(base: &Path, target: &Path) -> Result<PathBuf, String> {
    let canonical_base = crate::fs_safety::stable_canonical_directory(base)
        .map_err(|error| format!("validate directory base {}: {error}", base.display()))?;
    let relative = target
        .strip_prefix(base)
        .or_else(|_| target.strip_prefix(&canonical_base))
        .map_err(|error| {
            format!(
                "directory target {} is outside {}: {error}",
                target.display(),
                canonical_base.display()
            )
        })?;
    let mut current = canonical_base.clone();
    for component in relative.components() {
        let std::path::Component::Normal(fragment) = component else {
            return Err(format!(
                "unsafe directory component in {}",
                target.display()
            ));
        };
        let next = current.join(fragment);
        let guard = crate::fs_safety::guard_descendant_parent_chain(&canonical_base, &next, true)
            .map_err(|error| format!("guard directory {}: {error}", next.display()))?;
        guard
            .verify()
            .map_err(|error| format!("verify directory parent {}: {error}", next.display()))?;
        match crate::fs_safety::metadata_no_follow(&next)
            .map_err(|error| format!("inspect directory {}: {error}", next.display()))?
        {
            Some(metadata) if metadata.is_dir() => {}
            Some(_) => {
                return Err(format!(
                    "directory chain contains a non-directory: {}",
                    next.display()
                ));
            }
            None => {
                std::fs::create_dir(&next)
                    .map_err(|error| format!("create directory {}: {error}", next.display()))?;
            }
        }
        guard
            .verify()
            .map_err(|error| format!("directory parent changed {}: {error}", next.display()))?;
        let metadata = crate::fs_safety::require_metadata_no_follow(&next)
            .map_err(|error| format!("verify created directory {}: {error}", next.display()))?;
        if !metadata.is_dir() {
            return Err(format!(
                "created directory changed into another object: {}",
                next.display()
            ));
        }
        current = next;
    }
    Ok(current)
}

fn ensure_synced_directory_chain(project_root: &Path, target: &Path) -> Result<PathBuf, String> {
    let validated = crate::fs_safety::validate_synced_path(project_root, target, true)
        .map_err(|error| format!("validate synced directory {}: {error}", target.display()))?;
    ensure_descendant_directory_chain(project_root, &validated)
}

fn copy_backup_path(
    project_root: &Path,
    source_fence: &SafeSubtreeFence,
    transaction: &Path,
    destination: &Path,
) -> Result<(), String> {
    use std::io::{Read as _, Write as _};

    let mut ordered = source_fence.entries.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.relative
            .components()
            .count()
            .cmp(&right.relative.components().count())
            .then_with(|| left.relative.cmp(&right.relative))
    });
    let mut buffer = [0u8; 64 * 1024];
    for entry in ordered {
        let target = if entry.relative.as_os_str().is_empty() {
            destination.to_path_buf()
        } else {
            destination.join(&entry.relative)
        };
        match entry.kind {
            crate::fs_safety::SafeEntryKind::Directory => {
                let parent = target.parent().ok_or_else(|| {
                    format!("backup directory has no parent: {}", target.display())
                })?;
                ensure_descendant_directory_chain(transaction, parent)?;
                let guard =
                    crate::fs_safety::guard_descendant_parent_chain(transaction, &target, true)
                        .map_err(|error| {
                            format!("guard backup directory {}: {error}", target.display())
                        })?;
                guard.verify().map_err(|error| {
                    format!(
                        "verify backup directory parent {}: {error}",
                        target.display()
                    )
                })?;
                std::fs::create_dir(&target).map_err(|error| {
                    format!("create backup directory {}: {error}", target.display())
                })?;
                guard.verify().map_err(|error| {
                    format!(
                        "backup directory parent changed {}: {error}",
                        target.display()
                    )
                })?;
            }
            crate::fs_safety::SafeEntryKind::File => {
                let expected = entry.file_generation.as_ref().ok_or_else(|| {
                    format!(
                        "backup file is missing a generation: {}",
                        entry.path.display()
                    )
                })?;
                let source_guard =
                    crate::fs_safety::guard_synced_parent_chain(project_root, &entry.path, false)
                        .map_err(|error| {
                        format!("guard backup source {}: {error}", entry.path.display())
                    })?;
                source_guard.verify().map_err(|error| {
                    format!(
                        "verify backup source parent {}: {error}",
                        entry.path.display()
                    )
                })?;
                if crate::fs_safety::file_generation_no_follow(&entry.path)? != *expected {
                    return Err(format!(
                        "backup source changed before copy: {}",
                        entry.path.display()
                    ));
                }
                let parent = target
                    .parent()
                    .ok_or_else(|| format!("backup file has no parent: {}", target.display()))?;
                ensure_descendant_directory_chain(transaction, parent)?;
                let target_guard =
                    crate::fs_safety::guard_descendant_parent_chain(transaction, &target, true)
                        .map_err(|error| {
                            format!("guard backup target {}: {error}", target.display())
                        })?;
                target_guard.verify().map_err(|error| {
                    format!("verify backup target parent {}: {error}", target.display())
                })?;
                let mut source = crate::fs_safety::open_regular_file_no_follow(&entry.path)
                    .map_err(|error| {
                        format!("open backup source {}: {error}", entry.path.display())
                    })?;
                let mut target_file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map_err(|error| format!("create backup file {}: {error}", target.display()))?;
                loop {
                    let count = source.read(&mut buffer).map_err(|error| {
                        format!("read backup source {}: {error}", entry.path.display())
                    })?;
                    if count == 0 {
                        break;
                    }
                    target_file.write_all(&buffer[..count]).map_err(|error| {
                        format!("write backup file {}: {error}", target.display())
                    })?;
                }
                // The transaction directory is flushed once after the batch.
                drop(target_file);
                if crate::fs_safety::file_generation_no_follow(&entry.path)? != *expected {
                    return Err(format!(
                        "backup source changed during copy: {}",
                        entry.path.display()
                    ));
                }
                source_guard.verify().map_err(|error| {
                    format!(
                        "backup source parent changed {}: {error}",
                        entry.path.display()
                    )
                })?;
                target_guard.verify().map_err(|error| {
                    format!("backup target parent changed {}: {error}", target.display())
                })?;
            }
        }
    }
    Ok(())
}

fn backup_forced_removal(path: &Path, project_root: &Path) -> Result<BackupReceipt, String> {
    let source_fence = capture_synced_subtree(project_root, path)?
        .ok_or_else(|| format!("backup source disappeared: {}", path.display()))?;
    let canonical_project_root = crate::fs_safety::stable_canonical_directory(project_root)
        .map_err(|error| format!("validate backup project root: {error}"))?;
    let relative = source_fence
        .root
        .strip_prefix(project_root)
        .or_else(|_| source_fence.root.strip_prefix(&canonical_project_root))
        .map_err(|error| format!("backup path {}: {error}", path.display()))?;
    let service = relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .ok_or_else(|| format!("backup path has no synced service: {}", path.display()))?;
    if !snapshot::SYNCED_SERVICES.contains(&service) {
        return Err(format!(
            "refusing destructive write outside a synced service: {}",
            path.display()
        ));
    }

    let backup_root =
        ensure_descendant_directory_chain(project_root, &project_root.join(".rosync-backups"))?;
    static BACKUP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = BACKUP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let transaction = backup_root.join(format!("{stamp}-{sequence}"));
    let transaction_guard =
        crate::fs_safety::guard_descendant_parent_chain(project_root, &transaction, true).map_err(
            |error| {
                format!(
                    "guard backup transaction {}: {error}",
                    transaction.display()
                )
            },
        )?;
    transaction_guard.verify().map_err(|error| {
        format!(
            "verify backup transaction parent {}: {error}",
            transaction.display()
        )
    })?;
    std::fs::create_dir(&transaction).map_err(|error| {
        format!(
            "create backup transaction {}: {error}",
            transaction.display()
        )
    })?;
    transaction_guard.verify().map_err(|error| {
        format!(
            "backup transaction parent changed {}: {error}",
            transaction.display()
        )
    })?;

    let destination = transaction.join(relative);
    copy_backup_path(project_root, &source_fence, &transaction, &destination)?;
    sync_directory_best_effort(&transaction);
    let current = capture_synced_subtree(project_root, path)?
        .ok_or_else(|| format!("backup source disappeared during copy: {}", path.display()))?;
    if current != source_fence {
        return Err(format!(
            "backup source changed while it was copied: {}",
            path.display()
        ));
    }
    Ok(BackupReceipt {
        destination,
        source_fence,
    })
}

fn remove_synced_subtree(
    path: &Path,
    ctx: &PushCtx<'_>,
    expected: Option<&SafeSubtreeFence>,
) -> Result<bool, String> {
    let Some(current) = capture_synced_subtree(ctx.project_root, path)? else {
        return Ok(false);
    };
    if expected.is_some_and(|expected| expected != &current) {
        return Err(format!(
            "refusing to remove subtree that changed after backup: {}",
            path.display()
        ));
    }
    let mut entries = current.entries.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .relative
            .components()
            .count()
            .cmp(&left.relative.components().count())
            .then_with(|| right.relative.cmp(&left.relative))
            .then_with(|| {
                (right.kind == crate::fs_safety::SafeEntryKind::File)
                    .cmp(&(left.kind == crate::fs_safety::SafeEntryKind::File))
            })
    });
    for entry in entries {
        let guard =
            crate::fs_safety::guard_synced_parent_chain(ctx.project_root, &entry.path, false)
                .map_err(|error| format!("guard removal {}: {error}", entry.path.display()))?;
        guard
            .verify()
            .map_err(|error| format!("verify removal parent {}: {error}", entry.path.display()))?;
        ctx.mark_quiet(&entry.path);
        match entry.kind {
            crate::fs_safety::SafeEntryKind::File => {
                let expected_generation = entry.file_generation.as_ref().ok_or_else(|| {
                    format!(
                        "removal file is missing a generation: {}",
                        entry.path.display()
                    )
                })?;
                if crate::fs_safety::file_generation_no_follow(&entry.path)? != *expected_generation
                {
                    return Err(format!(
                        "refusing to remove file changed after validation: {}",
                        entry.path.display()
                    ));
                }
                std::fs::remove_file(&entry.path)
                    .map_err(|error| format!("remove file {}: {error}", entry.path.display()))?;
            }
            crate::fs_safety::SafeEntryKind::Directory => {
                let index = crate::fs_safety::PortableDirectoryIndex::read(&entry.path).map_err(
                    |error| format!("verify empty directory {}: {error}", entry.path.display()),
                )?;
                if !index.entries().is_empty() {
                    return Err(format!(
                        "refusing to remove directory that gained entries: {}",
                        entry.path.display()
                    ));
                }
                std::fs::remove_dir(&entry.path).map_err(|error| {
                    format!("remove directory {}: {error}", entry.path.display())
                })?;
            }
        }
        guard.verify().map_err(|error| {
            format!(
                "removal parent changed while deleting {}: {error}",
                entry.path.display()
            )
        })?;
    }
    ctx.mark_quiet(path);
    prune_emptied_ancestors(path, ctx);
    Ok(true)
}

/// Remove directories that a removal or move just emptied.
///
/// The scanner deliberately ignores empty plain directories, so a directory
/// left behind after its last syncable descendant is gone becomes invisible to
/// the projection while still dictating its parent's physical shape on disk. A
/// script parent then reads back as directory-form with zero projected
/// children — a state the comparison protocol rejects, and one no later
/// Studio-side delete can clear, because an empty directory is not sync-owned.
/// Pruning here keeps the write side from manufacturing trees its own reader
/// refuses.
///
/// Conservative by construction: stops at the first ancestor that still holds
/// any entry (including files this sync does not own), never touches a service
/// root or the project root, and treats every filesystem error as "stop".
fn prune_emptied_ancestors(start: &Path, ctx: &PushCtx<'_>) {
    let Some(parent) = start.parent() else {
        return;
    };
    let mut current = parent.to_path_buf();
    loop {
        let Ok(relative) = current.strip_prefix(ctx.project_root) else {
            return;
        };
        // Depth 1 is a service root; it must survive an empty projection.
        if relative.components().count() < 2 {
            return;
        }
        match std::fs::read_dir(&current) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return;
                }
            }
            Err(_) => return,
        }
        ctx.mark_quiet(&current);
        if std::fs::remove_dir(&current).is_err() {
            return;
        }
        let Some(next) = current.parent() else {
            return;
        };
        current = next.to_path_buf();
    }
}

fn remove_path_for_replace(path: &Path, ctx: &PushCtx<'_>) -> Result<(), String> {
    let backup = if ctx.backup_forced_removals
        && (ctx.force_overwrite || (ctx.strict && ctx.force_prune))
        && crate::fs_safety::metadata_no_follow(path)
            .map_err(|error| format!("inspect replacement target {}: {error}", path.display()))?
            .is_some()
    {
        Some(backup_forced_removal(path, ctx.project_root)?)
    } else {
        None
    };
    let expected = backup.as_ref().map(|backup| &backup.source_fence);
    remove_synced_subtree(path, ctx, expected)?;
    Ok(())
}

enum SourceWriteOutcome {
    Applied,
    Skipped,
    Conflict(PathBuf),
}

fn read_synced_file(project_root: &Path, path: &Path) -> Result<Vec<u8>, String> {
    let mut validation = crate::fs_safety::SyncedPathValidationCache::new(project_root)
        .map_err(|error| format!("validate source {}: {error}", path.display()))?;
    read_synced_file_cached(path, &mut validation)
}

fn read_synced_file_cached(
    path: &Path,
    validation: &mut crate::fs_safety::SyncedPathValidationCache,
) -> Result<Vec<u8>, String> {
    let validated = validation
        .validate(path, false)
        .map_err(|error| format!("validate source {}: {error}", path.display()))?;
    let guard = crate::fs_safety::guard_synced_parent_chain_cached(validation, &validated, false)
        .map_err(|error| format!("guard source {}: {error}", path.display()))?;
    guard
        .verify()
        .map_err(|error| format!("verify source parent {}: {error}", path.display()))?;
    let before = crate::fs_safety::file_generation_no_follow(&validated)?;
    let bytes = crate::fs_safety::read_file_no_follow(&validated)
        .map_err(|error| format!("read source {}: {error}", path.display()))?;
    let after = crate::fs_safety::file_generation_no_follow(&validated)?;
    if before != after {
        return Err(format!(
            "source changed while it was read: {}",
            path.display()
        ));
    }
    guard
        .verify()
        .map_err(|error| format!("source parent changed {}: {error}", path.display()))?;
    Ok(bytes)
}

fn write_synced_file_atomic(target: &Path, bytes: &[u8], ctx: &PushCtx<'_>) -> Result<(), String> {
    write_synced_file_atomic_with(target, bytes, ctx, || {})
}

fn write_synced_file_atomic_with<F>(
    target: &Path,
    bytes: &[u8],
    ctx: &PushCtx<'_>,
    before_commit: F,
) -> Result<(), String>
where
    F: FnOnce(),
{
    use std::io::Write as _;

    if ctx.private_stage {
        if !target.starts_with(ctx.project_root) {
            return Err(format!(
                "private stage write escapes its root: {}",
                target.display()
            ));
        }
        let parent = target
            .parent()
            .ok_or_else(|| format!("write target has no parent: {}", target.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create private stage {}: {error}", parent.display()))?;
        before_commit();
        std::fs::write(target, bytes)
            .map_err(|error| format!("write private stage source {}: {error}", target.display()))?;
        return Ok(());
    }

    let validated = crate::fs_safety::validate_synced_path(ctx.project_root, target, true)
        .map_err(|error| format!("validate write target {}: {error}", target.display()))?;
    let parent = validated
        .parent()
        .ok_or_else(|| format!("write target has no parent: {}", validated.display()))?;
    let parent_metadata = crate::fs_safety::require_metadata_no_follow(parent)
        .map_err(|error| format!("inspect write parent {}: {error}", parent.display()))?;
    if !parent_metadata.is_dir() {
        return Err(format!(
            "write parent is not a directory: {}",
            parent.display()
        ));
    }
    let target_permissions = match crate::fs_safety::metadata_no_follow(&validated)
        .map_err(|error| format!("inspect write target {}: {error}", validated.display()))?
    {
        Some(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Some(_) => {
            return Err(format!(
                "write target is not a regular file: {}",
                validated.display()
            ));
        }
        None => None,
    };
    let guard = crate::fs_safety::guard_synced_parent_chain(ctx.project_root, &validated, true)
        .map_err(|error| format!("guard write target {}: {error}", validated.display()))?;
    guard
        .verify()
        .map_err(|error| format!("verify write parent {}: {error}", parent.display()))?;

    static WRITE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut temporary = None;
    for _ in 0..64 {
        let sequence = WRITE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".rosync-write-{}-{sequence}.tmp",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Some(permissions) = target_permissions.clone() {
                    file.set_permissions(permissions).map_err(|error| {
                        format!(
                            "set staged write permissions {}: {error}",
                            candidate.display()
                        )
                    })?;
                }
                file.write_all(bytes).map_err(|error| {
                    format!("write staged source {}: {error}", candidate.display())
                })?;
                drop(file);
                temporary = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create staged source in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    let temporary = temporary
        .ok_or_else(|| format!("could not allocate staged source in {}", parent.display()))?;

    before_commit();
    if let Err(error) = guard.verify() {
        // Only clean up through the still-proven parent. If its identity
        // changed, leaving the inaccessible temp behind is safer than
        // deleting an attacker-controlled same-named external file.
        return Err(format!(
            "write parent changed before commit {}: {error}",
            parent.display()
        ));
    }
    ctx.mark_quiet(&validated);
    invalidate_cached_content_hash(&validated);
    if let Err(error) = crate::lifecycle::replace_file_atomic(&temporary, &validated) {
        if guard.verify().is_ok() {
            let _ = std::fs::remove_file(&temporary);
        }
        return Err(format!(
            "commit staged source {}: {error}",
            validated.display()
        ));
    }
    guard.verify().map_err(|error| {
        format!(
            "write parent changed during commit {}: {error}",
            parent.display()
        )
    })?;
    let metadata = crate::fs_safety::require_metadata_no_follow(&validated)
        .map_err(|error| format!("verify written source {}: {error}", validated.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "written source is not a regular file: {}",
            validated.display()
        ));
    }
    ctx.note_dirty_parent(parent);
    ctx.mark_quiet(&validated);
    Ok(())
}

fn apply_source_bytes(
    target: &Path,
    bytes: &[u8],
    ctx: &PushCtx<'_>,
) -> Result<SourceWriteOutcome, String> {
    // A private bootstrap stage has no competing local writer and is not
    // watched. Recording a baseline here would validate/canonicalize the full
    // ancestor chain once per script; live baselines are prepared in one pass
    // immediately before the service directory is installed.
    if ctx.private_stage {
        write_synced_file_atomic(target, bytes, ctx)?;
        return Ok(SourceWriteOutcome::Applied);
    }
    let conflicts = ctx.conflicts;
    if ctx.force_overwrite {
        write_synced_file_atomic(target, bytes, ctx)?;
        conflicts.record_sync(target, hash(bytes), fs_mtime(target));
        return Ok(SourceWriteOutcome::Applied);
    }

    let current = match crate::fs_safety::metadata_no_follow(target)
        .map_err(|error| format!("inspect source {}: {error}", target.display()))?
    {
        Some(metadata) if metadata.is_file() => Some((
            read_synced_file(ctx.project_root, target)?,
            fs_mtime(target),
        )),
        Some(_) => {
            return Err(format!("source target is not a file: {}", target.display()));
        }
        None => None,
    };
    let normalized_current: Option<Vec<u8>> = current
        .as_ref()
        .map(|(b, _)| normalize_line_endings(b).into_owned());
    let current_ref = current
        .as_ref()
        .zip(normalized_current.as_ref())
        .map(|((_, m), nb)| (nb.as_slice(), *m));
    match conflicts.on_studio_push(target, bytes, current_ref) {
        StudioDecision::Apply => {
            write_synced_file_atomic(target, bytes, ctx)?;
            conflicts.record_sync(target, hash(bytes), fs_mtime(target));
            Ok(SourceWriteOutcome::Applied)
        }
        StudioDecision::NoChange => Ok(SourceWriteOutcome::Skipped),
        StudioDecision::Conflict => Ok(SourceWriteOutcome::Conflict(target.to_path_buf())),
    }
}

fn apply_delete(root: &Path, segs: &[String], ctx: &PushCtx<'_>) -> Result<ApplyOutcome, String> {
    if segs.is_empty() {
        return Err("delete: empty path".into());
    }
    let target = match resolve_segments_to_path(root, segs)? {
        Some(p) => p,
        None => return Ok(ApplyOutcome::Skipped),
    };
    apply_delete_target(target, ctx)
}

fn apply_delete_target(target: PathBuf, ctx: &PushCtx<'_>) -> Result<ApplyOutcome, String> {
    let Some(metadata) = crate::fs_safety::metadata_no_follow(&target)
        .map_err(|error| format!("inspect delete target {}: {error}", target.display()))?
    else {
        return Ok(ApplyOutcome::Skipped);
    };
    if metadata.is_dir() && !disk_path_is_sync_owned(&target) {
        return Ok(ApplyOutcome::Skipped);
    }
    if !ctx.force_overwrite
        && !path_tree_matches_baselines(&target, ctx.conflicts, ctx.project_root)?
    {
        let is_dir = metadata.is_dir();
        let local = if is_dir {
            format!("[directory retained on disk: {}]", target.display()).into_bytes()
        } else {
            read_synced_file(ctx.project_root, &target)?
        };
        ctx.conflicts
            .park_studio_delete(&target, local, fs_mtime(&target), is_dir);
        return Ok(ApplyOutcome::Conflict(target));
    }
    remove_synced_subtree(&target, ctx, None)?;
    ctx.conflicts.forget_path(&target);
    Ok(ApplyOutcome::Applied(1))
}

fn path_tree_matches_baselines(
    path: &Path,
    conflicts: &crate::conflict::ConflictEngine,
    project_root: &Path,
) -> Result<bool, String> {
    let Some(fence) = capture_synced_subtree(project_root, path)? else {
        return Ok(false);
    };
    for entry in &fence.entries {
        if entry.kind != crate::fs_safety::SafeEntryKind::File {
            continue;
        }
        let Some(name) = entry.path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if classify_script_file(name).is_none() && !is_init_file(name) {
            continue;
        }
        let bytes = read_synced_file(project_root, &entry.path)?;
        if !conflicts.matches_baseline(&entry.path, &normalize_line_endings(&bytes)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A representation transition removes its old physical root wholesale.
/// Unlike an ordinary synced-tree conflict check, every descendant must be a
/// source Ro Sync owns; otherwise an unrelated sidecar file or empty folder
/// would be lost when the old directory is removed.
fn transition_tree_matches_baselines(
    path: &Path,
    conflicts: &crate::conflict::ConflictEngine,
    project_root: &Path,
) -> Result<bool, String> {
    let Some(fence) = capture_synced_subtree(project_root, path)? else {
        return Ok(false);
    };
    let root_is_file = fence
        .entries
        .first()
        .is_some_and(|entry| entry.kind == crate::fs_safety::SafeEntryKind::File);
    for (index, entry) in fence.entries.iter().enumerate() {
        match entry.kind {
            crate::fs_safety::SafeEntryKind::Directory => {
                if index != 0 && !disk_path_is_sync_owned(&entry.path) {
                    return Ok(false);
                }
            }
            crate::fs_safety::SafeEntryKind::File => {
                let Some(name) = entry.path.file_name().and_then(|value| value.to_str()) else {
                    return Ok(false);
                };
                if classify_script_file(name).is_none() && !is_init_file(name) {
                    return Ok(false);
                }
                let bytes = read_synced_file(project_root, &entry.path)?;
                if !conflicts.matches_baseline(&entry.path, &normalize_line_endings(&bytes)) {
                    return Ok(false);
                }
            }
        }
    }
    if root_is_file {
        let root = &fence.entries[0].path;
        let Some(name) = root.file_name().and_then(|value| value.to_str()) else {
            return Ok(false);
        };
        if classify_script_file(name).is_none() && !is_init_file(name) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn apply_update(
    root: &Path,
    segs: &[String],
    properties: Option<Value>,
    _new_name: Option<String>,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    let Some(target) = resolve_segments_to_path(root, segs)? else {
        if update_source_bytes(&properties)?.is_some() {
            return Err(format!(
                "update: Source target does not exist: {}",
                segs.join("/")
            ));
        }
        return Ok(ApplyOutcome::Skipped);
    };
    apply_update_target(target, properties, ctx)
}

fn update_source_bytes(properties: &Option<Value>) -> Result<Option<Vec<u8>>, String> {
    let Some(props) = properties.as_ref().and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(source) = props.get("Source") else {
        return Ok(None);
    };
    let source = source
        .as_str()
        .ok_or_else(|| "update: properties.Source must be a string".to_string())?;
    Ok(Some(normalize_line_endings(source.as_bytes()).into_owned()))
}

fn source_write_outcome(outcome: SourceWriteOutcome) -> ApplyOutcome {
    match outcome {
        SourceWriteOutcome::Applied => ApplyOutcome::Applied(1),
        // The requested source is already present. This is an accepted,
        // idempotent write rather than an unapplied mutation.
        SourceWriteOutcome::Skipped => ApplyOutcome::Applied(0),
        SourceWriteOutcome::Conflict(path) => ApplyOutcome::Conflict(path),
    }
}

fn resolve_legacy_init_update_target(target: &Path) -> Result<Option<PathBuf>, String> {
    let Some(file_name) = target.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let requested = parse_init_file(file_name)
        .map(|(class, name)| (class, Some(name)))
        .or_else(|| parse_plain_init_file(file_name).map(|class| (class, None)));
    let Some((requested_class, requested_name)) = requested else {
        return Ok(None);
    };
    let Some(parent) = target.parent() else {
        return Ok(None);
    };
    let Some(parent_metadata) = crate::fs_safety::metadata_no_follow(parent)
        .map_err(|error| format!("inspect legacy init parent {}: {error}", parent.display()))?
    else {
        return Ok(None);
    };
    if !parent_metadata.is_dir() {
        return Ok(None);
    }
    let Some((actual_class, actual_name, actual_path)) = script_with_children_source(parent)
        .map_err(|error| format!("inspect legacy init source {}: {error}", parent.display()))?
    else {
        return Ok(None);
    };
    if actual_class != requested_class
        || requested_name
            .as_ref()
            .is_some_and(|requested_name| !logical_names_equivalent(requested_name, &actual_name))
    {
        return Err(format!(
            "update: requested init source identity does not match {}",
            parent.display()
        ));
    }
    Ok(Some(actual_path))
}

fn apply_update_target(
    target: PathBuf,
    properties: Option<Value>,
    ctx: &PushCtx<'_>,
) -> Result<ApplyOutcome, String> {
    let Some(bytes) = update_source_bytes(&properties)? else {
        return Ok(ApplyOutcome::Skipped);
    };
    let metadata = crate::fs_safety::metadata_no_follow(&target)
        .map_err(|error| format!("inspect update target {}: {error}", target.display()))?
        .map(|metadata| (target.clone(), metadata));
    let (target, metadata) = match metadata {
        Some(target) => target,
        None => {
            let Some(legacy_target) = resolve_legacy_init_update_target(&target)? else {
                return Err(format!(
                    "update: Source target does not exist: {}",
                    target.display()
                ));
            };
            let metadata =
                crate::fs_safety::require_metadata_no_follow(&legacy_target).map_err(|error| {
                    format!(
                        "inspect resolved legacy init source {}: {error}",
                        legacy_target.display()
                    )
                })?;
            (legacy_target, metadata)
        }
    };

    // Script leaf: properties.Source replaces file contents.
    if metadata.is_file() {
        return apply_source_bytes(&target, &bytes, ctx).map(source_write_outcome);
    }

    if metadata.is_dir() {
        let Some((_class, _name, init_source)) = script_with_children_source(&target)
            .map_err(|error| format!("inspect update directory {}: {error}", target.display()))?
        else {
            return Err(format!(
                "update: Source target is not a script-with-children directory: {}",
                target.display()
            ));
        };
        return apply_source_bytes(&init_source, &bytes, ctx).map(source_write_outcome);
    }

    Err(format!(
        "update: Source target is not a regular file or script-with-children directory: {}",
        target.display()
    ))
}

fn apply_rename(
    root: &Path,
    segs: &[String],
    new_name: &str,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    let Some(target) = resolve_segments_to_path(root, segs)? else {
        return Ok(0);
    };
    apply_rename_target(target, new_name, None, ctx)
}

fn apply_rename_target(
    target: PathBuf,
    new_name: &str,
    exact_destination: Option<PathBuf>,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    if crate::fs_safety::metadata_no_follow(&target)
        .map_err(|error| format!("inspect rename source {}: {error}", target.display()))?
        .is_none()
    {
        return Ok(0);
    }
    let parent_dir = target
        .parent()
        .ok_or_else(|| format!("rename: no parent for {}", target.display()))?
        .to_path_buf();

    let inst = path_to_instance_meta(&target)
        .map_err(|error| format!("rename: inspect {}: {error}", target.display()))?
        .ok_or_else(|| {
            format!(
                "rename: source is not a synced instance: {}",
                target.display()
            )
        })?;
    let class = inst.class;
    let has_children = inst.is_dir;
    let script_with_children = inst.is_script_with_children;
    let current_frag = target
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let new_path = if let Some(destination) = exact_destination {
        let destination_parent = destination
            .parent()
            .ok_or_else(|| format!("rename: no parent for {}", destination.display()))?;
        if !paths_refer_to_same_entry(&parent_dir, destination_parent) {
            return Err(format!(
                "rename: destination must stay in the source directory: {}",
                destination.display()
            ));
        }
        let fragment = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                format!(
                    "rename: non-UTF-8 destination fragment {}",
                    destination.display()
                )
            })?;
        if !disk_fragment_matches_identity(fragment, new_name, &class, has_children) {
            return Err(format!(
                "rename: destination fragment {fragment:?} does not match renamed instance identity"
            ));
        }
        destination
    } else {
        let taken = siblings_except(&parent_dir, current_frag.as_deref())?;
        let new_frag = instance_to_path(
            &InstanceDescriptor {
                class: &class,
                name: new_name,
                has_children,
            },
            &taken,
        );
        parent_dir.join(&new_frag.fragment)
    };
    if crate::fs_safety::metadata_no_follow(&new_path)
        .map_err(|error| format!("inspect rename destination {}: {error}", new_path.display()))?
        .is_some()
        && !paths_refer_to_same_entry(&target, &new_path)
    {
        return Err(format!(
            "rename: destination already exists: {}",
            new_path.display()
        ));
    }
    rename_path_and_init(&target, &new_path, new_name, script_with_children, ctx)?;
    // The source bytes did not change, but conflict baselines are keyed by
    // filesystem path. Leaving them under the old name makes the next clean
    // Studio edit/delete look like an unknown post-restart divergence. Rebase
    // only after the outer + named-init rename has completed successfully.
    ctx.conflicts.forget_path(&target);
    let renamed_metadata = crate::fs_safety::require_metadata_no_follow(&new_path)
        .map_err(|error| format!("inspect renamed path {}: {error}", new_path.display()))?;
    if renamed_metadata.is_dir() {
        seed_script_baselines_in_dir(ctx.project_root, &new_path, ctx.conflicts)?;
    } else if classify_script_file(
        new_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default(),
    )
    .is_some()
    {
        let bytes = read_synced_file(ctx.project_root, &new_path)?;
        let normalized = normalize_line_endings(&bytes).into_owned();
        ctx.conflicts
            .record_sync(&new_path, hash(&normalized), fs_mtime(&new_path));
    }
    Ok(1)
}

#[derive(Debug)]
struct InitRenamePlan {
    old_name: std::ffi::OsString,
    new_name: String,
}

fn prepare_init_rename(
    dir: &Path,
    new_instance_name: &str,
    script_with_children: bool,
) -> Result<Option<InitRenamePlan>, String> {
    if !script_with_children {
        return Ok(None);
    }
    let Some((class, _, source_path)) = script_with_children_source(dir)
        .map_err(|error| format!("scan rename source {}: {error}", dir.display()))?
    else {
        return Err(format!(
            "rename: script-with-children has no init source in {}",
            dir.display()
        ));
    };
    let Some(old_name) = source_path.file_name() else {
        return Err(format!(
            "rename: init source has no filename: {}",
            source_path.display()
        ));
    };
    if parse_init_file(old_name.to_string_lossy().as_ref()).is_none() {
        // Plain Wally/Rojo `init.lua` roots derive their identity from the
        // directory name and therefore need no inner rename.
        return Ok(None);
    };
    let new_name = portable_init_file_name(new_instance_name, class);
    if old_name == std::ffi::OsStr::new(&new_name) {
        return Ok(None);
    }

    Ok(Some(InitRenamePlan {
        old_name: old_name.to_os_string(),
        new_name,
    }))
}

fn init_rename_temp_path(dir: &Path) -> Result<PathBuf, String> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    for _ in 0..32 {
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = dir.join(format!(
            ".rosync-init-rename-{}-{sequence}.tmp",
            std::process::id()
        ));
        match crate::fs_safety::metadata_no_follow(&candidate) {
            Ok(None) => return Ok(candidate),
            Ok(Some(_)) => {}
            Err(error) => {
                return Err(format!(
                    "rename: inspect temporary init path {}: {error}",
                    candidate.display()
                ));
            }
        }
    }
    Err(format!(
        "rename: could not allocate a temporary init path in {}",
        dir.display()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitRenameStage {
    Unchanged,
    Temporary,
    Renamed,
}

#[derive(Debug)]
struct InitRenameRollbackPaths {
    old: PathBuf,
    temporary: PathBuf,
    renamed: PathBuf,
    old_relative: PathBuf,
    temporary_relative: PathBuf,
    renamed_relative: PathBuf,
}

struct RenameRollbackContext<'a> {
    project_root: &'a Path,
    target: &'a Path,
    new_path: &'a Path,
    source_fence: &'a SafeSubtreeFence,
    source_directory_identity: Option<&'a crate::fs_safety::FileIdentity>,
    source_parent_guard: &'a crate::fs_safety::PathParentGuard,
    destination_parent_guard: &'a crate::fs_safety::PathParentGuard,
    init_paths: Option<&'a InitRenameRollbackPaths>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenamePathAndInitCheckpoint {
    PostOuterSourceParentVerify,
    PostOuterDestinationParentVerify,
    MovedFenceCapture,
    MovedDirectoryGuardCreate,
    MovedDirectoryGuardVerify,
    DestinationMetadataInspect,
    FinalMovedDirectoryVerify,
}

fn relocated_subtree_matches_init_stage(
    source: &SafeSubtreeFence,
    current: &SafeSubtreeFence,
    init_paths: Option<&InitRenameRollbackPaths>,
    stage: InitRenameStage,
) -> bool {
    if source.entries.len() != current.entries.len() {
        return false;
    }
    let current_by_relative = current
        .entries
        .iter()
        .map(|entry| (entry.relative.as_path(), entry))
        .collect::<HashMap<_, _>>();
    source.entries.iter().all(|source_entry| {
        let expected_relative = match (init_paths, stage) {
            (Some(paths), InitRenameStage::Temporary)
                if source_entry.relative == paths.old_relative =>
            {
                &paths.temporary_relative
            }
            (Some(paths), InitRenameStage::Renamed)
                if source_entry.relative == paths.old_relative =>
            {
                &paths.renamed_relative
            }
            _ => &source_entry.relative,
        };
        current_by_relative
            .get(expected_relative.as_path())
            .is_some_and(|current_entry| {
                source_entry.kind == current_entry.kind
                    && source_entry.file_generation == current_entry.file_generation
            })
    })
}

fn rollback_destination_is_available(
    destination: &Path,
    current_source: &Path,
) -> Result<(), String> {
    match crate::fs_safety::metadata_no_follow(destination) {
        Ok(None) => Ok(()),
        Ok(Some(_)) if paths_refer_to_same_entry(destination, current_source) => Ok(()),
        Ok(Some(_)) => Err(format!(
            "rollback destination appeared and is not the current source: {}",
            destination.display()
        )),
        Err(error) => Err(format!(
            "inspect rollback destination {}: {error}",
            destination.display()
        )),
    }
}

fn verify_renamed_tree_for_rollback(
    context: &RenameRollbackContext<'_>,
    stage: InitRenameStage,
) -> Result<(), String> {
    context.source_parent_guard.verify().map_err(|error| {
        format!(
            "source parent changed before rollback {}: {error}",
            context.target.display()
        )
    })?;
    context.destination_parent_guard.verify().map_err(|error| {
        format!(
            "destination parent changed before rollback {}: {error}",
            context.new_path.display()
        )
    })?;
    rollback_destination_is_available(context.target, context.new_path)
        .map_err(|reason| format!("outer {reason}"))?;
    if let Some(expected_identity) = context.source_directory_identity {
        let current = crate::fs_safety::directory_generation_no_follow(context.new_path)
            .map_err(|error| format!("inspect renamed directory identity: {error}"))?;
        if &current.identity != expected_identity {
            return Err(format!(
                "renamed directory identity changed before rollback: {}",
                context.new_path.display()
            ));
        }
    }
    let current =
        capture_synced_subtree(context.project_root, context.new_path)?.ok_or_else(|| {
            format!(
                "renamed path disappeared before rollback: {}",
                context.new_path.display()
            )
        })?;
    if !relocated_subtree_matches_init_stage(
        context.source_fence,
        &current,
        context.init_paths,
        stage,
    ) {
        return Err(format!(
            "renamed tree changed before rollback at stage {stage:?}: {}",
            context.new_path.display()
        ));
    }
    context.source_parent_guard.verify().map_err(|error| {
        format!(
            "source parent changed while preparing rollback {}: {error}",
            context.target.display()
        )
    })?;
    context.destination_parent_guard.verify().map_err(|error| {
        format!(
            "destination parent changed while preparing rollback {}: {error}",
            context.new_path.display()
        )
    })?;
    Ok(())
}

fn rollback_renamed_path_and_init<R>(
    context: &RenameRollbackContext<'_>,
    stage: InitRenameStage,
    rename: &mut R,
) -> String
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    if let Err(reason) = verify_renamed_tree_for_rollback(context, stage) {
        return format!("init rollback: refused; outer rollback: refused: {reason}");
    }

    let init_status = match stage {
        InitRenameStage::Unchanged => "not needed".to_string(),
        InitRenameStage::Temporary | InitRenameStage::Renamed => {
            let Some(paths) = context.init_paths else {
                return "init rollback: refused: missing init paths; outer rollback: refused"
                    .to_string();
            };
            let current = match stage {
                InitRenameStage::Temporary => &paths.temporary,
                InitRenameStage::Renamed => &paths.renamed,
                InitRenameStage::Unchanged => unreachable!(),
            };
            if let Err(reason) = rollback_destination_is_available(&paths.old, current) {
                return format!("init rollback: refused: {reason}; outer rollback: refused");
            }
            if let Err(error) = rename(current, &paths.old) {
                return format!(
                    "init rollback: failed {} → {}: {error}; outer rollback: refused",
                    current.display(),
                    paths.old.display()
                );
            }
            "ok".to_string()
        }
    };

    if let Err(reason) = verify_renamed_tree_for_rollback(context, InitRenameStage::Unchanged) {
        return format!("init rollback: {init_status}; outer rollback: refused: {reason}");
    }
    if let Err(error) = rename(context.new_path, context.target) {
        return format!(
            "init rollback: {init_status}; outer rollback: failed {} → {}: {error}",
            context.new_path.display(),
            context.target.display()
        );
    }

    let verification = (|| {
        context.source_parent_guard.verify().map_err(|error| {
            format!(
                "source parent changed after outer rollback {}: {error}",
                context.target.display()
            )
        })?;
        context.destination_parent_guard.verify().map_err(|error| {
            format!(
                "destination parent changed after outer rollback {}: {error}",
                context.new_path.display()
            )
        })?;
        if let Some(expected_identity) = context.source_directory_identity {
            let restored = crate::fs_safety::directory_generation_no_follow(context.target)
                .map_err(|error| format!("inspect restored directory identity: {error}"))?;
            if &restored.identity != expected_identity {
                return Err(format!(
                    "restored directory identity changed: {}",
                    context.target.display()
                ));
            }
        }
        let restored =
            capture_synced_subtree(context.project_root, context.target)?.ok_or_else(|| {
                format!(
                    "outer rollback target disappeared: {}",
                    context.target.display()
                )
            })?;
        if !relocated_subtree_matches(context.source_fence, &restored) {
            return Err(format!(
                "restored tree differs from the pre-rename tree: {}",
                context.target.display()
            ));
        }
        Ok::<(), String>(())
    })();
    match verification {
        Ok(()) => format!("init rollback: {init_status}; outer rollback: ok"),
        Err(reason) => format!(
            "init rollback: {init_status}; outer rollback: completed but verification failed: {reason}"
        ),
    }
}

fn rename_failure_after_outer<R>(
    primary: String,
    context: &RenameRollbackContext<'_>,
    stage: InitRenameStage,
    rename: &mut R,
) -> String
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let rollback = rollback_renamed_path_and_init(context, stage, rename);
    format!("{primary}; {rollback}")
}

fn rename_path_and_init(
    target: &Path,
    new_path: &Path,
    new_instance_name: &str,
    script_with_children: bool,
    ctx: &PushCtx<'_>,
) -> Result<(), String> {
    rename_path_and_init_with(
        target,
        new_path,
        new_instance_name,
        script_with_children,
        ctx,
        |from, to| std::fs::rename(from, to),
    )
}

fn rename_path_and_init_with<R>(
    target: &Path,
    new_path: &Path,
    new_instance_name: &str,
    script_with_children: bool,
    ctx: &PushCtx<'_>,
    rename: R,
) -> Result<(), String>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    rename_path_and_init_with_checkpoints(
        target,
        new_path,
        new_instance_name,
        script_with_children,
        ctx,
        rename,
        |_| Ok(()),
    )
}

fn rename_path_and_init_with_checkpoints<R, C>(
    target: &Path,
    new_path: &Path,
    new_instance_name: &str,
    script_with_children: bool,
    ctx: &PushCtx<'_>,
    mut rename: R,
    mut checkpoint: C,
) -> Result<(), String>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
    C: FnMut(RenamePathAndInitCheckpoint) -> Result<(), String>,
{
    let source_fence = capture_synced_subtree(ctx.project_root, target)?
        .ok_or_else(|| format!("rename source disappeared: {}", target.display()))?;
    let source_directory_identity = if source_fence
        .entries
        .first()
        .is_some_and(|entry| entry.kind == crate::fs_safety::SafeEntryKind::Directory)
    {
        Some(
            crate::fs_safety::directory_generation_no_follow(target)
                .map_err(|error| {
                    format!(
                        "inspect rename source directory identity {}: {error}",
                        target.display()
                    )
                })?
                .identity,
        )
    } else {
        None
    };
    let source_parent = target
        .parent()
        .ok_or_else(|| format!("rename source has no parent: {}", target.display()))?;
    let destination_parent = new_path
        .parent()
        .ok_or_else(|| format!("rename destination has no parent: {}", new_path.display()))?;
    let source_parent_guard =
        crate::fs_safety::guard_synced_directory_chain(ctx.project_root, source_parent).map_err(
            |error| {
                format!(
                    "guard rename source parent {}: {error}",
                    source_parent.display()
                )
            },
        )?;
    let destination_parent_guard =
        crate::fs_safety::guard_synced_directory_chain(ctx.project_root, destination_parent)
            .map_err(|error| {
                format!(
                    "guard rename destination parent {}: {error}",
                    destination_parent.display()
                )
            })?;
    let init_plan = prepare_init_rename(target, new_instance_name, script_with_children)?;
    let temp_name = if init_plan.is_some() {
        Some(
            init_rename_temp_path(target)?
                .file_name()
                .ok_or_else(|| {
                    format!("rename: invalid temporary path under {}", target.display())
                })?
                .to_os_string(),
        )
    } else {
        None
    };
    let init_rollback_paths =
        init_plan
            .as_ref()
            .zip(temp_name.as_ref())
            .map(|(plan, temp_name)| InitRenameRollbackPaths {
                old: new_path.join(&plan.old_name),
                temporary: new_path.join(temp_name),
                renamed: new_path.join(&plan.new_name),
                old_relative: PathBuf::from(&plan.old_name),
                temporary_relative: PathBuf::from(temp_name),
                renamed_relative: PathBuf::from(&plan.new_name),
            });
    let rollback_context = RenameRollbackContext {
        project_root: ctx.project_root,
        target,
        new_path,
        source_fence: &source_fence,
        source_directory_identity: source_directory_identity.as_ref(),
        source_parent_guard: &source_parent_guard,
        destination_parent_guard: &destination_parent_guard,
        init_paths: init_rollback_paths.as_ref(),
    };
    ctx.mark_quiet(target);
    ctx.mark_quiet(new_path);
    source_parent_guard.verify().map_err(|error| {
        format!(
            "rename source parent changed before commit {}: {error}",
            source_parent.display()
        )
    })?;
    destination_parent_guard.verify().map_err(|error| {
        format!(
            "rename destination parent changed before commit {}: {error}",
            destination_parent.display()
        )
    })?;
    rename(target, new_path).map_err(|error| {
        format!(
            "rename {} → {}: {error}",
            target.display(),
            new_path.display()
        )
    })?;
    let post_outer_source = checkpoint(RenamePathAndInitCheckpoint::PostOuterSourceParentVerify)
        .and_then(|()| {
            source_parent_guard.verify().map_err(|error| {
                format!(
                    "rename source parent changed during commit {}: {error}",
                    source_parent.display()
                )
            })
        });
    if let Err(error) = post_outer_source {
        return Err(rename_failure_after_outer(
            error,
            &rollback_context,
            InitRenameStage::Unchanged,
            &mut rename,
        ));
    }
    let post_outer_destination =
        checkpoint(RenamePathAndInitCheckpoint::PostOuterDestinationParentVerify).and_then(|()| {
            destination_parent_guard.verify().map_err(|error| {
                format!(
                    "rename destination parent changed during commit {}: {error}",
                    destination_parent.display()
                )
            })
        });
    if let Err(error) = post_outer_destination {
        return Err(rename_failure_after_outer(
            error,
            &rollback_context,
            InitRenameStage::Unchanged,
            &mut rename,
        ));
    }
    let moved_fence = match checkpoint(RenamePathAndInitCheckpoint::MovedFenceCapture)
        .and_then(|()| capture_synced_subtree(ctx.project_root, new_path))
    {
        Ok(Some(fence)) => fence,
        Ok(None) => {
            return Err(rename_failure_after_outer(
                format!("renamed path disappeared: {}", new_path.display()),
                &rollback_context,
                InitRenameStage::Unchanged,
                &mut rename,
            ));
        }
        Err(error) => {
            return Err(rename_failure_after_outer(
                error,
                &rollback_context,
                InitRenameStage::Unchanged,
                &mut rename,
            ));
        }
    };
    if !relocated_subtree_matches(&source_fence, &moved_fence) {
        return Err(rename_failure_after_outer(
            format!(
                "renamed subtree changed during commit: {}",
                new_path.display()
            ),
            &rollback_context,
            InitRenameStage::Unchanged,
            &mut rename,
        ));
    }

    if init_plan.is_none() {
        return Ok(());
    }
    let init_paths = init_rollback_paths
        .as_ref()
        .expect("init plan allocates rollback paths");
    let old_init = &init_paths.old;
    let new_init = &init_paths.renamed;
    let temp_init = &init_paths.temporary;
    ctx.mark_quiet(old_init);
    ctx.mark_quiet(new_init);
    ctx.mark_quiet(temp_init);
    let moved_directory_guard =
        match checkpoint(RenamePathAndInitCheckpoint::MovedDirectoryGuardCreate).and_then(|()| {
            crate::fs_safety::guard_synced_directory_chain(ctx.project_root, new_path)
                .map_err(|error| format!("guard renamed directory {}: {error}", new_path.display()))
        }) {
            Ok(guard) => guard,
            Err(error) => {
                return Err(rename_failure_after_outer(
                    error,
                    &rollback_context,
                    InitRenameStage::Unchanged,
                    &mut rename,
                ));
            }
        };
    let pre_init_verify = checkpoint(RenamePathAndInitCheckpoint::MovedDirectoryGuardVerify)
        .and_then(|()| {
            moved_directory_guard.verify().map_err(|error| {
                format!(
                    "renamed directory changed before init update {}: {error}",
                    new_path.display()
                )
            })
        });
    if let Err(error) = pre_init_verify {
        return Err(rename_failure_after_outer(
            error,
            &rollback_context,
            InitRenameStage::Unchanged,
            &mut rename,
        ));
    }

    if let Err(init_error) = rename(old_init, temp_init) {
        return Err(rename_failure_after_outer(
            format!(
                "rename init {} → {}: {init_error}",
                old_init.display(),
                temp_init.display()
            ),
            &rollback_context,
            InitRenameStage::Unchanged,
            &mut rename,
        ));
    }

    // Check only after moving the old file aside. On case-insensitive
    // filesystems a case-only destination aliases the old path and disappears
    // at this point; on case-sensitive filesystems a genuinely distinct
    // destination remains and must never be overwritten.
    let destination_metadata = checkpoint(RenamePathAndInitCheckpoint::DestinationMetadataInspect)
        .and_then(|()| {
            crate::fs_safety::metadata_no_follow(new_init).map_err(|error| {
                format!("inspect init destination {}: {error}", new_init.display())
            })
        });
    let destination_metadata = match destination_metadata {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(rename_failure_after_outer(
                error,
                &rollback_context,
                InitRenameStage::Temporary,
                &mut rename,
            ));
        }
    };
    if destination_metadata.is_some() {
        return Err(rename_failure_after_outer(
            format!(
                "rename: init destination already exists: {}",
                new_init.display()
            ),
            &rollback_context,
            InitRenameStage::Temporary,
            &mut rename,
        ));
    }

    if let Err(init_error) = rename(temp_init, new_init) {
        return Err(rename_failure_after_outer(
            format!(
                "rename init {} → {}: {init_error}",
                temp_init.display(),
                new_init.display()
            ),
            &rollback_context,
            InitRenameStage::Temporary,
            &mut rename,
        ));
    }
    let final_verify =
        checkpoint(RenamePathAndInitCheckpoint::FinalMovedDirectoryVerify).and_then(|()| {
            moved_directory_guard.verify().map_err(|error| {
                format!(
                    "renamed directory changed during init update {}: {error}",
                    new_path.display()
                )
            })
        });
    if let Err(error) = final_verify {
        return Err(rename_failure_after_outer(
            error,
            &rollback_context,
            InitRenameStage::Renamed,
            &mut rename,
        ));
    }
    Ok(())
}

fn apply_move(
    root: &Path,
    from_segs: &[String],
    to_segs: &[String],
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    let Some(src) = resolve_segments_to_path(root, from_segs)? else {
        return Ok(0);
    };
    // `to` is the new full path (including the target's new name as the last seg).
    if to_segs.is_empty() {
        return Err("move: empty 'to' path".into());
    }
    let to_parent_segs = &to_segs[..to_segs.len() - 1];
    let new_name = &to_segs[to_segs.len() - 1];
    let parent_dir = resolve_segments_to_dir(root, to_parent_segs)?;
    ensure_synced_directory_chain(ctx.project_root, &parent_dir)?;
    let inst = path_to_instance_meta(&src)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("move: unsupported source {}", src.display()))?;
    let taken = siblings_except(&parent_dir, None)?;
    let fragment = instance_to_path(
        &InstanceDescriptor {
            class: &inst.class,
            name: new_name,
            has_children: inst.is_dir,
        },
        &taken,
    );
    apply_move_target(src, Some(parent_dir.join(fragment.fragment)), new_name, ctx)
}

fn apply_move_target(
    src: PathBuf,
    exact_destination: Option<PathBuf>,
    new_name: &str,
    ctx: &PushCtx<'_>,
) -> Result<usize, String> {
    if crate::fs_safety::metadata_no_follow(&src)
        .map_err(|error| format!("inspect move source {}: {error}", src.display()))?
        .is_none()
    {
        return Ok(0);
    }
    let inst = path_to_instance_meta(&src)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("move: unsupported source {}", src.display()))?;
    let class = inst.class;
    let has_children = inst.is_dir;
    let script_with_children = inst.is_script_with_children;
    if let Some(destination) = exact_destination.as_ref() {
        let fragment = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("move: non-UTF-8 destination {}", destination.display()))?;
        if !disk_fragment_matches_identity(fragment, new_name, &class, has_children) {
            return Err(format!(
                "move: destination fragment {fragment:?} does not match moved instance identity"
            ));
        }
    }
    let parent_dir = exact_destination
        .as_ref()
        .and_then(|destination| destination.parent())
        .map(Path::to_path_buf)
        .or_else(|| src.parent().map(Path::to_path_buf))
        .ok_or_else(|| format!("move: no destination parent for {}", src.display()))?;
    ensure_synced_directory_chain(ctx.project_root, &parent_dir)?;
    let taken = siblings_except(&parent_dir, None)?;
    let dest = exact_destination.unwrap_or_else(|| {
        let frag = instance_to_path(
            &InstanceDescriptor {
                class: &class,
                name: new_name,
                has_children,
            },
            &taken,
        );
        parent_dir.join(frag.fragment)
    });
    if crate::fs_safety::metadata_no_follow(&dest)
        .map_err(|error| format!("inspect move destination {}: {error}", dest.display()))?
        .is_some()
        && !paths_refer_to_same_entry(&dest, &src)
    {
        return Err(format!(
            "move: destination already exists: {}",
            dest.display()
        ));
    }
    rename_path_and_init(&src, &dest, new_name, script_with_children, ctx)?;
    ctx.mark_quiet(&src);
    ctx.mark_quiet(&dest);
    ctx.conflicts.forget_path(&src);
    // A reparent empties the old parent chain exactly like a delete does.
    prune_emptied_ancestors(&src, ctx);
    let destination_metadata = crate::fs_safety::require_metadata_no_follow(&dest)
        .map_err(|error| format!("inspect moved destination {}: {error}", dest.display()))?;
    if destination_metadata.is_dir() {
        seed_script_baselines_in_dir(ctx.project_root, &dest, ctx.conflicts)?;
    } else {
        let bytes = read_synced_file(ctx.project_root, &dest)?;
        let normalized = normalize_line_endings(&bytes).into_owned();
        ctx.conflicts
            .record_sync(&dest, hash(&normalized), fs_mtime(&dest));
    }
    Ok(1)
}

// ---------------------------------------------------------------------------
// Path resolution helpers
// ---------------------------------------------------------------------------

/// Resolve `segs` (Studio instance names, last segment included) to a filesystem
/// path if it exists. Returns Ok(None) if any segment doesn't resolve.
fn resolve_segments_to_path(root: &Path, segs: &[String]) -> Result<Option<PathBuf>, String> {
    let mut cur = root.to_path_buf();
    for (i, seg) in segs.iter().enumerate() {
        let lookup_dir = if i == 0 {
            root.to_path_buf()
        } else {
            cur.clone()
        };
        match find_child_fragment_by_lookup_segment(&lookup_dir, seg).map_err(|e| e.to_string())? {
            Some(frag) => cur = lookup_dir.join(frag),
            None => {
                // Fallback: encoded segment literally (top-level services).
                let candidate = lookup_dir.join(encode_name(seg));
                if crate::fs_safety::metadata_no_follow(&candidate)
                    .map_err(|error| format!("inspect path {}: {error}", candidate.display()))?
                    .is_some()
                {
                    cur = candidate;
                } else {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(cur))
}

/// Resolve the segments to a filesystem *directory* to be used as a parent
/// (creating-along-the-way is deferred to the caller).
fn resolve_segments_to_dir(root: &Path, segs: &[String]) -> Result<PathBuf, String> {
    // Resolve each existing segment before appending a missing one. Rebuilding
    // the whole path after the first miss would discard a legacy literal-
    // Unicode or disambiguated prefix and create a second encoded branch.
    let mut p = root.to_path_buf();
    for seg in segs {
        let next =
            match find_child_fragment_by_lookup_segment(&p, seg).map_err(|e| e.to_string())? {
                Some(fragment) => p.join(fragment),
                None => p.join(encode_name(seg)),
            };
        if let Some(metadata) = crate::fs_safety::metadata_no_follow(&next)
            .map_err(|error| format!("inspect parent path {}: {error}", next.display()))?
        {
            if !metadata.is_dir() {
                return Err(format!(
                    "path {} is a file, not a directory (needed as parent)",
                    next.display()
                ));
            }
        }
        p = next;
    }
    Ok(p)
}

/// Scan `dir` for a child whose instance name is `name`. Returns the fragment
/// (file/dir name) if found.
fn find_child_fragment_by_name(dir: &Path, name: &str) -> std::io::Result<Option<String>> {
    Ok(index_child_fragments_by_name(dir)?
        .remove(name)
        .map(|(fragment, _priority)| fragment))
}

/// Resolve a plugin lookup segment. Plain segments retain the legacy
/// name-based behavior; generated `<Name> [N]` segments select that exact
/// filesystem ordinal. A literal Roblox name ending in the reserved grammar is
/// encoded as `%5B` on disk and therefore remains available as an exact
/// logical-name match before the generated fallback.
fn find_child_fragment_by_lookup_segment(
    dir: &Path,
    segment: &str,
) -> std::io::Result<Option<String>> {
    let index = index_child_fragments(dir)?;
    if let Some((fragment, _)) = index.best_by_name.get(segment) {
        return Ok(Some(fragment.clone()));
    }
    let Some((base, ordinal)) = parse_disambiguated(segment) else {
        return Ok(None);
    };
    Ok(index.all_by_name.get(&base).and_then(|fragments| {
        fragments
            .iter()
            .find(|fragment| fragment_disambiguation_ordinal(fragment) == ordinal)
            .cloned()
    }))
}

struct ExistingChildFragmentIndex {
    best_by_name: HashMap<String, (String, u8)>,
    all_by_name: HashMap<String, Vec<String>>,
}

/// Index existing filesystem fragments in one directory scan. The best entry
/// preserves legacy lookup behavior, while the complete list supports
/// deterministic one-to-one assignment of duplicate logical names.
fn index_child_fragments(dir: &Path) -> std::io::Result<ExistingChildFragmentIndex> {
    let Some(metadata) = crate::fs_safety::metadata_no_follow(dir)? else {
        return Ok(ExistingChildFragmentIndex {
            best_by_name: HashMap::new(),
            all_by_name: HashMap::new(),
        });
    };
    if !metadata.is_dir() {
        return Ok(ExistingChildFragmentIndex {
            best_by_name: HashMap::new(),
            all_by_name: HashMap::new(),
        });
    }
    let mut best = HashMap::new();
    let mut all = HashMap::<String, Vec<String>>::new();
    let index = crate::fs_safety::PortableDirectoryIndex::read(dir)?;
    for entry in index.entries() {
        let fstr = entry.fragment.as_str();
        if fstr == META_FILE {
            continue;
        }
        let inst = path_to_instance_meta(&entry.path)?;
        if let Some(i) = inst {
            let priority = fragment_lookup_priority(&entry.path, &i);
            all.entry(i.name.clone())
                .or_default()
                .push(fstr.to_string());
            let candidate = best
                .entry(i.name)
                .or_insert_with(|| (fstr.to_string(), priority));
            if priority > candidate.1 {
                *candidate = (fstr.to_string(), priority);
            }
        }
    }
    for fragments in all.values_mut() {
        fragments.sort_by(|left, right| {
            fragment_disambiguation_ordinal(left)
                .cmp(&fragment_disambiguation_ordinal(right))
                .then_with(|| left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase()))
                .then_with(|| left.cmp(right))
        });
    }
    Ok(ExistingChildFragmentIndex {
        best_by_name: best,
        all_by_name: all,
    })
}

fn fragment_disambiguation_ordinal(fragment: &str) -> usize {
    parse_disambiguated(fragment)
        .or_else(|| classify_script_file(fragment).and_then(|(_, stem)| parse_disambiguated(&stem)))
        .map(|(_, ordinal)| ordinal)
        .unwrap_or(0)
}

fn index_child_fragments_by_name(dir: &Path) -> std::io::Result<HashMap<String, (String, u8)>> {
    Ok(index_child_fragments(dir)?.best_by_name)
}

fn fragment_lookup_priority(path: &Path, inst: &PathInstance) -> u8 {
    if inst.is_script_with_children {
        return 4;
    }
    if inst.script_class.is_some() && !inst.is_dir {
        return 3;
    }
    if inst.class == "Folder" && is_empty_plain_folder(path).unwrap_or(false) {
        return 0;
    }
    if inst.class == "Folder" {
        return 1;
    }
    2
}

fn siblings_except(dir: &Path, except: Option<&str>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let Some(metadata) = crate::fs_safety::metadata_no_follow(dir)
        .map_err(|error| format!("inspect siblings directory {}: {error}", dir.display()))?
    else {
        return Ok(out);
    };
    if !metadata.is_dir() {
        return Err(format!(
            "siblings path is not a directory: {}",
            dir.display()
        ));
    }
    let index = crate::fs_safety::PortableDirectoryIndex::read(dir)
        .map_err(|error| format!("scan siblings directory {}: {error}", dir.display()))?;
    for entry in index.entries() {
        let s = entry.fragment.as_str();
        if Some(s) == except {
            continue;
        }
        out.push(s.to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Filesystem op → plugin op translation
// ---------------------------------------------------------------------------

fn collapse_plugin_ops(mut ops: Vec<Value>) -> Option<Value> {
    match ops.len() {
        0 => None,
        1 => ops.pop(),
        _ => Some(json!({ "op": "batch", "ops": ops })),
    }
}

/// Re-project one directory after its parent-source carrier was deleted or
/// renamed. A non-empty directory becomes a Folder; a directory with a
/// surviving source marker remains (or becomes) that script class. Empty plain
/// directories are absent from the Studio projection.
fn parent_projection_plugin_op(
    root: &Path,
    parent: &Path,
    source_override: Option<&[u8]>,
) -> Option<Value> {
    let rel = parent.strip_prefix(root).ok()?;
    let segs: Vec<String> = rel
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(String::from))
        .collect();
    // A service itself can never be replaced by a Folder or script.
    if segs.len() < 2 || !is_synced_service_segment(&segs[0]) {
        return None;
    }

    let inst = path_to_instance_meta(parent).ok().flatten()?;
    let instance_path = segs_to_instance_path(&segs)?;
    if path_is_avoid_synced(root, &instance_path) {
        return None;
    }
    let lookup_path = segs_to_lookup_path(&segs)?;

    if inst.class == "Folder" && is_empty_plain_folder(parent).ok() == Some(true) {
        return Some(json!({
            "op": "delete",
            "path": lookup_path,
            "diskPath": segs,
            "diskFragmentIsDir": true,
        }));
    }

    let mut properties = Map::new();
    if inst.script_class.is_some() {
        let source = if let Some(bytes) = source_override {
            bytes.to_vec()
        } else {
            let (_, _, source_path) = script_with_children_source(parent).ok().flatten()?;
            read_synced_file(root, &source_path).ok()?
        };
        properties.insert(
            "Source".to_string(),
            Value::String(String::from_utf8_lossy(&source).to_string()),
        );
    }

    let parent_segs = &segs[..segs.len() - 1];
    Some(json!({
        "op": "set",
        "path": segs_to_lookup_path(parent_segs)?,
        "diskPath": segs,
        "diskFragmentIsDir": true,
        "node": {
            "class": inst.class,
            "name": inst.name,
            "diskFragment": rel.file_name()?.to_str()?,
            "diskFragmentIsDir": true,
            "properties": Value::Object(properties),
            "children": Value::Array(Vec::new()),
        },
    }))
}

fn init_carrier_rename_plugin_op(
    root: &Path,
    op: &Op,
    from_path: &Path,
    from_is_carrier: bool,
    to_is_carrier: bool,
) -> Option<Value> {
    let source_parent = from_path.parent()?;
    let destination_parent = op.path.parent()?;
    let same_parent = source_parent == destination_parent;
    let mut ops = Vec::with_capacity(2);

    if from_is_carrier && (!to_is_carrier || !same_parent) {
        ops.push(parent_projection_plugin_op(root, source_parent, None)?);
    } else if !from_is_carrier {
        let delete_source = Op {
            kind: OpKind::Delete,
            path: from_path.to_path_buf(),
            from: None,
            content: None,
            is_dir: Some(false),
        };
        ops.push(fs_op_to_plugin_op(root, &delete_source)?);
    }

    if to_is_carrier {
        ops.push(parent_projection_plugin_op(
            root,
            destination_parent,
            op.content.as_deref(),
        )?);
    } else {
        let set_destination = Op {
            kind: OpKind::Update,
            path: op.path.clone(),
            from: None,
            content: op.content.clone(),
            is_dir: Some(false),
        };
        ops.push(fs_op_to_plugin_op(root, &set_destination)?);
    }

    collapse_plugin_ops(ops)
}

/// Convert a watcher `Op` into a plugin-facing op (`set` / `delete` / `update` /
/// `rename`). Directories (add/update) produce `set` ops with a minimal node
/// envelope; leaf scripts produce `set` ops carrying `properties.Source`.
pub(crate) fn fs_op_to_plugin_op(root: &Path, op: &Op) -> Option<Value> {
    let rel = op.path.strip_prefix(root).ok()?;
    let segs: Vec<String> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(String::from))
        .collect();
    if segs.is_empty() {
        return None;
    }

    // Ignore generated files (daemon-authored at the project root).
    if segs.last().map(|s| s.as_str()) == Some(snapshot::RO_SYNC_MD)
        || segs.last().map(|s| s.as_str()) == Some(snapshot::TREE_JSON)
        || segs.last().map(|s| s.as_str()) == Some(".tree.json.tmp")
    {
        return None;
    }

    if !is_synced_service_segment(&segs[0]) {
        return None;
    }

    // Raw leaf names in the reserved init-marker namespace must be migrated to
    // their escaped canonical fragment before they can cross the live
    // transport. Startup and watcher validation surface the actionable error;
    // this is a defense-in-depth guard for direct callers and conflict replay.
    // Deletes and raw -> canonical cleanup renames remain allowed.
    if matches!(op.kind, OpKind::Add | OpKind::Update | OpKind::Rename)
        && op.is_dir != Some(true)
        && !matches!(
            legacy_reserved_init_leaf_migration_message(root, &op.path),
            Ok(None)
        )
    {
        return None;
    }

    match op.kind {
        OpKind::Delete => {
            // The carrier file describes its parent instance, not a literal
            // child. Re-project that parent from the post-delete directory
            // shape so Script -> Folder/absent transitions cannot leave a stale
            // Studio class behind.
            if op.is_dir != Some(true) && init_path_describes_parent(&op.path) {
                return op
                    .path
                    .parent()
                    .and_then(|parent| parent_projection_plugin_op(root, parent, None));
            }
            let target_lookup_segs = segs_to_lookup_path(&segs)?;
            let target_name_segs = segs_to_instance_path(&segs)?;
            if deleted_path_is_shadowed_ignored_folder(root, &segs, &op.path) {
                return None;
            }
            if path_is_avoid_synced(root, &target_name_segs) {
                return None;
            }
            Some(json!({
                "op": "delete",
                "path": target_lookup_segs,
                "diskPath": segs,
                "diskFragmentIsDir": op.is_dir,
            }))
        }
        OpKind::Rename => {
            if is_empty_plain_folder(&op.path).unwrap_or(false) {
                return None;
            }
            // `op.path` is the destination (new) path; `op.from` is the source.
            let from_path = op.from.as_ref()?;
            let from_rel = from_path.strip_prefix(root).ok()?;
            let from_segs_fs: Vec<String> = from_rel
                .components()
                .filter_map(|c| c.as_os_str().to_str().map(String::from))
                .collect();
            if from_segs_fs.is_empty() {
                return None;
            }
            if !is_synced_service_segment(&from_segs_fs[0]) {
                return None;
            }
            let from_is_carrier = op.is_dir != Some(true) && init_path_describes_parent(from_path);
            let to_is_carrier =
                op.is_dir != Some(true) && path_is_parent_init_source(&op.path).ok() == Some(true);
            if from_is_carrier || to_is_carrier {
                return init_carrier_rename_plugin_op(
                    root,
                    op,
                    from_path,
                    from_is_carrier,
                    to_is_carrier,
                );
            }
            let from_lookup = segs_to_lookup_path(&from_segs_fs)?;
            let to_naming = segs_to_naming_path(&segs)?;
            let from_name = segs_to_instance_path(&from_segs_fs)?;
            let to_name = segs_to_instance_path(&segs)?;
            if path_is_avoid_synced(root, &from_name) || path_is_avoid_synced(root, &to_name) {
                return None;
            }
            let from_script = script_identity_from_segments(root, &from_segs_fs, from_path);
            let to_script = script_identity_from_segments(root, &segs, &op.path);
            if let (Some((from_lookup_path, _, from_class)), Some((_, to_naming_path, to_class))) =
                (from_script, to_script)
            {
                if from_class != to_class {
                    // Live watcher delivery must hydrate file renames through
                    // the stable, no-follow 32-MiB reader before this
                    // translation step. Never fall back to an unbounded
                    // path-based reread after the destructive preflight delay.
                    let source = String::from_utf8_lossy(op.content.as_deref()?).to_string();
                    return Some(json!({
                        "op": "class_change",
                        "path": from_lookup_path,
                        "to": to_naming_path,
                        "fromDiskPath": from_segs_fs,
                        "toDiskPath": segs,
                        "fromDiskFragmentIsDir": op.is_dir,
                        "toDiskFragmentIsDir": op.is_dir,
                        "class": to_class,
                        "properties": { "Source": source },
                    }));
                }
            }
            // Two cases the plugin handles with one op:
            //   (a) same-parent rename → just `Instance.Name = last(to_inst)`.
            //   (b) cross-parent move  → reparent + maybe rename.
            Some(json!({
                "op": "rename",
                "from": from_lookup,
                "to": to_naming,
                "fromDiskPath": from_segs_fs,
                "toDiskPath": segs,
                "fromDiskFragmentIsDir": op.is_dir,
                "toDiskFragmentIsDir": op.is_dir,
            }))
        }
        OpKind::Add | OpKind::Update => {
            let fname = segs.last()?.clone();
            // Canonical init files describe their parent directory. Legacy
            // reserved leaf spellings were rejected by the guard above.
            if is_init_file(&fname) && path_is_parent_init_source(&op.path).ok() == Some(true) {
                if let Some(parent_inst) = op
                    .path
                    .parent()
                    .and_then(|parent| path_to_instance_meta(parent).ok().flatten())
                    .filter(|instance| instance.is_script_with_children)
                {
                    // Translate into an update of the parent dir (Source on
                    // the script-with-children).
                    let parent_segs_fs: Vec<String> = segs[..segs.len() - 1].to_vec();
                    let inst_lookup_segs = segs_to_lookup_path(&parent_segs_fs)?;
                    let inst_naming_segs = segs_to_naming_path(&parent_segs_fs)?;
                    let inst_name_segs = segs_to_instance_path(&parent_segs_fs)?;
                    if path_is_avoid_synced(root, &inst_name_segs) {
                        return None;
                    }
                    let content = op.content.as_deref().unwrap_or(b"");
                    let source = String::from_utf8_lossy(content).to_string();
                    return Some(json!({
                        "op": "class_change",
                        "path": inst_lookup_segs,
                        "to": inst_naming_segs,
                        "diskPath": parent_segs_fs,
                        "diskFragmentIsDir": true,
                        "class": parent_inst.class,
                        "properties": { "Source": source },
                    }));
                }
            }
            // `.meta.json` is blacklisted at the watcher — if one still slips
            // through, swallow it here.
            if fname == META_FILE {
                return None;
            }

            // Regular file or directory: classify and emit `set` with a node.
            // Scripts carry their Source; non-scripts emit an empty properties
            // map (property sync is Studio-authoritative via live Studio reads).
            let inst = path_to_instance_meta(&op.path).ok().flatten()?;
            if inst.class == "Folder" && is_empty_plain_folder(&op.path).unwrap_or(false) {
                return None;
            }
            let parent_segs_fs: Vec<String> = segs[..segs.len() - 1].to_vec();
            let parent_lookup_segs = segs_to_lookup_path(&parent_segs_fs).unwrap_or_default();
            let parent_name_segs = segs_to_instance_path(&parent_segs_fs).unwrap_or_default();
            let inst_name_segs = segs_to_instance_path(&segs)?;
            if path_is_avoid_synced(root, &parent_name_segs)
                || path_is_avoid_synced(root, &inst_name_segs)
            {
                return None;
            }

            let mut props: Map<String, Value> = Map::new();
            if !inst.is_dir {
                if let Some(bytes) = &op.content {
                    let src = String::from_utf8_lossy(bytes).to_string();
                    props.insert("Source".to_string(), Value::String(src));
                }
            }
            Some(json!({
                "op": "set",
                "path": parent_lookup_segs,
                "diskPath": segs,
                "node": {
                    "class": inst.class,
                    "name": inst.name,
                    "diskFragment": fname,
                    "diskFragmentIsDir": inst.is_dir,
                    "properties": Value::Object(props),
                    "children": Value::Array(Vec::new()),
                },
            }))
        }
    }
}

fn script_identity_from_segments(
    root: &Path,
    segs: &[String],
    fs_path: &Path,
) -> Option<(Vec<String>, Vec<String>, String)> {
    let fname = segs.last()?;
    if let Some((script_class, inner_name)) = parse_init_file(fname) {
        let parent_segs = &segs[..segs.len().saturating_sub(1)];
        if crate::fs_map::named_init_describes_parent(fs_path, &inner_name) {
            if let Some(inst) = fs_path
                .parent()
                .and_then(|parent| path_to_instance_meta(parent).ok().flatten())
                .filter(|instance| instance.is_script_with_children)
            {
                let mut naming_path = segs_to_naming_path(parent_segs)?;
                if let Some(last) = naming_path.last_mut() {
                    *last = inst.name;
                }
                return Some((
                    segs_to_lookup_path(parent_segs)?,
                    naming_path,
                    script_class.class_name().to_string(),
                ));
            }
        }
    }

    if let Some((script_class, _)) = classify_script_file(fname) {
        return Some((
            segs_to_lookup_path(segs)?,
            segs_to_naming_path(segs)?,
            script_class.class_name().to_string(),
        ));
    }

    let rel = fs_path.strip_prefix(root).ok()?;
    let rel_segs: Vec<String> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(String::from))
        .collect();
    let inst = path_to_instance_meta(fs_path).ok().flatten()?;
    if inst.script_class.is_some() {
        return Some((
            segs_to_lookup_path(&rel_segs)?,
            segs_to_naming_path(&rel_segs)?,
            inst.class,
        ));
    }
    None
}

fn deleted_path_is_shadowed_ignored_folder(root: &Path, segs: &[String], path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    let Some(fname) = segs.last() else {
        return false;
    };
    if classify_script_file(fname).is_some() || is_init_file(fname) || fname == META_FILE {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(parent_rel) = parent.strip_prefix(root) else {
        return false;
    };
    if parent_rel.as_os_str().is_empty() || !parent.is_dir() {
        return false;
    }
    let instance_name = match parse_disambiguated(fname) {
        Some((name, _)) => crate::fs_map::decode_name(&name),
        None => crate::fs_map::decode_name(fname),
    };
    let Ok(Some(fragment)) = find_child_fragment_by_name(parent, &instance_name) else {
        return false;
    };
    fragment != *fname
}

fn is_synced_service_segment(segment: &str) -> bool {
    let service_name = match parse_disambiguated(segment) {
        Some((name, _)) => crate::fs_map::decode_name(&name),
        None => crate::fs_map::decode_name(segment),
    };
    snapshot::SYNCED_SERVICES
        .iter()
        .any(|service| *service == service_name)
}

fn path_is_avoid_synced(root: &Path, instance_path: &[String]) -> bool {
    if instance_path.is_empty() {
        return false;
    }
    let avoided = avoid_sync_paths(root);
    avoided
        .iter()
        .any(|path| path.len() <= instance_path.len() && path == &instance_path[..path.len()])
}

fn avoid_sync_paths(root: &Path) -> Vec<Vec<String>> {
    let cache = AVOID_SYNC_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref() {
            if cached.root == root {
                return cached.paths.clone();
            }
        }
    }
    Vec::new()
}

fn collect_avoid_sync_paths(node: &Value, parent: &[String], out: &mut Vec<Vec<String>>) {
    collect_marked_tree_paths(node, parent, out, "avoidSync", true);
}

fn collect_marked_tree_paths(
    node: &Value,
    parent: &[String],
    out: &mut Vec<Vec<String>>,
    marker: &str,
    stop_at_match: bool,
) {
    if let Some(nodes) = node.as_array() {
        for child in nodes {
            collect_marked_tree_paths(child, parent, out, marker, stop_at_match);
        }
        return;
    }

    let Some(name) = node.get("name").and_then(|v| v.as_str()) else {
        if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
            for child in children {
                collect_marked_tree_paths(child, parent, out, marker, stop_at_match);
            }
        }
        return;
    };

    let class = node.get("class").and_then(|v| v.as_str()).unwrap_or("");
    let is_data_model_root = parent.is_empty() && class == "DataModel";
    let mut path = parent.to_vec();
    if !is_data_model_root {
        path.push(name.to_string());
    }

    if node.get(marker).and_then(Value::as_bool) == Some(true) {
        out.push(path.clone());
        if stop_at_match {
            return;
        }
    }

    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for child in children {
            collect_marked_tree_paths(child, &path, out, marker, stop_at_match);
        }
    }
}

/// Translate a slice of filesystem segments (possibly disambiguated / encoded)
/// into their corresponding instance names. Returns None if any segment can't
/// be understood.
fn segs_to_instance_path(segs: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(segs.len());
    for (i, s) in segs.iter().enumerate() {
        if i == 0 {
            // Top-level is a service: name == segment (possibly disambiguated).
            out.push(match parse_disambiguated(s) {
                Some((n, _)) => crate::fs_map::decode_name(&n),
                None => crate::fs_map::decode_name(s),
            });
            continue;
        }
        // File: strip .luau variants.
        if let Some((_, stem)) = classify_script_file(s) {
            let name = match parse_disambiguated(&stem) {
                Some((n, _)) => n,
                None => stem,
            };
            out.push(crate::fs_map::decode_name(&name));
            continue;
        }
        // Directory fragment.
        let name = match parse_disambiguated(s) {
            Some((n, _)) => n,
            None => s.clone(),
        };
        out.push(crate::fs_map::decode_name(&name));
    }
    Some(out)
}

/// Convert filesystem segments to plugin lookup segments. Unlike
/// `segs_to_instance_path`, this preserves generated duplicate suffixes such as
/// `Foo [1]` so the Studio plugin can resolve the exact sibling.
fn segs_to_lookup_path(segs: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(segs.len());
    for (i, s) in segs.iter().enumerate() {
        if i == 0 {
            out.push(fs_segment_instance_name(s));
        } else {
            out.push(fs_segment_lookup_name(s));
        }
    }
    Some(out)
}

/// Convert filesystem segments to a path whose parents are lookup-safe but
/// whose final segment is the actual Roblox instance name. This is used for
/// rename/class-change destinations, where the parent may need disambiguation
/// but the final segment becomes `Instance.Name`.
fn segs_to_naming_path(segs: &[String]) -> Option<Vec<String>> {
    let mut out = segs_to_lookup_path(segs)?;
    if let (Some(last), Some(source_last)) = (out.last_mut(), segs.last()) {
        *last = fs_segment_instance_name(source_last);
    }
    Some(out)
}

fn fs_segment_lookup_name(segment: &str) -> String {
    if let Some((_, stem)) = classify_script_file(segment) {
        crate::fs_map::decode_name(&stem)
    } else {
        crate::fs_map::decode_name(segment)
    }
}

fn fs_segment_instance_name(segment: &str) -> String {
    if let Some((_, stem)) = classify_script_file(segment) {
        let name = match parse_disambiguated(&stem) {
            Some((n, _)) => n,
            None => stem,
        };
        crate::fs_map::decode_name(&name)
    } else {
        let name = match parse_disambiguated(segment) {
            Some((n, _)) => n,
            None => segment.to_string(),
        };
        crate::fs_map::decode_name(&name)
    }
}

fn fs_mtime(path: &Path) -> u64 {
    crate::fs_safety::metadata_no_follow(path)
        .ok()
        .flatten()
        .filter(|metadata| metadata.is_file())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
//
// These drive `apply_set` / `apply_delete` / `apply_rename` / `apply_move`
// directly against a scratch project root, which covers the same code path
// `/push` takes without needing an axum harness.
// ---------------------------------------------------------------------------
