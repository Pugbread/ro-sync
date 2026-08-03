use clap::{Args as ClapArgs, ValueEnum};
use serde_json::{json, Map, Value};
use sha2::{Digest as _, Sha256};
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::remote;
use crate::{PlaytestMode, RuntimeIdentity, DEFAULT_DAEMON_PORT};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const START_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const START_MAX_ATTEMPTS: usize = 2;
const POLL_WAIT_SECONDS: f64 = 2.0;
const POLL_REQUEST_GRACE: Duration = Duration::from_secs(4);
const POLL_MAX_EVENTS: u64 = 64;
const POLL_MAX_BYTES: u64 = 512 * 1024;
// Cleanup is allowed a small, shared grace period after the hard session
// deadline. Individual requests must never each restart this budget.
const CLEANUP_BUDGET: Duration = Duration::from_secs(3);
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(100);
const TRANSPORT_VERIFY_BUDGET: Duration = Duration::from_secs(5);
const HEARTBEAT_STALE_SECONDS: f64 = 6.0;
const HEARTBEAT_STATUS_RECHECK: Duration = Duration::from_secs(5);

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaytestRunLogs {
    Off,
    Info,
    Warn,
    Error,
}

impl PlaytestRunLogs {
    fn as_plugin_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestRunArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Main playscript. Its first completion ends the session.
    #[arg(long)]
    pub script: PathBuf,
    /// Runtime context for the main playscript: server or client:N.
    #[arg(long, default_value = "server")]
    pub context: String,
    /// Companion playscript injected into every PlayClient context.
    #[arg(long = "client-script")]
    pub client_script: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = PlaytestMode::Play)]
    pub mode: PlaytestMode,
    /// Number of PlayClients in multiplayer mode (1-8).
    #[arg(long, default_value_t = 1)]
    pub players: u8,
    /// JSON value exposed to playscripts as playtest.args.
    #[arg(long = "args", default_value = "{}")]
    pub script_args: String,
    /// Hard wall-clock deadline for the complete session, in seconds (max 3600).
    #[arg(long, default_value_t = 600.0)]
    pub timeout: f64,
    /// Game uses a temporary Script/LocalScript; plugin uses plugin identity.
    #[arg(long, value_enum, default_value_t = RuntimeIdentity::Game)]
    pub identity: RuntimeIdentity,
    /// Interleave Studio output from all runtime contexts.
    #[arg(long, value_enum, default_value_t = PlaytestRunLogs::Off)]
    pub logs: PlaytestRunLogs,
    /// Print the terminal result without stopping the underlying playtest.
    #[arg(long)]
    pub keep_open: bool,
    /// Suppress progress frames and print only the terminal result.
    #[arg(long)]
    pub quiet: bool,
    /// Print compact NDJSON, one independently parseable object per line.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Debug)]
pub struct PlaytestRunExit {
    code: i32,
}

impl PlaytestRunExit {
    fn new(code: i32) -> Self {
        debug_assert!(matches!(code, 2..=5));
        Self { code }
    }

    pub fn code(&self) -> i32 {
        self.code
    }
}

impl fmt::Display for PlaytestRunExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "playtest run exited with status {}", self.code)
    }
}

impl std::error::Error for PlaytestRunExit {}

#[derive(Clone, Debug)]
struct ScriptFile {
    path: String,
    source: String,
    sha256: String,
}

#[derive(Debug)]
struct PreparedRun {
    script: ScriptFile,
    client_script: Option<ScriptFile>,
    script_args: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutcomeKind {
    Success,
    Failure,
    Timeout,
    Aborted,
    BootFailure,
}

impl OutcomeKind {
    fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failure => 2,
            Self::Timeout => 3,
            Self::Aborted => 4,
            Self::BootFailure => 5,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
            Self::BootFailure => "bootFailure",
        }
    }
}

#[derive(Clone, Debug)]
struct RunOutcome {
    kind: OutcomeKind,
    elapsed: f64,
    value: Option<Value>,
    error: Option<String>,
    traceback: Option<String>,
    job_status: String,
    job_id: Option<String>,
    kept_open: bool,
}

impl RunOutcome {
    fn terminal_value(&self) -> Value {
        if self.kind == OutcomeKind::Aborted {
            let mut object = Map::new();
            object.insert("type".into(), Value::String("aborted".into()));
            object.insert(
                "reason".into(),
                Value::String(
                    self.error
                        .clone()
                        .unwrap_or_else(|| "job ended externally".into()),
                ),
            );
            object.insert("jobStatus".into(), Value::String(self.job_status.clone()));
            object.insert("elapsed".into(), json!(self.elapsed));
            object.insert("exitCode".into(), json!(self.kind.exit_code()));
            if let Some(traceback) = &self.traceback {
                object.insert("traceback".into(), Value::String(traceback.clone()));
            }
            if self.kept_open {
                object.insert("keptOpen".into(), Value::Bool(true));
                if let Some(job_id) = &self.job_id {
                    object.insert("jobId".into(), Value::String(job_id.clone()));
                }
            }
            return Value::Object(object);
        }

        let mut object = Map::new();
        object.insert("type".into(), Value::String("result".into()));
        object.insert("ok".into(), Value::Bool(self.kind == OutcomeKind::Success));
        object.insert("kind".into(), Value::String(self.kind.as_str().into()));
        object.insert("exitCode".into(), json!(self.kind.exit_code()));
        object.insert("elapsed".into(), json!(self.elapsed));
        object.insert("jobStatus".into(), Value::String(self.job_status.clone()));
        if let Some(value) = &self.value {
            object.insert("value".into(), value.clone());
        } else if self.kind == OutcomeKind::Success {
            object.insert("value".into(), Value::Null);
        }
        if let Some(error) = &self.error {
            object.insert("error".into(), Value::String(error.clone()));
        }
        if let Some(traceback) = &self.traceback {
            object.insert("traceback".into(), Value::String(traceback.clone()));
        }
        if self.kept_open {
            object.insert("keptOpen".into(), Value::Bool(true));
            if let Some(job_id) = &self.job_id {
                object.insert("jobId".into(), Value::String(job_id.clone()));
            }
        }
        Value::Object(object)
    }
}

struct RunOutput {
    raw: bool,
    quiet: bool,
}

impl RunOutput {
    fn progress(&self, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
        if self.quiet {
            return Ok(());
        }
        if self.raw {
            write_stdout_line(&compact_json_line(value)?)?;
        } else {
            write_human_progress(value)?;
        }
        Ok(())
    }

    fn terminal(&self, outcome: &RunOutcome) -> Result<(), Box<dyn std::error::Error>> {
        let value = outcome.terminal_value();
        if self.raw {
            write_stdout_line(&compact_json_line(&value)?)?;
            return Ok(());
        }

        match outcome.kind {
            OutcomeKind::Success => {
                write_stdout_line(&format!("✔ result ({:.1}s):", outcome.elapsed))?;
                let rendered =
                    serde_json::to_string_pretty(outcome.value.as_ref().unwrap_or(&Value::Null))?;
                write_stdout_line(&rendered)?;
            }
            OutcomeKind::Aborted => {
                write_stdout_line(&format!(
                    "✖ aborted ({:.1}s): {} (job: {})",
                    outcome.elapsed,
                    outcome.error.as_deref().unwrap_or("job ended externally"),
                    outcome.job_status
                ))?;
            }
            _ => {
                write_stdout_line(&format!(
                    "✖ {} ({:.1}s): {} (job: {})",
                    outcome.kind.as_str(),
                    outcome.elapsed,
                    outcome.error.as_deref().unwrap_or("playtest run failed"),
                    outcome.job_status
                ))?;
                if let Some(traceback) = &outcome.traceback {
                    write_stdout_line(traceback)?;
                }
            }
        }
        if outcome.kept_open {
            write_stdout_line(&format!(
                "playtest left running (job {})",
                outcome.job_id.as_deref().unwrap_or("unknown")
            ))?;
        }
        Ok(())
    }
}

