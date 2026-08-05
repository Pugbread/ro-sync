use clap::Parser;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch as tokio_watch};

mod artifact;
mod capture_command;
mod cli;
mod conflict;
mod daemon_manager;
mod diff;
mod fs_map;
mod fs_safety;
mod http;
mod img_upload;
mod initial_sync;
mod lifecycle;
mod lint_command;
mod native_capture;
mod path_resolver;
mod playtest_run;
mod project_config;
mod project_init;
#[cfg(test)]
mod query;
mod remote;
mod snapshot;
mod sourcemap;
mod studio_clipboard;
mod sync_scope;
mod upload_command;
mod watch;
mod watch_bridge;
mod workflow;
mod workflow_command;
mod ws;

use capture_command::*;
use cli::*;
use conflict::{ConflictEngine, FsDecision};
use daemon_manager::*;
use initial_sync::PendingInitial;
use lint_command::*;
use upload_command::*;
use watch::{Op, OpKind, Watch};
use watch_bridge::*;
use workflow_command::*;
use ws::{PendingRoutes, RequestEnvelope};

const COMMANDS_BUNDLE_JSON: &str = include_str!("../../docs/client-commands.generated.json");
const DEFAULT_DAEMON_PORT: u16 = 7878;
const DAEMON_PORT_SCAN_MAX: u16 = 7890;
const CAPTURE_MAX_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
const CAPTURE_MAX_DIMENSION: u32 = 16_384;
const CAPTURE_MAX_PIXELS: u64 = 64 * 1024 * 1024;
const PHOTO_MAX_DIMENSION: u32 = 4096;
const PHOTO_MAX_PIXELS: u64 = 16 * 1024 * 1024;
const LOCAL_HTTP_MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const LOCAL_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const LOCAL_HTTP_DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const CAPTURE_CLEANUP_RESERVE: Duration = Duration::from_millis(250);
const RECOMMENDED_LUAU_LSP_VERSION: (u64, u64, u64) = (1, 68, 1);
const ROBLOX_DEFINITIONS_SHA256: &str =
    "08fbcafcf6d17643886d8fe0ec297fc9bfab33d3bf8d96d88b6eefe29f6d5490";

#[derive(Clone)]
pub struct AppState {
    pub project: Arc<PathBuf>,
    /// Fully-resolved project root. Used as the canonical form for every
    /// filesystem path the daemon hands to the conflict engine or the
    /// filesystem — guarantees `/private/tmp/...` from the watcher and
    /// `/tmp/...` from `/push` hash into the same key.
    pub canonical_project: Arc<PathBuf>,
    /// Canonical desktop-authorized parent for plugin-created projects.
    /// Absent on ordinary CLI/manual daemons, which disables `/projects/init`.
    pub projects_root: Arc<Option<PathBuf>>,
    pub events: broadcast::Sender<String>,
    pub conflict: Arc<ConflictEngine>,
    /// Short-lived, bounded binary artifacts uploaded by the Studio plugin.
    pub artifacts: artifact::ArtifactStore,
    pub project_name: Arc<RwLock<String>>,
    pub game_id: Arc<RwLock<Option<String>>>,
    pub group_id: Arc<RwLock<Option<String>>>,
    pub place_ids: Arc<RwLock<Vec<String>>>,
    pub wally_enabled: Arc<RwLock<bool>>,
    pub wally_folder: Arc<RwLock<Option<String>>>,
    pub pending_initial: Arc<Mutex<Option<PendingInitial>>>,
    /// Paths that we've written via `/push` within the last ~200ms.
    /// `spawn_watch_bridge` drops watcher ops for paths whose deadline hasn't
    /// passed yet — prevents our own writes from being re-emitted as FS
    /// changes (Argon `SYNCBACK_DEBOUNCE_TIME`).
    pub push_quiet: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    /// Broadcast channel carrying `{type:"request",...}` frames from any
    /// connected CLI client. The plugin's WS connection subscribes and
    /// forwards matching frames to Studio.
    pub request_tx: broadcast::Sender<RequestEnvelope>,
    /// Route map keyed by `request_id`: when a CLI client sends a request its
    /// outbound mpsc sender is stashed here so the plugin's response frame
    /// can be steered back to the right connection.
    pub pending_routes: PendingRoutes,
    /// The single active Roblox Studio plugin WebSocket connection. CLI/watch
    /// clients are allowed to come and go, but only one plugin may own the live
    /// Studio bridge at a time.
    pub active_plugin: Arc<Mutex<Option<u64>>>,
    /// Whether this daemon was launched by the Terminal 64 widget and should
    /// exit with that widget instead of lingering as a background service.
    pub widget_owned: bool,
    /// Whether a lifecycle manager owns this process.
    pub managed: bool,
    /// Human-readable manager label (for example cli, desktop, or terminal64-widget).
    pub managed_by: Arc<String>,
    /// Per-process identity used to reject stale runtime records.
    pub boot_id: Arc<String>,
    pub listen_port: u16,
    pub process_id: u32,
    pub started_at: u64,
    /// Shared secret for generic lifecycle control endpoints.
    pub manager_owner_token: Arc<Option<String>>,
    /// Last heartbeat received from a heartbeat-driven manager.
    pub manager_last_seen: Arc<Mutex<Option<Instant>>>,
    /// Backward-compatible aliases used only by legacy widget tests/routes.
    pub widget_owner_token: Arc<Option<String>>,
    pub widget_last_seen: Arc<Mutex<Option<Instant>>>,
    /// Graceful shutdown trigger used by local lifecycle endpoints.
    pub shutdown_tx: tokio_watch::Sender<Option<String>>,
}

/// Duration of the per-path quiet window after a `/push` write.
pub const PUSH_QUIET_MS: u64 = 1500;
const WIDGET_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DESKTOP_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const OWNER_HEARTBEAT_CHECK_INTERVAL: Duration = Duration::from_secs(2);
// `Instant` advances while a machine is asleep on supported platforms. Treat a
// newly-observed stale heartbeat as suspect first so the manager gets a chance
// to run after wake before the daemon commits to shutdown.
const OWNER_HEARTBEAT_SUSPECT_GRACE: Duration = Duration::from_secs(30);

fn owner_heartbeat_expired(last_seen: Option<Instant>, timeout: Duration) -> bool {
    last_seen.is_some_and(|last_seen| last_seen.elapsed() > timeout)
}

fn owner_heartbeat_should_shutdown(
    last_seen: Option<Instant>,
    suspect_since: Option<Instant>,
    timeout: Duration,
    suspect_grace: Duration,
) -> bool {
    owner_heartbeat_expired(last_seen, timeout)
        && suspect_since.is_some_and(|since| since.elapsed() > suspect_grace)
}

fn resolve_command_port(command: &mut Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Context(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "context")?
        }
        Command::Run(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "run")?,
        Command::Capabilities(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "capabilities")?
        }
        Command::Capture(args) => match &mut args.command {
            CaptureCommand::Status(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "capture status")?
            }
            CaptureCommand::Authorize(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "capture authorize")?
            }
            CaptureCommand::Screen(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "capture screen")?
            }
            CaptureCommand::Photo(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "capture photo")?
            }
            CaptureCommand::Scene(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "capture scene")?
            }
        },
        Command::Playtest(args) => match &mut args.command {
            // `playtest run` deliberately resolves its daemon only after its
            // local script/JSON preflight has completed. That keeps malformed
            // invocations completely offline and unable to launch a playtest.
            PlaytestCommand::Run(_) => {}
            PlaytestCommand::Start(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest start")?
            }
            PlaytestCommand::Status(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest status")?
            }
            PlaytestCommand::Contexts(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest contexts")?
            }
            PlaytestCommand::Wait(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest wait")?
            }
            PlaytestCommand::Stop(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest stop")?
            }
            PlaytestCommand::Exec(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest exec")?
            }
            PlaytestCommand::Logs(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest logs")?
            }
            PlaytestCommand::Ui(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest ui")?
            }
            PlaytestCommand::Input(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest input")?
            }
            PlaytestCommand::Capture(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest capture")?
            }
            PlaytestCommand::Request(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "playtest request")?
            }
        },
        Command::Query(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "query")?
        }
        Command::Path(args) => resolve_port_field(&mut args.port, Some(&args.project), "path")?,
        Command::Get(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "get")?,
        Command::Set(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "set")?,
        Command::Ls(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "ls")?,
        Command::Tree(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "tree")?,
        Command::Snapshot(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "snapshot")?
        }
        Command::Diff(args) | Command::Changes(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "diff")?
        }
        Command::Open(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "open")?,
        Command::Where(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "where")?
        }
        Command::Props(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "props")?
        }
        Command::Source(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "source")?
        }
        Command::Meta(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "meta")?,
        Command::Services(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "services")?
        }
        Command::Conflicts(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "conflicts")?
        }
        Command::Resolve(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "resolve")?
        }
        Command::Decision(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "decision")?
        }
        Command::Tail(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "tail")?,
        Command::Watch(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "watch")?
        }
        Command::Repair(args) => {
            if let RepairCommand::Tree(tree_args) = &mut args.command {
                resolve_port_field(
                    &mut tree_args.port,
                    tree_args.project.as_deref(),
                    "repair tree",
                )?;
            }
        }
        Command::Find(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "find")?,
        Command::Eval(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "eval")?,
        Command::Transmit(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "transmit")?
        }
        Command::Logs(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "logs")?,
        Command::Save(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "save")?,
        Command::Undo(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "undo")?,
        Command::Redo(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "redo")?,
        Command::Waypoint(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "waypoint")?
        }
        Command::Ping(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "ping")?,
        Command::Version(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "version")?
        }
        Command::Status(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "status")?
        }
        Command::Doctor(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "doctor")?
        }
        Command::New(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "new")?,
        Command::Rm(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "rm")?,
        Command::Mv(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "mv")?,
        Command::Attr(args) => match &mut args.command {
            AttrCommand::Set(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "attr set")?
            }
            AttrCommand::Rm(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "attr rm")?
            }
            AttrCommand::Ls(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "attr ls")?
            }
        },
        Command::Tag(args) => match &mut args.command {
            TagCommand::Add(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "tag add")?
            }
            TagCommand::Rm(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "tag rm")?
            }
        },
        Command::Call(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "call")?,
        Command::Select(args) => match &mut args.command {
            SelectCommand::Get(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "select get")?
            }
            SelectCommand::Set(args) => {
                resolve_port_field(&mut args.port, args.project.as_deref(), "select set")?
            }
        },
        Command::Copy(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "copy")?,
        Command::Paste(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "paste")?
        }
        Command::Classinfo(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "classinfo")?
        }
        Command::Enums(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "enums")?
        }
        Command::Enum(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "enum")?,
        Command::FindAttr(args) => {
            resolve_port_field(&mut args.port, args.project.as_deref(), "find-attr")?
        }
        Command::Lint(args) => resolve_port_field(&mut args.port, args.project.as_deref(), "lint")?,
        Command::Auth(_)
        | Command::Commands(_)
        | Command::Daemon(_)
        | Command::Init(_)
        | Command::Plan(_)
        | Command::Plugin(_)
        | Command::Serve(_)
        | Command::Upload(_)
        | Command::Monetization(_)
        | Command::Img(_)
        | Command::Imgs(_)
        | Command::Refresh(_) => {}
    }

    Ok(())
}

fn resolve_port_field(
    port: &mut u16,
    project: Option<&std::path::Path>,
    context: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = project_or_cwd(project, context)?;
    if let Some(resolved) = discover_project_daemon_port(&project, *port)? {
        *port = resolved;
    }
    Ok(())
}

fn discover_project_daemon_port(
    project: &std::path::Path,
    requested_port: u16,
) -> Result<Option<u16>, Box<dyn std::error::Error>> {
    discover_project_daemon_port_in_range(
        project,
        requested_port,
        DEFAULT_DAEMON_PORT..=DAEMON_PORT_SCAN_MAX,
    )
}

fn discover_project_daemon_port_in_range(
    project: &std::path::Path,
    requested_port: u16,
    ports: std::ops::RangeInclusive<u16>,
) -> Result<Option<u16>, Box<dyn std::error::Error>> {
    let canonical_project = canonicalize_project_path(project);

    // Probe every candidate concurrently rather than one at a time. A closed
    // port only costs nothing when the OS refuses it immediately; where it
    // does not, each miss burns the probe's full connect timeout and the
    // sweep costs their sum. Windows is the case that matters here — a closed
    // loopback port was measured taking ~2s to refuse on a normal desktop,
    // far past the 750ms probe timeout, so discovering a daemon across this
    // 13-port range cost ~10s on every command that had to find its own port
    // (enough to blow the desktop packaging step's 10s budget). Probing in
    // parallel bounds the sweep by one timeout instead of fourteen. Results
    // are still consumed in the original order below, so which daemon wins is
    // unchanged on every platform.
    let scanned: Vec<u16> = ports
        .clone()
        .filter(|port| *port != requested_port)
        .collect();
    let mut hellos: std::collections::HashMap<u16, serde_json::Value> =
        std::collections::HashMap::new();
    std::thread::scope(|scope| {
        let probes: Vec<_> = std::iter::once(requested_port)
            .chain(scanned.iter().copied())
            .map(|port| scope.spawn(move || (port, fetch_daemon_hello(port).ok())))
            .collect();
        for probe in probes {
            if let Ok((port, Some(hello))) = probe.join() {
                hellos.insert(port, hello);
            }
        }
    });

    let requested_hello = hellos.get(&requested_port).cloned();
    if requested_hello
        .as_ref()
        .is_some_and(|hello| daemon_hello_matches_project(hello, &canonical_project))
    {
        return Ok(Some(requested_port));
    }

    for port in scanned {
        if hellos
            .get(&port)
            .is_some_and(|hello| daemon_hello_matches_project(hello, &canonical_project))
        {
            return Ok(Some(port));
        }
    }

    if let Some(hello) = requested_hello {
        let daemon_project = hello
            .get("project")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown project");
        return Err(format!(
            "daemon routing refused: port {requested_port} serves {daemon_project}, not {}",
            canonical_project.display()
        )
        .into());
    }

    Ok(None)
}

fn daemon_hello_matches_project(
    hello: &serde_json::Value,
    canonical_project: &std::path::Path,
) -> bool {
    let Some(daemon_project) = hello.get("project").and_then(|value| value.as_str()) else {
        return false;
    };
    canonicalize_project_path(std::path::Path::new(daemon_project)) == canonical_project
}

fn canonicalize_project_path(path: &std::path::Path) -> PathBuf {
    lifecycle::canonical_project(path).unwrap_or_else(|_| path.to_path_buf())
}

const CLI_WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

fn main() {
    // Windows executables default to a 1 MiB main-thread stack. Clap's command
    // graph plus the top-level CLI future can exceed that before lifecycle
    // commands have a chance to arm their parent-stdin lease. Poll the future
    // on an explicitly sized stack instead of relying on platform linker
    // defaults; the coordinator thread does nothing but join it.
    let worker = std::thread::Builder::new()
        .name("rosync-cli".to_string())
        .stack_size(CLI_WORKER_STACK_SIZE)
        .spawn(run_cli_worker)
        .unwrap_or_else(|error| {
            eprintln!("Error: spawn CLI worker: {error}");
            std::process::exit(1);
        });
    match worker.join() {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn run_cli_worker() -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: initialize async runtime: {error}");
            return 1;
        }
    };
    match runtime.block_on(run_cli()) {
        Ok(()) => 0,
        Err(error) => {
            let code = if let Some(exit) = error.downcast_ref::<playtest_run::PlaytestRunExit>() {
                exit.code()
            } else {
                eprintln!("Error: {error}");
                1
            };
            // The old `#[tokio::main]` entrypoint exited the process directly
            // on errors. Avoid making the coordinator wait indefinitely for a
            // spawned blocking task before it can preserve that exit code.
            runtime.shutdown_background();
            code
        }
    }
}

async fn run_cli() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut command = cli.command;

    if let Some(command) = command.as_mut() {
        resolve_command_port(command)?;
    }

    match command {
        Some(Command::Init(args)) => run_init(args),
        Some(Command::Plugin(args)) => run_plugin(args),
        Some(Command::Auth(args)) => run_auth(args),
        Some(Command::Commands(args)) => run_commands(args),
        Some(Command::Context(args)) => run_context(args),
        Some(Command::Run(args)) => run_workflow(args).await,
        Some(Command::Capabilities(args)) => run_capabilities(args).await,
        Some(Command::Capture(args)) => run_capture(args).await,
        Some(Command::Playtest(args)) => run_playtest(args).await,
        Some(Command::Plan(args)) => run_plan(args),
        Some(Command::Query(args)) => run_query(args).await,
        Some(Command::Path(args)) => run_path(args).await,
        Some(Command::Serve(args)) => run_serve(args).await,
        Some(Command::Daemon(args)) => run_daemon(args).await,
        Some(Command::Get(args)) => run_get(args).await,
        Some(Command::Set(args)) => run_set(args).await,
        Some(Command::Ls(args)) => run_ls(args).await,
        Some(Command::Tree(args)) => run_tree(args).await,
        Some(Command::Snapshot(args)) => run_snapshot(args).await,
        Some(Command::Diff(args)) => run_diff(args).await,
        Some(Command::Changes(args)) => run_changes(args).await,
        Some(Command::Open(args)) => run_open(args).await,
        Some(Command::Where(args)) => run_where(args).await,
        Some(Command::Props(args)) => run_props(args).await,
        Some(Command::Source(args)) => run_source(args).await,
        Some(Command::Meta(args)) => run_meta(args).await,
        Some(Command::Services(args)) => run_services(args).await,
        Some(Command::Conflicts(args)) => run_conflicts(args).await,
        Some(Command::Resolve(args)) => run_resolve(args).await,
        Some(Command::Decision(args)) => run_decision(args).await,
        Some(Command::Tail(args)) => run_tail(args).await,
        Some(Command::Watch(args)) => run_watch(args).await,
        Some(Command::Repair(args)) => run_repair(args).await,
        Some(Command::Upload(args)) => run_upload(args).await,
        Some(Command::Monetization(args)) => run_monetization(args).await,
        Some(Command::Img(args)) => run_img(args).await,
        Some(Command::Imgs(args)) => run_imgs(args).await,
        Some(Command::Find(args)) => run_find(args).await,
        Some(Command::Eval(args)) => run_eval(args).await,
        Some(Command::Transmit(args)) => run_transmit(args).await,
        Some(Command::Logs(args)) => run_logs(args).await,
        Some(Command::Save(args)) => run_save(args).await,
        Some(Command::Undo(args)) => run_undo(args).await,
        Some(Command::Redo(args)) => run_redo(args).await,
        Some(Command::Waypoint(args)) => run_waypoint(args).await,
        Some(Command::Ping(args)) => run_ping(args).await,
        Some(Command::Version(args)) => run_version(args).await,
        Some(Command::Status(args)) => run_status(args).await,
        Some(Command::Doctor(args)) => run_doctor(args).await,
        Some(Command::Refresh(args)) => run_refresh(args),
        Some(Command::New(args)) => run_new(args).await,
        Some(Command::Rm(args)) => run_rm(args).await,
        Some(Command::Mv(args)) => run_mv(args).await,
        Some(Command::Attr(args)) => run_attr(args).await,
        Some(Command::Tag(args)) => run_tag(args).await,
        Some(Command::Call(args)) => run_call(args).await,
        Some(Command::Select(args)) => run_select(args).await,
        Some(Command::Copy(args)) => studio_clipboard::run_copy(args).await,
        Some(Command::Paste(args)) => studio_clipboard::run_paste(args).await,
        Some(Command::Classinfo(args)) => run_classinfo(args).await,
        Some(Command::Enums(args)) => run_enums(args).await,
        Some(Command::Enum(args)) => run_enum(args).await,
        Some(Command::FindAttr(args)) => run_find_attr(args).await,
        Some(Command::Lint(args)) => run_lint(args).await,
        None => {
            // Back-compat: bare invocation runs the daemon using top-level flags.
            let project = cli.project.ok_or_else(|| -> Box<dyn std::error::Error> {
                "missing --project (required for daemon mode; use a subcommand for other modes)"
                    .into()
            })?;
            run_serve(ServeArgs {
                project,
                port: cli.port,
                game_id: cli.game_id,
                group_id: cli.group_id,
                place_id: cli.place_id,
                projects_root: None,
                widget_owned: false,
                owner_token: None,
                owner_token_state_file: None,
                managed: false,
                managed_by: None,
                control_token: None,
                control_token_env: None,
                boot_id: None,
                runtime_record: None,
                log_path: None,
                started_at: None,
            })
            .await
        }
    }
}

