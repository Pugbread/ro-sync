use super::*;

pub(super) async fn run_workflow(args: RunWorkflowArgs) -> Result<(), Box<dyn std::error::Error>> {
    let source = std::fs::read_to_string(&args.file)
        .map_err(|e| format!("run: read {}: {e}", args.file.display()))?;
    let workflow = workflow::Workflow::from_json(&source).map_err(|e| format!("run: {e}"))?;
    workflow.validate().map_err(|e| format!("run: {e}"))?;
    if args.dry_run {
        let dependencies = workflow
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let (wire_op, wire_args) = step.operation.request_parts();
                Ok(serde_json::json!({
                    "id": step.id,
                    "op": step.operation.op_name(),
                    "wire": { "op": wire_op, "args": wire_args },
                    "dependencies": workflow.dependencies_for(index)?,
                    "atomicSafe": step.operation.atomic_safe(),
                }))
            })
            .collect::<Result<Vec<_>, workflow::ResolveError>>()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "dryRun": true,
                "workflow": workflow,
                "steps": dependencies,
            }))?
        );
        return Ok(());
    }

    let project = project_or_cwd(args.project.as_deref(), "run")?;
    let workflow_hash = workflow_content_hash(&workflow)?;
    let idempotency_path = workflow
        .idempotency_key
        .as_deref()
        .map(|key| workflow_idempotency_path(&project, key));
    if let Some(path) = &idempotency_path {
        if workflow_replay_idempotency(path, &workflow_hash)? {
            return Ok(());
        }
    }
    // Serialize executions for one idempotency key. Without this lock, two
    // agents starting simultaneously can both miss the result and repeat all
    // side effects before either writes its record.
    let _idempotency_lock = idempotency_path
        .as_deref()
        .map(WorkflowIdempotencyLock::acquire)
        .transpose()?;
    if let Some(path) = &idempotency_path {
        if workflow_replay_idempotency(path, &workflow_hash)? {
            return Ok(());
        }
    }

    let mut session = remote::RemoteSession::connect(args.port)
        .await
        .map_err(|e| format!("run: {e}"))?;
    workflow_check_environment(&mut session, &workflow).await?;
    let transaction_defs = workflow
        .transactions
        .iter()
        .map(|group| (group.id.clone(), group.atomic))
        .collect::<HashMap<_, _>>();
    let mut results = workflow::StepResults::new();
    let mut reports = Vec::with_capacity(workflow.steps.len());
    let mut active_atomic: Option<String> = None;
    let mut failed = false;
    let mut rollback_errors = Vec::new();
    let mut transaction_errors = Vec::new();
    let mut transaction_outcomes = Vec::new();

    for original_step in &workflow.steps {
        let resolving_atomic = original_step
            .transaction
            .as_ref()
            .is_some_and(|id| transaction_defs.get(id).copied().unwrap_or(false));
        let step = match original_step.resolve(&results) {
            Ok(step) => step,
            Err(error) => {
                let response =
                    workflow_error_response("REFERENCE_RESOLUTION", error.to_string(), false);
                results.insert(original_step.id.clone(), response.clone());
                reports.push(workflow_step_report(original_step, &response, 0));
                failed = true;
                let failed_inside_atomic = resolving_atomic || active_atomic.is_some();
                if let Some(id) = active_atomic.take() {
                    if let Err(cancel_error) = workflow_finish_transaction_recorded(
                        &mut session,
                        &id,
                        "cancel",
                        &mut transaction_outcomes,
                    )
                    .await
                    {
                        rollback_errors.push(format!("{id}: {cancel_error}"));
                    }
                }
                // An atomic group is one unit. Continuing later members in a
                // fresh recording would commit only a suffix of that group.
                if failed_inside_atomic {
                    break;
                }
                if !args.keep_going {
                    break;
                }
                continue;
            }
        };

        let desired_atomic = step
            .transaction
            .as_ref()
            .filter(|id| transaction_defs.get(*id).copied().unwrap_or(false))
            .cloned();
        if active_atomic != desired_atomic {
            if let Some(id) = active_atomic.take() {
                if let Err(commit_error) = workflow_finish_transaction_recorded(
                    &mut session,
                    &id,
                    "commit",
                    &mut transaction_outcomes,
                )
                .await
                {
                    failed = true;
                    transaction_errors.push(format!("{id}: commit failed: {commit_error}"));
                    if let Err(cancel_error) = workflow_finish_transaction_recorded(
                        &mut session,
                        &id,
                        "cancel",
                        &mut transaction_outcomes,
                    )
                    .await
                    {
                        rollback_errors.push(format!("{id}: {cancel_error}"));
                    }
                    break;
                }
            }
            if let Some(id) = desired_atomic.clone() {
                let name = workflow
                    .name
                    .as_deref()
                    .map(|name| format!("{name}: {id}"))
                    .unwrap_or_else(|| format!("Ro Sync workflow: {id}"));
                let response = session
                    .request(
                        "transaction_begin",
                        serde_json::json!({ "id": id, "name": name }),
                        Duration::from_secs(10),
                    )
                    .await
                    .map_err(|e| format!("run: begin transaction: {e}"))?;
                response_value_or_err(&response, "run: begin transaction")?;
                active_atomic = desired_atomic;
            }
        }

        let started = Instant::now();
        let timeout = Duration::from_millis(step.timeout_ms.unwrap_or(30_000));
        let response =
            match workflow_execute_step(&mut session, args.port, &project, &step, timeout).await {
                Ok(response) => response,
                Err(error) => workflow_error_response("STEP_TRANSPORT", error.to_string(), true),
            };
        let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let step_ok = response
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        results.insert(step.id.clone(), response.clone());
        reports.push(workflow_step_report(&step, &response, duration_ms));
        if !step_ok {
            failed = true;
            if let Some(id) = active_atomic.take() {
                if let Err(cancel_error) = workflow_finish_transaction_recorded(
                    &mut session,
                    &id,
                    "cancel",
                    &mut transaction_outcomes,
                )
                .await
                {
                    rollback_errors.push(format!("{id}: {cancel_error}"));
                }
                break;
            }
            if !args.keep_going {
                break;
            }
        }
    }

    if let Some(id) = active_atomic.take() {
        // Any failure inside this recording cancels and takes the branch above.
        // A previous unrelated non-atomic failure must not roll back a fully
        // successful final transaction when --keep-going is used.
        if let Err(commit_error) = workflow_finish_transaction_recorded(
            &mut session,
            &id,
            "commit",
            &mut transaction_outcomes,
        )
        .await
        {
            failed = true;
            transaction_errors.push(format!("{id}: commit failed: {commit_error}"));
            if let Err(cancel_error) = workflow_finish_transaction_recorded(
                &mut session,
                &id,
                "cancel",
                &mut transaction_outcomes,
            )
            .await
            {
                rollback_errors.push(format!("{id}: {cancel_error}"));
            }
        }
    }
    let _ = session.close().await;
    let rolled_back = transaction_outcomes.iter().any(|outcome| {
        outcome.get("action").and_then(serde_json::Value::as_str) == Some("cancel")
            && outcome.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
    });
    let outcome = serde_json::json!({
        "ok": !failed,
        "schema": "ro-sync.workflow-result.v1",
        "name": workflow.name,
        "idempotencyKey": workflow.idempotency_key,
        "workflowHash": workflow_hash,
        "steps": reports,
        "results": results,
        "rollbackErrors": rollback_errors,
        "transactionErrors": transaction_errors,
        "transactions": transaction_outcomes,
        "rolledBack": rolled_back,
        "replayed": false,
    });
    if !failed {
        if let Some(path) = idempotency_path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("run: create {}: {e}", parent.display()))?;
            }
            write_json_atomic(&path, &outcome)
                .map_err(|e| format!("run: write idempotency record {}: {e}", path.display()))?;
        }
    }
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        for report in outcome["steps"].as_array().into_iter().flatten() {
            println!(
                "{} · {} · {}ms",
                report
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                if report.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
                    "ok"
                } else {
                    "failed"
                },
                report
                    .get("durationMs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
            );
        }
    }
    if failed {
        return Err("workflow failed; inspect the step results above".into());
    }
    Ok(())
}