pub async fn run(mut args: PlaytestRunArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Everything below this line is allowed to contact Studio. Keep all parsing,
    // file reads, hashing, and cross-field validation in this preflight first.
    let prepared = prepare_run(&args)?;
    let command_started = Instant::now();
    let deadline = command_started + Duration::from_secs_f64(args.timeout);
    crate::resolve_port_field(&mut args.port, args.project.as_deref(), "playtest run")?;
    let output = RunOutput {
        raw: args.raw,
        quiet: args.quiet,
    };
    let client_run_id = crate::artifact::random_hex(16)?;
    let start_payload = start_request(&args, &prepared, &client_run_id);
    let (mut session, start_value) = match start_owned_run(
        args.port,
        &start_payload,
        &client_run_id,
        deadline,
        command_started,
    )
    .await
    {
        Ok(started) => started,
        Err(outcome) => return finish(&output, outcome),
    };
    let Some(job_id) = job_id(&start_value) else {
        let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
        let cancel = cancel_fresh(
            args.port,
            None,
            &client_run_id,
            "start response omitted its job id",
            true,
            cleanup_deadline,
        )
        .await;
        let mut status = job_status(&start_value);
        let cancel_status = cancel.observed_status();
        if cancel_status != "unavailable" {
            status = cancel_status;
        }
        if let Some(value) = cancel.canonical_value() {
            let observed = job_status(value);
            if observed != "unavailable" {
                status = observed;
            }
            if let Some(stop_error) = terminal_stop_error(value) {
                return finish(
                    &output,
                    RunOutcome {
                        kind: OutcomeKind::Aborted,
                        elapsed: command_started.elapsed().as_secs_f64(),
                        value: None,
                        error: Some(format!("playtest start cleanup failed: {stop_error}")),
                        traceback: None,
                        job_status: status,
                        job_id: job_id(value),
                        kept_open: false,
                    },
                );
            }
            if !is_start_cleanup_terminal(value) {
                let recovered_job_id = job_id(value).unwrap_or_else(|| client_run_id.to_owned());
                if let Some(outcome) = received_outcome(
                    value,
                    command_started.elapsed().as_secs_f64(),
                    &recovered_job_id,
                    false,
                ) {
                    return finish(&output, outcome);
                }
            }
        }
        if cancel.teardown_failed() || (cancel.failed() && cancel.response.is_none()) {
            return finish(
                &output,
                RunOutcome {
                    kind: OutcomeKind::Aborted,
                    elapsed: command_started.elapsed().as_secs_f64(),
                    value: None,
                    error: Some(
                        cancel.error_message("playtest start cleanup could not be confirmed"),
                    ),
                    traceback: None,
                    job_status: status,
                    job_id: None,
                    kept_open: false,
                },
            );
        }
        return finish(
            &output,
            RunOutcome {
                kind: OutcomeKind::BootFailure,
                elapsed: command_started.elapsed().as_secs_f64(),
                value: None,
                error: Some("playtest run start response omitted its job id".into()),
                traceback: None,
                job_status: status,
                job_id: None,
                kept_open: false,
            },
        );
    };

    if let Err(error) = output.progress(&json!({
        "type": "started",
        "jobId": job_id,
        "mode": args.mode.as_plugin_str(),
        "timeout": args.timeout,
    })) {
        let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
        let _ = cancel_with_session(
            &mut session,
            &job_id,
            &client_run_id,
            "output stream closed",
            true,
            cleanup_deadline,
        )
        .await;
        return Err(error);
    }

    let mut after_seq = 0_u64;
    let mut pending_outcome: Option<RunOutcome> = None;
    let mut last_heartbeat_status_check: Option<Instant> = None;
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());

    loop {
        let remaining = remaining_duration(deadline);
        if remaining.is_zero() {
            return timeout_run(
                &output,
                args.port,
                &job_id,
                &client_run_id,
                args.timeout,
                args.keep_open,
                command_started,
            )
            .await;
        }

        enum PollResult {
            Response(Result<Value, String>),
            Deadline,
            Interrupted,
        }

        let wait_seconds = POLL_WAIT_SECONDS.min(remaining.as_secs_f64());
        let poll_result = {
            let poll_request = session.request(
                "playtest_run_poll",
                json!({
                    "jobId": job_id,
                    "afterSeq": after_seq,
                    "waitSeconds": wait_seconds,
                    "maxEvents": POLL_MAX_EVENTS,
                    "maxBytes": POLL_MAX_BYTES,
                }),
                Duration::from_secs_f64(wait_seconds) + POLL_REQUEST_GRACE,
            );
            tokio::pin!(poll_request);
            let deadline_wait = tokio::time::sleep(remaining);
            tokio::pin!(deadline_wait);
            tokio::select! {
                response = &mut poll_request => PollResult::Response(response),
                _ = &mut deadline_wait => PollResult::Deadline,
                _ = &mut ctrl_c => PollResult::Interrupted,
            }
        };

        let response = match poll_result {
            PollResult::Deadline => {
                return timeout_run(
                    &output,
                    args.port,
                    &job_id,
                    &client_run_id,
                    args.timeout,
                    args.keep_open,
                    command_started,
                )
                .await
            }
            PollResult::Interrupted => {
                let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
                let cancel = cancel_with_session(
                    &mut session,
                    &job_id,
                    &client_run_id,
                    "interrupted",
                    true,
                    cleanup_deadline,
                )
                .await;
                if let Some(outcome) = cancel.canonical_value().and_then(|value| {
                    received_outcome(
                        value,
                        command_started.elapsed().as_secs_f64(),
                        &job_id,
                        false,
                    )
                }) {
                    return finish(&output, outcome);
                }
                let status = cancel.observed_status();
                let status =
                    if status == "unavailable" && !remaining_duration(cleanup_deadline).is_zero() {
                        fetch_job_observation(args.port, &job_id, cleanup_deadline)
                            .await
                            .status
                    } else {
                        status
                    };
                return finish(
                    &output,
                    RunOutcome {
                        kind: OutcomeKind::Aborted,
                        elapsed: command_started.elapsed().as_secs_f64(),
                        value: None,
                        error: Some("interrupted by user".into()),
                        traceback: None,
                        job_status: status,
                        job_id: Some(job_id.clone()),
                        kept_open: false,
                    },
                );
            }
            PollResult::Response(response) => response,
        };
        let poll_value = match response.and_then(|response| {
            crate::response_value_or_err(&response, "playtest run poll")
                .map_err(|error| error.to_string())
        }) {
            Ok(value) => value,
            Err(error) => match recover_transport(
                args.port,
                &job_id,
                args.keep_open,
                deadline,
                command_started,
            )
            .await
            {
                TransportRecovery::Reconnected(reconnected) => {
                    session = reconnected;
                    continue;
                }
                TransportRecovery::Terminal(outcome) => return finish(&output, outcome),
                TransportRecovery::Deadline(_observation) => {
                    return timeout_run(
                        &output,
                        args.port,
                        &job_id,
                        &client_run_id,
                        args.timeout,
                        args.keep_open,
                        command_started,
                    )
                    .await;
                }
                TransportRecovery::Lost(observation) => {
                    let outcome = transport_abort_outcome(
                        args.port,
                        &job_id,
                        &client_run_id,
                        &error,
                        args.keep_open,
                        command_started,
                        observation,
                    )
                    .await;
                    return finish(&output, outcome);
                }
                TransportRecovery::Unverified(observation) => {
                    let outcome = transport_abort_outcome(
                        args.port,
                        &job_id,
                        &client_run_id,
                        &format!(
                            "{error}; job status remained unverified for {:.0}s",
                            TRANSPORT_VERIFY_BUDGET.as_secs_f64()
                        ),
                        args.keep_open,
                        command_started,
                        observation,
                    )
                    .await;
                    return finish(&output, outcome);
                }
            },
        };

        if let Some(frames) = poll_frames(&poll_value) {
            for frame in frames {
                after_seq = delivered_cursor(after_seq, frame);
                // Terminal rendering is centralized below, so a plugin alias that
                // also includes its terminal record in `frames` cannot duplicate it.
                if !is_terminal_frame(frame) {
                    if let Err(error) = output.progress(frame) {
                        let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
                        let _ = cancel_with_session(
                            &mut session,
                            &job_id,
                            &client_run_id,
                            "output stream closed",
                            true,
                            cleanup_deadline,
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
        }

        if let Some(outcome) = received_outcome(
            &poll_value,
            command_started.elapsed().as_secs_f64(),
            &job_id,
            args.keep_open,
        ) {
            pending_outcome = Some(outcome);
        }

        if pending_outcome.is_none()
            && heartbeat_age_seconds(&poll_value, &args.context)
                .is_some_and(|age| age >= HEARTBEAT_STALE_SECONDS)
            && last_heartbeat_status_check
                .is_none_or(|checked| checked.elapsed() >= HEARTBEAT_STATUS_RECHECK)
        {
            last_heartbeat_status_check = Some(Instant::now());
            let observation = fetch_job_observation(args.port, &job_id, deadline).await;
            if let Some(outcome) = observation.payload.as_ref().and_then(|value| {
                received_outcome(
                    value,
                    command_started.elapsed().as_secs_f64(),
                    &job_id,
                    args.keep_open,
                )
            }) {
                return finish(&output, outcome);
            }
            // A stale heartbeat alone never ends a run: a wedged playscript is
            // still owned by the hard timeout. Abort only after the low-level
            // job query positively says the playtest itself is no longer active.
            if observation.confirmed_inactive() {
                let mut status = observation.status;
                let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
                let cancel = cancel_with_session(
                    &mut session,
                    &job_id,
                    &client_run_id,
                    "main heartbeat stopped",
                    true,
                    cleanup_deadline,
                )
                .await;
                if let Some(value) = cancel.canonical_value() {
                    if let Some(outcome) = received_outcome(
                        value,
                        command_started.elapsed().as_secs_f64(),
                        &job_id,
                        false,
                    ) {
                        return finish(&output, outcome);
                    }
                    let confirmed = job_status(value);
                    if confirmed != "unavailable" {
                        status = confirmed;
                    }
                }
                return finish(
                    &output,
                    RunOutcome {
                        kind: OutcomeKind::Aborted,
                        elapsed: command_started.elapsed().as_secs_f64(),
                        value: None,
                        error: Some(format!(
                            "heartbeat for {} stopped and the playtest job ended",
                            args.context
                        )),
                        traceback: None,
                        job_status: status,
                        job_id: Some(job_id.clone()),
                        kept_open: false,
                    },
                );
            }
        }
        let has_more = poll_value
            .get("hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_more {
            if let Some(outcome) = pending_outcome.take() {
                return finish(&output, outcome);
            }
        }
    }
}

fn prepare_run(args: &PlaytestRunArgs) -> Result<PreparedRun, Box<dyn std::error::Error>> {
    if !args.timeout.is_finite() || args.timeout <= 0.0 || args.timeout > 3600.0 {
        return Err(
            "playtest run: --timeout must be finite, greater than zero, and at most 3600 seconds"
                .into(),
        );
    }
    if !(1..=8).contains(&args.players) {
        return Err("playtest run: --players must be between 1 and 8".into());
    }

    let client_index = parse_context(&args.context)?;
    match args.mode {
        PlaytestMode::Run => {
            if client_index.is_some() {
                return Err("playtest run: --mode run only provides the server context".into());
            }
            if args.client_script.is_some() {
                return Err("playtest run: --client-script cannot be used with --mode run".into());
            }
            if args.players != 1 {
                return Err("playtest run: --players only applies to --mode multiplayer".into());
            }
        }
        PlaytestMode::Play => {
            if client_index.is_some_and(|index| index != 1) {
                return Err("playtest run: --mode play only provides client:1".into());
            }
            if args.players != 1 {
                return Err("playtest run: --players only applies to --mode multiplayer".into());
            }
        }
        PlaytestMode::Multiplayer => {
            if client_index.is_some_and(|index| index > args.players) {
                return Err(format!(
                    "playtest run: context {} exceeds --players {}",
                    args.context, args.players
                )
                .into());
            }
        }
    }

    let script_args = serde_json::from_str(&args.script_args)
        .map_err(|error| format!("playtest run --args: invalid JSON: {error}"))?;
    let script = read_script(&args.script, "playtest run --script")?;
    let client_script = args
        .client_script
        .as_deref()
        .map(|path| read_script(path, "playtest run --client-script"))
        .transpose()?;

    Ok(PreparedRun {
        script,
        client_script,
        script_args,
    })
}

fn parse_context(context: &str) -> Result<Option<u8>, Box<dyn std::error::Error>> {
    if context == "server" {
        return Ok(None);
    }
    let Some(index_text) = context.strip_prefix("client:") else {
        return Err("playtest run: --context must be server or client:N".into());
    };
    let index = index_text
        .parse::<u8>()
        .map_err(|_| "playtest run: --context must be server or client:N")?;
    if !(1..=8).contains(&index) {
        return Err("playtest run: client context index must be between 1 and 8".into());
    }
    if index_text != index.to_string() {
        return Err("playtest run: --context must use canonical client:N syntax".into());
    }
    Ok(Some(index))
}

fn read_script(path: &Path, label: &str) -> Result<ScriptFile, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("{label}: read {}: {error}", path.display()))?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let source = String::from_utf8(bytes)
        .map_err(|error| format!("{label}: {} is not valid UTF-8: {error}", path.display()))?;
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(ScriptFile {
        path: absolute.display().to_string(),
        source,
        sha256,
    })
}

async fn connect_before(port: u16, deadline: Instant) -> Result<remote::RemoteSession, String> {
    let remaining = remaining_duration(deadline);
    if remaining.is_zero() {
        return Err("session deadline expired before daemon connection".into());
    }
    let timeout = CONNECT_TIMEOUT.min(remaining);
    tokio::time::timeout(timeout, remote::RemoteSession::connect(port))
        .await
        .map_err(|_| {
            format!(
                "daemon WebSocket handshake timed out after {:.1}s",
                timeout.as_secs_f64()
            )
        })?
}

async fn start_owned_run(
    port: u16,
    start_payload: &Value,
    client_run_id: &str,
    deadline: Instant,
    command_started: Instant,
) -> Result<(remote::RemoteSession, Value), RunOutcome> {
    let mut last_error = "playtest did not start".to_owned();
    let mut last_status = "unavailable".to_owned();
    let mut request_may_have_started = false;

    for attempt in 0..START_MAX_ATTEMPTS {
        let mut session = match connect_before(port, deadline).await {
            Ok(session) => session,
            Err(error) => {
                last_error = format!("unable to connect to the Ro Sync daemon: {error}");
                if attempt + 1 < START_MAX_ATTEMPTS && !remaining_duration(deadline).is_zero() {
                    sleep_before_deadline(RECONNECT_BACKOFF, deadline).await;
                    continue;
                }
                break;
            }
        };
        let request_timeout = START_REQUEST_TIMEOUT.min(remaining_duration(deadline));
        if request_timeout.is_zero() {
            last_error = "playtest start exceeded the session deadline".into();
            break;
        }
        request_may_have_started = true;
        match session
            .request("playtest_run_start", start_payload.clone(), request_timeout)
            .await
        {
            Ok(response) => match crate::response_value_or_err(&response, "playtest run start") {
                Ok(value) => return Ok((session, value)),
                Err(error) => {
                    last_error = error.to_string();
                    let observed = job_status(&response);
                    if observed != "unavailable" {
                        last_status = observed;
                    }
                    let retryable = remote::plugin_error(&response)
                        .and_then(|error| error.retryable)
                        .unwrap_or(false);
                    if !retryable {
                        return Err(RunOutcome {
                            kind: OutcomeKind::BootFailure,
                            elapsed: command_started.elapsed().as_secs_f64(),
                            value: None,
                            error: Some(last_error),
                            traceback: None,
                            job_status: last_status,
                            job_id: None,
                            kept_open: false,
                        });
                    }
                }
            },
            Err(error) => {
                last_error = format!("playtest did not start: {error}");
            }
        }
        if attempt + 1 < START_MAX_ATTEMPTS && !remaining_duration(deadline).is_zero() {
            sleep_before_deadline(RECONNECT_BACKOFF, deadline).await;
        }
    }

    if request_may_have_started {
        let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
        let cancel = cancel_fresh(
            port,
            None,
            client_run_id,
            "start response lost",
            true,
            cleanup_deadline,
        )
        .await;
        let cancel_status = cancel.observed_status();
        if cancel_status != "unavailable" {
            last_status = cancel_status;
        }
        if let Some(value) = cancel.canonical_value() {
            let observed = job_status(value);
            if observed != "unavailable" {
                last_status = observed;
            }
            if let Some(stop_error) = terminal_stop_error(value) {
                return Err(RunOutcome {
                    kind: OutcomeKind::Aborted,
                    elapsed: command_started.elapsed().as_secs_f64(),
                    value: None,
                    error: Some(format!("playtest start cleanup failed: {stop_error}")),
                    traceback: None,
                    job_status: last_status,
                    job_id: job_id(value),
                    kept_open: false,
                });
            }
            if !is_start_cleanup_terminal(value) {
                let recovered_job_id = job_id(value).unwrap_or_else(|| client_run_id.to_owned());
                if let Some(outcome) = received_outcome(
                    value,
                    command_started.elapsed().as_secs_f64(),
                    &recovered_job_id,
                    false,
                ) {
                    return Err(outcome);
                }
            }
        }
        if cancel.teardown_failed() || (cancel.failed() && cancel.response.is_none()) {
            return Err(RunOutcome {
                kind: OutcomeKind::Aborted,
                elapsed: command_started.elapsed().as_secs_f64(),
                value: None,
                error: Some(cancel.error_message("playtest start cleanup failed")),
                traceback: None,
                job_status: cancel.observed_status(),
                job_id: None,
                kept_open: false,
            });
        }
    }

    Err(RunOutcome {
        kind: OutcomeKind::BootFailure,
        elapsed: command_started.elapsed().as_secs_f64(),
        value: None,
        error: Some(last_error),
        traceback: None,
        job_status: last_status,
        job_id: None,
        kept_open: false,
    })
}

fn start_request(args: &PlaytestRunArgs, prepared: &PreparedRun, client_run_id: &str) -> Value {
    let mut request = Map::new();
    request.insert(
        "clientRunId".into(),
        Value::String(client_run_id.to_owned()),
    );
    request.insert(
        "mode".into(),
        Value::String(args.mode.as_plugin_str().into()),
    );
    request.insert("players".into(), json!(args.players));
    request.insert("context".into(), Value::String(args.context.clone()));
    request.insert(
        "identity".into(),
        Value::String(args.identity.as_plugin_str().into()),
    );
    request.insert(
        "script".into(),
        json!({
            "path": prepared.script.path,
            "source": prepared.script.source,
            "sha256": prepared.script.sha256,
        }),
    );
    if let Some(client_script) = &prepared.client_script {
        request.insert(
            "clientScript".into(),
            json!({
                "path": client_script.path,
                "source": client_script.source,
                "sha256": client_script.sha256,
            }),
        );
    }
    request.insert("scriptArgs".into(), prepared.script_args.clone());
    // Preserve the exact JSON representation as well. In Luau, a decoded
    // top-level JSON null becomes nil and cannot be distinguished from an
    // omitted field by looking at `scriptArgs` alone.
    request.insert(
        "scriptArgsJson".into(),
        Value::String(args.script_args.clone()),
    );
    request.insert("timeout".into(), json!(args.timeout));
    request.insert(
        "logs".into(),
        Value::String(args.logs.as_plugin_str().into()),
    );
    request.insert("keepOpen".into(), Value::Bool(args.keep_open));
    Value::Object(request)
}

fn job_id(value: &Value) -> Option<String> {
    value
        .get("jobId")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/job/id").and_then(Value::as_str))
        .or_else(|| value.pointer("/run/jobId").and_then(Value::as_str))
        .map(str::to_owned)
}

fn poll_frames(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("events")
        .and_then(Value::as_array)
        .or_else(|| value.get("frames").and_then(Value::as_array))
        .or_else(|| value.pointer("/run/events").and_then(Value::as_array))
        .or_else(|| value.pointer("/run/frames").and_then(Value::as_array))
}

fn heartbeat_age_seconds(value: &Value, context: &str) -> Option<f64> {
    let heartbeats = value
        .get("heartbeats")
        .or_else(|| value.pointer("/run/heartbeats"))?
        .as_object()?;
    let heartbeat = heartbeats.get(context)?;
    let age = heartbeat
        .get("ageSeconds")
        .or_else(|| heartbeat.get("age"))
        .and_then(Value::as_f64)?;
    age.is_finite().then_some(age.max(0.0))
}

fn delivered_cursor(current: u64, frame: &Value) -> u64 {
    // `afterSeq` means the last frame actually delivered, while some plugin
    // builds expose `nextSeq` as the first frame not yet delivered. Advancing
    // from the envelope's nextSeq would therefore skip a frame. Only an
    // observed frame may move this acknowledgement cursor.
    frame
        .get("seq")
        .and_then(Value::as_u64)
        .map_or(current, |sequence| current.max(sequence))
}

fn is_terminal_frame(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("result" | "error" | "failure" | "timeout" | "bootFailure" | "aborted")
    )
}