fn run_init(args: InitArgs) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&args.project).map_err(|error| {
        format!(
            "init: create project directory {}: {error}",
            args.project.display()
        )
    })?;
    let project = lifecycle::canonical_project(&args.project)
        .map_err(|error| format!("init: canonicalize {}: {error}", args.project.display()))?;
    let mut config = project_config::load_or_create(&project)
        .map_err(|error| format!("init: load ro-sync.json: {error}"))?;
    let mut config_changed = false;
    if let Some(name) = args.name {
        let name = name.trim();
        if name.is_empty() {
            return Err("init: --name cannot be empty".into());
        }
        if config.name != name {
            config.name = name.to_string();
            config_changed = true;
        }
    }
    config_changed |= project_config::apply_overrides(
        &mut config,
        args.game_id,
        args.group_id,
        (!args.place_id.is_empty()).then_some(args.place_id),
    );
    if config_changed {
        project_config::write(&project, &config)
            .map_err(|error| format!("init: write ro-sync.json: {error}"))?;
    }

    let ro_sync_md = snapshot::write_ro_sync_md_if_missing(&project)?;
    let claude_md = snapshot::write_claude_md_if_missing_or_merge(&project)?;
    let codex_context = snapshot::write_codex_context_if_missing_or_merge(&project)?;
    let tooling = snapshot::write_project_tooling_if_missing_or_merge(&project)?;
    let value = serde_json::json!({
        "ok": true,
        "project": project.display().to_string(),
        "config": project.join(project_config::CONFIG_FILE).display().to_string(),
        "changed": {
            "config": config_changed,
            "roSyncMd": ro_sync_md,
            "claudeMd": claude_md,
            "codexContext": codex_context,
            "tooling": tooling,
        },
    });
    if args.raw {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!("Initialized Ro Sync project at {}.", project.display());
    }
    Ok(())
}

fn run_plugin(args: PluginArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        PluginCommand::Install(args) => plugin_install(args),
        PluginCommand::Status(args) => plugin_status(args),
    }
}

fn bundled_plugin_path(
    explicit: Option<&std::path::Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path.to_path_buf());
    } else {
        if let Some(path) = std::env::var_os("ROSYNC_PLUGIN_PATH") {
            if !path.is_empty() {
                candidates.push(PathBuf::from(path));
            }
        }
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                candidates.push(parent.join("plugin").join("Plugin.rbxm"));
                if let Some(grandparent) = parent.parent() {
                    candidates.push(grandparent.join("plugin").join("Plugin.rbxm"));
                    // macOS app bundle layout: the binary lives in
                    // Contents/MacOS while its resources sit in
                    // Contents/Resources, so neither the exe directory nor its
                    // parent contains plugin/. Without this the bundled CLI
                    // could not find the plugin it ships with.
                    candidates.push(
                        grandparent
                            .join("Resources")
                            .join("plugin")
                            .join("Plugin.rbxm"),
                    );
                }
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("plugin").join("Plugin.rbxm"));
            candidates.push(cwd.join("..").join("plugin").join("Plugin.rbxm"));
        }
        // A standalone binary copied onto PATH (for example ~/.local/bin/rosync)
        // has no sibling plugin/ directory at all. Fall back to the widget
        // install, which is where the artifact actually lives.
        if let Some(widget) = crate::lifecycle::legacy_widget_dir() {
            candidates.push(widget.join("plugin").join("Plugin.rbxm"));
        }
    }
    for candidate in &candidates {
        if candidate.is_file() {
            return std::fs::canonicalize(candidate).map_err(|error| {
                format!("plugin: resolve {}: {error}", candidate.display()).into()
            });
        }
    }
    let searched = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "plugin: bundled Plugin.rbxm was not found{}",
        if searched.is_empty() {
            String::new()
        } else {
            format!(" (searched {searched})")
        }
    )
    .into())
}

fn roblox_plugin_dir(
    explicit: Option<&std::path::Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = explicit {
        return if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(std::env::current_dir()?.join(path))
        };
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .map(|home| home.join("Documents").join("Roblox").join("Plugins"))
            .ok_or_else(|| "plugin: home directory not found".into())
    }
    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir()
            .map(|local| local.join("Roblox").join("Plugins"))
            .ok_or_else(|| "plugin: local app-data directory not found".into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("plugin: automatic Roblox Studio plugin installation is supported on macOS and Windows; pass --plugin-dir to override".into())
    }
}

fn file_sha256(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    use sha2::{Digest as _, Sha256};
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read {} for SHA-256: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn atomic_replace_bytes(
    path: &std::path::Path,
    bytes: &[u8],
    #[allow(unused_variables)] unix_mode: u32,
) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid filename"))?;
    let temporary = parent.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        unix_nanos()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(unix_mode);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        lifecycle::replace_file_atomic(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn plugin_install(args: PluginInstallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let source = bundled_plugin_path(args.source.as_deref())?;
    let directory = roblox_plugin_dir(args.plugin_dir.as_deref())?;
    let destination = directory.join("RoSync.rbxm");
    let bytes = std::fs::read(&source)
        .map_err(|error| format!("plugin install: read {}: {error}", source.display()))?;
    atomic_replace_bytes(&destination, &bytes, 0o644)
        .map_err(|error| format!("plugin install: write {}: {error}", destination.display()))?;
    for stale_name in ["RoSync.lua", "RoSync.luau"] {
        let stale = directory.join(stale_name);
        if stale.is_file() {
            std::fs::remove_file(&stale).map_err(|error| {
                format!("plugin install: remove stale {}: {error}", stale.display())
            })?;
        }
    }
    let sha256 = file_sha256(&destination)?;
    let value = serde_json::json!({
        "ok": true,
        "installed": true,
        "current": true,
        "source": source.display().to_string(),
        "path": destination.display().to_string(),
        "sha256": sha256,
        "restartRequired": true,
    });
    if args.raw {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!(
            "Installed Ro Sync Studio plugin at {}. Restart Roblox Studio to load it.",
            destination.display()
        );
    }
    Ok(())
}

fn plugin_status(args: PluginStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let source = bundled_plugin_path(args.source.as_deref())?;
    let directory = roblox_plugin_dir(args.plugin_dir.as_deref())?;
    let destination = directory.join("RoSync.rbxm");
    let expected_sha256 = file_sha256(&source)?;
    let installed_sha256 = destination
        .is_file()
        .then(|| file_sha256(&destination))
        .transpose()?;
    let installed = installed_sha256.is_some();
    let current = installed_sha256.as_deref() == Some(expected_sha256.as_str());
    let value = serde_json::json!({
        "ok": true,
        "installed": installed,
        "current": current,
        "source": source.display().to_string(),
        "path": destination.display().to_string(),
        "expectedSha256": expected_sha256,
        "installedSha256": installed_sha256,
        "restartRequired": installed && !current,
    });
    if args.raw {
        println!("{}", serde_json::to_string(&value)?);
    } else if current {
        println!(
            "Ro Sync Studio plugin is installed and current at {}.",
            destination.display()
        );
    } else if installed {
        println!(
            "Ro Sync Studio plugin is installed but outdated at {}.",
            destination.display()
        );
    } else {
        println!(
            "Ro Sync Studio plugin is not installed at {}.",
            destination.display()
        );
    }
    Ok(())
}

const OPEN_CLOUD_CREDENTIAL_KEY: &str = "robloxCloudApiKey";

fn run_auth(args: AuthArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        AuthCommand::Set(args) => auth_set(args),
        AuthCommand::Status(args) => auth_status(args),
        AuthCommand::Clear(args) => auth_clear(args),
    }
}

fn auth_store_path(
    data_dir: Option<&std::path::Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let state_dir = lifecycle::state_dir(data_dir)
        .map_err(|error| format!("auth: resolve Ro Sync state directory: {error}"))?;
    Ok(lifecycle::credentials_path(&state_dir))
}

fn auth_set(args: AuthSetArgs) -> Result<(), Box<dyn std::error::Error>> {
    let sources = usize::from(args.from_stdin)
        + usize::from(args.file.is_some())
        + usize::from(args.from_env.is_some());
    if sources != 1 {
        return Err("auth set: choose exactly one of --from-stdin, --file, or --from-env".into());
    }
    let mut credential = if args.from_stdin {
        use std::io::Read as _;
        let mut value = String::new();
        std::io::stdin()
            .take(64 * 1024 + 1)
            .read_to_string(&mut value)?;
        value
    } else if let Some(path) = args.file.as_ref() {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("auth set: inspect {}: {error}", path.display()))?;
        if metadata.len() > 64 * 1024 {
            return Err("auth set: credential file exceeds 64 KiB".into());
        }
        std::fs::read_to_string(path)
            .map_err(|error| format!("auth set: read {}: {error}", path.display()))?
    } else {
        read_named_secret_env(args.from_env.as_deref().unwrap_or_default(), "auth set")?
    };
    credential = credential.trim().to_string();
    if credential.is_empty() {
        return Err("auth set: credential is empty".into());
    }
    if credential.len() > 64 * 1024 {
        return Err("auth set: credential exceeds 64 KiB".into());
    }
    let path = auth_store_path(args.data_dir.as_deref())?;
    lifecycle::write_credential(&path, OPEN_CLOUD_CREDENTIAL_KEY, &credential)?;
    drop(credential);
    print_auth_result(&path, true, args.raw)
}

fn auth_status(args: AuthStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let path = auth_store_path(args.data_dir.as_deref())?;
    let configured = lifecycle::read_credential(&path, OPEN_CLOUD_CREDENTIAL_KEY)?.is_some();
    print_auth_result(&path, configured, args.raw)
}

fn auth_clear(args: AuthClearArgs) -> Result<(), Box<dyn std::error::Error>> {
    let path = auth_store_path(args.data_dir.as_deref())?;
    lifecycle::remove_credential(&path, OPEN_CLOUD_CREDENTIAL_KEY)?;
    print_auth_result(&path, false, args.raw)
}

fn print_auth_result(
    path: &std::path::Path,
    configured: bool,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let value = serde_json::json!({
        "ok": true,
        "configured": configured,
        "path": path.display().to_string(),
        "protection": "filesystem-permissions",
        "warning": "The CLI fallback store is protected by per-user filesystem permissions (0600 on Unix), not an OS keychain.",
    });
    if raw {
        println!("{}", serde_json::to_string(&value)?);
    } else if configured {
        println!(
            "Roblox Open Cloud credential is configured in {} (filesystem-permission protected; not an OS keychain).",
            path.display()
        );
    } else {
        println!("No CLI Roblox Open Cloud credential is configured.");
    }
    Ok(())
}

async fn run_serve(args: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.project.is_dir() {
        return Err(format!(
            "serve: project directory does not exist: {}",
            args.project.display()
        )
        .into());
    }
    let canonical_project = lifecycle::canonical_project(&args.project)
        .map_err(|error| format!("serve: canonicalize {}: {error}", args.project.display()))?;
    let projects_root = args
        .projects_root
        .as_deref()
        .map(project_init::resolve_projects_root)
        .transpose()
        .map_err(|error| format!("serve: {error}"))?;
    let widget_owner_token = resolve_widget_owner_token(
        args.owner_token.clone(),
        args.owner_token_state_file.as_deref(),
    )?;

    // Bind before generating project docs or starting the watcher. A port
    // collision must fail without mutating the project or leaving background
    // work behind.
    let requested_addr = format!("127.0.0.1:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&requested_addr)
        .await
        .map_err(|error| format!("serve: bind {requested_addr}: {error}"))?;
    let listen_port = listener.local_addr()?.port();

    if let Err(e) = snapshot::write_ro_sync_md_if_missing(&canonical_project) {
        eprintln!("rosync: failed to write ro-sync.md: {e}");
    }
    if let Err(e) = snapshot::write_claude_md_if_missing_or_merge(&canonical_project) {
        eprintln!("rosync: failed to write CLAUDE.md: {e}");
    }
    if let Err(e) = snapshot::write_codex_context_if_missing_or_merge(&canonical_project) {
        eprintln!("rosync: failed to write Codex context: {e}");
    }
    if let Err(e) = snapshot::write_project_tooling_if_missing_or_merge(&canonical_project) {
        eprintln!("rosync: failed to write project tooling files: {e}");
    }

    // Project config: load or create, then apply CLI overrides (persist if anything changed).
    let mut cfg = project_config::load_or_create(&canonical_project).map_err(|error| {
        format!(
            "serve: load {}: {error}",
            canonical_project.join("ro-sync.json").display()
        )
    })?;
    let place_ids_override = if args.place_id.is_empty() {
        None
    } else {
        Some(args.place_id.clone())
    };
    let changed = project_config::apply_overrides(
        &mut cfg,
        args.game_id.clone(),
        args.group_id.clone(),
        place_ids_override,
    );
    if changed {
        project_config::write(&canonical_project, &cfg)
            .map_err(|error| format!("serve: write ro-sync.json: {error}"))?;
    }

    // Filesystem bursts are drained into bounded multi-op WebSocket frames.
    // Keep room for scheduling jitter without retaining an entire 25k-file
    // install (or thousands of large Sources) as serialized strings.
    let (tx, _rx) = broadcast::channel::<String>(8_192);

    let watcher = Watch::new(canonical_project.clone())?;
    let canonical_project = watcher.root().to_path_buf();
    let conflict_engine = Arc::new(ConflictEngine::new());
    let push_quiet: Arc<Mutex<HashMap<PathBuf, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let (request_tx, _) = broadcast::channel::<RequestEnvelope>(256);
    let (shutdown_tx, shutdown_rx) = tokio_watch::channel::<Option<String>>(None);

    let managed = args.managed || args.widget_owned;
    let manager_owner_token = resolve_optional_secret(
        args.control_token.clone(),
        args.control_token_env.as_deref(),
        "serve control token",
    )?
    .or_else(|| widget_owner_token.clone());
    if let Some(control_token_env) = args.control_token_env.as_deref() {
        // The token has been copied into private process state. Remove the
        // source variable before this long-lived daemon launches analysis,
        // capture, or other helper processes that would otherwise inherit it.
        std::env::remove_var(control_token_env);
    }
    if managed && manager_owner_token.as_deref().is_none_or(str::is_empty) {
        return Err("serve: a managed daemon requires --control-token or --owner-token".into());
    }
    let managed_by = args.managed_by.clone().unwrap_or_else(|| {
        if args.widget_owned {
            "terminal64-widget".to_string()
        } else if managed {
            "unknown".to_string()
        } else {
            "manual".to_string()
        }
    });
    let boot_id = match args.boot_id.clone() {
        Some(boot_id) => boot_id,
        None => artifact::random_hex(32)?,
    };
    let started_at = args.started_at.unwrap_or_else(unix_secs);
    let process_id = std::process::id();

    let state = AppState {
        project: Arc::new(canonical_project.clone()),
        canonical_project: Arc::new(canonical_project.clone()),
        projects_root: Arc::new(projects_root),
        events: tx.clone(),
        conflict: conflict_engine.clone(),
        artifacts: artifact::ArtifactStore::new(
            canonical_project.join(".rosync-artifacts"),
            256 * 1024 * 1024,
            Duration::from_secs(5 * 60),
        )?,
        project_name: Arc::new(RwLock::new(cfg.name.clone())),
        game_id: Arc::new(RwLock::new(cfg.game_id.clone())),
        group_id: Arc::new(RwLock::new(cfg.group_id.clone())),
        place_ids: Arc::new(RwLock::new(cfg.place_ids.clone())),
        wally_enabled: Arc::new(RwLock::new(cfg.wally_enabled)),
        wally_folder: Arc::new(RwLock::new(cfg.wally_folder.clone())),
        pending_initial: Arc::new(Mutex::new(None)),
        push_quiet: push_quiet.clone(),
        request_tx,
        pending_routes: Arc::new(Mutex::new(HashMap::new())),
        active_plugin: Arc::new(Mutex::new(None)),
        widget_owned: args.widget_owned,
        managed,
        managed_by: Arc::new(managed_by.clone()),
        boot_id: Arc::new(boot_id.clone()),
        listen_port,
        process_id,
        started_at,
        manager_owner_token: Arc::new(manager_owner_token.clone()),
        manager_last_seen: Arc::new(Mutex::new(managed.then(Instant::now))),
        widget_owner_token: Arc::new(widget_owner_token),
        widget_last_seen: Arc::new(Mutex::new(args.widget_owned.then(Instant::now))),
        shutdown_tx,
    };

    let runtime_guard = if let Some(record_path) = args.runtime_record.as_ref() {
        if !managed {
            return Err("serve: --runtime-record requires a managed daemon".into());
        }
        let control_token = manager_owner_token
            .clone()
            .ok_or("serve: managed runtime record requires a control token")?;
        let log_path = args
            .log_path
            .clone()
            .unwrap_or_else(|| record_path.with_extension("log"));
        let record = lifecycle::RuntimeRecord {
            version: lifecycle::RUNTIME_RECORD_VERSION,
            project: canonical_project.display().to_string(),
            canonical_project: canonical_project.display().to_string(),
            pid: process_id,
            port: listen_port,
            boot_id: boot_id.clone(),
            control_token,
            managed_by: managed_by.clone(),
            log_path: log_path.display().to_string(),
            started_at,
        };
        lifecycle::write_record(record_path, &record).map_err(|error| {
            format!(
                "serve: write runtime record {}: {error}",
                record_path.display()
            )
        })?;
        Some(lifecycle::RuntimeRecordGuard::new(
            record_path.clone(),
            boot_id.clone(),
        ))
    } else {
        None
    };

    spawn_watch_bridge(
        watcher,
        canonical_project.clone(),
        tx.clone(),
        conflict_engine.clone(),
        push_quiet.clone(),
    )
    .map_err(|error| format!("serve: validate watched filesystem: {error}"))?;
    spawn_config_hot_reload(state.clone());
    if args.widget_owned {
        spawn_widget_owner_watchdog(state.clone());
    } else if managed_by == "desktop" {
        spawn_desktop_owner_watchdog(state.clone());
    }

    let addr = format!("127.0.0.1:{listen_port}");
    eprintln!(
        "rosync listening on http://{} (project: {})",
        addr,
        canonical_project.display()
    );

    let app = http::router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(serve_shutdown_signal(tx.clone(), shutdown_rx))
        .await?;
    drop(runtime_guard);
    Ok(())
}

async fn serve_shutdown_signal(
    events: broadcast::Sender<String>,
    mut shutdown_rx: tokio_watch::Receiver<Option<String>>,
) {
    let reason = tokio::select! {
        _ = wait_for_shutdown_signal() => "daemon shutting down".to_string(),
        changed = shutdown_rx.changed() => {
            match changed {
                Ok(()) => shutdown_rx
                    .borrow()
                    .clone()
                    .unwrap_or_else(|| "daemon shutting down".to_string()),
                Err(_) => "daemon shutting down".to_string(),
            }
        },
    };
    let _ = events.send(
        serde_json::json!({
            "type": "shutdown",
            "reason": reason,
        })
        .to_string(),
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
}

fn spawn_widget_owner_watchdog(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(OWNER_HEARTBEAT_CHECK_INTERVAL);
        let mut suspect_since = None;
        loop {
            interval.tick().await;
            let last_seen = *state.widget_last_seen.lock().unwrap();
            if owner_heartbeat_expired(last_seen, WIDGET_HEARTBEAT_TIMEOUT) {
                let plugin_connected = state.active_plugin.lock().unwrap().is_some();
                if plugin_connected {
                    suspect_since = None;
                    continue;
                }
                let first_suspect = *suspect_since.get_or_insert_with(Instant::now);
                if !owner_heartbeat_should_shutdown(
                    last_seen,
                    Some(first_suspect),
                    WIDGET_HEARTBEAT_TIMEOUT,
                    OWNER_HEARTBEAT_SUSPECT_GRACE,
                ) {
                    continue;
                }
                let _ = state
                    .shutdown_tx
                    .send(Some("widget heartbeat lost".to_string()));
                break;
            } else {
                suspect_since = None;
            }
        }
    });
}

fn spawn_desktop_owner_watchdog(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(OWNER_HEARTBEAT_CHECK_INTERVAL);
        let mut suspect_since = None;
        loop {
            interval.tick().await;
            let last_seen = *state.manager_last_seen.lock().unwrap();
            if owner_heartbeat_expired(last_seen, DESKTOP_HEARTBEAT_TIMEOUT) {
                let first_suspect = *suspect_since.get_or_insert_with(Instant::now);
                if !owner_heartbeat_should_shutdown(
                    last_seen,
                    Some(first_suspect),
                    DESKTOP_HEARTBEAT_TIMEOUT,
                    OWNER_HEARTBEAT_SUSPECT_GRACE,
                ) {
                    continue;
                }
                let _ = state
                    .shutdown_tx
                    .send(Some("desktop heartbeat lost".to_string()));
                break;
            } else {
                suspect_since = None;
            }
        }
    });
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = async {
            if let Some(signal) = terminate.as_mut() {
                signal.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {},
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn run_query(args: QueryArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !(1..=10_000).contains(&args.limit) {
        return Err("query: --limit must be between 1 and 10000".into());
    }
    let response = remote::request(
        args.port,
        "query",
        serde_json::json!({
            "selector": args.selector,
            "props": args.props,
            "attributes": args.attributes,
            "tags": args.tags,
            "limit": args.limit,
        }),
    )
    .await?;
    let value = response_value_or_err(&response, "query")?;
    let matches = value
        .get("matches")
        .and_then(serde_json::Value::as_array)
        .ok_or("query: plugin returned an invalid matches payload")?;
    let truncated = value
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    match args.format {
        QueryFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        QueryFormat::Paths => {
            for m in matches {
                if let Some(path) = m.get("path").and_then(serde_json::Value::as_str) {
                    println!("{path}");
                }
            }
        }
        QueryFormat::Classes => {
            for m in matches {
                let class = m
                    .get("class")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let path = m
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                println!("{class}\t{path}");
            }
        }
    }
    if truncated {
        let reason = value
            .get("truncationReason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let guidance = match reason {
            "matches" => "raise --limit (up to 10000) or narrow the selector",
            "nodes" | "time" => "narrow the selector to reduce Studio traversal",
            "response-bytes" => "request fewer properties/attributes/tags or narrow the selector",
            _ => "narrow the selector",
        };
        eprintln!(
            "query: results were truncated after {} match(es) ({reason}); {guidance}",
            matches.len()
        );
    }
    Ok(())
}

async fn run_path(args: PathArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resolved =
        resolve_live_path(args.port, &args.project, &args.target, args.from, "path").await?;
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "inputKind": resolved.input_kind.as_str(),
                "studioPath": resolved.studio_path,
                "studioPathString": resolved.studio_path_string(),
                "class": resolved.class,
                "fsPath": resolved.fs_path,
                "fsExists": resolved.fs_exists,
            }))?
        );
    } else if resolved.input_kind == path_resolver::PathInputKind::Studio {
        println!("{}", resolved.fs_path.display());
    } else {
        println!("{}", resolved.studio_path_string());
    }
    Ok(())
}