pub(super) fn workflow_idempotency_path(project: &std::path::Path, key: &str) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
    project
        .join(".rosync-workflows")
        .join(format!("{digest}.json"))
}

pub(super) fn workflow_content_hash(
    workflow: &workflow::Workflow,
) -> Result<String, Box<dyn std::error::Error>> {
    use sha2::{Digest as _, Sha256};
    let normalized = serde_json::to_vec(workflow)?;
    Ok(format!("{:x}", Sha256::digest(normalized)))
}

pub(super) fn workflow_replay_idempotency(
    path: &std::path::Path,
    expected_hash: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut previous: serde_json::Value = serde_json::from_slice(
        &std::fs::read(path)
            .map_err(|e| format!("run: read idempotency record {}: {e}", path.display()))?,
    )
    .map_err(|e| format!("run: parse idempotency record {}: {e}", path.display()))?;
    let recorded_hash = previous
        .get("workflowHash")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!(
                "run: idempotency record {} predates workflow hashing; choose a new idempotencyKey or remove the stale record",
                path.display()
            )
        })?;
    if recorded_hash != expected_hash {
        return Err(format!(
            "run: idempotencyKey collision at {}: the key was already used for a different workflow",
            path.display()
        )
        .into());
    }
    let object = previous
        .as_object_mut()
        .ok_or_else(|| format!("run: invalid idempotency record {}", path.display()))?;
    object.insert("replayed".into(), serde_json::Value::Bool(true));
    println!("{}", serde_json::to_string_pretty(&previous)?);
    Ok(true)
}

pub(super) struct WorkflowIdempotencyLock {
    path: PathBuf,
}

impl WorkflowIdempotencyLock {
    pub(super) fn acquire(
        record_path: &std::path::Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let parent = record_path
            .parent()
            .ok_or("run: invalid idempotency record path")?;
        std::fs::create_dir_all(parent)?;
        let path = record_path.with_extension("lock");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "run: workflow with this idempotencyKey is already active (lock {}); remove the lock only after confirming no run is active",
                    path.display()
                )
            } else {
                format!("run: create idempotency lock {}: {error}", path.display())
            }
        })?;
        writeln!(file, "pid={}", std::process::id())?;
        file.sync_all()?;
        Ok(Self { path })
    }
}