fn terminal_object(value: &Value) -> Option<&Value> {
    value
        .get("outcome")
        .filter(|value| value.is_object())
        .or_else(|| value.get("terminal").filter(|value| value.is_object()))
        .or_else(|| {
            value
                .pointer("/run/outcome")
                .filter(|value| value.is_object())
        })
        .or_else(|| {
            value
                .pointer("/run/terminal")
                .filter(|value| value.is_object())
        })
        .or_else(|| {
            poll_frames(value)
                .and_then(|frames| frames.iter().find(|frame| is_terminal_frame(frame)))
        })
}

fn terminal_stop_error(value: &Value) -> Option<String> {
    terminal_object(value)?
        .get("stopError")
        .map(display_json_value)
}

fn is_start_cleanup_terminal(value: &Value) -> bool {
    let Some(terminal) = terminal_object(value) else {
        return false;
    };
    if outcome_kind(
        terminal
            .get("kind")
            .or_else(|| terminal.get("outcome"))
            .or_else(|| terminal.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("aborted"),
        terminal.get("ok").and_then(Value::as_bool),
    ) != OutcomeKind::Aborted
    {
        return false;
    }
    let reason = terminal
        .get("error")
        .or_else(|| terminal.get("reason"))
        .or_else(|| terminal.get("message"))
        .map(display_json_value)
        .unwrap_or_default()
        .to_ascii_lowercase();
    reason.contains("start response lost")
        || reason.contains("start response omitted")
        || reason.contains("cancelled before start")
        || reason.contains("canceled before start")
}

fn parse_terminal_outcome(value: &Value, fallback_elapsed: f64) -> Option<RunOutcome> {
    let terminal = terminal_object(value);
    let declared_terminal = value
        .get("terminal")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .pointer("/run/terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let Some(terminal) = terminal else {
        return declared_terminal.then(|| RunOutcome {
            kind: OutcomeKind::Aborted,
            elapsed: fallback_elapsed,
            value: None,
            error: Some("playtest ended without a terminal outcome".into()),
            traceback: None,
            job_status: job_status(value),
            job_id: None,
            kept_open: false,
        });
    };

    let raw_kind = terminal
        .get("kind")
        .or_else(|| terminal.get("outcome"))
        .or_else(|| terminal.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("aborted");
    let kind = outcome_kind(raw_kind, terminal.get("ok").and_then(Value::as_bool));
    let elapsed = terminal
        .get("elapsed")
        .and_then(Value::as_f64)
        .or_else(|| value.get("elapsed").and_then(Value::as_f64))
        .unwrap_or(fallback_elapsed);
    let result_value = terminal
        .get("value")
        .or_else(|| terminal.get("result"))
        .cloned();
    let error = terminal
        .get("error")
        .or_else(|| terminal.get("message"))
        .map(display_json_value);
    let traceback = terminal
        .get("traceback")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let terminal_status = terminal
        .get("jobStatus")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| job_status(value));

    Some(RunOutcome {
        kind,
        elapsed,
        value: result_value,
        error,
        traceback,
        job_status: terminal_status,
        job_id: None,
        kept_open: false,
    })
}

fn received_outcome(
    value: &Value,
    fallback_elapsed: f64,
    job_id: &str,
    keep_open: bool,
) -> Option<RunOutcome> {
    let mut outcome = parse_terminal_outcome(value, fallback_elapsed)?;
    outcome.job_id = Some(job_id.to_owned());
    if let Some(stop_error) = terminal_stop_error(value) {
        outcome.kind = OutcomeKind::Aborted;
        outcome.value = None;
        outcome.error = Some(format!("playtest teardown failed: {stop_error}"));
    }
    outcome.kept_open = keep_open && is_active_job_status(&outcome.job_status);
    Some(outcome)
}

fn outcome_kind(value: &str, ok: Option<bool>) -> OutcomeKind {
    match value {
        "success" | "done" | "returned" => OutcomeKind::Success,
        "result" if ok != Some(false) => OutcomeKind::Success,
        "failure" | "failed" | "error" | "fail" | "result" => OutcomeKind::Failure,
        "timeout" | "timedOut" => OutcomeKind::Timeout,
        "bootFailure" | "boot_failure" | "boot-failure" => OutcomeKind::BootFailure,
        "aborted" | "external" | "externalStop" => OutcomeKind::Aborted,
        _ => OutcomeKind::Aborted,
    }
}

fn display_json_value(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn job_status(value: &Value) -> String {
    value
        .get("jobStatus")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/job/status").and_then(Value::as_str))
        .or_else(|| value.pointer("/run/jobStatus").and_then(Value::as_str))
        .or_else(|| value.pointer("/run/job/status").and_then(Value::as_str))
        .or_else(|| value.pointer("/outcome/jobStatus").and_then(Value::as_str))
        .or_else(|| value.pointer("/terminal/jobStatus").and_then(Value::as_str))
        .unwrap_or("unavailable")
        .to_owned()
}

fn is_active_job_status(status: &str) -> bool {
    matches!(
        status,
        "starting" | "running" | "ready" | "active" | "playing"
    )
}

#[derive(Debug)]
struct JobObservation {
    status: String,
    active: Option<bool>,
    job_id: Option<String>,
    payload: Option<Value>,
}

impl JobObservation {
    fn confirmed_inactive(&self) -> bool {
        self.active == Some(false)
            || matches!(
                self.status.as_str(),
                "stopped" | "completed" | "failed" | "aborted" | "cancelled" | "canceled"
            )
    }

    fn confirmed_active_for(&self, expected_job_id: &str) -> bool {
        self.active == Some(true) && self.job_id.as_deref() == Some(expected_job_id)
    }

    fn confirmed_active_for_other_job(&self, expected_job_id: &str) -> bool {
        self.active == Some(true)
            && !self.confirmed_active_for(expected_job_id)
            && self
                .job_id
                .as_deref()
                .is_some_and(|job_id| job_id != expected_job_id)
    }
}

#[derive(Debug)]
struct CancelAttempt {
    value: Option<Value>,
    response: Option<Value>,
    error: Option<String>,
}

impl CancelAttempt {
    fn canonical_value(&self) -> Option<&Value> {
        self.value.as_ref().or_else(|| {
            self.response
                .as_ref()
                .and_then(|response| response.pointer("/error/details/canonical"))
        })
    }

    fn observed_status(&self) -> String {
        self.canonical_value()
            .map(job_status)
            .filter(|status| status != "unavailable")
            .or_else(|| {
                self.response
                    .as_ref()
                    .map(job_status)
                    .filter(|status| status != "unavailable")
            })
            .unwrap_or_else(|| "unavailable".into())
    }

    fn teardown_failed(&self) -> bool {
        self.error.as_ref().is_some_and(|error| {
            let error = error.to_ascii_lowercase();
            error.contains("teardown")
                || error.contains("cleanup")
                || error.contains("could not confirm")
                || error.contains("did not finish")
        })
    }

    fn error_message(&self, fallback: &str) -> String {
        self.error.clone().unwrap_or_else(|| fallback.to_owned())
    }

    fn failed(&self) -> bool {
        self.error.is_some()
    }
}

fn cancel_request(job_id: Option<&str>, client_run_id: &str, reason: &str, force: bool) -> Value {
    let mut args = Map::new();
    if let Some(job_id) = job_id {
        args.insert("jobId".into(), Value::String(job_id.to_owned()));
    }
    args.insert(
        "clientRunId".into(),
        Value::String(client_run_id.to_owned()),
    );
    args.insert("reason".into(), Value::String(reason.to_owned()));
    args.insert("force".into(), Value::Bool(force));
    Value::Object(args)
}

fn cancel_response(response: Value) -> CancelAttempt {
    match crate::response_value_or_err(&response, "playtest run cancel") {
        Ok(value) => CancelAttempt {
            value: Some(value),
            response: Some(response),
            error: None,
        },
        Err(error) => CancelAttempt {
            value: None,
            response: Some(response),
            error: Some(error.to_string()),
        },
    }
}

fn cancelled_before_request(message: impl Into<String>) -> CancelAttempt {
    CancelAttempt {
        value: None,
        response: None,
        error: Some(message.into()),
    }
}

async fn cancel_with_session(
    session: &mut remote::RemoteSession,
    job_id: &str,
    client_run_id: &str,
    reason: &str,
    force: bool,
    deadline: Instant,
) -> CancelAttempt {
    let timeout = remaining_duration(deadline);
    if timeout.is_zero() {
        return cancelled_before_request("playtest cleanup budget expired before cancellation");
    }
    match session
        .request(
            "playtest_run_cancel",
            cancel_request(Some(job_id), client_run_id, reason, force),
            timeout,
        )
        .await
    {
        Ok(response) => cancel_response(response),
        Err(error) => cancelled_before_request(format!("playtest run cancel: {error}")),
    }
}

async fn cancel_fresh(
    port: u16,
    job_id: Option<&str>,
    client_run_id: &str,
    reason: &str,
    force: bool,
    deadline: Instant,
) -> CancelAttempt {
    let mut session = match connect_before(port, deadline).await {
        Ok(session) => session,
        Err(error) => return cancelled_before_request(error),
    };
    let timeout = remaining_duration(deadline);
    if timeout.is_zero() {
        return cancelled_before_request("playtest cleanup budget expired before cancellation");
    }
    match session
        .request(
            "playtest_run_cancel",
            cancel_request(job_id, client_run_id, reason, force),
            timeout,
        )
        .await
    {
        Ok(response) => cancel_response(response),
        Err(error) => cancelled_before_request(format!("playtest run cancel: {error}")),
    }
}

async fn timeout_run(
    output: &RunOutput,
    port: u16,
    job_id: &str,
    client_run_id: &str,
    timeout: f64,
    keep_open: bool,
    command_started: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
    let cancel = cancel_fresh(
        port,
        Some(job_id),
        client_run_id,
        "timeout",
        !keep_open,
        cleanup_deadline,
    )
    .await;
    // The plugin owns first-wins completion. A return/done/fail/external stop
    // may have beaten the local deadline; preserve its canonical terminal.
    if let Some(outcome) = cancel.canonical_value().and_then(|value| {
        received_outcome(
            value,
            command_started.elapsed().as_secs_f64(),
            job_id,
            keep_open,
        )
    }) {
        return finish(output, outcome);
    }

    let mut status = cancel.observed_status();
    let mut observation = None;
    if status == "unavailable" && !remaining_duration(cleanup_deadline).is_zero() {
        let observed = fetch_job_observation(port, job_id, cleanup_deadline).await;
        if let Some(outcome) = observed.payload.as_ref().and_then(|value| {
            received_outcome(
                value,
                command_started.elapsed().as_secs_f64(),
                job_id,
                keep_open,
            )
        }) {
            return finish(output, outcome);
        }
        status = observed.status.clone();
        observation = Some(observed);
    }

    let cleanup_unconfirmed = cancel.teardown_failed()
        || (cancel.failed()
            && !keep_open
            && !observation
                .as_ref()
                .is_some_and(JobObservation::confirmed_inactive));
    if cleanup_unconfirmed {
        let kept_open = keep_open && is_active_job_status(&status);
        return finish(
            output,
            RunOutcome {
                kind: OutcomeKind::Aborted,
                elapsed: command_started.elapsed().as_secs_f64(),
                value: None,
                error: Some(
                    cancel.error_message("playtest timeout cleanup could not be confirmed"),
                ),
                traceback: None,
                job_status: status,
                job_id: Some(job_id.to_owned()),
                kept_open,
            },
        );
    }

    let kept_open = keep_open && is_active_job_status(&status);
    finish(
        output,
        RunOutcome {
            kind: OutcomeKind::Timeout,
            elapsed: command_started.elapsed().as_secs_f64(),
            value: None,
            error: Some(format!("playtest run timed out after {timeout}s")),
            traceback: None,
            job_status: status,
            job_id: Some(job_id.to_owned()),
            kept_open,
        },
    )
}

enum TransportRecovery {
    Reconnected(remote::RemoteSession),
    Terminal(RunOutcome),
    Lost(JobObservation),
    Unverified(JobObservation),
    Deadline(JobObservation),
}

async fn recover_transport(
    port: u16,
    job_id: &str,
    keep_open: bool,
    deadline: Instant,
    command_started: Instant,
) -> TransportRecovery {
    // A single failed status request is not evidence that the playtest died.
    // Give the daemon/status channel a bounded recovery window before the
    // foreground owner performs exact-generation cleanup and reports exit 4.
    let verification_deadline = std::cmp::min(deadline, Instant::now() + TRANSPORT_VERIFY_BUDGET);
    let mut observation = fetch_job_observation(port, job_id, verification_deadline).await;
    loop {
        if let Some(outcome) = observation.payload.as_ref().and_then(|value| {
            received_outcome(
                value,
                command_started.elapsed().as_secs_f64(),
                job_id,
                keep_open,
            )
        }) {
            return TransportRecovery::Terminal(outcome);
        }
        if observation.confirmed_inactive() || observation.confirmed_active_for_other_job(job_id) {
            return TransportRecovery::Lost(observation);
        }
        if remaining_duration(deadline).is_zero() {
            return TransportRecovery::Deadline(observation);
        }
        if remaining_duration(verification_deadline).is_zero() {
            return TransportRecovery::Unverified(observation);
        }
        if observation.confirmed_active_for(job_id) {
            match connect_before(port, verification_deadline).await {
                Ok(session) => return TransportRecovery::Reconnected(session),
                Err(_) => sleep_before_deadline(RECONNECT_BACKOFF, verification_deadline).await,
            }
        } else {
            // A bare WebSocket handshake only proves that the daemon accepts
            // connections. It does not prove this exact playtest is alive, so
            // unavailable status must stay inside this verification window.
            sleep_before_deadline(RECONNECT_BACKOFF, verification_deadline).await;
        }
        if remaining_duration(deadline).is_zero() {
            return TransportRecovery::Deadline(observation);
        }
        if remaining_duration(verification_deadline).is_zero() {
            return TransportRecovery::Unverified(observation);
        }
        observation = fetch_job_observation(port, job_id, verification_deadline).await;
    }
}

async fn transport_abort_outcome(
    port: u16,
    job_id: &str,
    client_run_id: &str,
    error: &str,
    keep_open: bool,
    command_started: Instant,
    observation: JobObservation,
) -> RunOutcome {
    let mut status = observation.status;
    let mut diagnostic = format!("playtest event stream ended: {error}");
    if !keep_open {
        let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
        let cancel = cancel_fresh(
            port,
            Some(job_id),
            client_run_id,
            "transport failure",
            true,
            cleanup_deadline,
        )
        .await;
        if let Some(outcome) = cancel.canonical_value().and_then(|value| {
            received_outcome(
                value,
                command_started.elapsed().as_secs_f64(),
                job_id,
                false,
            )
        }) {
            return outcome;
        }
        let confirmed = cancel.observed_status();
        if confirmed != "unavailable" {
            status = confirmed;
        }
        if cancel.failed() {
            diagnostic.push_str("; ");
            diagnostic.push_str(&cancel.error_message("cleanup could not be confirmed"));
        }
    }
    let kept_open = keep_open && is_active_job_status(&status);
    RunOutcome {
        kind: OutcomeKind::Aborted,
        elapsed: command_started.elapsed().as_secs_f64(),
        value: None,
        error: Some(diagnostic),
        traceback: None,
        job_status: status,
        job_id: Some(job_id.to_owned()),
        kept_open,
    }
}

async fn fetch_job_observation(port: u16, job_id: &str, deadline: Instant) -> JobObservation {
    let unavailable = || JobObservation {
        status: "unavailable".into(),
        active: None,
        job_id: None,
        payload: None,
    };
    let mut session = match connect_before(port, deadline).await {
        Ok(session) => session,
        Err(_) => return unavailable(),
    };
    let timeout = STATUS_REQUEST_TIMEOUT.min(remaining_duration(deadline));
    if timeout.is_zero() {
        return unavailable();
    }
    let response = session
        .request("playtest_status", json!({ "jobId": job_id }), timeout)
        .await;
    let Ok(response) = response else {
        return unavailable();
    };
    crate::response_value_or_err(&response, "playtest run status")
        .map(|value| {
            let status = job_status(&value);
            let active = value.get("active").and_then(Value::as_bool);
            let observed_job_id = crate::playtest_run::job_id(&value);
            JobObservation {
                status,
                active,
                job_id: observed_job_id,
                payload: Some(value),
            }
        })
        .unwrap_or_else(|_| unavailable())
}

fn finish(output: &RunOutput, outcome: RunOutcome) -> Result<(), Box<dyn std::error::Error>> {
    output.terminal(&outcome)?;
    let code = outcome.kind.exit_code();
    if code == 0 {
        Ok(())
    } else {
        Err(Box::new(PlaytestRunExit::new(code)))
    }
}

fn remaining_duration(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

async fn sleep_before_deadline(duration: Duration, deadline: Instant) {
    let remaining = remaining_duration(deadline);
    if !remaining.is_zero() {
        tokio::time::sleep(duration.min(remaining)).await;
    }
}

fn compact_json_line(value: &Value) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

fn write_stdout_line(line: &str) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()
}

fn write_human_progress(value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("event");
    let context = value
        .get("context")
        .and_then(Value::as_str)
        .unwrap_or("playtest");
    let elapsed = value.get("t").and_then(Value::as_f64).unwrap_or(0.0);
    match kind {
        "started" => write_stdout_line(&format!(
            "▶ playtest {} {} (timeout {}s)",
            value.get("jobId").and_then(Value::as_str).unwrap_or("?"),
            value.get("mode").and_then(Value::as_str).unwrap_or("play"),
            value.get("timeout").and_then(Value::as_f64).unwrap_or(0.0)
        ))?,
        "ready" => write_stdout_line(&format!("{elapsed:.1}s  {context}  ready"))?,
        "event" => write_stdout_line(&format!(
            "{elapsed:.1}s  {context}  {}",
            value
                .get("data")
                .map(Value::to_string)
                .unwrap_or_else(|| "null".into())
        ))?,
        "log" => write_stdout_line(&format!(
            "{elapsed:.1}s  {context}  [{}] {}",
            value.get("level").and_then(Value::as_str).unwrap_or("info"),
            value.get("message").and_then(Value::as_str).unwrap_or("")
        ))?,
        "clientResult" => {
            write_stdout_line(&format!("{elapsed:.1}s  {context}  client result: {value}"))?
        }
        "dropped" => write_stdout_line(&format!(
            "{elapsed:.1}s  {context}  dropped {} event(s)",
            value.get("count").and_then(Value::as_u64).unwrap_or(0)
        ))?,
        _ => write_stdout_line(&value.to_string())?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command, PlaytestCommand};
    use clap::Parser as _;

    fn parse_run(arguments: &[&str]) -> PlaytestRunArgs {
        let mut input = vec!["rosync", "playtest", "run"];
        input.extend_from_slice(arguments);
        let cli = Cli::try_parse_from(input).unwrap();
        let Some(Command::Playtest(playtest)) = cli.command else {
            panic!("expected playtest command");
        };
        let PlaytestCommand::Run(args) = playtest.command else {
            panic!("expected playtest run command");
        };
        args
    }

    fn fixture_script(contents: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.server.luau");
        std::fs::write(&path, contents).unwrap();
        (directory, path)
    }

    #[test]
    fn clap_defaults_match_the_ticket() {
        let args = parse_run(&["--script", "main.server.luau"]);
        assert_eq!(args.context, "server");
        assert!(matches!(args.mode, PlaytestMode::Play));
        assert_eq!(args.players, 1);
        assert_eq!(args.script_args, "{}");
        assert_eq!(args.timeout, 600.0);
        assert!(matches!(args.identity, RuntimeIdentity::Game));
        assert_eq!(args.logs, PlaytestRunLogs::Off);
        assert!(!args.keep_open);
        assert!(!args.quiet);
        assert!(!args.raw);
    }

    #[test]
    fn clap_accepts_every_run_flag_and_raw_quiet_together() {
        let args = parse_run(&[
            "--project",
            ".",
            "--port",
            "7880",
            "--script",
            "main.server.luau",
            "--context",
            "client:2",
            "--client-script",
            "client.client.luau",
            "--mode",
            "multiplayer",
            "--players",
            "2",
            "--args",
            "[1,true,null]",
            "--timeout",
            "3600",
            "--identity",
            "plugin",
            "--logs",
            "warn",
            "--keep-open",
            "--quiet",
            "--raw",
        ]);
        assert_eq!(args.port, 7880);
        assert_eq!(args.context, "client:2");
        assert!(matches!(args.mode, PlaytestMode::Multiplayer));
        assert_eq!(args.players, 2);
        assert_eq!(args.script_args, "[1,true,null]");
        assert_eq!(args.timeout, 3600.0);
        assert!(matches!(args.identity, RuntimeIdentity::Plugin));
        assert_eq!(args.logs, PlaytestRunLogs::Warn);
        assert!(args.keep_open && args.quiet && args.raw);
    }

    #[test]
    fn preflight_accepts_arbitrary_json_and_hashes_exact_bytes() {
        let bytes = b"return {line = 'a'}\r\n";
        let (_directory, path) = fixture_script(bytes);
        for value in ["42", "true", "null", "\"text\"", "[1,2]", "{\"x\":1}"] {
            let mut args = parse_run(&["--script", path.to_str().unwrap()]);
            args.script_args = value.into();
            let prepared = prepare_run(&args).unwrap();
            assert_eq!(
                prepared.script_args,
                serde_json::from_str::<Value>(value).unwrap()
            );
            assert_eq!(
                prepared.script.sha256,
                format!("{:x}", Sha256::digest(bytes))
            );
            assert!(prepared.script.source.ends_with("\r\n"));
            let request = start_request(&args, &prepared, "client-run-test");
            assert_eq!(request["clientRunId"], "client-run-test");
            assert_eq!(request["scriptArgsJson"], value);
            assert_eq!(request["scriptArgs"], prepared.script_args);
        }
    }

    #[test]
    fn preflight_rejects_invalid_json_timeout_context_and_mode_combinations() {
        let (_directory, path) = fixture_script(b"return true\n");
        let base = || parse_run(&["--script", path.to_str().unwrap()]);

        let mut args = base();
        args.script_args = "{".into();
        assert!(prepare_run(&args)
            .unwrap_err()
            .to_string()
            .contains("invalid JSON"));

        let mut args = base();
        args.timeout = 3600.1;
        assert!(prepare_run(&args).is_err());

        let mut args = base();
        args.context = "client:0".into();
        assert!(prepare_run(&args).is_err());

        let mut args = base();
        args.context = "client:01".into();
        assert!(prepare_run(&args).is_err());

        let mut args = base();
        args.mode = PlaytestMode::Run;
        args.context = "client:1".into();
        assert!(prepare_run(&args).is_err());

        let mut args = base();
        args.mode = PlaytestMode::Multiplayer;
        args.players = 2;
        args.context = "client:3".into();
        assert!(prepare_run(&args).is_err());
    }

    #[test]
    fn outcome_exit_codes_are_stable() {
        assert_eq!(OutcomeKind::Success.exit_code(), 0);
        assert_eq!(OutcomeKind::Failure.exit_code(), 2);
        assert_eq!(OutcomeKind::Timeout.exit_code(), 3);
        assert_eq!(OutcomeKind::Aborted.exit_code(), 4);
        assert_eq!(OutcomeKind::BootFailure.exit_code(), 5);
        assert_eq!(outcome_kind("result", Some(true)), OutcomeKind::Success);
        assert_eq!(outcome_kind("result", Some(false)), OutcomeKind::Failure);
        assert_eq!(outcome_kind("error", None), OutcomeKind::Failure);
        assert_eq!(outcome_kind("bootFailure", None), OutcomeKind::BootFailure);
    }

    #[test]
    fn compact_ndjson_escapes_embedded_newlines() {
        let line = compact_json_line(&json!({
            "type": "event",
            "data": { "message": "first\nsecond" }
        }))
        .unwrap();
        assert!(!line.contains('\n'));
        let decoded: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded["data"]["message"], "first\nsecond");
    }

    #[test]
    fn terminal_envelopes_are_independently_parseable() {
        let success = RunOutcome {
            kind: OutcomeKind::Success,
            elapsed: 2.5,
            value: Some(json!({ "answer": 42 })),
            error: None,
            traceback: None,
            job_status: "stopped".into(),
            job_id: None,
            kept_open: false,
        }
        .terminal_value();
        assert_eq!(success["type"], "result");
        assert_eq!(success["ok"], true);
        assert_eq!(success["value"]["answer"], 42);
        serde_json::from_str::<Value>(&compact_json_line(&success).unwrap()).unwrap();

        let aborted = RunOutcome {
            kind: OutcomeKind::Aborted,
            elapsed: 3.0,
            value: None,
            error: Some("Studio stopped".into()),
            traceback: None,
            job_status: "completed".into(),
            job_id: None,
            kept_open: false,
        }
        .terminal_value();
        assert_eq!(aborted["type"], "aborted");
        assert_eq!(aborted["jobStatus"], "completed");
        serde_json::from_str::<Value>(&compact_json_line(&aborted).unwrap()).unwrap();
    }

    #[test]
    fn parser_accepts_both_poll_shapes_and_terminal_aliases() {
        let value = json!({
            "frames": [{"seq": 1, "type": "event", "data": 1}],
            "run": {
                "jobStatus": "stopped",
                "terminal": {
                    "kind": "result",
                    "ok": true,
                    "elapsed": 1.2,
                    "value": {"done": true}
                }
            }
        });
        assert_eq!(poll_frames(&value).unwrap().len(), 1);
        let outcome = parse_terminal_outcome(&value, 9.0).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Success);
        assert_eq!(outcome.elapsed, 1.2);
        assert_eq!(outcome.value.unwrap()["done"], true);
    }

    #[test]
    fn poll_cursor_acknowledges_only_frames_that_were_delivered() {
        let poll = json!({
            "frames": [
                {"seq": 7, "type": "event", "data": "a"},
                {"seq": 8, "type": "event", "data": "b"}
            ],
            // This is the first unseen sequence, not a valid afterSeq cursor.
            "nextSeq": 9
        });
        let cursor = poll_frames(&poll).unwrap().iter().fold(6, delivered_cursor);
        assert_eq!(cursor, 8);
    }

    #[test]
    fn canonical_terminal_frame_is_accepted_without_a_duplicate_envelope() {
        let poll = json!({
            "frames": [{
                "seq": 4,
                "type": "result",
                "ok": true,
                "elapsed": 0.5,
                "value": {"answer": 42},
                "jobStatus": "stopped"
            }],
            "hasMore": false
        });
        let outcome = parse_terminal_outcome(&poll, 1.0).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Success);
        assert_eq!(outcome.value.unwrap()["answer"], 42);
        assert_eq!(outcome.job_status, "stopped");
    }

    #[test]
    fn keep_open_terminal_result_carries_the_job_id() {
        let value = RunOutcome {
            kind: OutcomeKind::Success,
            elapsed: 1.0,
            value: Some(json!(true)),
            error: None,
            traceback: None,
            job_status: "running".into(),
            job_id: Some("job-autopsy".into()),
            kept_open: true,
        }
        .terminal_value();
        assert_eq!(value["keptOpen"], true);
        assert_eq!(value["jobId"], "job-autopsy");
    }

    #[test]
    fn stop_error_overrides_success_even_when_keep_open_was_requested() {
        let poll = json!({
            "outcome": {
                "kind": "result",
                "ok": true,
                "value": 42,
                "jobStatus": "running",
                "stopError": "runner cancellation was not acknowledged"
            }
        });
        let outcome = received_outcome(&poll, 1.0, "job-autopsy", true).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Aborted);
        assert!(outcome.kept_open);
        assert_eq!(outcome.job_id.as_deref(), Some("job-autopsy"));
        assert!(outcome.error.unwrap().contains("runner cancellation"));
    }

    #[test]
    fn heartbeat_watchdog_reads_only_the_selected_context_age() {
        let poll = json!({
            "heartbeats": {
                "server": {"lastSeen": 50.0, "ageSeconds": 7.5},
                "client:1": {"lastSeen": 56.0, "ageSeconds": 1.5}
            }
        });
        assert_eq!(heartbeat_age_seconds(&poll, "server"), Some(7.5));
        assert_eq!(heartbeat_age_seconds(&poll, "client:1"), Some(1.5));
        assert_eq!(heartbeat_age_seconds(&poll, "client:2"), None);

        let active = JobObservation {
            status: "running".into(),
            active: Some(true),
            job_id: Some("job-active".into()),
            payload: None,
        };
        assert!(!active.confirmed_inactive());
        assert!(active.confirmed_active_for("job-active"));
        assert!(!active.confirmed_active_for("job-other"));
        assert!(!active.confirmed_active_for_other_job("job-active"));
        assert!(active.confirmed_active_for_other_job("job-other"));
        let ended = JobObservation {
            status: "completed".into(),
            active: Some(false),
            job_id: Some("job-active".into()),
            payload: None,
        };
        assert!(ended.confirmed_inactive());

        let unavailable = JobObservation {
            status: "unavailable".into(),
            active: None,
            job_id: None,
            payload: None,
        };
        assert!(!unavailable.confirmed_inactive());
        assert!(!unavailable.confirmed_active_for("job-active"));
        assert!(!unavailable.confirmed_active_for_other_job("job-active"));
    }

    #[test]
    fn plugin_audit_acknowledgement_requires_json_success_body() {
        let plugin = include_str!("../../plugin/RemoteControl.luau");
        let helper_start = plugin
            .find("local function postRemoteWriteLogSync")
            .expect("plugin audit helper");
        let helper_tail = &plugin[helper_start..];
        let helper_end = helper_tail
            .find("local function postRemoteWriteLog(entry)")
            .expect("plugin async audit helper");
        let helper = &helper_tail[..helper_end];

        assert!(helper.contains(r#"httpJson("/writelog", "POST", redacted)"#));
        assert!(helper.contains("response.ok ~= true"));
        assert!(!helper.contains(r#"httpRequest("/writelog""#));
    }

    #[test]
    fn plugin_completion_audit_retains_both_script_hashes() {
        let plugin = include_str!("../../plugin/RemoteControl.luau");
        let helper_start = plugin
            .find("local function auditPlayscriptCompletion")
            .expect("playscript completion audit helper");
        let helper_tail = &plugin[helper_start..];
        let helper_end = helper_tail
            .find("local function cancelOpenPlayscriptRunners")
            .expect("next playscript helper");
        let helper = &helper_tail[..helper_end];

        assert!(helper.contains("sha256 = run._main.sha256"));
        assert!(helper.contains("sha256 = run._client.sha256"));
    }

    #[test]
    fn plugin_decodes_playscript_chunks_with_the_studio_buffer_overload() {
        let plugin = include_str!("../../plugin/RemoteControl.luau");
        assert!(plugin.contains(
            "EncodingService:Base64Decode(buffer.fromstring(tostring(chunk.bytesBase64 or \"\")))"
        ));
        assert!(
            !plugin.contains("EncodingService:Base64Decode(tostring(chunk.bytesBase64 or \"\"))")
        );
    }

    #[test]
    fn plugin_gives_one_player_multiplayer_runs_a_server_context() {
        let plugin = include_str!("../../plugin/RemoteControl.luau");
        let multiplayer_branch = plugin
            .split_once("elseif mode == \"multiplayer\" then")
            .expect("multiplayer launch branch")
            .1;

        assert!(multiplayer_branch.contains("if players == 1 then"));
        assert!(multiplayer_branch.contains("return service:ExecutePlayModeAsync(testArgs)"));
        assert!(multiplayer_branch
            .contains("return service:ExecuteMultiplayerTestAsync(players, testArgs)"));
    }
}