async fn live_tree(
    port: u16,
    context: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let resp = remote::request(
        port,
        "tree",
        serde_json::json!({ "path": "", "depth": u32::MAX }),
    )
    .await
    .map_err(|e| format!("{context}: live tree request failed: {e}"))?;
    let tree = response_value_or_err(&resp, &format!("{context} tree"))?;
    Ok(tree)
}

async fn resolve_live_path(
    port: u16,
    project: &std::path::Path,
    target: &str,
    from: path_resolver::PathInputKind,
    context: &str,
) -> Result<path_resolver::ResolvedPath, Box<dyn std::error::Error>> {
    let tree = live_tree(port, context).await?;
    path_resolver::resolve_with_tree(project, &tree, target, from)
        .map_err(|e| format!("{context}: {e}").into())
}

fn run_commands(args: CommandsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let bundle: serde_json::Value = serde_json::from_str(COMMANDS_BUNDLE_JSON)
        .map_err(|e| format!("commands: embedded command registry is invalid: {e}"))?;
    if args.compact {
        println!(
            "{}",
            serde_json::to_string_pretty(&compact_command_registry(
                &bundle,
                args.name.as_deref()
            )?)?
        );
        return Ok(());
    }
    let Some(name) = args.name.as_deref() else {
        println!("{}", serde_json::to_string_pretty(&bundle)?);
        return Ok(());
    };
    let commands = bundle
        .get("commands")
        .and_then(|value| value.as_array())
        .ok_or("commands: embedded registry missing commands array")?;
    let Some(command) = commands
        .iter()
        .find(|command| command.get("name").and_then(|value| value.as_str()) == Some(name))
    else {
        return Err(format!("commands: unknown command {name:?}").into());
    };
    println!("{}", serde_json::to_string_pretty(command)?);
    Ok(())
}

fn run_context(args: ContextArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = project_or_cwd(args.project.as_deref(), "context")?;
    let canonical_project = canonicalize_project_path(&project);
    let command_bundle: serde_json::Value = serde_json::from_str(COMMANDS_BUNDLE_JSON)
        .map_err(|e| format!("context: embedded command registry is invalid: {e}"))?;
    let command_names = command_names_from_bundle(&command_bundle);
    let config = match project_config::read_from_disk(&project) {
        Ok(Some(cfg)) => serde_json::json!({
            "ok": true,
            "name": cfg.name,
            "gameId": cfg.game_id,
            "groupId": cfg.group_id,
            "placeIds": cfg.place_ids,
            "wallyEnabled": cfg.wally_enabled,
            "wallyFolder": cfg.wally_folder,
            "version": cfg.version,
        }),
        Ok(None) => serde_json::json!({ "ok": false, "missing": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    };
    let daemon_hello = fetch_daemon_hello(args.port);
    let daemon_project_mismatch = match &daemon_hello {
        Ok(value) => daemon_project_mismatch(value, &canonical_project),
        Err(_) => serde_json::Value::Null,
    };
    let daemon = match daemon_hello {
        Ok(value) => serde_json::json!({
            "reachable": true,
            "hello": value,
            "projectMismatch": daemon_project_mismatch,
        }),
        Err(e) => serde_json::json!({ "reachable": false, "error": e }),
    };
    let conflicts = match http_get_json(args.port, "/resolve") {
        Ok(value) => {
            let count = value
                .get("conflicts")
                .and_then(|value| value.as_array())
                .map(|items| items.len())
                .unwrap_or(0);
            serde_json::json!({ "reachable": true, "count": count, "response": value })
        }
        Err(e) => serde_json::json!({ "reachable": false, "error": e }),
    };

    let mut commands = serde_json::json!({
        "count": command_names.len(),
        "names": command_names,
        "registryCommand": "rosync commands",
        "compactRegistryCommand": "rosync commands --compact",
        "singleCommandExample": "rosync commands get",
        "llmPolicy": {
            "startup": "Use `rosync context --project .` once, then `rosync commands --compact` only when choosing command families.",
            "lookup": "Use `rosync commands <name>` for exact flags. Avoid plain `rosync commands` unless a full registry dump is explicitly needed.",
            "cheapFirst": ["tree", "ls", "query", "path", "meta", "services", "status --raw"],
            "targetedReads": ["get --prop", "props", "read local files directly", "lint touched paths"],
            "expensiveReads": ["changes", "diff --raw", "snapshot", "get without --prop", "source live only on suspected divergence", "logs --tail", "watch", "conflicts only when resolving a reported conflict"],
            "mutationRule": "Before mutating Studio, inspect the target with focused live reads, use waypoints for multi-step edits, and only run the mutating command after explicit user intent. Use `rosync plan` only when a dry-run explanation is useful."
        },
    });
    if args.full_commands {
        commands["registry"] = command_bundle;
    }

    let context = serde_json::json!({
        "schema": "ro-sync.context.v1",
        "generatedAtUnix": unix_secs(),
        "project": {
            "path": project.display().to_string(),
            "canonicalPath": canonical_project.display().to_string(),
            "exists": project.exists(),
            "isDirectory": project.is_dir(),
            "config": config,
        },
        "daemon": {
            "port": args.port,
            "status": daemon,
        },
        "sync": {
            "services": context_services(&project),
            "files": context_project_files(&project),
            "conflicts": conflicts,
        },
        "commands": commands,
        "nextActions": [
            "Use `rosync commands <name>` for exact command usage JSON.",
            "Before mutating Studio, inspect the exact target with focused live reads; use `rosync plan` only when a dry-run explanation is useful.",
            "Use `rosync status --raw` or `rosync doctor --raw` when a health check is needed.",
            "Use `rosync changes --raw` before choosing Keep Disk or Keep Studio."
        ],
    });

    println!("{}", serde_json::to_string_pretty(&context)?);
    Ok(())
}

fn run_plan(args: PlanArgs) -> Result<(), Box<dyn std::error::Error>> {
    let plan = match args.command {
        PlanCommand::Set(args) => plan_set(args)?,
        PlanCommand::New(args) => plan_new(args)?,
        PlanCommand::Rm(args) => plan_rm(args),
        PlanCommand::Mv(args) => plan_mv(args),
        PlanCommand::Resolve(args) => plan_resolve(args)?,
    };
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}

fn plan_set(args: PlanSetArgs) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let value: serde_json::Value = serde_json::from_str(&args.value)
        .map_err(|e| format!("plan set: --value must be a JSON literal ({e})"))?;
    let mut risks = Vec::new();
    if args.prop == "Parent" {
        risks.push("raw Parent writes are blocked by `rosync set`; use `rosync mv` instead");
    }
    Ok(plan_json(
        "set",
        serde_json::json!({
            "path": args.path,
            "prop": args.prop,
            "value": value,
        }),
        vec!["studio"],
        vec!["studio_connected"],
        risks,
        format!(
            "rosync set --path {} --prop {} --value {}",
            shell_quote(&args.path),
            shell_quote(&args.prop),
            shell_quote(&args.value)
        ),
    ))
}

fn plan_new(args: PlanNewArgs) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let props = match args.props.as_deref() {
        Some(raw) => {
            let parsed: serde_json::Value = serde_json::from_str(raw)
                .map_err(|e| format!("plan new: --props must be a JSON object ({e})"))?;
            if !parsed.is_object() {
                return Err("plan new: --props must be a JSON object".into());
            }
            Some(parsed)
        }
        None => None,
    };
    let mut command = format!(
        "rosync new --path {} --class {}",
        shell_quote(&args.path),
        shell_quote(&args.class)
    );
    if let Some(name) = &args.name {
        command.push_str(&format!(" --name {}", shell_quote(name)));
    }
    if let Some(raw) = &args.props {
        command.push_str(&format!(" --props {}", shell_quote(raw)));
    }
    Ok(plan_json(
        "new",
        serde_json::json!({
            "parentPath": args.path,
            "class": args.class,
            "name": args.name,
            "props": props,
        }),
        vec!["studio"],
        vec!["studio_connected"],
        Vec::new(),
        command,
    ))
}

fn plan_rm(args: PlanRmArgs) -> serde_json::Value {
    plan_json(
        "rm",
        serde_json::json!({ "path": args.path }),
        vec!["studio"],
        vec!["studio_connected"],
        vec!["destructive: destroys the target instance in Studio"],
        format!("rosync rm --path {}", shell_quote(&args.path)),
    )
}

fn plan_mv(args: PlanMvArgs) -> serde_json::Value {
    let mut risks = Vec::new();
    if service_segment(&args.from) != service_segment(&args.to) && !args.force {
        risks.push("cross-service move will be rejected unless `--force` is supplied");
    }
    let mut command = format!(
        "rosync mv --from {} --to {}",
        shell_quote(&args.from),
        shell_quote(&args.to)
    );
    if args.force {
        command.push_str(" --force");
    }
    plan_json(
        "mv",
        serde_json::json!({
            "from": args.from,
            "to": args.to,
            "force": args.force,
        }),
        vec!["studio"],
        vec!["studio_connected"],
        risks,
        command,
    )
}

fn plan_resolve(args: PlanResolveArgs) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let choice = match (args.disk, args.studio) {
        (true, false) => "disk",
        (false, true) => "studio",
        _ => return Err("plan resolve: pass exactly one of --disk or --studio".into()),
    };
    let mut command = format!("rosync resolve --path {}", shell_quote(&args.path));
    if args.disk {
        command.push_str(" --disk");
    } else {
        command.push_str(" --studio");
    }
    Ok(plan_json(
        "resolve",
        serde_json::json!({
            "path": args.path,
            "choice": choice,
        }),
        vec!["disk", "studio"],
        vec!["daemon_reachable", "parked_conflict"],
        Vec::new(),
        command,
    ))
}

fn plan_json(
    op: &str,
    args: serde_json::Value,
    mutates: Vec<&str>,
    requires: Vec<&str>,
    risks: Vec<&str>,
    command: String,
) -> serde_json::Value {
    serde_json::json!({
        "schema": "ro-sync.plan.v1",
        "ok": true,
        "readOnly": true,
        "createdAtUnix": unix_secs(),
        "operation": op,
        "args": args,
        "mutates": mutates,
        "requires": requires,
        "risks": risks,
        "executeCommand": command,
        "notes": [
            "This plan does not execute anything.",
            "Review `mutates`, `requires`, and `risks` before running `executeCommand`."
        ],
    })
}

fn service_segment(path: &str) -> Option<&str> {
    path.split('/').find(|part| !part.is_empty())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Remote-control subcommands (get / set / ls / tree / find / eval)
//
// Each of these boils down to a single WS request/response round-trip against
// the running daemon, which forwards the op to the plugin. Writes additionally
// get logged to `~/.terminal64/widgets/ro-sync/writes.log` via `POST /writelog`.
// ---------------------------------------------------------------------------

async fn run_get(args: GetArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut req_args = serde_json::json!({ "path": args.path });
    if let Some(prop) = &args.prop {
        req_args["prop"] = serde_json::Value::String(prop.clone());
    }
    let resp = remote::request(args.port, "get", req_args).await?;
    print_response(&resp, args.raw, |v| print_get(&args, v));
    ok_or_err(&resp)
}

async fn run_set(args: SetArgs) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(batch_path) = args.batch.clone() {
        return run_set_batch(args, batch_path).await;
    }
    let path = args.path.clone().ok_or("set: --path is required")?;
    let prop = args.prop.clone().ok_or("set: --prop is required")?;
    if prop == "Parent" && !args.force_parent {
        eprintln!("========================================================");
        eprintln!("  rosync set: refusing to assign .Parent from the CLI.");
        eprintln!();
        eprintln!("  Reparenting via raw property writes is the single most");
        eprintln!("  common way to corrupt a DataModel. Use `rosync mv` to");
        eprintln!("  reparent safely, or re-run with `--force-parent` if");
        eprintln!("  you really need the raw write.");
        eprintln!("========================================================");
        return Err(format!(
            "set: refusing to set .Parent on {} without --force-parent (use `rosync mv` instead)",
            path
        )
        .into());
    }
    let value_raw = args
        .value
        .clone()
        .ok_or("set: --value is required (JSON literal)")?;
    let value: serde_json::Value = serde_json::from_str(&value_raw)
        .map_err(|e| format!("set: --value must be a JSON literal ({e})"))?;
    let req_args = serde_json::json!({
        "path": path,
        "prop": prop,
        "value": value,
    });
    let waypoint = args.waypoint.clone();
    if let Some(name) = &waypoint {
        send_waypoint(args.port, &format!("{name} (start)")).await?;
    }
    let resp = remote::request(args.port, "set", req_args).await?;
    // Plugin POSTs to /writelog itself on successful writes; the CLI doesn't
    // duplicate the entry.
    print_response(&resp, args.raw, |v| print_set(&path, &prop, v));
    if let Some(name) = &waypoint {
        send_waypoint(args.port, &format!("{name} (end)")).await?;
    }
    ok_or_err(&resp)
}

/// Best-effort `waypoint` call. Logged on failure but doesn't abort the
/// primary write — a dropped waypoint only costs undo granularity.
async fn send_waypoint(port: u16, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let req_args = serde_json::json!({ "name": name });
    match remote::request(port, "waypoint", req_args).await {
        Ok(resp) => {
            let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            if !ok {
                let err = response_error_message(&resp);
                eprintln!("warning: waypoint {name:?}: {err}");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("warning: waypoint {name:?}: {e}");
            Ok(())
        }
    }
}

async fn run_set_batch(
    args: SetArgs,
    batch_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(&batch_path)
        .map_err(|e| format!("read {}: {e}", batch_path.display()))?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| {
        format!(
            "parse {}: {e} (expected a JSON array)",
            batch_path.display()
        )
    })?;
    if !args.force_parent {
        if let Some((index, path)) = entries.iter().enumerate().find_map(|(index, entry)| {
            (entry.get("prop").and_then(|value| value.as_str()) == Some("Parent")).then(|| {
                (
                    index,
                    entry
                        .get("path")
                        .and_then(|value| value.as_str())
                        .unwrap_or("<missing path>"),
                )
            })
        }) {
            return Err(format!(
                "set: refusing batch entry {} that assigns .Parent on {} without --force-parent (use `rosync mv` instead)",
                index + 1,
                path
            )
            .into());
        }
    }
    let total = entries.len();
    let mut ok_count = 0usize;
    let mut fail_count = 0usize;
    let waypoint = args.waypoint.clone();
    if let Some(name) = &waypoint {
        send_waypoint(args.port, &format!("{name} (start)")).await?;
    }
    for (i, entry) in entries.iter().enumerate() {
        let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let prop = entry.get("prop").and_then(|v| v.as_str()).unwrap_or("");
        let value = entry
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if path.is_empty() || prop.is_empty() {
            let msg = format!("[{}/{total}] invalid entry (missing path/prop)", i + 1);
            eprintln!("{msg}");
            fail_count += 1;
            if !args.keep_going {
                return Err(msg.into());
            }
            continue;
        }
        let req_args = serde_json::json!({ "path": path, "prop": prop, "value": value });
        eprintln!("[{}/{total}] set {path}.{prop}", i + 1);
        match remote::request(args.port, "set", req_args).await {
            Ok(resp) => {
                let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                if ok {
                    ok_count += 1;
                } else {
                    fail_count += 1;
                    let err = response_error_message(&resp);
                    eprintln!("  ! {err}");
                    if !args.keep_going {
                        return Err(format!("aborting at entry {}/{total}: {err}", i + 1).into());
                    }
                }
            }
            Err(e) => {
                fail_count += 1;
                eprintln!("  ! {e}");
                if !args.keep_going {
                    return Err(e.into());
                }
            }
        }
    }
    if let Some(name) = &waypoint {
        send_waypoint(args.port, &format!("{name} (end)")).await?;
    }
    eprintln!("batch done: {ok_count} ok, {fail_count} failed ({total} total)");
    if fail_count > 0 && !args.keep_going {
        return Err("batch completed with failures".into());
    }
    Ok(())
}

async fn run_ls(args: LsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req_args = serde_json::json!({ "path": args.path });
    let resp = remote::request(args.port, "ls", req_args).await?;
    print_response(&resp, args.raw, print_ls);
    ok_or_err(&resp)
}

async fn run_tree(args: TreeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req_args = serde_json::json!({ "path": args.path, "depth": args.depth });
    let resp = remote::request(args.port, "tree", req_args).await?;
    print_response(&resp, args.raw, |v| print_tree(v, 0));
    ok_or_err(&resp)
}

async fn run_snapshot(args: SnapshotArgs) -> Result<(), Box<dyn std::error::Error>> {
    const INSPECTION_CONCURRENCY: usize = 16;

    let timestamp = unix_secs();
    let output = snapshot_output_path(args.output.as_deref(), args.project.as_deref(), timestamp)?;
    // Whole-place snapshots can contain tens of thousands of instances. Opening
    // a fresh WebSocket for every inspection made connection setup dominate the
    // command (Switch and Shoot has 11k+ nodes). Keep one authenticated session
    // for the tree and every subsequent get instead.
    let mut session = remote::RemoteSession::connect(args.port)
        .await
        .map_err(|e| format!("snapshot: connect failed: {e}"))?;
    let tree_resp = session
        .request(
            "tree",
            serde_json::json!({ "path": "", "depth": u32::MAX }),
            Duration::from_secs(5),
        )
        .await
        .map_err(|e| format!("snapshot: tree request failed: {e}"))?;
    let tree = response_value_or_err(&tree_resp, "snapshot tree")?;
    let _ = session.close().await;

    let mut paths = Vec::new();
    collect_snapshot_paths(&tree, "", &mut paths);
    // Roblox permits same-name siblings, so the human-readable path projection
    // is not unique. Repeating the same ambiguous get cannot distinguish those
    // siblings and previously accounted for almost half the live requests in a
    // large place. Inspect each resolvable path once and share that result with
    // the equivalent tree nodes, preserving the command's existing semantics.
    let mut seen_paths = HashSet::new();
    let unique_paths = paths
        .iter()
        .filter(|path| seen_paths.insert((*path).clone()))
        .cloned()
        .collect::<Vec<_>>();
    let duplicate_path_count = paths.len().saturating_sub(unique_paths.len());

    let worker_count = INSPECTION_CONCURRENCY.min(unique_paths.len().max(1));
    let mut partitions = vec![Vec::new(); worker_count];
    for (index, path) in unique_paths.iter().cloned().enumerate() {
        partitions[index % worker_count].push(path);
    }
    // A snapshot enumerates the tree first and inspects afterwards, so any
    // instance that disappears in between yields NOT_FOUND on its get. Studio
    // services such as Stats churn constantly (Stats/PerformanceStats/Memory/...
    // is rebuilt continuously), which made a whole-DataModel snapshot abort
    // essentially every time. A vanished path is expected, not fatal: skip it,
    // keep the rest, and report how many were dropped.
    let worker_results = futures::future::try_join_all(
        partitions
            .into_iter()
            .map(|partition| inspect_snapshot_partition(args.port, partition)),
    )
    .await
    .map_err(|error| format!("snapshot: {error}"))?;
    let mut inspections = BTreeMap::new();
    let mut skipped = Vec::new();
    for (worker_inspections, worker_skipped) in worker_results {
        inspections.extend(worker_inspections);
        skipped.extend(worker_skipped);
    }
    skipped.sort();
    if !skipped.is_empty() && !args.raw {
        eprintln!(
            "note: {} instance(s) vanished while snapshotting and were skipped (e.g. {})",
            skipped.len(),
            skipped
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let root = build_snapshot_node(&tree, "", &inspections);
    let mut body = serde_json::Map::new();
    body.insert("schema".into(), serde_json::json!("ro-sync.snapshot.v1"));
    body.insert("captured_at_unix".into(), serde_json::json!(timestamp));
    body.insert("source".into(), serde_json::json!({ "port": args.port }));
    body.insert("root".into(), root);
    let snapshot = serde_json::Value::Object(body);
    let text = format!("{}\n", serde_json::to_string_pretty(&snapshot)?);
    std::fs::write(&output, text)
        .map_err(|e| format!("snapshot: write {}: {e}", output.display()))?;

    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "output": output,
                "nodes": paths.len(),
                "uniquePaths": unique_paths.len(),
                "duplicatePaths": duplicate_path_count,
                "inspected": inspections.len(),
                "skipped": skipped.len(),
                "skippedPaths": skipped,
            }))?
        );
    } else if skipped.is_empty() {
        println!(
            "snapshot: wrote {} ({} nodes)",
            output.display(),
            paths.len()
        );
    } else {
        println!(
            "snapshot: wrote {} ({} nodes, {} skipped)",
            output.display(),
            paths.len(),
            skipped.len()
        );
    }
    Ok(())
}