impl Drop for WorkflowIdempotencyLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) fn write_json_atomic(
    path: &std::path::Path,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        serde_json::to_writer_pretty(&mut file, value).map_err(std::io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

pub(super) fn workflow_step_report(
    step: &workflow::WorkflowStep,
    response: &serde_json::Value,
    duration_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "id": step.id,
        "op": step.operation.op_name(),
        "transaction": step.transaction,
        "ok": response.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false),
        "durationMs": duration_ms,
        "error": response.get("error"),
    })
}

pub(super) fn workflow_error_response(
    code: &str,
    message: String,
    retryable: bool,
) -> serde_json::Value {
    serde_json::json!({
        "type": "response",
        "ok": false,
        "error": {
            "code": code,
            "message": message,
            "retryable": retryable,
        }
    })
}

pub(super) async fn workflow_finish_transaction(
    session: &mut remote::RemoteSession,
    id: &str,
    action: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = session
        .request(
            "transaction_finish",
            serde_json::json!({ "id": id, "action": action }),
            Duration::from_secs(10),
        )
        .await
        .map_err(|e| format!("run: finish transaction {id}: {e}"))?;
    response_value_or_err(&response, &format!("run: {action} transaction {id}"))?;
    Ok(())
}

pub(super) async fn workflow_finish_transaction_recorded(
    session: &mut remote::RemoteSession,
    id: &str,
    action: &str,
    outcomes: &mut Vec<serde_json::Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = workflow_finish_transaction(session, id, action).await;
    match &result {
        Ok(()) => outcomes.push(serde_json::json!({
            "id": id,
            "action": action,
            "ok": true,
        })),
        Err(error) => outcomes.push(serde_json::json!({
            "id": id,
            "action": action,
            "ok": false,
            "error": error.to_string(),
        })),
    }
    result
}