async fn inspect_snapshot_partition(
    port: u16,
    paths: Vec<String>,
) -> Result<(BTreeMap<String, serde_json::Value>, Vec<String>), String> {
    let mut session = remote::RemoteSession::connect(port)
        .await
        .map_err(|error| format!("inspection worker connect failed: {error}"))?;
    let mut inspections = BTreeMap::new();
    let mut skipped = Vec::new();
    for path in paths {
        let label = snapshot_path_label(&path);
        let resp = session
            .request(
                "get",
                serde_json::json!({ "path": &path }),
                Duration::from_secs(5),
            )
            .await
            .map_err(|error| format!("get {label} failed: {error}"))?;
        if response_is_not_found(&resp) {
            skipped.push(label.to_string());
            continue;
        }
        let value = response_value_or_err(&resp, &format!("snapshot get {label}"))
            .map_err(|error| error.to_string())?;
        inspections.insert(path, value);
    }
    let _ = session.close().await;
    Ok((inspections, skipped))
}

fn snapshot_output_path(
    output: Option<&std::path::Path>,
    project: Option<&std::path::Path>,
    timestamp: u64,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let filename = format!("rosync-snapshot-{timestamp}.json");
    if let Some(path) = output {
        if path.is_dir() {
            return Ok(path.join(filename));
        }
        return Ok(path.to_path_buf());
    }
    let dir = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(|e| format!("snapshot: current directory: {e}"))?,
    };
    Ok(dir.join(filename))
}

fn response_value_or_err(
    resp: &serde_json::Value,
    context: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Ok(resp
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null));
    }
    Err(format!("{context}: {}", response_error_message(resp)).into())
}

fn response_is_not_found(resp: &serde_json::Value) -> bool {
    remote::plugin_error(resp).is_some_and(|error| {
        matches!(
            error.code.as_deref(),
            Some("NOT_FOUND" | "INSTANCE_NOT_FOUND")
        )
    })
}

fn response_error_message(resp: &serde_json::Value) -> String {
    remote::plugin_error(resp)
        .map(|error| error.to_string())
        .unwrap_or_else(|| "request failed".to_string())
}

fn collect_snapshot_paths(node: &serde_json::Value, path: &str, out: &mut Vec<String>) {
    out.push(path.to_string());
    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for child in children {
            let child_path = snapshot_child_path(path, child);
            collect_snapshot_paths(child, &child_path, out);
        }
    }
}

fn build_snapshot_node(
    node: &serde_json::Value,
    path: &str,
    inspections: &BTreeMap<String, serde_json::Value>,
) -> serde_json::Value {
    let inspect = inspections.get(path);
    let class = inspect
        .and_then(|v| v.get("class"))
        .or_else(|| node.get("class"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!("?"));
    let name = inspect
        .and_then(|v| v.get("name"))
        .or_else(|| node.get("name"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!("?"));
    let resolved_path = inspect
        .and_then(|v| v.get("path"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!(path));

    let mut children: Vec<(&serde_json::Value, String)> = node
        .get("children")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|child| {
                    let child_path = snapshot_child_path(path, child);
                    (child, child_path)
                })
                .collect()
        })
        .unwrap_or_default();
    children.sort_by(|(a, a_path), (b, b_path)| {
        let a_name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let b_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let a_class = a.get("class").and_then(|v| v.as_str()).unwrap_or("");
        let b_class = b.get("class").and_then(|v| v.as_str()).unwrap_or("");
        (a_name, a_class, a_path).cmp(&(b_name, b_class, b_path))
    });
    let child_values: Vec<serde_json::Value> = children
        .iter()
        .map(|(child, child_path)| build_snapshot_node(child, child_path, inspections))
        .collect();

    let mut out = serde_json::Map::new();
    out.insert("class".into(), class);
    out.insert("name".into(), name);
    out.insert("path".into(), resolved_path);
    out.insert(
        "properties".into(),
        normalize_snapshot_value(
            inspect
                .and_then(|v| v.get("properties"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        ),
    );
    out.insert(
        "attributes".into(),
        normalize_snapshot_value(
            inspect
                .and_then(|v| v.get("attributes"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        ),
    );
    out.insert("tags".into(), sorted_snapshot_tags(inspect));
    out.insert("children".into(), serde_json::Value::Array(child_values));
    serde_json::Value::Object(out)
}

fn snapshot_child_path(parent_path: &str, child: &serde_json::Value) -> String {
    let name = child.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
    }
}

fn normalize_snapshot_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                out.insert(key, normalize_snapshot_value(value));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(normalize_snapshot_value).collect())
        }
        other => other,
    }
}

fn sorted_snapshot_tags(inspect: Option<&serde_json::Value>) -> serde_json::Value {
    let mut tags: Vec<String> = inspect
        .and_then(|v| v.get("tags"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    tags.sort();
    serde_json::json!(tags)
}

fn snapshot_path_label(path: &str) -> &str {
    if path.is_empty() {
        "<root>"
    } else {
        path
    }
}

async fn run_diff(args: DiffArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = match args.project {
        Some(p) => p,
        None => std::env::current_dir().map_err(|e| format!("diff: current directory: {e}"))?,
    };
    if !project.exists() {
        return Err(format!("diff: project path does not exist: {}", project.display()).into());
    }
    if !project.is_dir() {
        return Err(format!(
            "diff: project path is not a directory: {}",
            project.display()
        )
        .into());
    }

    let local_services = snapshot::emit_services(&project)
        .map_err(|e| format!("diff: scan {}: {e}", project.display()))?;
    let local = diff::collect_local_nodes(&local_services);

    let tree_resp = remote::request(
        args.port,
        "tree",
        serde_json::json!({ "path": "", "depth": args.depth }),
    )
    .await?;
    let live_tree = response_value_or_err(&tree_resp, "diff tree")?;
    if diff::has_truncated_tree(&live_tree) {
        return Err(format!(
            "diff: live tree was truncated at --depth {}; rerun with a larger --depth",
            args.depth
        )
        .into());
    }

    let mut studio = diff::collect_studio_tree_nodes(&live_tree);
    // The tree is enumerated first and each Source read afterwards, so anything
    // that disappears in between answers NOT_FOUND. Runtime-spawned VFX models are
    // routinely gone by the time their turn comes, and aborting the whole
    // comparison on the first one made `diff` unusable on busy places — it also
    // starved the widget's per-path staging list, which loaded 0 of N paths and
    // left no way to stage disk files at all. Drop the vanished node from the
    // Studio side instead: it is genuinely not there to compare against.
    let mut vanished: Vec<String> = Vec::new();
    for (path, source_path) in diff::studio_script_paths(&studio) {
        let resp = remote::request(
            args.port,
            "get",
            serde_json::json!({ "path": source_path, "prop": "Source" }),
        )
        .await
        .map_err(|error| format!("diff: get {source_path}.Source failed: {error}"))?;
        if response_is_not_found(&resp) {
            vanished.push(source_path.clone());
            diff::remove_studio_node(&mut studio, &path);
            continue;
        }
        let source = response_value_or_err(&resp, &format!("diff get {source_path}.Source"))?
            .as_str()
            .unwrap_or("")
            .to_string();
        diff::set_node_source(&mut studio, &path, source);
    }
    if !vanished.is_empty() && !args.raw {
        eprintln!(
            "note: {} script(s) disappeared while comparing and were skipped (e.g. {})",
            vanished.len(),
            vanished
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let report = diff::compare(&local, &studio);
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_diff_report(&report);
    }
    Ok(())
}

async fn run_changes(args: DiffArgs) -> Result<(), Box<dyn std::error::Error>> {
    run_diff(args).await
}

async fn run_open(args: OpenArgs) -> Result<(), Box<dyn std::error::Error>> {
    let paths = serde_json::Value::Array(
        args.paths
            .iter()
            .map(|path| serde_json::Value::String(path.clone()))
            .collect(),
    );
    let resp = remote::request(
        args.port,
        "select_set",
        serde_json::json!({ "paths": paths }),
    )
    .await?;
    print_response(&resp, args.raw, |v| {
        let count = v.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("ok: opened {count} instance(s)");
        for path in &args.paths {
            println!("  {path}");
        }
    });
    ok_or_err(&resp)
}

async fn run_where(args: WhereArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = serde_json::Map::new();
    if let Some(project) = args.project.as_deref() {
        if let Ok(resolved) = resolve_live_path(
            args.port,
            project,
            &args.target,
            path_resolver::PathInputKind::Auto,
            "where",
        )
        .await
        {
            out.insert(
                "path".into(),
                serde_json::json!({
                    "studioPath": resolved.studio_path_string(),
                    "class": resolved.class,
                    "fsPath": resolved.fs_path,
                    "fsExists": resolved.fs_exists,
                }),
            );
        }
    }

    let mut req_args = serde_json::Map::new();
    req_args.insert(
        "name".into(),
        serde_json::Value::String(args.target.clone()),
    );
    if let Some(under) = &args.under {
        req_args.insert("under".into(), serde_json::Value::String(under.clone()));
    }
    let resp = remote::request(args.port, "find", serde_json::Value::Object(req_args)).await?;
    if let Ok(value) = response_value_or_err(&resp, "where find") {
        out.insert("matches".into(), value);
    } else {
        out.insert(
            "liveError".into(),
            serde_json::Value::String(response_error_message(&resp)),
        );
    }

    let value = serde_json::Value::Object(out);
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    if let Some(path) = value.get("path") {
        let studio = path
            .get("studioPath")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let fs = path.get("fsPath").and_then(|v| v.as_str()).unwrap_or("?");
        println!("Path:");
        println!("  Studio: {studio}");
        println!("  Disk:   {fs}");
    }
    println!("Matches:");
    print_find(
        value
            .get("matches")
            .unwrap_or(&serde_json::Value::Array(vec![])),
    );
    Ok(())
}

async fn run_props(args: PropsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request(args.port, "get", serde_json::json!({ "path": args.path })).await?;
    let value = response_value_or_err(&resp, "props get")?;
    let props = value
        .get("properties")
        .cloned()
        .unwrap_or(serde_json::Value::Object(Default::default()));
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&props)?);
    } else if let Some(map) = props.as_object() {
        if map.is_empty() {
            println!("(no inspectable properties)");
        } else {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                println!("{key} = {}", format_pretty_value(&map[key]));
            }
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&props)?);
    }
    Ok(())
}

async fn run_source(args: SourceArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.disk {
        let project = project_or_cwd(args.project.as_deref(), "source")?;
        let resolved = resolve_live_path(
            args.port,
            &project,
            &args.path,
            path_resolver::PathInputKind::Auto,
            "source",
        )
        .await?;
        let source_path = disk_source_path(&resolved.fs_path)?
            .ok_or_else(|| format!("source: no source file at {}", resolved.fs_path.display()))?;
        let source = fs_safety::read_to_string_no_follow(&source_path)
            .map_err(|e| format!("source: read {}: {e}", source_path.display()))?;
        if args.raw {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "source": "disk",
                    "studioPath": resolved.studio_path_string(),
                    "fsPath": source_path,
                    "text": source,
                }))?
            );
        } else {
            print!("{source}");
        }
        return Ok(());
    }

    let resp = remote::request(
        args.port,
        "get",
        serde_json::json!({ "path": args.path, "prop": "Source" }),
    )
    .await?;
    let source = response_value_or_err(&resp, "source get")?;
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source": "studio",
                "path": args.path,
                "text": source,
            }))?
        );
    } else if let Some(text) = source.as_str() {
        print!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(&source)?);
    }
    Ok(())
}

async fn run_meta(args: MetaArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = project_or_cwd(args.project.as_deref(), "meta")?;
    let resolved = resolve_live_path(args.port, &project, &args.target, args.from, "meta").await?;
    let value = serde_json::json!({
        "studioPath": resolved.studio_path_string(),
        "class": resolved.class,
        "fsPath": resolved.fs_path,
        "fsExists": resolved.fs_exists,
        "syncedService": resolved.studio_path.first().cloned(),
    });
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("Studio: {}", resolved.studio_path_string());
        println!("Class:   {}", resolved.class);
        println!("Disk:    {}", resolved.fs_path.display());
        println!("Exists:  {}", resolved.fs_exists);
    }
    Ok(())
}

async fn run_services(args: ServicesArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = project_or_cwd(args.project.as_deref(), "services")?;
    let mut live = std::collections::BTreeSet::new();
    if let Ok(resp) = remote::request(
        args.port,
        "tree",
        serde_json::json!({ "path": "", "depth": 1 }),
    )
    .await
    {
        if let Ok(tree) = response_value_or_err(&resp, "services tree") {
            collect_live_service_names(&tree, &mut live);
        }
    }
    let rows: Vec<serde_json::Value> = snapshot::SYNCED_SERVICES
        .iter()
        .map(|service| {
            let path = project.join(service);
            let disk = fs_safety::validate_service_path(&project, service, true)
                .and_then(|safe| fs_safety::metadata_no_follow(&safe))
                .ok()
                .flatten()
                .is_some_and(|metadata| metadata.is_dir());
            serde_json::json!({
                "name": service,
                "disk": disk,
                "studio": live.contains(*service),
                "path": path,
            })
        })
        .collect();
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for row in rows {
            let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let disk = if row.get("disk").and_then(|v| v.as_bool()).unwrap_or(false) {
                "disk"
            } else {
                "----"
            };
            let studio = if row.get("studio").and_then(|v| v.as_bool()).unwrap_or(false) {
                "studio"
            } else {
                "------"
            };
            println!("{name:24} {disk:4} {studio:6}");
        }
    }
    Ok(())
}

async fn run_conflicts(args: ConflictsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let value = http_get_json(args.port, "/resolve").map_err(|e| format!("conflicts: {e}"))?;
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let conflicts = value
        .get("conflicts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if conflicts.is_empty() {
        println!("no parked conflicts");
        return Ok(());
    }
    println!("{} parked conflict(s):", conflicts.len());
    for item in conflicts {
        let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let fs = item.get("fsHash").and_then(|v| v.as_str()).unwrap_or("");
        let studio = item
            .get("studioHash")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!("  {path}");
        println!("    disk   {}", short_hash(fs));
        println!("    studio {}", short_hash(studio));
    }
    Ok(())
}

async fn run_resolve(args: ResolveArgs) -> Result<(), Box<dyn std::error::Error>> {
    let choice = match (args.disk, args.studio) {
        (true, false) => "disk",
        (false, true) => "studio",
        _ => return Err("resolve: pass exactly one of --disk or --studio".into()),
    };
    let value = http_post_json(
        args.port,
        "/resolve",
        &serde_json::json!({ "path": args.path, "choice": choice }),
    )
    .await
    .map_err(|e| format!("resolve: {e}"))?;
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let action = value
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("resolved");
        println!(
            "ok: {action} {}",
            value.get("path").and_then(|v| v.as_str()).unwrap_or("")
        );
    } else {
        let err = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("request failed");
        return Err(err.to_string().into());
    }
    Ok(())
}

async fn run_decision(args: DecisionArgs) -> Result<(), Box<dyn std::error::Error>> {
    let status =
        http_get_json(args.port, "/initial-choice").map_err(|e| format!("decision: {e}"))?;
    let pending = status
        .get("pending")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let choice = match (args.disk, args.studio, args.cancel) {
        (true, false, false) => Some("disk"),
        (false, true, false) => Some("studio"),
        (false, false, true) => Some("cancel"),
        (false, false, false) => None,
        _ => return Err("decision: pass at most one of --disk, --studio, or --cancel".into()),
    };

    let Some(choice) = choice else {
        if args.raw {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else if pending {
            print_pending_decision(&status);
        } else {
            println!("no pending initial sync decision");
        }
        return Ok(());
    };

    if !pending {
        return Err("decision: no pending initial sync decision".into());
    }
    let choice_id = args
        .choice_id
        .or_else(|| {
            status
                .get("choiceId")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .ok_or("decision: pending choice has no choiceId")?;
    let value = http_post_json(
        args.port,
        "/initial-choice",
        &serde_json::json!({ "choiceId": choice_id, "choice": choice }),
    )
    .await
    .map_err(|e| format!("decision: {e}"))?;
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if value
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        println!("ok: initial sync decision set to {choice}");
    } else {
        let err = value
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("request failed");
        return Err(err.to_string().into());
    }
    Ok(())
}

fn print_pending_decision(value: &serde_json::Value) {
    let choice_id = value
        .get("choiceId")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    println!("pending initial sync decision: {choice_id}");
    if let Some(disk) = value.get("diskStats") {
        println!(
            "  disk:   {} script(s), {} instance(s)",
            disk.get("scriptCount")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            disk.get("instanceCount")
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        );
    }
    if let Some(studio) = value.get("studioStats") {
        println!(
            "  studio: {} script(s), {} instance(s)",
            studio
                .get("scriptCount")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            studio
                .get("instanceCount")
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        );
    }
    println!("  choose: rosync decision --disk | --studio | --cancel");
}

async fn run_tail(args: TailArgs) -> Result<(), Box<dyn std::error::Error>> {
    run_logs(LogsArgs {
        project: args.project,
        port: args.port,
        since: args.since,
        level: args.level,
        limit: args.limit,
        tail: true,
        raw: args.raw,
    })
    .await
}

async fn run_watch(args: WatchArgs) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("ws://127.0.0.1:{}/ws", args.port);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        watch_hello_payload(),
    ))
    .await?;
    // The daemon does not otherwise acknowledge a quiet observer connection.
    // Request one pong so callers and audit probes can distinguish an accepted,
    // authenticated watch session from a socket that merely stayed open.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        r#"{"type":"ping"}"#.into(),
    ))
    .await?;
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                eprintln!();
                return Ok(());
            }
            msg = ws.next() => {
                let Some(msg) = msg else { return Ok(()); };
                let msg = msg?;
                if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                    if args.compact {
                        print_ws_frame_compact(&text);
                    } else {
                        println!("{text}");
                    }
                }
            }
        }
    }
}

fn watch_hello_payload() -> String {
    // The daemon rejects every role whose hello omits the protocol version.
    serde_json::json!({
        "type": "hello",
        "clientId": "rosync-watch",
        "role": "watch",
        "protocol": crate::ws::PLUGIN_PROTOCOL_VERSION,
    })
    .to_string()
}

async fn run_repair(args: RepairArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        RepairCommand::Tree(args) => run_repair_tree(args).await,
        RepairCommand::Sourcemap(args) => run_repair_sourcemap(args),
    }
}

async fn run_repair_tree(args: RepairTreeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request(
        args.port,
        "tree",
        serde_json::json!({ "path": "", "depth": args.depth }),
    )
    .await?;
    let tree = response_value_or_err(&resp, "repair tree")?;
    if diff::has_truncated_tree(&tree) {
        return Err(format!(
            "repair tree: live tree was truncated at --depth {}; rerun with a larger --depth",
            args.depth
        )
        .into());
    }
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "source": "live",
                "nodes": count_tree_nodes(&tree),
            }))?
        );
    } else {
        println!(
            "ok: live Studio tree readable ({} node(s))",
            count_tree_nodes(&tree)
        );
    }
    Ok(())
}

fn run_repair_sourcemap(args: RepairSourcemapArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = project_or_cwd(args.project.as_deref(), "repair sourcemap")?;
    let output = args
        .output
        .unwrap_or_else(|| project.join("sourcemap.json"));
    let map = sourcemap::generate(&project)
        .map_err(|e| format!("repair sourcemap: generate {}: {e}", project.display()))?;
    std::fs::write(
        &output,
        format!("{}\n", serde_json::to_string_pretty(&map)?),
    )
    .map_err(|e| format!("repair sourcemap: write {}: {e}", output.display()))?;
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "output": output,
            }))?
        );
    } else {
        println!("ok: wrote {}", output.display());
    }
    Ok(())
}

async fn run_find(args: FindArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut req_args = serde_json::Map::new();
    if let Some(c) = &args.class_name {
        req_args.insert("className".into(), serde_json::Value::String(c.clone()));
    }
    if let Some(n) = &args.name {
        req_args.insert("name".into(), serde_json::Value::String(n.clone()));
    }
    if req_args.is_empty() {
        return Err("find: at least one of --class or --name is required".into());
    }
    if let Some(u) = &args.under {
        req_args.insert("under".into(), serde_json::Value::String(u.clone()));
    }
    let resp = remote::request(args.port, "find", serde_json::Value::Object(req_args)).await?;
    print_response(&resp, args.raw, print_find);
    ok_or_err(&resp)
}

async fn run_eval(args: EvalArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req_args = serde_json::json!({ "source": args.source });
    let resp = remote::request(args.port, "eval", req_args).await?;
    print_response(&resp, args.raw, |v| {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    });
    ok_or_err(&resp)
}

async fn run_capabilities(args: CapabilitiesArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request(args.port, "capabilities", serde_json::json!({})).await?;
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        let value = response_value_or_err(&resp, "capabilities")?;
        let plugin = value
            .get("pluginVersion")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let protocol = value
            .get("protocolVersion")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let host = value
            .get("hostDataModelType")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        println!("plugin {plugin} · protocol {protocol} · {host}");
        if let Some(features) = value.get("features").and_then(serde_json::Value::as_object) {
            for (name, supported) in features {
                let state = if supported.as_bool().unwrap_or(false) {
                    "yes"
                } else {
                    "no"
                };
                println!("  {name}: {state}");
            }
        }
    }
    ok_or_err(&resp)
}

async fn run_playtest(args: PlaytestArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        PlaytestCommand::Run(args) => playtest_run::run(args).await,
        PlaytestCommand::Start(args) => run_playtest_start(args).await,
        PlaytestCommand::Status(args) => run_playtest_status(args).await,
        PlaytestCommand::Contexts(args) => run_playtest_contexts(args).await,
        PlaytestCommand::Wait(args) => run_playtest_wait(args).await,
        PlaytestCommand::Stop(args) => run_playtest_stop(args).await,
        PlaytestCommand::Exec(args) => run_playtest_exec(args).await,
        PlaytestCommand::Logs(args) => run_playtest_logs(args).await,
        PlaytestCommand::Ui(args) => run_playtest_ui(args).await,
        PlaytestCommand::Input(args) => run_playtest_input(args).await,
        PlaytestCommand::Capture(args) => run_playtest_capture(args).await,
        PlaytestCommand::Request(args) => run_playtest_direct_request(args).await,
    }
}

fn parse_json_object(
    value: &str,
    context: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let parsed: serde_json::Value =
        serde_json::from_str(value).map_err(|e| format!("{context}: invalid JSON: {e}"))?;
    if !parsed.is_object() {
        return Err(format!("{context}: expected a JSON object").into());
    }
    Ok(parsed)
}

fn validate_runtime_timeout(timeout: f64) -> Result<Duration, Box<dyn std::error::Error>> {
    if timeout <= 0.0 || !timeout.is_finite() || timeout > 120.0 {
        return Err("playtest: timeout must be finite and between 0 and 120 seconds".into());
    }
    Ok(Duration::from_secs_f64(timeout + 2.0))
}

async fn runtime_request(
    port: u16,
    context: &str,
    op: &str,
    args: serde_json::Value,
    timeout: f64,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let outer_timeout = validate_runtime_timeout(timeout)?;
    Ok(remote::request_with_timeout(
        port,
        "playtest_request",
        serde_json::json!({
            "context": context,
            "op": op,
            "args": args,
            "timeout": timeout,
        }),
        outer_timeout,
    )
    .await?)
}

fn print_playtest_contexts(value: &serde_json::Value) {
    let Some(contexts) = value.get("contexts").and_then(serde_json::Value::as_array) else {
        println!("no playtest contexts");
        return;
    };
    if contexts.is_empty() {
        println!("no playtest contexts");
        return;
    }
    for context in contexts {
        let id = context
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let role = context
            .get("role")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let player = context
            .get("playerName")
            .and_then(serde_json::Value::as_str)
            .map(|name| format!(" · {name}"))
            .unwrap_or_default();
        println!("{id} · {role}{player}");
    }
}

async fn run_playtest_start(args: PlaytestStartArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !(1..=8).contains(&args.players) {
        return Err("playtest start: --players must be between 1 and 8".into());
    }
    let test_args = match args.test_args {
        Some(value) => parse_json_object(&value, "playtest start --test-args")?,
        None => serde_json::json!({}),
    };
    let response = remote::request_with_timeout(
        args.port,
        "playtest_start",
        serde_json::json!({
            "mode": args.mode.as_plugin_str(),
            "players": args.players,
            "testArgs": test_args,
        }),
        Duration::from_secs(15),
    )
    .await?;
    let job = response_value_or_err(&response, "playtest start")?;
    let mut output = serde_json::json!({ "job": job });
    if args.wait {
        let minimum = match args.mode {
            PlaytestMode::Multiplayer => usize::from(args.players) + 1,
            PlaytestMode::Play => 2,
            PlaytestMode::Run => 1,
        };
        let wait_response = remote::request_with_timeout(
            args.port,
            "playtest_wait",
            serde_json::json!({ "minimum": minimum, "timeout": args.timeout }),
            validate_runtime_timeout(args.timeout)?,
        )
        .await?;
        let contexts = response_value_or_err(&wait_response, "playtest wait")?;
        output["contexts"] = contexts;
    }
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        let id = output["job"]
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let status = output["job"]
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("starting");
        println!("playtest {id}: {status}");
        if let Some(contexts) = output.get("contexts") {
            print_playtest_contexts(contexts);
        }
    }
    Ok(())
}

async fn run_playtest_status(args: PlaytestStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut request = serde_json::Map::new();
    if let Some(job_id) = args.job_id {
        request.insert("jobId".into(), serde_json::Value::String(job_id));
    }
    let response = remote::request(
        args.port,
        "playtest_status",
        serde_json::Value::Object(request),
    )
    .await?;
    print_response(&response, args.raw, |value| {
        if let Some(job) = value.get("job") {
            println!(
                "{}: {}",
                job.get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("playtest"),
                job.get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
            );
        } else {
            println!("no playtest job");
        }
        print_playtest_contexts(value);
    });
    ok_or_err(&response)
}

async fn run_playtest_contexts(
    args: PlaytestContextsArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = remote::request(args.port, "playtest_contexts", serde_json::json!({})).await?;
    print_response(&response, args.raw, print_playtest_contexts);
    ok_or_err(&response)
}

async fn run_playtest_wait(args: PlaytestWaitArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.minimum == 0 || args.minimum > 9 {
        return Err("playtest wait: --minimum must be between 1 and 9".into());
    }
    let response = remote::request_with_timeout(
        args.port,
        "playtest_wait",
        serde_json::json!({ "minimum": args.minimum, "timeout": args.timeout }),
        validate_runtime_timeout(args.timeout)?,
    )
    .await?;
    print_response(&response, args.raw, print_playtest_contexts);
    ok_or_err(&response)
}

async fn run_playtest_stop(args: PlaytestStopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut request = serde_json::Map::new();
    if let Some(job_id) = args.job_id {
        request.insert("jobId".into(), serde_json::Value::String(job_id));
    }
    let response = remote::request_with_timeout(
        args.port,
        "playtest_stop",
        serde_json::Value::Object(request),
        Duration::from_secs(15),
    )
    .await?;
    print_response(&response, args.raw, |_| println!("playtest stop requested"));
    ok_or_err(&response)
}

async fn run_playtest_exec(args: PlaytestExecArgs) -> Result<(), Box<dyn std::error::Error>> {
    let source = match (args.source, args.source_file) {
        (Some(source), None) => source,
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map_err(|e| format!("playtest exec: read {}: {e}", path.display()))?,
        (None, None) => return Err("playtest exec: provide --source or --source-file".into()),
        (Some(_), Some(_)) => {
            return Err("playtest exec: use --source or --source-file, not both".into())
        }
    };
    let response = runtime_request(
        args.port,
        &args.context,
        "exec",
        serde_json::json!({
            "source": source,
            "identity": args.identity.as_plugin_str(),
            "timeout": args.timeout,
        }),
        args.timeout,
    )
    .await?;
    print_response(&response, args.raw, |value| {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    });
    ok_or_err(&response)
}

async fn run_playtest_logs(args: PlaytestLogsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let response = runtime_request(
        args.port,
        &args.context,
        "logs",
        serde_json::json!({ "sinceSeq": args.since_seq, "limit": args.limit }),
        args.timeout,
    )
    .await?;
    print_response(&response, args.raw, |value| {
        if let Some(entries) = value.get("entries").and_then(serde_json::Value::as_array) {
            for entry in entries {
                println!(
                    "[{}] {}",
                    entry
                        .get("level")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("info"),
                    entry
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                );
            }
        }
    });
    ok_or_err(&response)
}

async fn run_playtest_ui(args: PlaytestUiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut request = serde_json::Map::new();
    request.insert("limit".into(), serde_json::json!(args.limit));
    if let Some(root) = args.root {
        request.insert("root".into(), serde_json::Value::String(root));
    }
    if let Some(class_name) = args.class_name {
        request.insert("class".into(), serde_json::Value::String(class_name));
    }
    if let Some(name) = args.name {
        request.insert("name".into(), serde_json::Value::String(name));
    }
    let response = runtime_request(
        args.port,
        &args.context,
        "ui_tree",
        serde_json::Value::Object(request),
        args.timeout,
    )
    .await?;
    print_response(&response, args.raw, |value| {
        if let Some(items) = value.get("items").and_then(serde_json::Value::as_array) {
            for item in items {
                let path = item
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let geometry = match (item.get("position"), item.get("size")) {
                    (Some(position), Some(size)) => format!(
                        " @ {},{} {}x{}",
                        position
                            .get("x")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0),
                        position
                            .get("y")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0),
                        size.get("x")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0),
                        size.get("y")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0)
                    ),
                    _ => String::new(),
                };
                println!("{path}{geometry}");
            }
        }
    });
    ok_or_err(&response)
}

async fn run_playtest_input(args: PlaytestInputArgs) -> Result<(), Box<dyn std::error::Error>> {
    validate_runtime_timeout(args.timeout)?;
    let source = match (args.actions, args.file) {
        (Some(source), None) => source,
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map_err(|e| format!("playtest input: read {}: {e}", path.display()))?,
        (None, None) => return Err("playtest input: provide --actions or --file".into()),
        (Some(_), Some(_)) => {
            return Err("playtest input: use --actions or --file, not both".into())
        }
    };
    let value: serde_json::Value = serde_json::from_str(&source)
        .map_err(|e| format!("playtest input: invalid action JSON: {e}"))?;
    let actions = if let Some(actions) = value.as_array() {
        actions.as_slice()
    } else if let Some(actions) = value.get("actions").and_then(serde_json::Value::as_array) {
        actions.as_slice()
    } else if value.is_object() {
        std::slice::from_ref(&value)
    } else {
        &[]
    };
    if actions.is_empty() || actions.len() > 200 {
        return Err("playtest input: action count must be between 1 and 200".into());
    }
    let mut planned_seconds = 0.0;
    for (index, action) in actions.iter().enumerate() {
        let object = action
            .as_object()
            .ok_or_else(|| format!("playtest input: action {} must be an object", index + 1))?;
        let kind = object
            .get("type")
            .or_else(|| object.get("kind"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let duration = match kind {
            "key_press" | "click" => object
                .get("duration")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.05),
            "wait" => object
                .get("seconds")
                .or_else(|| object.get("duration"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            "key" | "mouse_move" | "mouse_delta" | "mouse_button" | "text" => 0.0,
            _ => {
                return Err(format!(
                    "playtest input: action {} has unknown type {kind:?}",
                    index + 1
                )
                .into())
            }
        };
        if !duration.is_finite() || !(0.0..=30.0).contains(&duration) {
            return Err(format!(
                "playtest input: action {} duration must be between 0 and 30 seconds",
                index + 1
            )
            .into());
        }
        planned_seconds += duration;
    }
    if planned_seconds > args.timeout.min(30.0) {
        return Err(format!(
            "playtest input: planned duration {planned_seconds}s exceeds the request budget"
        )
        .into());
    }
    let request = if value.is_array() {
        serde_json::json!({ "actions": value })
    } else if value.is_object() {
        value
    } else {
        return Err("playtest input: actions must be a JSON object or array".into());
    };
    let response =
        runtime_request(args.port, &args.context, "input", request, args.timeout).await?;
    print_response(&response, args.raw, |value| {
        println!(
            "applied {} input action(s)",
            value
                .get("applied")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        );
    });
    ok_or_err(&response)
}

async fn run_playtest_capture(args: PlaytestCaptureArgs) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = capture_deadline(args.timeout, "playtest capture")?;
    let region = args
        .region
        .as_deref()
        .map(parse_capture_region)
        .transpose()?;
    let output_size = args
        .output_size
        .as_deref()
        .map(parse_capture_size)
        .transpose()?;
    if let Some(region) = region {
        validate_capture_dimensions(region.width, region.height)?;
    }
    if let Some([width, height]) = output_size {
        validate_capture_dimensions(width, height)?;
    }
    let mut options = serde_json::Map::new();
    options.insert(
        "ui".into(),
        serde_json::Value::String(args.ui.as_plugin_str().to_string()),
    );
    options.insert(
        "resample".into(),
        serde_json::Value::String(args.resample.as_plugin_str().to_string()),
    );
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
    if let Some([width, height]) = output_size {
        options.insert(
            "outputSize".into(),
            serde_json::json!({ "x": width, "y": height }),
        );
    }
    let filename = args
        .output
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("playtest-capture.png");
    let work_deadline = capture_work_deadline(deadline);
    let request_timeout = capture_deadline_remaining(work_deadline, "playtest capture")?;
    let response = capture_remote_request_until(
        args.port,
        "playtest_capture",
        serde_json::json!({
            "context": args.context,
            "options": options,
            "filename": filename,
            "timeout": request_timeout.as_secs_f64(),
        }),
        work_deadline,
        "playtest capture",
    )
    .await?;
    let value = response_value_or_err(&response, "playtest capture")?;
    let artifact = value
        .get("artifact")
        .ok_or("playtest capture: response omitted artifact metadata")?;
    let artifact_id = plugin_artifact_id(artifact, "playtest capture")?.to_string();
    let capture_details = (|| -> Result<(u64, u32, u32), Box<dyn std::error::Error>> {
        let capture = value
            .get("capture")
            .ok_or("playtest capture: response omitted capture metadata")?;
        let size = capture
            .get("byteLength")
            .and_then(serde_json::Value::as_u64)
            .ok_or("playtest capture: response omitted capture byteLength")?;
        if size == 0 || size > CAPTURE_MAX_ARTIFACT_BYTES {
            return Err(format!("playtest capture: invalid capture size {size}").into());
        }
        let width = capture
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or("playtest capture: response omitted valid capture width")?;
        let height = capture
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or("playtest capture: response omitted valid capture height")?;
        validate_capture_dimensions(width, height)?;
        Ok((size, width, height))
    })();
    let (capture_size, capture_width, capture_height) = match capture_details {
        Ok(details) => details,
        Err(error) => {
            let _ = consume_artifact_transport_until(args.port, &artifact_id, deadline).await;
            return Err(error);
        }
    };
    let materialized = materialize_capture_artifact(
        args.port,
        &artifact_id,
        Some(capture_size),
        Some((capture_width, capture_height)),
        Some(&args.output),
        deadline,
        "playtest capture",
    )
    .await?;
    let absolute = materialized
        .output_path
        .clone()
        .ok_or("playtest capture: output path was not materialized")?;
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "path": absolute.clone(),
                "artifact": {
                    "path": absolute,
                    "mime": "image/png",
                    "size": materialized.size,
                    "sha256": materialized.sha256,
                    "transport": {
                        "metadata": materialized.metadata,
                        "consumed": materialized.consumed,
                    },
                },
                "capture": value.get("capture"),
                "context": value.get("context"),
            }))?
        );
    } else {
        println!(
            "wrote {} ({}x{})",
            absolute.display(),
            materialized.width,
            materialized.height
        );
    }
    Ok(())
}

async fn run_playtest_direct_request(
    args: PlaytestRequestArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = parse_json_object(&args.args, "playtest request --args")?;
    let response =
        runtime_request(args.port, &args.context, &args.op, request, args.timeout).await?;
    print_response(&response, args.raw, |value| {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    });
    ok_or_err(&response)
}

#[derive(Debug, Deserialize)]
struct TransmitPrepared {
    #[serde(rename = "sessionId")]
    session_id: String,
    images: Vec<TransmitImageMeta>,
}

#[derive(Debug, Deserialize)]
struct TransmitImageMeta {
    token: String,
    name: Option<String>,
    path: Option<String>,
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize)]
struct TransmittedImage {
    name: Option<String>,
    width: u32,
    height: u32,
    #[serde(rename = "pixelsBase64")]
    pixels_base64: String,
}

async fn run_transmit(args: TransmitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let source = match (&args.source, &args.source_file) {
        (Some(source), None) => Some(source.clone()),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| format!("transmit: read {}: {e}", path.display()))?,
        ),
        (None, None) => None,
        (Some(_), Some(_)) => {
            return Err("transmit: use --source or --source-file, not both".into())
        }
    };

    if source.is_none() && args.from_path.is_none() && args.paths.is_empty() {
        return Err(
            "transmit: provide --source/--source-file, --from, or at least one --path".into(),
        );
    }
    if !args.timeout.is_finite()
        || args.timeout <= 0.0
        || args.timeout > ws::MAX_REQUEST_TIMEOUT.as_secs_f64()
    {
        return Err(format!(
            "transmit: --timeout must be finite, greater than zero, and at most {} seconds",
            ws::MAX_REQUEST_TIMEOUT.as_secs()
        )
        .into());
    }

    let mut req = serde_json::Map::new();
    if let Some(source) = source {
        req.insert("source".into(), serde_json::Value::String(source));
    }
    if let Some(path) = args.from_path {
        req.insert("from".into(), serde_json::Value::String(path));
    }
    if !args.paths.is_empty() {
        req.insert(
            "paths".into(),
            serde_json::Value::Array(
                args.paths
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }

    let timeout = Duration::from_secs_f64(args.timeout);
    let resp = remote::request_with_timeout(
        args.port,
        "transmit_prepare",
        serde_json::Value::Object(req),
        timeout,
    )
    .await?;
    let value = response_value_or_err(&resp, "transmit prepare")?;
    let prepared: TransmitPrepared = serde_json::from_value(value)
        .map_err(|e| format!("transmit: plugin returned invalid prepare response: {e}"))?;
    if prepared.images.is_empty() {
        let _ = remote::request_with_timeout(
            args.port,
            "transmit_close",
            serde_json::json!({
                "sessionId": prepared.session_id,
            }),
            Duration::from_secs(5),
        )
        .await;
        return Err("transmit: plugin returned no images".into());
    }

    let output_paths = match transmit_output_paths(&prepared.images, &args.output) {
        Ok(paths) => paths,
        Err(e) => {
            let _ = remote::request_with_timeout(
                args.port,
                "transmit_close",
                serde_json::json!({
                    "sessionId": prepared.session_id,
                }),
                Duration::from_secs(5),
            )
            .await;
            return Err(e);
        }
    };
    let mut written = Vec::with_capacity(prepared.images.len());
    let mut read_result: Result<(), Box<dyn std::error::Error>> = Ok(());
    for (image, output_path) in prepared.images.iter().zip(output_paths.iter()) {
        let resp = remote::request_with_timeout(
            args.port,
            "transmit_read",
            serde_json::json!({
                "sessionId": prepared.session_id,
                "token": image.token,
            }),
            timeout,
        )
        .await;
        let resp = match resp {
            Ok(resp) => resp,
            Err(e) => {
                read_result = Err(format!(
                    "transmit: read {}: {e}",
                    image.name.as_deref().unwrap_or(&image.token)
                )
                .into());
                break;
            }
        };
        let value = match response_value_or_err(&resp, "transmit read") {
            Ok(value) => value,
            Err(e) => {
                read_result = Err(e);
                break;
            }
        };
        let transmitted: TransmittedImage = match serde_json::from_value(value) {
            Ok(image) => image,
            Err(e) => {
                read_result =
                    Err(format!("transmit: plugin returned invalid image response: {e}").into());
                break;
            }
        };
        if let Err(e) = write_png_rgba(&transmitted, output_path) {
            read_result = Err(e);
            break;
        }
        written.push(output_path.clone());
        if !args.raw {
            println!("wrote {}", output_path.display());
        }
    }

    let _ = remote::request_with_timeout(
        args.port,
        "transmit_close",
        serde_json::json!({
            "sessionId": prepared.session_id,
        }),
        Duration::from_secs(5),
    )
    .await;

    read_result?;
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "files": written,
            }))?
        );
    }
    Ok(())
}

fn transmit_output_paths(
    images: &[TransmitImageMeta],
    output: &std::path::Path,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let output_is_file = images.len() == 1
        && output
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("png"))
            .unwrap_or(false);

    if output_is_file {
        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("transmit: create {}: {e}", parent.display()))?;
            }
        }
    } else {
        std::fs::create_dir_all(output)
            .map_err(|e| format!("transmit: create {}: {e}", output.display()))?;
    }

    let mut used_names: HashMap<String, usize> = HashMap::new();
    let mut written = Vec::with_capacity(images.len());
    for (index, image) in images.iter().enumerate() {
        if image.width == 0 || image.height == 0 {
            return Err(format!(
                "transmit: image {} has invalid size {}x{}",
                image.name.as_deref().unwrap_or("<unnamed>"),
                image.width,
                image.height
            )
            .into());
        }

        let path = if output_is_file {
            output.to_path_buf()
        } else {
            let fallback = image
                .path
                .as_deref()
                .and_then(|path| path.rsplit('/').next())
                .unwrap_or("image");
            let name = image.name.as_deref().unwrap_or(fallback);
            let stem = unique_transmit_stem(sanitize_transmit_stem(name), &mut used_names, index);
            output.join(format!("{stem}.png"))
        };
        written.push(path);
    }
    Ok(written)
}