pub(super) async fn workflow_check_environment(
    session: &mut remote::RemoteSession,
    workflow: &workflow::Workflow,
) -> Result<(), Box<dyn std::error::Error>> {
    if workflow.expected_mode.is_none() && workflow.expected_place_id.is_none() {
        return Ok(());
    }
    let mut responses = session
        .request_many([remote::RemoteRequest::new(
            "capabilities",
            serde_json::json!({}),
            Duration::from_secs(10),
        )])
        .await
        .map_err(|e| format!("run: capability precondition: {e}"))?;
    let response = responses
        .pop()
        .ok_or("run: capability precondition returned no response")?;
    let capabilities = response_value_or_err(&response, "run: capability precondition")?;
    if let Some(expected_place) = &workflow.expected_place_id {
        let actual = capabilities
            .get("placeId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("0");
        if actual != expected_place {
            return Err(format!(
                "run: place precondition failed: expected {expected_place}, connected to {actual}"
            )
            .into());
        }
    }
    if let Some(mode) = workflow.expected_mode {
        use workflow::ExpectedMode;
        match mode {
            ExpectedMode::Edit => {
                let actual = capabilities
                    .get("hostDataModelType")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Unknown");
                if actual != "Edit" {
                    return Err(format!(
                        "run: mode precondition failed: expected Edit, connected to {actual}"
                    )
                    .into());
                }
            }
            ExpectedMode::PlayServer
            | ExpectedMode::PlayClient
            | ExpectedMode::Play
            | ExpectedMode::Run => {
                let status = session
                    .request(
                        "playtest_status",
                        serde_json::json!({}),
                        Duration::from_secs(10),
                    )
                    .await
                    .map_err(|e| format!("run: playtest mode precondition: {e}"))?;
                let status = response_value_or_err(&status, "run: playtest mode precondition")?;
                if status.get("active").and_then(serde_json::Value::as_bool) != Some(true) {
                    return Err("run: mode precondition failed: no active playtest".into());
                }
                if matches!(mode, ExpectedMode::PlayServer | ExpectedMode::PlayClient) {
                    let role = if matches!(mode, ExpectedMode::PlayServer) {
                        "server"
                    } else {
                        "client"
                    };
                    let found = status
                        .get("contexts")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|contexts| {
                            contexts.iter().any(|context| {
                                context.get("role").and_then(serde_json::Value::as_str)
                                    == Some(role)
                            })
                        });
                    if !found {
                        return Err(
                            format!("run: mode precondition failed: no {role} context").into()
                        );
                    }
                }
                if matches!(mode, ExpectedMode::Run) {
                    let actual = status
                        .get("job")
                        .and_then(|job| job.get("mode"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");
                    if actual != "run" {
                        return Err(format!(
                            "run: mode precondition failed: expected run, found {actual}"
                        )
                        .into());
                    }
                } else if matches!(mode, ExpectedMode::Play) {
                    let actual = status
                        .get("job")
                        .and_then(|job| job.get("mode"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");
                    if actual != "play" {
                        return Err(format!(
                            "run: mode precondition failed: expected play, found {actual}"
                        )
                        .into());
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn workflow_execute_step(
    session: &mut remote::RemoteSession,
    port: u16,
    project: &std::path::Path,
    step: &workflow::WorkflowStep,
    timeout: Duration,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("workflow step deadline overflow")?;
    if let Some(precondition_failure) = workflow_check_target_precondition(
        session,
        step,
        workflow_deadline_remaining(deadline, "target precondition")?,
    )
    .await?
    {
        return Ok(precondition_failure);
    }
    use workflow::StepOperation;
    let mut response = match &step.operation {
        StepOperation::Assert {
            actual,
            check,
            message,
        } => {
            let passed = workflow_assertion_matches(actual, check);
            if passed {
                serde_json::json!({
                    "type": "response",
                    "ok": true,
                    "value": { "passed": true, "actual": actual },
                })
            } else {
                workflow_error_response(
                    "ASSERTION_FAILED",
                    message.clone().unwrap_or_else(|| {
                        format!("assertion did not match actual value {actual}")
                    }),
                    false,
                )
            }
        }
        StepOperation::Wait {
            path,
            property,
            check,
            poll_interval_ms,
        } => {
            workflow_wait(
                session,
                path,
                property.as_deref(),
                check,
                workflow_deadline_remaining(deadline, "wait")?,
                Duration::from_millis(poll_interval_ms.unwrap_or(100)),
            )
            .await?
        }
        StepOperation::Capture { .. } => {
            workflow_capture(session, port, project, &step.operation, deadline).await?
        }
        StepOperation::Playtest { action, args } => {
            let (op, mapped_args) = workflow_playtest_request(*action, args.clone())?;
            session
                .request(
                    op,
                    mapped_args,
                    workflow_deadline_remaining(deadline, "playtest")?,
                )
                .await
                .map_err(|e| format!("playtest workflow step: {e}"))?
        }
        StepOperation::Upload {
            paths,
            asset_type,
            creator,
        } => {
            workflow_upload(
                project,
                paths,
                asset_type.as_deref(),
                creator.as_deref(),
                workflow_deadline_remaining(deadline, "upload")?,
            )
            .await?
        }
        operation => {
            let (op, args) = workflow_wire_request(operation);
            session
                .request(
                    op,
                    args,
                    workflow_deadline_remaining(deadline, operation.op_name())?,
                )
                .await
                .map_err(|e| format!("{}: {e}", operation.op_name()))?
        }
    };

    if response.get("ok").and_then(serde_json::Value::as_bool) == Some(true) && step.verify {
        match workflow_verify_step(
            session,
            step,
            &response,
            workflow_deadline_remaining(deadline, "verification")?,
        )
        .await
        {
            Ok(verification) => response["verification"] = verification,
            Err(error) => {
                return Ok(workflow_error_response(
                    "VERIFICATION_FAILED",
                    error.to_string(),
                    false,
                ));
            }
        }
    }
    Ok(response)
}

pub(super) fn workflow_deadline_remaining(
    deadline: Instant,
    phase: &str,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(format!("workflow step timed out before {phase}").into())
    } else {
        Ok(remaining)
    }
}

pub(super) fn workflow_target_path(operation: &workflow::StepOperation) -> Option<&str> {
    use workflow::StepOperation;
    match operation {
        StepOperation::Get { path, .. }
        | StepOperation::Set { path, .. }
        | StepOperation::New { path, .. }
        | StepOperation::Rm { path }
        | StepOperation::AttrSet { path, .. }
        | StepOperation::AttrRm { path, .. }
        | StepOperation::AttrLs { path }
        | StepOperation::TagAdd { path, .. }
        | StepOperation::TagRm { path, .. }
        | StepOperation::Wait { path, .. }
        | StepOperation::Call { path, .. } => Some(path),
        StepOperation::Mv { from, .. } => Some(from),
        StepOperation::Capture { path, .. } => path.as_deref(),
        StepOperation::Assert { .. }
        | StepOperation::Eval { .. }
        | StepOperation::Playtest { .. }
        | StepOperation::Upload { .. } => None,
    }
}

pub(super) async fn workflow_check_target_precondition(
    session: &mut remote::RemoteSession,
    step: &workflow::WorkflowStep,
    timeout: Duration,
) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error>> {
    if step.expected_class.is_none() && step.etag.is_none() {
        return Ok(None);
    }
    let path = workflow_target_path(&step.operation)
        .ok_or_else(|| format!("step {} has a precondition but no target path", step.id))?;
    let response = session
        .request(
            "inspect_ref",
            serde_json::json!({
                "path": path,
                "expectedClass": step.expected_class,
                "etag": step.etag,
            }),
            timeout,
        )
        .await
        .map_err(|e| format!("step {} precondition: {e}", step.id))?;
    if let Some(error) = remote::plugin_error(&response) {
        return Ok(Some(workflow_error_response(
            "PRECONDITION_FAILED",
            format!("step {} precondition: {error}", step.id),
            false,
        )));
    }
    Ok(None)
}

pub(super) fn workflow_wire_request(
    operation: &workflow::StepOperation,
) -> (&'static str, serde_json::Value) {
    use workflow::StepOperation;
    match operation {
        StepOperation::Get { path, property } => {
            ("get", serde_json::json!({ "path": path, "prop": property }))
        }
        StepOperation::Set {
            path,
            property,
            value,
        } => (
            "set",
            serde_json::json!({ "path": path, "prop": property, "value": value }),
        ),
        StepOperation::New {
            path,
            class,
            name,
            props,
        } => (
            "new",
            serde_json::json!({ "path": path, "class": class, "name": name, "props": props }),
        ),
        StepOperation::Rm { path } => ("rm", serde_json::json!({ "path": path })),
        StepOperation::Mv { from, to, force } => (
            "mv",
            serde_json::json!({ "from": from, "to": to, "force": force }),
        ),
        StepOperation::AttrSet { path, name, value } => (
            "set_attr",
            serde_json::json!({ "path": path, "name": name, "value": value }),
        ),
        StepOperation::AttrRm { path, name } => {
            ("rm_attr", serde_json::json!({ "path": path, "name": name }))
        }
        StepOperation::AttrLs { path } => ("attr_ls", serde_json::json!({ "path": path })),
        StepOperation::TagAdd { path, tag } => {
            ("add_tag", serde_json::json!({ "path": path, "tag": tag }))
        }
        StepOperation::TagRm { path, tag } => {
            ("rm_tag", serde_json::json!({ "path": path, "tag": tag }))
        }
        StepOperation::Eval { source } => ("eval", serde_json::json!({ "source": source })),
        StepOperation::Call { path, method, args } => (
            "call",
            serde_json::json!({ "path": path, "method": method, "args": args }),
        ),
        _ => unreachable!("local/special workflow operations are handled before wire mapping"),
    }
}

pub(super) fn workflow_assertion_matches(
    actual: &serde_json::Value,
    check: &workflow::Assertion,
) -> bool {
    use workflow::Assertion;
    match check {
        Assertion::Equals { expected } => actual == expected,
        Assertion::NotEquals { expected } => actual != expected,
        Assertion::Exists { expected } => (actual != &serde_json::Value::Null) == *expected,
        Assertion::Truthy { expected } => {
            let truthy = !matches!(
                actual,
                serde_json::Value::Null | serde_json::Value::Bool(false)
            );
            truthy == *expected
        }
        Assertion::Contains { expected } => match (actual, expected) {
            (serde_json::Value::String(actual), serde_json::Value::String(expected)) => {
                actual.contains(expected)
            }
            (serde_json::Value::Array(actual), expected) => actual.contains(expected),
            (serde_json::Value::Object(actual), serde_json::Value::String(key)) => {
                actual.contains_key(key)
            }
            (serde_json::Value::Object(actual), serde_json::Value::Object(expected)) => expected
                .iter()
                .all(|(key, value)| actual.get(key) == Some(value)),
            _ => false,
        },
        Assertion::GreaterThan { expected } => {
            actual.as_f64().is_some_and(|value| value > *expected)
        }
        Assertion::GreaterThanOrEqual { expected } => {
            actual.as_f64().is_some_and(|value| value >= *expected)
        }
        Assertion::LessThan { expected } => actual.as_f64().is_some_and(|value| value < *expected),
        Assertion::LessThanOrEqual { expected } => {
            actual.as_f64().is_some_and(|value| value <= *expected)
        }
    }
}

pub(super) async fn workflow_wait(
    session: &mut remote::RemoteSession,
    path: &str,
    property: Option<&str>,
    check: &workflow::Assertion,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let poll_interval = poll_interval.clamp(Duration::from_millis(10), Duration::from_secs(5));
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(workflow_error_response(
                "WAIT_TIMEOUT",
                format!(
                    "condition at {path} did not match within {:.3}s",
                    timeout.as_secs_f64()
                ),
                true,
            ));
        }
        let response = session
            .request(
                "get",
                serde_json::json!({ "path": path, "prop": property }),
                remaining.min(Duration::from_secs(10)),
            )
            .await
            .map_err(|e| format!("wait at {path}: {e}"))?;
        let actual = if response.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            response
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        } else if remote::plugin_error(&response)
            .and_then(|error| error.code)
            .as_deref()
            == Some("NOT_FOUND")
        {
            // A missing target is the only failure that is meaningfully null;
            // this permits `exists:false` waits without treating permissions,
            // invalid properties, or plugin faults as a satisfied condition.
            serde_json::Value::Null
        } else {
            return Err(remote::plugin_error(&response)
                .map(|error| format!("wait at {path}: {error}"))
                .unwrap_or_else(|| format!("wait at {path}: request failed"))
                .into());
        };
        if workflow_assertion_matches(&actual, check) {
            return Ok(serde_json::json!({
                "type": "response",
                "ok": true,
                "value": {
                    "matched": true,
                    "actual": actual,
                    "attempts": attempts,
                    "elapsedMs": started.elapsed().as_millis(),
                }
            }));
        }
        tokio::time::sleep(poll_interval.min(remaining)).await;
    }
}

pub(super) async fn workflow_verify_step(
    session: &mut remote::RemoteSession,
    step: &workflow::WorkflowStep,
    response: &serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use workflow::StepOperation;
    let value = response.get("value").unwrap_or(&serde_json::Value::Null);
    match &step.operation {
        StepOperation::Set {
            path,
            property,
            value: expected,
        } => {
            let read = session
                .request(
                    "inspect_ref",
                    serde_json::json!({ "path": path, "prop": property }),
                    timeout,
                )
                .await?;
            let inspected = response_value_or_err(&read, "verify set")?;
            if inspected.get("value") != Some(expected) {
                return Err(format!(
                    "set readback mismatch at {path}.{property}: expected {expected}, found {}",
                    inspected.get("value").unwrap_or(&serde_json::Value::Null)
                )
                .into());
            }
            Ok(serde_json::json!({ "verified": true, "ref": inspected }))
        }
        StepOperation::New {
            class, name, props, ..
        } => {
            let path = value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or("new response omitted path")?;
            let read = session
                .request(
                    "inspect_ref",
                    serde_json::json!({
                        "path": path,
                        "expectedClass": class,
                        "props": props.keys().collect::<Vec<_>>(),
                    }),
                    timeout,
                )
                .await?;
            let inspected = response_value_or_err(&read, "verify new")?;
            if inspected.get("name").and_then(serde_json::Value::as_str) != Some(name.as_str()) {
                return Err(format!("new instance name mismatch at {path}").into());
            }
            let values = inspected
                .get("values")
                .and_then(serde_json::Value::as_object);
            if !props.is_empty() && values.is_none() {
                return Err("new verification omitted property values".into());
            }
            for (property, expected) in props {
                if values.and_then(|values| values.get(property)) != Some(expected) {
                    return Err(format!(
						"new property readback mismatch at {path}.{property}: expected {expected}, found {}",
						values
							.and_then(|values| values.get(property))
							.unwrap_or(&serde_json::Value::Null)
					)
                    .into());
                }
            }
            Ok(serde_json::json!({ "verified": true, "ref": inspected }))
        }
        StepOperation::Rm { path } => {
            let read = session
                .request("get", serde_json::json!({ "path": path }), timeout)
                .await?;
            if read.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
                return Err(format!("removed instance still exists at {path}").into());
            }
            let error =
                remote::plugin_error(&read).ok_or("remove verification failed without an error")?;
            if error.code.as_deref() != Some("NOT_FOUND") {
                return Err(format!("remove verification at {path}: {error}").into());
            }
            Ok(serde_json::json!({ "verified": true, "absent": path }))
        }
        StepOperation::Mv { .. } => {
            let path = value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or("mv response omitted path")?;
            let read = session
                .request("inspect_ref", serde_json::json!({ "path": path }), timeout)
                .await?;
            let inspected = response_value_or_err(&read, "verify mv")?;
            Ok(serde_json::json!({ "verified": true, "ref": inspected }))
        }
        StepOperation::AttrSet {
            path,
            name,
            value: expected,
        } => {
            let read = session
                .request("attr_ls", serde_json::json!({ "path": path }), timeout)
                .await?;
            let attributes = response_value_or_err(&read, "verify attr-set")?;
            if attributes.get(name) != Some(expected) {
                return Err(format!("attribute readback mismatch at {path}.{name}").into());
            }
            Ok(serde_json::json!({ "verified": true, "attributes": attributes }))
        }
        StepOperation::AttrRm { path, name } => {
            let read = session
                .request("attr_ls", serde_json::json!({ "path": path }), timeout)
                .await?;
            let attributes = response_value_or_err(&read, "verify attr-rm")?;
            if attributes.get(name).is_some() {
                return Err(format!("attribute {name} still exists at {path}").into());
            }
            Ok(serde_json::json!({ "verified": true, "attributes": attributes }))
        }
        StepOperation::TagAdd { path, tag } | StepOperation::TagRm { path, tag } => {
            let read = session
                .request("tag_ls", serde_json::json!({ "path": path }), timeout)
                .await?;
            let tags = response_value_or_err(&read, "verify tag")?;
            let has_tag = tags
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(tag)));
            let expected = matches!(step.operation, StepOperation::TagAdd { .. });
            if has_tag != expected {
                return Err(format!("tag {tag} verification mismatch at {path}").into());
            }
            Ok(serde_json::json!({ "verified": true, "tags": tags }))
        }
        _ => Ok(serde_json::json!({ "verified": true })),
    }
}

pub(super) fn workflow_playtest_request(
    action: workflow::PlaytestAction,
    mut args: serde_json::Value,
) -> Result<(&'static str, serde_json::Value), Box<dyn std::error::Error>> {
    use workflow::PlaytestAction;
    if !args.is_object() && !args.is_null() {
        return Err("playtest workflow args must be an object".into());
    }
    if args.is_null() {
        args = serde_json::json!({});
    }
    match action {
        PlaytestAction::Start => Ok(("playtest_start", args)),
        PlaytestAction::Stop => Ok(("playtest_stop", args)),
        PlaytestAction::Status => Ok(("playtest_status", args)),
        PlaytestAction::Contexts => Ok(("playtest_contexts", args)),
        PlaytestAction::Wait => Ok(("playtest_wait", args)),
        PlaytestAction::Exec => {
            let object = args
                .as_object_mut()
                .ok_or("playtest exec args must be an object")?;
            let context = object
                .remove("context")
                .ok_or("playtest exec args require context")?;
            let timeout = object
                .get("timeout")
                .cloned()
                .unwrap_or(serde_json::json!(30));
            Ok((
                "playtest_request",
                serde_json::json!({
                    "context": context,
                    "op": "exec",
                    "args": serde_json::Value::Object(object.clone()),
                    "timeout": timeout,
                }),
            ))
        }
        PlaytestAction::Logs => {
            let object = args
                .as_object_mut()
                .ok_or("playtest logs args must be an object")?;
            let context = object
                .remove("context")
                .ok_or("playtest logs args require context")?;
            Ok((
                "playtest_request",
                serde_json::json!({
                    "context": context,
                    "op": "logs",
                    "args": serde_json::Value::Object(object.clone()),
                }),
            ))
        }
        PlaytestAction::Ui | PlaytestAction::Input => {
            let object = args
                .as_object_mut()
                .ok_or("playtest runtime args must be an object")?;
            let context = object
                .remove("context")
                .ok_or("playtest runtime args require context")?;
            let timeout = object.remove("timeout").unwrap_or(serde_json::json!(30));
            let operation = if matches!(action, PlaytestAction::Ui) {
                "ui_tree"
            } else {
                "input"
            };
            Ok((
                "playtest_request",
                serde_json::json!({
                    "context": context,
                    "op": operation,
                    "args": serde_json::Value::Object(object.clone()),
                    "timeout": timeout,
                }),
            ))
        }
        PlaytestAction::Capture => Ok(("playtest_capture", args)),
        PlaytestAction::Request => Ok(("playtest_request", args)),
    }
}

pub(super) async fn workflow_capture(
    session: &mut remote::RemoteSession,
    port: u16,
    project: &std::path::Path,
    operation: &workflow::StepOperation,
    deadline: Instant,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use workflow::{CaptureTarget, CaptureUi, StepOperation};
    let StepOperation::Capture {
        target,
        path,
        context,
        region,
        output_size,
        ui,
        output,
        ..
    } = operation
    else {
        unreachable!()
    };
    let effective_ui = match (target, ui) {
        (CaptureTarget::Scene | CaptureTarget::Viewport, _)
        | (_, CaptureUi::None | CaptureUi::Game) => "none",
        _ => "all",
    };
    let mut options = serde_json::Map::new();
    options.insert("ui".into(), serde_json::Value::String(effective_ui.into()));
    if let Some(path) = path {
        options.insert("focus".into(), serde_json::Value::String(path.clone()));
        options.insert("view".into(), serde_json::Value::String("isometric".into()));
    }
    if let Some(region) = region {
        options.insert(
            "position".into(),
            serde_json::json!({ "x": region.x, "y": region.y }),
        );
        options.insert(
            "captureSize".into(),
            serde_json::json!({ "x": region.width, "y": region.height }),
        );
    }
    if let Some(size) = output_size {
        options.insert(
            "outputSize".into(),
            serde_json::json!({ "x": size.width, "y": size.height }),
        );
    }
    let filename = output
        .as_deref()
        .and_then(|path| std::path::Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("workflow-capture.png");
    let destination = output.as_deref().map(|output| {
        if std::path::Path::new(output).is_absolute() {
            PathBuf::from(output)
        } else {
            project.join(output)
        }
    });
    let work_deadline = capture_work_deadline(deadline);
    let mut edit_session_id: Option<String> = None;
    let mut edit_lease: Option<(String, String)> = None;
    let mut runtime_artifact_id: Option<String> = None;

    enum CaptureFlow {
        Response(serde_json::Value),
        Artifact {
            response: serde_json::Value,
            id: String,
            expected_size: u64,
            dimensions: (u32, u32),
            nested_artifact: bool,
        },
    }

    let flow: Result<CaptureFlow, Box<dyn std::error::Error>> = async {
        if let Some(context) = context {
            let timeout = workflow_deadline_remaining(work_deadline, "runtime capture")?;
            let response = session
                .request(
                    "playtest_capture",
                    serde_json::json!({
                        "context": context,
                        "options": options,
                        "filename": filename,
                        "timeout": timeout.as_secs_f64(),
                    }),
                    timeout,
                )
                .await?;
            if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                return Ok(CaptureFlow::Response(response));
            }
            let value = response_value_or_err(&response, "workflow runtime capture")?;
            let artifact = value
                .get("artifact")
                .ok_or("workflow runtime capture omitted artifact")?;
            let id = plugin_artifact_id(artifact, "workflow runtime capture")?.to_string();
            runtime_artifact_id = Some(id.clone());
            let capture = value
                .get("capture")
                .ok_or("workflow runtime capture omitted capture metadata")?;
            let expected_size = capture
                .get("byteLength")
                .and_then(serde_json::Value::as_u64)
                .ok_or("workflow runtime capture omitted byteLength")?;
            let width = capture
                .get("width")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or("workflow runtime capture omitted valid width")?;
            let height = capture
                .get("height")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or("workflow runtime capture omitted valid height")?;
            validate_capture_dimensions(width, height)?;
            if expected_size == 0 || expected_size > CAPTURE_MAX_ARTIFACT_BYTES {
                return Err(format!(
                    "workflow runtime capture reported invalid size {expected_size}"
                )
                .into());
            }
            return Ok(CaptureFlow::Artifact {
                response,
                id,
                expected_size,
                dimensions: (width, height),
                nested_artifact: true,
            });
        }

        let prepare_timeout = workflow_deadline_remaining(work_deadline, "capture prepare")?;
        let mut prepare_options = options;
        prepare_options.insert(
            "timeoutSeconds".into(),
            serde_json::json!(prepare_timeout.as_secs_f64()),
        );
        let prepared_response = session
            .request(
                "capture_prepare",
                serde_json::Value::Object(prepare_options),
                prepare_timeout,
            )
            .await?;
        let prepared_value = response_value_or_err(&prepared_response, "workflow capture prepare")?;
        edit_session_id = prepared_value
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let prepared: CapturePrepared = serde_json::from_value(prepared_value)
            .map_err(|error| format!("workflow capture prepare metadata: {error}"))?;
        validate_capture_dimensions(prepared.width, prepared.height)?;
        let expected_size = u64::try_from(prepared.byte_length)
            .map_err(|_| "workflow capture byteLength does not fit u64")?;
        if expected_size == 0 || expected_size > CAPTURE_MAX_ARTIFACT_BYTES {
            return Err(format!("workflow capture reported invalid size {expected_size}").into());
        }
        edit_session_id = Some(prepared.session_id.clone());
        let lease_response = http_post_json_until(
            port,
            "/artifacts/lease",
            &serde_json::json!({
                "filename": filename,
                "mime": "image/png",
                "expectedSize": expected_size,
            }),
            work_deadline,
        )
        .await?;
        if lease_response
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(format!("workflow capture lease rejected: {lease_response}").into());
        }
        let lease = lease_response
            .get("lease")
            .cloned()
            .ok_or("workflow capture lease response omitted lease")?;
        let id = plugin_artifact_id(&lease, "workflow capture lease")?.to_string();
        let token = lease
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or("workflow capture lease omitted token")?
            .to_string();
        edit_lease = Some((id.clone(), token));
        let export_timeout = workflow_deadline_remaining(work_deadline, "capture export")?;
        let export = session
            .request(
                "capture_export",
                serde_json::json!({
                    "sessionId": prepared.session_id,
                    "lease": lease,
                    "timeoutSeconds": export_timeout.as_secs_f64(),
                }),
                export_timeout,
            )
            .await?;
        if export.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Ok(CaptureFlow::Response(export));
        }
        let plugin_artifact = response_value_or_err(&export, "workflow capture export")?;
        let returned_id = plugin_artifact_id(&plugin_artifact, "workflow capture export")?;
        if returned_id != id {
            return Err(format!(
                "workflow capture export returned artifact {returned_id}, expected {id}"
            )
            .into());
        }
        Ok(CaptureFlow::Artifact {
            response: export,
            id,
            expected_size,
            dimensions: (prepared.width, prepared.height),
            nested_artifact: false,
        })
    }
    .await;

    if let Some(session_id) = &edit_session_id {
        if let Ok(remaining) = workflow_deadline_remaining(deadline, "capture close") {
            let _ = session
                .request(
                    "capture_close",
                    serde_json::json!({ "sessionId": session_id }),
                    remaining,
                )
                .await;
        }
    }
    let flow = match flow {
        Ok(flow) => flow,
        Err(error) => {
            if let Some((id, token)) = &edit_lease {
                cleanup_artifact_lease_until(port, id, token, deadline).await;
            }
            if let Some(id) = &runtime_artifact_id {
                let _ = consume_artifact_transport_until(port, id, deadline).await;
            }
            return Err(error);
        }
    };
    let (mut response, id, expected_size, dimensions, nested_artifact) = match flow {
        CaptureFlow::Artifact {
            response,
            id,
            expected_size,
            dimensions,
            nested_artifact,
        } => (response, id, expected_size, dimensions, nested_artifact),
        CaptureFlow::Response(response) => {
            if let Some((id, token)) = &edit_lease {
                cleanup_artifact_lease_until(port, id, token, deadline).await;
            }
            return Ok(response);
        }
    };

    let materialized = match materialize_capture_artifact(
        port,
        &id,
        Some(expected_size),
        Some(dimensions),
        destination.as_deref(),
        deadline,
        "workflow capture",
    )
    .await
    {
        Ok(materialized) => materialized,
        Err(error) => {
            if let Some((id, token)) = &edit_lease {
                cleanup_artifact_lease_until(port, id, token, deadline).await;
            }
            return Err(error);
        }
    };
    let durable_path = materialized
        .output_path
        .as_ref()
        .map(|path| path.display().to_string());
    let transport_metadata = materialized.metadata.clone();
    let sanitized = serde_json::json!({
        "id": materialized.metadata.id,
        "filename": materialized.metadata.filename,
        "mime": "image/png",
        "path": durable_path,
        "size": materialized.size,
        "sha256": materialized.sha256,
        "transport": {
            "metadata": transport_metadata,
            "consumed": materialized.consumed,
        },
    });
    if nested_artifact {
        response["value"]["artifact"] = sanitized;
    } else {
        response["value"] = sanitized;
    }
    if let Some(output_path) = materialized.output_path {
        response["outputPath"] = serde_json::Value::String(output_path.display().to_string());
    }
    Ok(response)
}

pub(super) async fn workflow_upload(
    project: &std::path::Path,
    paths: &[String],
    asset_type: Option<&str>,
    creator: Option<&str>,
    timeout: Duration,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe().map_err(|e| format!("workflow upload: {e}"))?;
    let mut command = tokio::process::Command::new(executable);
    command.kill_on_drop(true);
    command.arg("upload");
    for path in paths {
        command.arg(if std::path::Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            project.join(path)
        });
    }
    command.arg("--project").arg(project).arg("--raw");
    if let Some(asset_type) = asset_type {
        command
            .arg("--asset-type")
            .arg(asset_type.to_ascii_lowercase());
    }
    if let Some(creator) = creator {
        command.arg("--creator").arg(creator);
    }
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| {
            format!(
                "workflow upload timed out after {:.3}s",
                timeout.as_secs_f64()
            )
        })??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = serde_json::from_str::<serde_json::Value>(stdout.trim()).unwrap_or_else(|_| {
        serde_json::json!({
            "stdout": stdout,
            "stderr": String::from_utf8_lossy(&output.stderr),
        })
    });
    if output.status.success() {
        Ok(serde_json::json!({ "type": "response", "ok": true, "value": parsed }))
    } else {
        Ok(workflow_error_response(
            "UPLOAD_FAILED",
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stderr),
                if stdout.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n{}", stdout.trim())
                }
            ),
            true,
        ))
    }
}