fn write_png_rgba(
    image: &TransmittedImage,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine as _;

    let rgba = base64::engine::general_purpose::STANDARD
        .decode(&image.pixels_base64)
        .map_err(|e| {
            format!(
                "transmit: decode {}: {e}",
                image.name.as_deref().unwrap_or("<unnamed>")
            )
        })?;
    let expected = image.width as usize * image.height as usize * 4;
    if rgba.len() != expected {
        return Err(format!(
            "transmit: {} pixel buffer is {} bytes, expected {} for {}x{} RGBA",
            image.name.as_deref().unwrap_or("<unnamed>"),
            rgba.len(),
            expected,
            image.width,
            image.height
        )
        .into());
    }

    let file = std::fs::File::create(path)
        .map_err(|e| format!("transmit: create {}: {e}", path.display()))?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("transmit: png header {}: {e}", path.display()))?;
    writer
        .write_image_data(&rgba)
        .map_err(|e| format!("transmit: png write {}: {e}", path.display()))?;
    Ok(())
}

fn sanitize_transmit_stem(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() || ch == '.' || ch == '/' || ch == '\\' {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').trim_matches('.').to_string();
    if trimmed.is_empty() || trimmed.starts_with('.') {
        "image".to_string()
    } else {
        trimmed
    }
}

fn unique_transmit_stem(
    stem: String,
    used_names: &mut HashMap<String, usize>,
    index: usize,
) -> String {
    let base = if stem.is_empty() {
        format!("image-{}", index + 1)
    } else {
        stem
    };
    let count = used_names.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base
    } else {
        format!("{base}-{}", *count)
    }
}

/// Parse `30s` / `5m` / `2h` / `500ms` → seconds as f64. Bare digits are
/// treated as seconds for convenience.
fn parse_duration_seconds(s: &str) -> Result<f64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num, unit) = s
        .find(|c: char| c.is_alphabetic())
        .map(|i| (&s[..i], &s[i..]))
        .unwrap_or((s, ""));
    let n: f64 = num
        .parse()
        .map_err(|e| format!("invalid duration {s:?}: {e}"))?;
    let secs = match unit {
        "" | "s" | "sec" | "secs" => n,
        "ms" => n / 1000.0,
        "m" | "min" | "mins" => n * 60.0,
        "h" | "hr" | "hrs" => n * 3600.0,
        other => return Err(format!("unknown duration unit {other:?}")),
    };
    Ok(secs)
}

async fn run_logs(args: LogsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let since_secs = match &args.since {
        Some(s) => parse_duration_seconds(s)?,
        None => 30.0,
    };
    if args.tail {
        return run_logs_tail(args, since_secs).await;
    }
    let req_args = serde_json::json!({
        "since_seconds": since_secs,
        "level_min": args.level.as_plugin_str(),
        "limit": args.limit,
    });
    let resp = remote::request(args.port, "logs", req_args).await?;
    if args.raw {
        print_response(&resp, true, |_| {});
        return ok_or_err(&resp);
    }
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        let err = response_error_message(&resp);
        eprintln!("error: {err}");
        return Err(err.into());
    }
    let empty = serde_json::Value::Null;
    let value = resp.get("value").unwrap_or(&empty);
    print_log_entries(value);
    Ok(())
}

async fn run_logs_tail(
    args: LogsArgs,
    initial_since: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_seq: Option<u64> = None;
    let mut req_args = serde_json::json!({
        "since_seconds": initial_since,
        "level_min": args.level.as_plugin_str(),
        "limit": args.limit,
    });
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    loop {
        let req = remote::request(args.port, "logs", req_args.clone());
        tokio::select! {
            _ = &mut ctrl_c => { eprintln!(); return Ok(()); }
            resp = req => {
                let resp = resp?;
                let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                if !ok {
                    return Err(response_error_message(&resp).into());
                }
                let empty = serde_json::Value::Null;
                let value = resp.get("value").unwrap_or(&empty);
                if let Some(entries) = value.get("entries").and_then(|v| v.as_array()) {
                    for e in entries {
                        print_log_entry(e);
                        if let Some(seq) = e.get("seq").and_then(|v| v.as_u64()) {
                            last_seq = Some(match last_seq { Some(p) => p.max(seq), None => seq });
                        }
                    }
                }
            }
        }
        // Switch to seq-based polling after the first successful batch.
        if let Some(seq) = last_seq {
            req_args = serde_json::json!({
                "since_seq": seq,
                "level_min": args.level.as_plugin_str(),
                "limit": args.limit,
            });
        }
        let sleep = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut ctrl_c => { eprintln!(); return Ok(()); }
            _ = &mut sleep => {}
        }
    }
}

fn print_log_entries(value: &serde_json::Value) {
    let entries = match value.get("entries").and_then(|v| v.as_array()) {
        Some(e) => e,
        None => {
            println!(
                "{}",
                serde_json::to_string_pretty(value).unwrap_or_default()
            );
            return;
        }
    };
    if entries.is_empty() {
        eprintln!("(no matching log entries)");
        return;
    }
    for e in entries {
        print_log_entry(e);
    }
}

fn print_log_entry(e: &serde_json::Value) {
    let level = e.get("level").and_then(|v| v.as_str()).unwrap_or("info");
    let wall = e.get("wall").and_then(|v| v.as_i64()).unwrap_or(0);
    let message = e.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let hms = format_hms_local(wall);
    println!("[{level:>5}] {hms} {message}");
}

fn format_hms_local(ts: i64) -> String {
    if ts == 0 {
        return "--:--:--".into();
    }
    format_hms_local_impl(ts).unwrap_or_else(|| "--:--:--".into())
}

#[cfg(unix)]
fn format_hms_local_impl(ts: i64) -> Option<String> {
    // SAFETY: `localtime_r` is thread-safe; we pass valid pointers.
    unsafe {
        let mut tm: libc_tm = std::mem::zeroed();
        let t: i64 = ts;
        if localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some(format!(
            "{:02}:{:02}:{:02}",
            tm.tm_hour, tm.tm_min, tm.tm_sec
        ))
    }
}

#[cfg(unix)]
#[repr(C)]
#[allow(non_camel_case_types)]
struct libc_tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const i8,
}

#[cfg(unix)]
extern "C" {
    fn localtime_r(time: *const i64, tm: *mut libc_tm) -> *mut libc_tm;
}

#[cfg(windows)]
fn format_hms_local_impl(ts: i64) -> Option<String> {
    // SAFETY: `localtime_s` writes to the provided tm buffer and returns 0 on
    // success. On 64-bit Windows, C `time_t` is 64-bit.
    unsafe {
        let mut tm: windows_tm = std::mem::zeroed();
        let t: i64 = ts;
        if localtime_s(&mut tm, &t) != 0 {
            return None;
        }
        Some(format!(
            "{:02}:{:02}:{:02}",
            tm.tm_hour, tm.tm_min, tm.tm_sec
        ))
    }
}

#[cfg(windows)]
#[repr(C)]
#[allow(non_camel_case_types)]
struct windows_tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
}

#[cfg(windows)]
extern "C" {
    #[link_name = "_localtime64_s"]
    fn localtime_s(tm: *mut windows_tm, time: *const i64) -> i32;
}

async fn run_save(args: SaveArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request(args.port, "save", serde_json::json!({})).await?;
    print_response(&resp, args.raw, |_v| println!("ok: save started"));
    ok_or_err(&resp)
}

async fn run_undo(args: UndoArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request(args.port, "undo", serde_json::json!({})).await?;
    print_response(&resp, args.raw, |_v| println!("ok: undo"));
    ok_or_err(&resp)
}

async fn run_redo(args: RedoArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request(args.port, "redo", serde_json::json!({})).await?;
    print_response(&resp, args.raw, |_v| println!("ok: redo"));
    ok_or_err(&resp)
}

async fn run_waypoint(args: WaypointArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.name.is_empty() {
        return Err("waypoint: --name must not be empty".into());
    }
    let req_args = serde_json::json!({ "name": args.name });
    let resp = remote::request(args.port, "waypoint", req_args).await?;
    let name = args.name.clone();
    print_response(&resp, args.raw, |_v| println!("ok: waypoint {name:?}"));
    ok_or_err(&resp)
}

async fn run_ping(args: PingArgs) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let ping_resp = remote::request(args.port, "ping", serde_json::json!({})).await?;
    let rtt = start.elapsed();
    let ok = ping_resp
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !ok {
        let err = response_error_message(&ping_resp);
        if args.raw {
            println!(
                "{}",
                serde_json::to_string_pretty(&ping_resp).unwrap_or_default()
            );
        }
        return Err(err.into());
    }
    // Version is a separate round-trip; failures are non-fatal.
    let plugin_version = match remote::request(args.port, "version", serde_json::json!({})).await {
        Ok(v) => v
            .get("value")
            .and_then(|v| v.get("plugin_version"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        Err(_) => "unknown".into(),
    };
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&ping_resp).unwrap_or_default()
        );
        return Ok(());
    }
    println!(
        "pong from plugin v{plugin_version}  (round-trip {:.1} ms, daemon responsive)",
        rtt.as_secs_f64() * 1000.0
    );
    Ok(())
}

async fn run_version(args: VersionArgs) -> Result<(), Box<dyn std::error::Error>> {
    let daemon = env!("CARGO_PKG_VERSION");
    let build_commit = env!("ROSYNC_BUILD_COMMIT");
    let build_dirty = env!("ROSYNC_BUILD_DIRTY") == "true";
    // Plugin may be offline — treat failures as "no plugin connected" rather
    // than aborting the subcommand.
    let value = match fetch_plugin_version(args.port).await {
        Ok(v) => v,
        Err(e) => {
            if args.raw {
                println!(
                    "{}",
                    serde_json::json!({
                        "daemon": daemon,
                        "buildCommit": build_commit,
                        "buildDirty": build_dirty,
                        "plugin": null,
                        "error": e,
                    })
                );
            } else {
                println!(
                    "daemon: {}",
                    daemon_build_label(daemon, build_commit, build_dirty)
                );
                println!("plugin: (not connected — {e})");
            }
            return Ok(());
        }
    };
    if args.raw {
        println!(
            "{}",
            serde_json::json!({
                "daemon": daemon,
                "buildCommit": build_commit,
                "buildDirty": build_dirty,
                "plugin": value,
            })
        );
        return Ok(());
    }
    println!(
        "daemon: {}",
        daemon_build_label(daemon, build_commit, build_dirty)
    );
    let pv = value
        .get("plugin_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let proto = value.get("protocol").and_then(|v| v.as_u64()).unwrap_or(0);
    let sv = value
        .get("studio_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    println!("plugin: {pv} (protocol {proto}, Studio {sv})");
    Ok(())
}

fn daemon_build_label(version: &str, commit: &str, dirty: bool) -> String {
    format!(
        "rosync {version} ({commit}{})",
        if dirty { ", dirty" } else { "" }
    )
}

async fn fetch_plugin_version(port: u16) -> Result<serde_json::Value, String> {
    let resp = remote::request(port, "version", serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())?;
    if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(response_error_message(&resp));
    }
    Ok(resp
        .get("value")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

impl DoctorStatus {
    fn as_str(self) -> &'static str {
        match self {
            DoctorStatus::Ok => "ok",
            DoctorStatus::Warn => "warn",
            DoctorStatus::Fail => "fail",
        }
    }
}

struct DoctorCheck {
    name: &'static str,
    status: DoctorStatus,
    detail: String,
}

#[derive(Serialize)]
struct RefreshFileStatus {
    path: &'static str,
    status: &'static str,
    note: Option<&'static str>,
}

async fn run_status(args: StatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = match args.project {
        Some(p) => p,
        None => std::env::current_dir().map_err(|e| format!("status: current directory: {e}"))?,
    };
    let checks = vec![
        check_project_path(&project),
        check_daemon_hello(args.port),
        check_plugin_version(args.port).await,
        check_project_config(&project),
        check_sourcemap(&project),
        check_writes_log_path(),
    ];
    let ok = !checks.iter().any(|c| c.status == DoctorStatus::Fail);

    if args.raw {
        let mut body = serde_json::Map::new();
        body.insert("ok".into(), serde_json::Value::Bool(ok));
        body.insert(
            "project".into(),
            serde_json::json!(project.display().to_string()),
        );
        body.insert("port".into(), serde_json::json!(args.port));
        for check in &checks {
            body.insert(status_json_key(check.name).into(), status_check_json(check));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(body))?
        );
    } else {
        println!("Ro-Sync status");
        println!("project: {}", project.display());
        println!("port: {}", args.port);
        for check in &checks {
            println!(
                "[{:<4}] {:<14} {}",
                check.status.as_str(),
                check.name,
                check.detail
            );
        }
    }

    if !ok {
        return Err("status: one or more checks failed".into());
    }
    Ok(())
}

async fn run_doctor(args: DoctorArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = match args.project {
        Some(p) => p,
        None => std::env::current_dir().map_err(|e| format!("doctor: current directory: {e}"))?,
    };
    let mut checks = Vec::new();

    let safe_project = lifecycle::canonical_project(&project);
    let project_ok = safe_project.is_ok();
    checks.push(check_project_path(&project));
    if let Ok(project) = safe_project.as_ref() {
        checks.push(check_project_config(project));
        checks.push(check_sourcemap(project));
    } else {
        let detail = safe_project.as_ref().unwrap_err().to_string();
        checks.push(doctor_check(
            "ro-sync.json",
            DoctorStatus::Fail,
            format!("skipped for unsafe project path: {detail}"),
        ));
        checks.push(doctor_check(
            "sourcemap",
            DoctorStatus::Fail,
            format!("skipped for unsafe project path: {detail}"),
        ));
    }
    checks.push(check_daemon_hello(args.port));
    if let Ok(project) = safe_project.as_ref() {
        checks.push(check_luau_lsp(project));
        checks.push(check_luau_compile(project));
        checks.push(check_luau_definitions(project));
        checks.push(check_luaurc(project));
    } else {
        let detail = safe_project.as_ref().unwrap_err().to_string();
        for name in ["luau-lsp", "luau-compile", "roblox defs", ".luaurc"] {
            checks.push(doctor_check(
                name,
                DoctorStatus::Fail,
                format!("skipped for unsafe project path: {detail}"),
            ));
        }
    }
    checks.push(check_writes_log_path());
    checks.push(check_plugin_version(args.port).await);

    if args.raw {
        let arr: Vec<serde_json::Value> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "status": c.status.as_str(),
                    "detail": c.detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": !checks.iter().any(|c| c.status == DoctorStatus::Fail),
                "project": project,
                "port": args.port,
                "checks": arr,
            }))?
        );
    } else {
        println!("Ro-Sync doctor");
        println!("project: {}", project.display());
        println!("port: {}", args.port);
        println!();
        for check in &checks {
            println!(
                "[{:<4}] {:<18} {}",
                check.status.as_str(),
                check.name,
                check.detail
            );
        }
    }

    if !project_ok {
        return Err("doctor: project path is not a directory".into());
    }
    if checks.iter().any(|c| c.status == DoctorStatus::Fail) {
        return Err("doctor: one or more checks failed".into());
    }
    Ok(())
}

fn run_refresh(args: RefreshArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = match args.project {
        Some(p) => p,
        None => std::env::current_dir().map_err(|e| format!("refresh: current directory: {e}"))?,
    };
    let project = lifecycle::canonical_project(&project)
        .map_err(|error| format!("refresh: validate project {}: {error}", project.display()))?;

    let ro_sync_status = snapshot::refresh_ro_sync_md(&project)?;
    let mut files = vec![RefreshFileStatus {
        path: snapshot::RO_SYNC_MD,
        status: ro_sync_status.as_str(),
        note: if matches!(ro_sync_status, snapshot::RoSyncDocRefresh::SkippedCustom) {
            Some("unmarked custom file left untouched")
        } else {
            None
        },
    }];

    let claude_existed =
        snapshot::project_tool_file_exists(&project, &project.join(snapshot::CLAUDE_MD))?;
    let claude_changed = snapshot::write_claude_md_if_missing_or_merge(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::CLAUDE_MD,
        status: refresh_file_status(claude_existed, claude_changed),
        note: Some("custom content preserved; @AGENTS.md ensured"),
    });

    let codex_config_path = project
        .join(snapshot::CODEX_DIR)
        .join(snapshot::CODEX_CONFIG_TOML);
    let codex_config_existed = snapshot::project_tool_file_exists(&project, &codex_config_path)?;
    let codex_config_changed = snapshot::write_codex_config_if_missing_or_merge(&project)?;
    files.push(RefreshFileStatus {
        path: ".codex/config.toml",
        status: refresh_file_status(codex_config_existed, codex_config_changed),
        note: Some("project doc fallbacks merged"),
    });

    let agents_existed =
        snapshot::project_tool_file_exists(&project, &project.join(snapshot::AGENTS_MD))?;
    let agents_changed = snapshot::write_agents_md_if_missing_or_merge(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::AGENTS_MD,
        status: refresh_file_status(agents_existed, agents_changed),
        note: Some("only the Ro Sync marker block was regenerated"),
    });

    let stylua_existed =
        snapshot::project_tool_file_exists(&project, &project.join(snapshot::STYLUA_TOML))?;
    let stylua_changed = snapshot::write_stylua_toml_if_missing(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::STYLUA_TOML,
        status: refresh_file_status(stylua_existed, stylua_changed),
        note: Some("Luau formatter config ensured"),
    });

    let aftman_existed =
        snapshot::project_tool_file_exists(&project, &project.join(snapshot::AFTMAN_TOML))?;
    let aftman_changed = snapshot::write_aftman_stylua_if_missing_or_merge(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::AFTMAN_TOML,
        status: refresh_file_status(aftman_existed, aftman_changed),
        note: Some("StyLua and luau-lsp tool pins ensured"),
    });

    let definitions_existed = snapshot::project_tool_file_exists(
        &project,
        &project.join(snapshot::ROBLOX_DEFINITIONS_PATH),
    )?;
    let definitions_changed = snapshot::write_roblox_definitions_if_missing_or_update(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::ROBLOX_DEFINITIONS_PATH,
        status: refresh_file_status(definitions_existed, definitions_changed),
        note: Some("Roblox Luau definitions ensured"),
    });

    let luaurc_existed =
        snapshot::project_tool_file_exists(&project, &project.join(snapshot::LUAURC))?;
    let luaurc_changed = snapshot::write_luaurc_if_missing_or_cleanup(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::LUAURC,
        status: refresh_file_status(luaurc_existed, luaurc_changed),
        note: Some("Luau configuration ensured; obsolete definitions key removed"),
    });

    let changed = files
        .iter()
        .filter(|file| file.status == "created" || file.status == "updated")
        .count();

    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "project": project.display().to_string(),
                "changed": changed,
                "files": files,
            }))?
        );
    } else {
        println!("Ro-Sync refresh");
        println!("project: {}", project.display());
        println!();
        for file in &files {
            match file.note {
                Some(note) => println!("[{:<14}] {:<18} {}", file.status, file.path, note),
                None => println!("[{:<14}] {}", file.status, file.path),
            }
        }
    }

    Ok(())
}

fn refresh_file_status(existed: bool, changed: bool) -> &'static str {
    match (existed, changed) {
        (false, true) => "created",
        (true, true) => "updated",
        _ => "unchanged",
    }
}

fn doctor_check(
    name: &'static str,
    status: DoctorStatus,
    detail: impl Into<String>,
) -> DoctorCheck {
    DoctorCheck {
        name,
        status,
        detail: detail.into(),
    }
}

fn status_json_key(name: &str) -> &str {
    match name {
        "project" => "project_path",
        "ro-sync.json" => "project_config",
        "writes.log" => "writes_log",
        other => other,
    }
}

fn status_check_json(check: &DoctorCheck) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("status".into(), serde_json::json!(check.status.as_str()));
    obj.insert("detail".into(), serde_json::json!(check.detail));
    match check.name {
        "daemon" => {
            obj.insert(
                "reachable".into(),
                serde_json::json!(check.status == DoctorStatus::Ok),
            );
        }
        "plugin" => {
            obj.insert(
                "connected".into(),
                serde_json::json!(check.status == DoctorStatus::Ok),
            );
        }
        "ro-sync.json" => {
            obj.insert(
                "present".into(),
                serde_json::json!(check.status == DoctorStatus::Ok),
            );
        }
        "sourcemap" => {
            obj.insert("freshness".into(), serde_json::json!(check.detail));
        }
        "writes.log" => {
            obj.insert("location".into(), serde_json::json!(check.detail));
        }
        _ => {}
    }
    serde_json::Value::Object(obj)
}

fn check_project_path(project: &std::path::Path) -> DoctorCheck {
    match lifecycle::canonical_project(project) {
        Ok(path) => doctor_check("project", DoctorStatus::Ok, path.display().to_string()),
        Err(e) => doctor_check(
            "project",
            DoctorStatus::Fail,
            format!("unsafe or unavailable: {e}"),
        ),
    }
}

fn check_project_config(project: &std::path::Path) -> DoctorCheck {
    match project_config::read_from_disk(project) {
        Ok(Some(cfg)) => doctor_check(
            "ro-sync.json",
            DoctorStatus::Ok,
            format!(
                "name={} gameId={} groupId={}",
                cfg.name,
                cfg.game_id.unwrap_or_else(|| "-".into()),
                cfg.group_id.unwrap_or_else(|| "-".into())
            ),
        ),
        Ok(None) => doctor_check("ro-sync.json", DoctorStatus::Warn, "missing"),
        Err(e) => doctor_check("ro-sync.json", DoctorStatus::Fail, format!("invalid: {e}")),
    }
}

fn check_sourcemap(project: &std::path::Path) -> DoctorCheck {
    match sourcemap::generate(project) {
        Ok(map) => {
            let services = map
                .get("children")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if services == 0 {
                doctor_check(
                    "sourcemap",
                    DoctorStatus::Warn,
                    "generated, but no service dirs found",
                )
            } else {
                doctor_check(
                    "sourcemap",
                    DoctorStatus::Ok,
                    format!("{services} service dirs"),
                )
            }
        }
        Err(e) => doctor_check("sourcemap", DoctorStatus::Fail, format!("generate: {e}")),
    }
}

fn check_daemon_hello(port: u16) -> DoctorCheck {
    match fetch_daemon_hello(port) {
        Ok(v) => {
            let version = v.get("version").and_then(|v| v.as_str()).unwrap_or("?");
            let name = v.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            doctor_check("daemon", DoctorStatus::Ok, format!("{name} v{version}"))
        }
        Err(e) => doctor_check("daemon", DoctorStatus::Fail, e),
    }
}

async fn check_plugin_version(port: u16) -> DoctorCheck {
    match fetch_plugin_version(port).await {
        Ok(value) => {
            let plugin = value
                .get("plugin_version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let studio = value
                .get("studio_version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            doctor_check(
                "plugin",
                DoctorStatus::Ok,
                format!("v{plugin}, Studio {studio}"),
            )
        }
        Err(e) => doctor_check("plugin", DoctorStatus::Fail, e),
    }
}

fn check_luau_lsp(project: &std::path::Path) -> DoctorCheck {
    let luau_lsp = resolve_luau_lsp(None);
    match std::process::Command::new(&luau_lsp)
        .arg("--version")
        .current_dir(project)
        .output()
    {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let status = if parse_semver_triplet(&version)
                .is_some_and(|version| version < RECOMMENDED_LUAU_LSP_VERSION)
            {
                DoctorStatus::Warn
            } else {
                DoctorStatus::Ok
            };
            let recommendation = if status == DoctorStatus::Warn {
                format!(
                    "; tested with {}.{}.{} (run `aftman install` after `rosync refresh`)",
                    RECOMMENDED_LUAU_LSP_VERSION.0,
                    RECOMMENDED_LUAU_LSP_VERSION.1,
                    RECOMMENDED_LUAU_LSP_VERSION.2,
                )
            } else {
                String::new()
            };
            doctor_check(
                "luau-lsp",
                status,
                format!("{} ({version}){recommendation}", luau_lsp.to_string_lossy()),
            )
        }
        Ok(out) => doctor_check(
            "luau-lsp",
            DoctorStatus::Fail,
            format!("{} exited with {}", luau_lsp.to_string_lossy(), out.status),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            doctor_check("luau-lsp", DoctorStatus::Fail, "not found")
        }
        Err(e) => doctor_check("luau-lsp", DoctorStatus::Fail, format!("run: {e}")),
    }
}

fn check_luau_definitions(project: &std::path::Path) -> DoctorCheck {
    use sha2::{Digest as _, Sha256};

    let project_copy = project.join(snapshot::ROBLOX_DEFINITIONS_PATH);
    let active_path = match find_luau_definitions(project) {
        Ok(Some(path)) => path,
        Ok(None) => return doctor_check("roblox defs", DoctorStatus::Warn, "not found"),
        Err(error) => return doctor_check("roblox defs", DoctorStatus::Fail, error),
    };
    let read_hash = |path: &std::path::Path| -> Result<String, String> {
        let bytes = crate::fs_safety::read_file_no_follow(path)
            .map_err(|error| format!("read: {error}"))?;
        Ok(format!("{:x}", Sha256::digest(&bytes)))
    };
    let read_project_hash = || -> Result<String, String> {
        let text = snapshot::read_project_tool_text(project, &project_copy)
            .map_err(|error| format!("read: {error}"))?
            .ok_or_else(|| "not found".to_string())?;
        Ok(format!("{:x}", Sha256::digest(text.as_bytes())))
    };
    let active_hash = match if active_path == project_copy {
        read_project_hash()
    } else {
        read_hash(&active_path)
    } {
        Ok(hash) => hash,
        Err(error) => {
            return doctor_check(
                "roblox defs",
                DoctorStatus::Fail,
                format!("lint: {} ({error})", active_path.display()),
            );
        }
    };
    let mut status = if active_hash == ROBLOX_DEFINITIONS_SHA256 {
        DoctorStatus::Ok
    } else {
        DoctorStatus::Warn
    };
    let mut details = vec![format!(
        "lint: {} ({})",
        active_path.display(),
        if active_hash == ROBLOX_DEFINITIONS_SHA256 {
            "security=None, current".to_string()
        } else {
            format!("untested sha256 {active_hash}")
        }
    )];

    if active_path != project_copy {
        match read_project_hash() {
            Ok(hash) if hash == ROBLOX_DEFINITIONS_SHA256 => details.push(format!(
                "editor: {} (security=None, current)",
                project_copy.display()
            )),
            Ok(hash) => {
                status = DoctorStatus::Warn;
                details.push(format!(
                    "editor: {} (stale sha256 {hash}; run `rosync refresh`)",
                    project_copy.display()
                ));
            }
            Err(error) if error == "not found" => {
                status = DoctorStatus::Warn;
                details.push(format!(
                    "editor: {} (missing; run `rosync refresh`)",
                    project_copy.display()
                ));
            }
            Err(error) => {
                status = DoctorStatus::Fail;
                details.push(format!("editor: {} ({error})", project_copy.display()));
            }
        }
    } else if active_hash == ROBLOX_DEFINITIONS_SHA256 {
        details[0] = format!(
            "lint/editor: {} (security=None, current)",
            active_path.display()
        );
    }

    if active_hash != ROBLOX_DEFINITIONS_SHA256 {
        details.push("run `rosync refresh` or update the installed Ro Sync bundle".to_string());
    }
    doctor_check("roblox defs", status, details.join("; "))
}

fn check_luau_compile(project: &std::path::Path) -> DoctorCheck {
    let Some(executable) = resolve_luau_compile(None) else {
        return doctor_check(
            "luau-compile",
            DoctorStatus::Warn,
            "not found; `rosync lint --compile auto` will skip bytecode checks",
        );
    };
    match std::process::Command::new(&executable)
        .arg("--help")
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => doctor_check(
            "luau-compile",
            DoctorStatus::Ok,
            executable.to_string_lossy(),
        ),
        Ok(status) => doctor_check(
            "luau-compile",
            DoctorStatus::Fail,
            format!(
                "{} rejected --help (exit {})",
                executable.to_string_lossy(),
                status.code().unwrap_or(1)
            ),
        ),
        Err(error) => doctor_check(
            "luau-compile",
            DoctorStatus::Fail,
            format!("run {}: {error}", executable.to_string_lossy()),
        ),
    }
}

fn check_luaurc(project: &std::path::Path) -> DoctorCheck {
    let path = project.join(snapshot::LUAURC);
    let text = match snapshot::read_project_tool_text(project, &path) {
        Ok(Some(text)) => text,
        Ok(None) => return doctor_check(".luaurc", DoctorStatus::Warn, "missing"),
        Err(error) => {
            return doctor_check(".luaurc", DoctorStatus::Fail, format!("read: {error}"));
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return doctor_check(
                ".luaurc",
                DoctorStatus::Fail,
                format!("invalid JSON: {error}"),
            );
        }
    };
    let Some(object) = value.as_object() else {
        return doctor_check(
            ".luaurc",
            DoctorStatus::Fail,
            "top-level value must be a JSON object",
        );
    };
    if object.contains_key("definitions") {
        return doctor_check(
            ".luaurc",
            DoctorStatus::Warn,
            "contains unsupported `definitions`; run `rosync refresh` to remove it",
        );
    }
    let language_mode = match object.get("languageMode") {
        None => "default".to_string(),
        Some(serde_json::Value::String(mode))
            if matches!(mode.as_str(), "nocheck" | "nonstrict" | "strict") =>
        {
            mode.clone()
        }
        Some(value) => {
            return doctor_check(
                ".luaurc",
                DoctorStatus::Fail,
                format!(
                    "invalid languageMode {}; expected nocheck, nonstrict, or strict",
                    value
                ),
            );
        }
    };
    doctor_check(
        ".luaurc",
        DoctorStatus::Ok,
        format!("{} (languageMode={language_mode})", path.display()),
    )
}

fn check_writes_log_path() -> DoctorCheck {
    if let Ok(dir) = lifecycle::state_dir(None) {
        let log = dir.join("writes.log");
        if log.exists() {
            return doctor_check("writes.log", DoctorStatus::Ok, log.display().to_string());
        }
        if dir.is_dir() {
            return doctor_check(
                "writes.log",
                DoctorStatus::Warn,
                format!("not created yet: {}", log.display()),
            );
        }
    }
    if let Some(dir) = lifecycle::legacy_widget_dir() {
        let legacy = dir.join("writes.log");
        if legacy.exists() {
            return doctor_check(
                "writes.log",
                DoctorStatus::Warn,
                format!("legacy location: {}", legacy.display()),
            );
        }
    }
    doctor_check("writes.log", DoctorStatus::Warn, "not created yet")
}

fn fetch_daemon_hello(port: u16) -> Result<serde_json::Value, String> {
    http_get_json(port, "/hello")
}

fn http_get_json(port: u16, path: &str) -> Result<serde_json::Value, String> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = Duration::from_millis(750);
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "HTTP deadline overflow".to_string())?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("connect http://127.0.0.1:{port}{path}: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("set HTTP write timeout: {error}"))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;
    const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
    let max_wire_bytes = LOCAL_HTTP_MAX_JSON_BYTES + MAX_HTTP_HEADER_BYTES;
    let mut response = Vec::with_capacity(8 * 1024);
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("GET http://127.0.0.1:{port}{path} timed out"));
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("set HTTP read timeout: {error}"))?;
        let count = stream
            .read(&mut buffer)
            .map_err(|error| format!("read response: {error}"))?;
        if count == 0 {
            break;
        }
        if response.len().saturating_add(count) > max_wire_bytes {
            return Err(format!(
                "GET http://127.0.0.1:{port}{path} response exceeded {max_wire_bytes} bytes"
            ));
        }
        response.extend_from_slice(&buffer[..count]);
    }
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "HTTP response omitted the header separator".to_string())?;
    if separator > MAX_HTTP_HEADER_BYTES {
        return Err("HTTP response headers exceeded the byte limit".into());
    }
    let head = std::str::from_utf8(&response[..separator])
        .map_err(|error| format!("HTTP response headers are not UTF-8: {error}"))?;
    let body = &response[separator + 4..];
    if body.len() > LOCAL_HTTP_MAX_JSON_BYTES {
        return Err(format!(
            "HTTP JSON response exceeded {} bytes",
            LOCAL_HTTP_MAX_JSON_BYTES
        ));
    }
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        let status = head.lines().next().unwrap_or("no HTTP status");
        return Err(status.to_string());
    }
    serde_json::from_slice(body).map_err(|e| format!("parse response JSON: {e}"))
}

async fn read_bounded_json_response(
    mut response: reqwest::Response,
    url: &str,
) -> Result<(reqwest::StatusCode, serde_json::Value), String> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > LOCAL_HTTP_MAX_JSON_BYTES as u64)
    {
        return Err(format!(
            "{url} response exceeds the {}-byte JSON limit",
            LOCAL_HTTP_MAX_JSON_BYTES
        ));
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(LOCAL_HTTP_MAX_JSON_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read {url} response: {error}"))?
    {
        let next = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| format!("{url} response size overflow"))?;
        if next > LOCAL_HTTP_MAX_JSON_BYTES {
            return Err(format!(
                "{url} response exceeds the {}-byte JSON limit",
                LOCAL_HTTP_MAX_JSON_BYTES
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {url} response JSON: {error}"))?;
    Ok((status, value))
}

async fn http_json_until(
    port: u16,
    method: reqwest::Method,
    path: &str,
    body: Option<&serde_json::Value>,
    deadline: Instant,
) -> Result<serde_json::Value, String> {
    if !path.starts_with('/') || path.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
        return Err("invalid local HTTP path".into());
    }
    let remaining = capture_deadline_remaining(deadline, "local HTTP request")?;
    let connect_timeout = LOCAL_HTTP_CONNECT_TIMEOUT.min(remaining);
    let client = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(remaining)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build local HTTP client: {error}"))?;
    let remaining = capture_deadline_remaining(deadline, "local HTTP request")?;
    let url = format!("http://127.0.0.1:{port}{path}");
    let method_name = method.as_str().to_string();
    let mut request = client.request(method, &url).timeout(remaining);
    if let Some(body) = body {
        request = request.json(body);
    }
    let operation = async {
        let response = request
            .send()
            .await
            .map_err(|error| format!("{method_name} {url}: {error}"))?;
        let (status, value) = read_bounded_json_response(response, &url).await?;
        if !status.is_success() {
            return Err(format!("{method_name} {url}: {status}: {value}"));
        }
        Ok(value)
    };
    tokio::time::timeout(remaining, operation)
        .await
        .map_err(|_| format!("{method_name} {url} timed out"))?
}

async fn http_get_json_until(
    port: u16,
    path: &str,
    deadline: Instant,
) -> Result<serde_json::Value, String> {
    http_json_until(port, reqwest::Method::GET, path, None, deadline).await
}

async fn http_post_json_until(
    port: u16,
    path: &str,
    body: &serde_json::Value,
    deadline: Instant,
) -> Result<serde_json::Value, String> {
    http_json_until(port, reqwest::Method::POST, path, Some(body), deadline).await
}

async fn http_post_json(
    port: u16,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now()
        .checked_add(LOCAL_HTTP_DEFAULT_TIMEOUT)
        .ok_or_else(|| "local HTTP deadline overflow".to_string())?;
    http_post_json_until(port, path, body, deadline).await
}

async fn consume_artifact_transport_until(
    port: u16,
    id: &str,
    deadline: Instant,
) -> Result<(), String> {
    validate_artifact_id(id)?;
    let response = http_post_json_until(
        port,
        &format!("/artifacts/{id}/consume"),
        &serde_json::json!({}),
        deadline,
    )
    .await?;
    if response.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!("artifact consume rejected: {response}"))
    }
}

fn project_or_cwd(
    project: Option<&std::path::Path>,
    context: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    match project {
        Some(path) => Ok(path.to_path_buf()),
        None => {
            std::env::current_dir().map_err(|e| format!("{context}: current directory: {e}").into())
        }
    }
}

fn command_names_from_bundle(bundle: &serde_json::Value) -> Vec<String> {
    bundle
        .get("commands")
        .and_then(|value| value.as_array())
        .map(|commands| {
            commands
                .iter()
                .filter_map(|command| {
                    command
                        .get("name")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn daemon_project_mismatch(
    hello: &serde_json::Value,
    canonical_project: &std::path::Path,
) -> serde_json::Value {
    let Some(daemon_project) = hello.get("project").and_then(|value| value.as_str()) else {
        return serde_json::Value::Null;
    };
    let daemon_path = std::path::Path::new(daemon_project);
    let daemon_canonical = canonicalize_project_path(daemon_path);
    let mismatch = daemon_canonical != canonical_project;
    serde_json::json!({
        "mismatch": mismatch,
        "daemonProject": daemon_project,
        "daemonCanonicalPath": daemon_canonical.display().to_string(),
        "requestedCanonicalPath": canonical_project.display().to_string(),
    })
}

fn compact_command_registry(
    bundle: &serde_json::Value,
    name: Option<&str>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let commands = bundle
        .get("commands")
        .and_then(|value| value.as_array())
        .ok_or("commands: embedded registry missing commands array")?;
    let mut rows = Vec::new();
    for command in commands {
        let command_name = command
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if name.is_some_and(|needle| needle != command_name) {
            continue;
        }
        let mut row = serde_json::json!({
            "name": command_name,
            "category": command.get("category").and_then(|value| value.as_str()).unwrap_or(""),
            "summary": command.get("description").and_then(|value| value.as_str()).unwrap_or(""),
            "outputCost": command_output_cost(command_name),
            "safety": command_safety_class(command_name),
            "requires": command_requirements(command_name),
            "preferBefore": command_prefer_before(command_name),
            "usageLookup": format!("rosync commands {command_name}"),
        });
        for key in ["subcommands", "subcommandPaths", "subcommandMetadata"] {
            if let Some(value) = command.get(key) {
                row[key] = value.clone();
            }
        }
        rows.push(row);
    }
    if name.is_some() && rows.is_empty() {
        return Err(format!("commands: unknown command {name:?}").into());
    }
    Ok(serde_json::json!({
        "schema": "ro-sync.commands.compact.v1",
        "count": rows.len(),
        "rules": [
            "Use this compact index for command choice; use `rosync commands <name>` for exact flags.",
            "Avoid plain `rosync commands` unless the full registry is explicitly needed.",
            "Prefer cheap/offline commands before live reads; prefer plan/preflight before mutating commands.",
            "Avoid stream commands (`watch`, `tail`, `logs --tail`) in delegated agents unless explicitly requested."
        ],
        "commands": rows,
    }))
}

fn command_output_cost(name: &str) -> &'static str {
    match name {
        "commands" => "high-full-or-low-single",
        "context" | "capabilities" | "plan" | "query" | "path" | "meta" | "services" | "where"
        | "open" | "classinfo" | "enums" | "enum" | "ping" | "version" => "low",
        "init" | "plugin" | "auth" | "daemon" | "status" | "doctor" | "ls" | "tree" | "props"
        | "find" | "find-attr" | "logs" | "resolve" | "decision" | "lint" | "upload"
        | "monetization" | "set" | "new" | "rm" | "mv" | "attr" | "tag" | "select" | "copy"
        | "paste" | "save" | "waypoint" | "undo" | "redo" | "refresh" | "repair" | "capture"
        | "playtest" | "run" => "medium",
        "source" | "conflicts" => "medium-special-case",
        "diff" | "changes" | "snapshot" | "get" | "eval" | "transmit" | "call" | "tail"
        | "watch" | "serve" => "high-or-streaming",
        _ => "unknown",
    }
}

fn command_safety_class(name: &str) -> &'static str {
    match name {
        "set" | "new" | "rm" | "mv" | "attr" | "tag" | "paste" | "save" | "waypoint" | "undo"
        | "redo" => "mutates-studio",
        "copy" => "reads-studio-and-writes-private-clipboard",
        "resolve" | "decision" => "mutates-disk-or-studio",
        "eval" | "call" | "transmit" => "risky-live-execution",
        "playtest" => "controls-playtest-and-runtime-execution",
        "run" => "workflow-declared-mutations",
        "capture" => "captures-screen-and-writes-local-artifacts",
        "select" | "open" => "mutates-studio-selection",
        "upload" | "monetization" => "open-cloud-mutating",
        "init" | "plugin" | "refresh" | "snapshot" | "repair" => "writes-local-files",
        "auth" => "writes-local-credentials",
        "daemon" => "controls-local-service",
        "tail" | "watch" => "streaming-read",
        "serve" => "starts-local-service",
        "commands" | "context" | "capabilities" | "plan" | "query" | "path" | "lint" | "get"
        | "ls" | "tree" | "diff" | "changes" | "services" | "meta" | "props" | "source"
        | "where" | "conflicts" | "find" | "find-attr" | "classinfo" | "enums" | "enum"
        | "logs" | "status" | "doctor" | "ping" | "version" => "read-only",
        _ => "unclassified-assume-mutating",
    }
}

fn command_requirements(name: &str) -> Vec<&'static str> {
    match name {
        "query" | "path" | "meta" | "services" | "source" | "decision" | "capabilities"
        | "capture" | "playtest" | "copy" | "paste" => {
            vec!["project", "daemon", "studio-plugin"]
        }
        "run" => vec!["project", "daemon", "studio-plugin", "workflow-file"],
        "lint" => vec!["project"],
        "upload" | "monetization" => vec!["project", "roblox-open-cloud-credential"],
        "commands" | "plan" => vec![],
        "snapshot" | "diff" | "changes" => vec!["project", "daemon", "studio-plugin"],
        "context" | "status" | "doctor" | "refresh" | "init" | "daemon" | "serve" => {
            vec!["project"]
        }
        "plugin" => vec!["bundled-plugin", "roblox-plugin-directory"],
        "auth" => vec!["credential-input-for-set"],
        _ => vec!["project", "daemon", "studio-plugin"],
    }
}

fn command_prefer_before(name: &str) -> Vec<&'static str> {
    match name {
        "get" | "props" => vec!["meta", "get --prop when possible"],
        "source" => vec![
            "local file read first",
            "lint touched paths for verification",
            "live source only for Studio/editor divergence",
        ],
        "diff" | "changes" => vec!["lint touched paths first", "status --raw", "services --raw"],
        "snapshot" => vec!["tree --depth 3", "changes"],
        "set" | "new" | "rm" | "mv" => {
            vec!["meta/get target first", "waypoint for multi-step edits"]
        }
        "resolve" => vec![
            "conflicts only when resolving",
            "inspect pending conflict first",
        ],
        "decision" => vec!["decision without flags to inspect pending choice first"],
        "attr" | "tag" | "call" | "eval" | "transmit" | "select" | "save" => {
            vec!["status --raw", "waypoint for multi-step edits"]
        }
        "copy" => vec!["select get when copying the current selection"],
        "paste" => vec![
            "status --raw",
            "use --to when original parents do not exist",
        ],
        "upload" => vec!["enumerate exact files", "use --manifest for bulk uploads"],
        "capture" => vec![
            "capabilities",
            "capture status",
            "authorize only when required",
        ],
        "playtest" => vec!["capabilities", "playtest status", "playtest contexts"],
        "run" => vec![
            "run --dry-run",
            "use transactions for multi-step Studio writes",
        ],
        "monetization" => vec![
            "monetization discover",
            "monetization list",
            "prefer --id over --name",
        ],
        "watch" | "tail" | "logs" => vec!["logs --limit 50 unless streaming is requested"],
        _ => Vec::new(),
    }
}

fn context_services(project: &std::path::Path) -> Vec<serde_json::Value> {
    snapshot::SYNCED_SERVICES
        .iter()
        .map(|service| {
            let path = project.join(service);
            let exists = fs_safety::validate_service_path(project, service, true)
                .and_then(|safe| fs_safety::metadata_no_follow(&safe))
                .ok()
                .flatten()
                .is_some_and(|metadata| metadata.is_dir());
            serde_json::json!({
                "name": service,
                "diskPath": path.display().to_string(),
                "exists": exists,
            })
        })
        .collect()
}

fn count_tree_nodes(node: &serde_json::Value) -> usize {
    let mut count = 0usize;
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        count = count.saturating_add(1);
        if let Some(children) = current.get("children").and_then(|value| value.as_array()) {
            pending.extend(children);
        }
    }
    count
}

fn context_project_files(project: &std::path::Path) -> serde_json::Value {
    serde_json::json!({
        "projectConfig": file_summary(&project.join(project_config::CONFIG_FILE)),
        "sourcemapJson": file_summary(&project.join("sourcemap.json")),
        "roSyncMd": file_summary(&project.join("ro-sync.md")),
        "agentsMd": file_summary(&project.join("AGENTS.md")),
        "claudeMd": file_summary(&project.join("CLAUDE.md")),
        "codexConfig": file_summary(&project.join(".codex").join("config.toml")),
    })
}

fn file_summary(path: &std::path::Path) -> serde_json::Value {
    let metadata = std::fs::metadata(path).ok();
    let modified_unix = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    serde_json::json!({
        "path": path.display().to_string(),
        "exists": metadata.is_some(),
        "bytes": metadata.as_ref().map(|metadata| metadata.len()),
        "modifiedUnix": modified_unix,
    })
}

fn disk_source_path(path: &std::path::Path) -> Result<Option<PathBuf>, String> {
    let Some(metadata) = fs_safety::metadata_no_follow(path)
        .map_err(|error| format!("inspect disk source {}: {error}", path.display()))?
    else {
        return Ok(None);
    };
    if metadata.is_file() {
        fs_safety::file_generation_no_follow(path)?;
        return Ok(Some(path.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Ok(None);
    }
    let index = fs_safety::PortableDirectoryIndex::read(path)
        .map_err(|error| format!("scan disk source directory {}: {error}", path.display()))?;
    let Some(source) = index.unique_init_source() else {
        return Ok(None);
    };
    fs_safety::file_generation_no_follow(&source.path)?;
    Ok(Some(source.path.clone()))
}

fn collect_live_service_names(
    node: &serde_json::Value,
    out: &mut std::collections::BTreeSet<String>,
) {
    let is_root = node.get("class").and_then(|v| v.as_str()) == Some("DataModel");
    if is_root {
        if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
            for child in children {
                if let Some(name) = child.get("name").and_then(|v| v.as_str()) {
                    out.insert(name.to_string());
                }
            }
        }
    }
}

fn short_hash(hash: &str) -> &str {
    if hash.len() > 12 {
        &hash[..12]
    } else {
        hash
    }
}

fn print_ws_frame_compact(text: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        println!("{text}");
        return;
    };
    let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    match kind {
        "op" => {
            let op = value.get("op").unwrap_or(&serde_json::Value::Null);
            let op_kind = op
                .get("op")
                .or_else(|| op.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("op");
            let path = op
                .get("path")
                .and_then(|v| v.as_array())
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.as_str())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            println!("{op_kind:12} {path}");
        }
        "request" => {
            let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("?");
            println!("request     {op}");
        }
        "response" => {
            let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            println!("response    ok={ok}");
        }
        other => println!("{other:12} {text}"),
    }
}

// ---------------------------------------------------------------------------
// Tier 1 runners. `mv` requires `--force` to cross service boundaries
// (enforced plugin-side).
// ---------------------------------------------------------------------------

async fn run_new(args: NewArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut req = serde_json::Map::new();
    req.insert(
        "parent".into(),
        serde_json::Value::String(args.path.clone()),
    );
    req.insert(
        "class".into(),
        serde_json::Value::String(args.class.clone()),
    );
    if let Some(n) = &args.name {
        req.insert("name".into(), serde_json::Value::String(n.clone()));
    }
    if let Some(props_raw) = &args.props {
        let props: serde_json::Value = serde_json::from_str(props_raw)
            .map_err(|e| format!("new: --props must be a JSON object ({e})"))?;
        req.insert("initial_props".into(), props);
    }
    let resp = remote::request(args.port, "new", serde_json::Value::Object(req)).await?;
    let class_label = args.class.clone();
    print_response(&resp, args.raw, |v| {
        let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let class = v
            .get("class")
            .and_then(|v| v.as_str())
            .unwrap_or(&class_label);
        println!("ok: created {class} at {path}");
    });
    ok_or_err(&resp)
}

async fn run_rm(args: RmArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req = serde_json::json!({ "path": args.path });
    let resp = remote::request(args.port, "rm", req).await?;
    let fallback_path = args.path.clone();
    print_response(&resp, args.raw, |v| {
        let path = v
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(&fallback_path);
        println!("ok: destroyed {path}");
    });
    ok_or_err(&resp)
}

async fn run_mv(args: MvArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req = serde_json::json!({
        "from": args.from,
        "to": args.to,
        "force": args.force,
    });
    let resp = remote::request(args.port, "mv", req).await?;
    print_response(&resp, args.raw, |v| {
        let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let parent = v.get("parent").and_then(|v| v.as_str()).unwrap_or("?");
        println!("ok: {path} (parent: {parent})");
    });
    ok_or_err(&resp)
}

async fn run_attr(args: AttrArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        AttrCommand::Set(a) => run_attr_set(a).await,
        AttrCommand::Rm(a) => run_attr_rm(a).await,
        AttrCommand::Ls(a) => run_attr_ls(a).await,
    }
}

async fn run_attr_set(args: AttrSetArgs) -> Result<(), Box<dyn std::error::Error>> {
    let value: serde_json::Value = serde_json::from_str(&args.value)
        .map_err(|e| format!("attr set: --value must be a JSON literal ({e})"))?;
    let req = serde_json::json!({
        "path": args.path,
        "name": args.name,
        "value": value,
    });
    let resp = remote::request(args.port, "set_attr", req).await?;
    let path_label = args.path.clone();
    let name_label = args.name.clone();
    print_response(&resp, args.raw, |_| {
        println!("ok: {path_label}@{name_label} set");
    });
    ok_or_err(&resp)
}

async fn run_attr_rm(args: AttrRmArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req = serde_json::json!({ "path": args.path, "name": args.name });
    let resp = remote::request(args.port, "rm_attr", req).await?;
    let path_label = args.path.clone();
    let name_label = args.name.clone();
    print_response(&resp, args.raw, |_| {
        println!("ok: {path_label}@{name_label} cleared");
    });
    ok_or_err(&resp)
}

async fn run_attr_ls(args: AttrLsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req = serde_json::json!({ "path": args.path });
    let resp = remote::request(args.port, "attr_ls", req).await?;
    print_response(&resp, args.raw, |v| {
        let obj = match v.as_object() {
            Some(o) => o,
            None => {
                println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
                return;
            }
        };
        if obj.is_empty() {
            println!("(no attributes)");
            return;
        }
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        for k in keys {
            println!("  {k} = {}", format_pretty_value(&obj[k]));
        }
    });
    ok_or_err(&resp)
}

async fn run_tag(args: TagArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        TagCommand::Add(a) => run_tag_mut(a, "add_tag", "added").await,
        TagCommand::Rm(a) => run_tag_mut(a, "rm_tag", "removed").await,
    }
}

async fn run_tag_mut(
    args: TagMutArgs,
    op: &'static str,
    verb: &'static str,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = serde_json::json!({ "path": args.path, "tag": args.tag });
    let resp = remote::request(args.port, op, req).await?;
    let path_label = args.path.clone();
    let tag_label = args.tag.clone();
    print_response(&resp, args.raw, |_| {
        println!("ok: tag {tag_label:?} {verb} on {path_label}");
    });
    ok_or_err(&resp)
}

async fn run_call(args: CallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let call_args: serde_json::Value = match &args.args {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| format!("call: --args must be a JSON array ({e})"))?,
        None => serde_json::Value::Array(vec![]),
    };
    if !call_args.is_array() {
        return Err("call: --args must be a JSON array".into());
    }
    let req = serde_json::json!({
        "path": args.path,
        "method": args.method,
        "args": call_args,
    });
    let resp = remote::request(args.port, "call", req).await?;
    print_response(&resp, args.raw, |v| {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    });
    ok_or_err(&resp)
}

async fn run_select(args: SelectArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        SelectCommand::Get(a) => run_select_get(a).await,
        SelectCommand::Set(a) => run_select_set(a).await,
    }
}

async fn run_select_get(args: SelectGetArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request(args.port, "select_get", serde_json::json!({})).await?;
    print_response(&resp, args.raw, |v| {
        let Some(arr) = v.as_array() else {
            println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
            return;
        };
        if arr.is_empty() {
            println!("(empty selection)");
            return;
        }
        for item in arr {
            if let Some(s) = item.as_str() {
                println!("{s}");
            }
        }
    });
    ok_or_err(&resp)
}

async fn run_select_set(args: SelectSetArgs) -> Result<(), Box<dyn std::error::Error>> {
    let paths: serde_json::Value = serde_json::from_str(&args.paths)
        .map_err(|e| format!("select set: --paths must be a JSON array ({e})"))?;
    if !paths.is_array() {
        return Err("select set: --paths must be a JSON array".into());
    }
    let req = serde_json::json!({ "paths": paths });
    let resp = remote::request(args.port, "select_set", req).await?;
    print_response(&resp, args.raw, |v| {
        let count = v.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("ok: selection set ({count} instance(s))");
    });
    ok_or_err(&resp)
}

fn print_response<F: FnOnce(&serde_json::Value)>(resp: &serde_json::Value, raw: bool, pretty: F) {
    if raw {
        println!(
            "{}",
            serde_json::to_string_pretty(resp).unwrap_or_else(|_| resp.to_string())
        );
        return;
    }
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        eprintln!("error: {}", response_error_message(resp));
        return;
    }
    let empty = serde_json::Value::Null;
    let value = resp.get("value").unwrap_or(&empty);
    pretty(value);
}

fn ok_or_err(resp: &serde_json::Value) -> Result<(), Box<dyn std::error::Error>> {
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(response_error_message(resp).into())
    }
}

fn print_get(args: &GetArgs, value: &serde_json::Value) {
    if let Some(prop) = &args.prop {
        println!(
            "{} = {}",
            prop,
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
        return;
    }
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            println!(
                "{}",
                serde_json::to_string_pretty(value).unwrap_or_default()
            );
            return;
        }
    };
    let class = obj.get("class").and_then(|v| v.as_str()).unwrap_or("?");
    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    println!("{class} {name}  ({path})");
    if let Some(props) = obj.get("properties").and_then(|v| v.as_object()) {
        if !props.is_empty() {
            println!("Properties:");
            let mut keys: Vec<&String> = props.keys().collect();
            keys.sort();
            for k in keys {
                println!("  {k} = {}", format_pretty_value(&props[k]));
            }
        }
    }
    if let Some(attrs) = obj.get("attributes").and_then(|v| v.as_object()) {
        if !attrs.is_empty() {
            println!("Attributes:");
            let mut keys: Vec<&String> = attrs.keys().collect();
            keys.sort();
            for k in keys {
                println!("  {k} = {}", format_pretty_value(&attrs[k]));
            }
        }
    }
    if let Some(tags) = obj.get("tags").and_then(|v| v.as_array()) {
        if !tags.is_empty() {
            let labels: Vec<String> = tags
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            println!("Tags: {}", labels.join(", "));
        }
    }
    if let Some(kids) = obj.get("children").and_then(|v| v.as_array()) {
        if !kids.is_empty() {
            println!("Children ({}):", kids.len());
            for k in kids {
                let c = k.get("class").and_then(|v| v.as_str()).unwrap_or("?");
                let n = k.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  {c:20} {n}");
            }
        }
    }
}

fn format_pretty_value(v: &serde_json::Value) -> String {
    if let Some(obj) = v.as_object() {
        if let Some(tag) = obj.get("__type").and_then(|v| v.as_str()) {
            let num = |k: &str| obj.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            match tag {
                "Vector3" => {
                    return format!("Vector3({:.3}, {:.3}, {:.3})", num("x"), num("y"), num("z"))
                }
                "Vector2" => return format!("Vector2({:.3}, {:.3})", num("x"), num("y")),
                "Color3" => {
                    return format!("Color3({:.3}, {:.3}, {:.3})", num("r"), num("g"), num("b"))
                }
                "UDim" => return format!("UDim({:.3}, {})", num("scale"), num("offset") as i64),
                "UDim2" => {
                    return format!(
                        "UDim2({:.3}, {}, {:.3}, {})",
                        num("xScale"),
                        num("xOffset") as i64,
                        num("yScale"),
                        num("yOffset") as i64
                    )
                }
                "BrickColor" => {
                    if let Some(n) = obj.get("name").and_then(|v| v.as_str()) {
                        return format!("BrickColor({n})");
                    }
                }
                "EnumItem" => {
                    let e = obj.get("enumType").and_then(|v| v.as_str()).unwrap_or("?");
                    let n = obj.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    return format!("Enum.{e}.{n}");
                }
                "Instance" => {
                    if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
                        return format!("→ {p}");
                    }
                }
                "CFrame" => {
                    return format!(
                        "CFrame(pos=({:.3}, {:.3}, {:.3}))",
                        num("x"),
                        num("y"),
                        num("z")
                    )
                }
                "NumberRange" => {
                    return format!("NumberRange({:.3}..{:.3})", num("min"), num("max"))
                }
                _ => {}
            }
        }
    }
    serde_json::to_string(v).unwrap_or_default()
}

fn print_set(path: &str, prop: &str, value: &serde_json::Value) {
    let _ = value;
    println!("ok: {path}.{prop} set");
}

fn print_ls(value: &serde_json::Value) {
    let Some(arr) = value.as_array() else {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
        return;
    };
    for item in arr {
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let class = item.get("class").and_then(|v| v.as_str()).unwrap_or("?");
        println!("  {class:20} {name}");
    }
}

fn print_tree(value: &serde_json::Value, depth: usize) {
    let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    let class = value.get("class").and_then(|v| v.as_str()).unwrap_or("?");
    let indent = "  ".repeat(depth);
    println!("{indent}{class} {name}");
    if let Some(kids) = value.get("children").and_then(|v| v.as_array()) {
        for k in kids {
            print_tree(k, depth + 1);
        }
    }
}

fn print_find(value: &serde_json::Value) {
    let Some(arr) = value.as_array() else {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
        return;
    };
    // Plugin returns `[path, ...]` (array of strings); some test responders
    // return `[{class,name,path}, ...]` — handle both.
    for item in arr {
        if let Some(path) = item.as_str() {
            println!("  {path}");
            continue;
        }
        let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let class = item.get("class").and_then(|v| v.as_str()).unwrap_or("?");
        println!("  {class:20} {path}");
    }
}

fn print_diff_report(report: &diff::DiffReport) {
    if report.is_clean() {
        println!("in sync: local project matches Studio scripts/folders");
        return;
    }

    println!(
        "diff: {} added locally, {} removed locally, {} changed",
        report.summary.added, report.summary.removed, report.summary.changed
    );
    if !report.added.is_empty() {
        println!("Added locally:");
        for item in &report.added {
            print_diff_item("+", &item.class, item.kind, &item.path);
        }
    }
    if !report.removed.is_empty() {
        println!("Removed locally:");
        for item in &report.removed {
            print_diff_item("-", &item.class, item.kind, &item.path);
        }
    }
    if !report.changed.is_empty() {
        println!("Changed:");
        for item in &report.changed {
            let mut reasons = Vec::new();
            if item.class_changed {
                reasons.push("class");
            }
            if item.source_changed {
                reasons.push("source");
            }
            let reason = if reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", reasons.join(", "))
            };
            let class = if item.local_class == item.studio_class {
                item.local_class.clone()
            } else {
                format!("{} -> {}", item.studio_class, item.local_class)
            };
            println!(
                "  ~ {:20} {:7} {}{}",
                class,
                diff_kind_label(item.kind),
                item.path,
                reason
            );
        }
    }
}

fn print_diff_item(prefix: &str, class: &str, kind: diff::DiffKind, path: &str) {
    println!("  {prefix} {class:20} {:7} {path}", diff_kind_label(kind));
}

fn diff_kind_label(kind: diff::DiffKind) -> &'static str {
    match kind {
        diff::DiffKind::Folder => "folder",
        diff::DiffKind::Script => "script",
    }
}

// ---------------------------------------------------------------------------
// Tier 3 — class introspection, enum listing, attribute-scoped find.
// ---------------------------------------------------------------------------

async fn run_classinfo(args: ClassInfoArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req = serde_json::json!({ "class_name": args.class_name });
    let resp = remote::request(args.port, "class_info", req).await?;
    let cls = args.class_name.clone();
    print_response(&resp, args.raw, |v| print_classinfo(&cls, v));
    ok_or_err(&resp)
}

async fn run_enums(args: EnumsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request(args.port, "enums", serde_json::json!({})).await?;
    print_response(&resp, args.raw, |v| {
        if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(s) = item.as_str() {
                    println!("{s}");
                }
            }
        }
    });
    ok_or_err(&resp)
}

async fn run_enum(args: EnumArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req = serde_json::json!({ "enum_name": args.name });
    let resp = remote::request(args.port, "enum_list", req).await?;
    let name = args.name.clone();
    print_response(&resp, args.raw, |v| print_enum_items(&name, v));
    ok_or_err(&resp)
}

async fn run_find_attr(args: FindAttrArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut req = serde_json::Map::new();
    req.insert("name".into(), serde_json::Value::String(args.name.clone()));
    if let Some(u) = &args.under {
        req.insert("under".into(), serde_json::Value::String(u.clone()));
    }
    if let Some(raw) = &args.value {
        let parsed: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| format!("find-attr: --value must be a JSON literal ({e})"))?;
        req.insert("value".into(), parsed);
    }
    let resp = remote::request(args.port, "find_by_attr", serde_json::Value::Object(req)).await?;
    print_response(&resp, args.raw, print_find);
    ok_or_err(&resp)
}

fn print_classinfo(class_name: &str, value: &serde_json::Value) {
    let Some(obj) = value.as_object() else {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
        return;
    };
    println!("{class_name}");
    if let Some(props) = obj.get("properties").and_then(|v| v.as_array()) {
        // Group by category. Preserve first-seen order per category so the
        // output is deterministic without requiring stable group ordering.
        let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for p in props {
            let name = p
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cat = p
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let ty = p
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let cat_label = if cat.is_empty() {
                "(uncategorized)".to_string()
            } else {
                cat
            };
            match groups.iter_mut().find(|(c, _)| c == &cat_label) {
                Some((_, entries)) => entries.push((name, ty)),
                None => groups.push((cat_label, vec![(name, ty)])),
            }
        }
        if !groups.is_empty() {
            println!("Properties:");
            for (cat, entries) in &groups {
                println!("  [{cat}]");
                for (name, ty) in entries {
                    if ty.is_empty() {
                        println!("    {name}");
                    } else {
                        println!("    {name:28} : {ty}");
                    }
                }
            }
        }
    }
    if let Some(methods) = obj.get("methods").and_then(|v| v.as_array()) {
        let names: Vec<&str> = methods.iter().filter_map(|v| v.as_str()).collect();
        if !names.is_empty() {
            println!("Methods:");
            for n in names {
                println!("  {n}");
            }
        }
    }
    // Events are only populated by the ReflectionService path; older reflection
    // sources report none, so an empty list simply prints nothing.
    if let Some(events) = obj.get("events").and_then(|v| v.as_array()) {
        let names: Vec<&str> = events.iter().filter_map(|v| v.as_str()).collect();
        if !names.is_empty() {
            println!("Events:");
            for n in names {
                println!("  {n}");
            }
        }
    }
}

fn print_enum_items(enum_name: &str, value: &serde_json::Value) {
    let Some(arr) = value.as_array() else {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
        return;
    };
    println!("Enum.{enum_name}");
    for item in arr {
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let val = item.get("value");
        if let Some(n) = val.and_then(|v| v.as_i64()) {
            println!("  {name:30} = {n}");
        } else if let Some(n) = val.and_then(|v| v.as_f64()) {
            println!("  {name:30} = {n}");
        } else {
            println!("  {name}");
        }
    }
}

#[cfg(test)]
mod tier2_tests;
