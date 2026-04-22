use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

mod conflict;
mod fs_map;
mod http;
mod initial_sync;
mod project_config;
mod query;
mod remote;
mod snapshot;
mod watch;
mod ws;

use conflict::{ConflictEngine, FsDecision};
use initial_sync::PendingInitial;
use watch::{Op, OpKind, Watch};
use ws::{PendingRoutes, RequestEnvelope};

#[derive(Parser, Debug)]
#[command(name = "rosync", version, about = "Ro Sync — Roblox Studio sync daemon")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Project directory. Required when no subcommand is given (back-compat
    /// daemon mode); subcommands accept their own `--project`.
    #[arg(long)]
    pub project: Option<PathBuf>,

    #[arg(long, default_value_t = 7878)]
    pub port: u16,

    /// Roblox GameId (Int64 — stored as string to avoid JSON precision loss).
    #[arg(long = "game-id")]
    pub game_id: Option<String>,

    /// Roblox PlaceId — may be repeated.
    #[arg(long = "place-id")]
    pub place_id: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the HTTP/WebSocket sync daemon.
    Serve(ServeArgs),
    /// Match a selector against the plugin-emitted `tree.json` skeleton.
    Query(QueryArgs),
    /// Read an instance (or a single property) from the live Studio session
    /// via the plugin.
    Get(GetArgs),
    /// Set a property on a Studio instance. Requires `--yes` unless run in
    /// `--batch` mode.
    Set(SetArgs),
    /// List the children of a Studio instance.
    Ls(LsArgs),
    /// Print a subtree rooted at a Studio instance.
    Tree(TreeArgs),
    /// Find instances matching a class and/or name.
    Find(FindArgs),
    /// Execute Luau source inside Studio. Requires `--yes`. Escape hatch for
    /// anything the structured ops don't cover.
    Eval(EvalArgs),
    /// Read recent Studio output/warn/error messages from the plugin's ring
    /// buffer.
    Logs(LogsArgs),
    /// Ask Studio to save the place (async — returns immediately).
    Save(SaveArgs),
    /// Pop one entry off Studio's change history (equivalent to ctrl-Z).
    Undo(UndoArgs),
    /// Re-apply the last undone change (equivalent to ctrl-shift-Z).
    Redo(RedoArgs),
    /// Set a named change-history waypoint. Bracketing a batch of `set` calls
    /// in a pair of waypoints makes one ctrl-Z reverse the whole batch.
    Waypoint(WaypointArgs),
    /// Round-trip a ping to the plugin; prints latency + plugin version.
    Ping(PingArgs),
    /// Print the daemon build version and (if reachable) the plugin version.
    Version(VersionArgs),
    /// Construct a new instance under a parent path. Requires `--yes`.
    New(NewArgs),
    /// Destroy an instance. Requires `--yes`.
    Rm(RmArgs),
    /// Reparent an instance. Requires `--yes`. Cross-service moves require
    /// `--force`.
    Mv(MvArgs),
    /// Attribute ops: `attr set|rm|ls`.
    Attr(AttrArgs),
    /// CollectionService tag ops: `tag add|rm`.
    Tag(TagArgs),
    /// Invoke a method on an instance (`inst:Method(args...)`). Requires
    /// `--yes`.
    Call(CallArgs),
    /// Selection service: `select get|set`.
    Select(SelectArgs),
    /// Class introspection — list properties (by category) and methods for a
    /// class, so agents can build a mental model before calling `get`/`set`.
    Classinfo(ClassInfoArgs),
    /// List every Enum type name exposed by Studio.
    Enums(EnumsArgs),
    /// List items for one Enum type, e.g. `--name Material`.
    Enum(EnumArgs),
    /// Find instances that have a given attribute set (optionally scoped to a
    /// subtree and filtered by value).
    FindAttr(FindAttrArgs),
}

#[derive(ClapArgs, Debug)]
pub struct GetArgs {
    /// Project directory (informational; daemon connection uses `--port`).
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Daemon port.
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Instance path, `/`-separated (e.g. `Workspace/Baseplate`).
    #[arg(long)]
    pub path: String,
    /// Return only this property. If omitted, returns the full instance view.
    #[arg(long)]
    pub prop: Option<String>,
    /// Print raw JSON response instead of pretty form.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct SetArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Instance path (ignored when `--batch` is passed).
    #[arg(long)]
    pub path: Option<String>,
    /// Property name (ignored when `--batch` is passed).
    #[arg(long)]
    pub prop: Option<String>,
    /// Value as a JSON literal. Examples: `true`, `42`, `"Bright red"`,
    /// `{"__type":"Vector3","x":1,"y":2,"z":3}`.
    #[arg(long)]
    pub value: Option<String>,
    /// Confirm — required for single-op mode since it's a destructive write.
    #[arg(long)]
    pub yes: bool,
    /// Read a JSON array of `{path,prop,value}` from this file and execute
    /// each entry sequentially. Batch mode implies user intent; `--yes` is
    /// not required.
    #[arg(long)]
    pub batch: Option<PathBuf>,
    /// In batch mode, continue past failures instead of aborting on the
    /// first error.
    #[arg(long = "keep-going")]
    pub keep_going: bool,
    /// Wrap the write(s) in a named change-history waypoint before and after,
    /// so one ctrl-Z in Studio reverses the whole operation.
    #[arg(long)]
    pub waypoint: Option<String>,
    /// Override the `set Parent` guardrail. `Parent =` is the single most
    /// common way to corrupt a DataModel — the CLI refuses by default and
    /// suggests `rosync mv` instead. Pass this only when you know why.
    #[arg(long = "force-parent")]
    pub force_parent: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct LsArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Instance path to list children under. Use empty string or omit for the
    /// DataModel root (services).
    #[arg(long, default_value_t = String::new())]
    pub path: String,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct TreeArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long, default_value_t = String::new())]
    pub path: String,
    /// Max recursion depth (0 = just the root itself).
    #[arg(long, default_value_t = 3)]
    pub depth: u32,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct FindArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Match instances whose `ClassName` equals this.
    #[arg(long = "class")]
    pub class_name: Option<String>,
    /// Match instances whose name contains this substring.
    #[arg(long)]
    pub name: Option<String>,
    /// Limit traversal to this instance's descendants. Empty/omitted = whole
    /// DataModel. Example: `--under Workspace/Map`.
    #[arg(long)]
    pub under: Option<String>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct LogsArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// How far back to look (e.g. `30s`, `5m`, `1h`). Defaults to `30s`.
    #[arg(long)]
    pub since: Option<String>,
    /// Minimum severity. Levels: `info` (default), `warn`, `error`.
    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    pub level: LogLevel,
    /// Cap the number of entries returned per poll.
    #[arg(long, default_value_t = 200)]
    pub limit: u32,
    /// Stream new entries as they arrive; exits on ctrl-C.
    #[arg(long)]
    pub tail: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn as_plugin_str(self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct SaveArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Required — `save` mutates the place file on disk.
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct UndoArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct RedoArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct WaypointArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Label shown in Studio's change-history UI.
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PingArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct VersionArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

// ---------------------------------------------------------------------------
// Tier 1 args — construction / destruction / reparent / attrs / tags / call /
// selection. `--yes` is required on anything that mutates the DataModel.
// ---------------------------------------------------------------------------

#[derive(ClapArgs, Debug)]
pub struct NewArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Parent instance path. The new child is created under this path.
    #[arg(long)]
    pub path: String,
    /// Roblox class name (e.g. `Part`, `Folder`, `RemoteEvent`).
    #[arg(long)]
    pub class: String,
    /// Optional Name. If omitted, the class's default is used.
    #[arg(long)]
    pub name: Option<String>,
    /// JSON object of initial properties. Values use the same encoding as
    /// `rosync set --value`.
    #[arg(long)]
    pub props: Option<String>,
    /// Required — `new` mutates the DataModel.
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct RmArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Instance path to destroy.
    #[arg(long)]
    pub path: String,
    /// Required — `rm` calls `:Destroy()`.
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct MvArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Instance path to reparent.
    #[arg(long)]
    pub from: String,
    /// Destination parent path.
    #[arg(long)]
    pub to: String,
    /// Allow moves that cross a service boundary (top-level segment change).
    #[arg(long)]
    pub force: bool,
    /// Required — `mv` mutates the DataModel.
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct AttrArgs {
    #[command(subcommand)]
    pub command: AttrCommand,
}

#[derive(Subcommand, Debug)]
pub enum AttrCommand {
    /// Set an attribute. Requires `--yes`.
    Set(AttrSetArgs),
    /// Clear an attribute. Requires `--yes`.
    Rm(AttrRmArgs),
    /// List attributes on an instance.
    Ls(AttrLsArgs),
}

#[derive(ClapArgs, Debug)]
pub struct AttrSetArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub name: String,
    /// Value as a JSON literal. Same codec as `rosync set --value`.
    #[arg(long)]
    pub value: String,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct AttrRmArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct AttrLsArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct TagArgs {
    #[command(subcommand)]
    pub command: TagCommand,
}

#[derive(Subcommand, Debug)]
pub enum TagCommand {
    /// Add a CollectionService tag. Requires `--yes`.
    Add(TagMutArgs),
    /// Remove a CollectionService tag. Requires `--yes`.
    Rm(TagMutArgs),
}

#[derive(ClapArgs, Debug)]
pub struct TagMutArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub tag: String,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct CallArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Instance path the method is invoked on (self).
    #[arg(long)]
    pub path: String,
    /// Method name (e.g. `FindFirstChild`, `GetChildren`).
    #[arg(long)]
    pub method: String,
    /// JSON array of arguments. Values use the same codec as `--value`.
    #[arg(long)]
    pub args: Option<String>,
    /// Required — `call` invokes arbitrary methods.
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct SelectArgs {
    #[command(subcommand)]
    pub command: SelectCommand,
}

#[derive(Subcommand, Debug)]
pub enum SelectCommand {
    /// Print current Studio Selection, one path per line.
    Get(SelectGetArgs),
    /// Replace the Studio Selection with the given paths. Requires `--yes`.
    Set(SelectSetArgs),
}

#[derive(ClapArgs, Debug)]
pub struct SelectGetArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct SelectSetArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// JSON array of instance paths.
    #[arg(long)]
    pub paths: String,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct EvalArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Luau source to execute. Wrap in `return ...` to get a return value.
    #[arg(long)]
    pub source: String,
    /// Required — `eval` is an unrestricted escape hatch.
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct ServeArgs {
    #[arg(long)]
    pub project: PathBuf,

    #[arg(long, default_value_t = 7878)]
    pub port: u16,

    #[arg(long = "game-id")]
    pub game_id: Option<String>,

    #[arg(long = "place-id")]
    pub place_id: Vec<String>,
}

#[derive(ClapArgs, Debug)]
pub struct QueryArgs {
    /// Project directory containing `tree.json`.
    #[arg(long)]
    pub project: PathBuf,

    /// Selector. `/`-separated; `*` matches one segment, `**` matches zero or more.
    pub selector: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = QueryFormat::Json)]
    pub format: QueryFormat,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum QueryFormat {
    Json,
    Paths,
    Classes,
}

#[derive(Clone)]
pub struct AppState {
    pub project: Arc<PathBuf>,
    /// Fully-resolved project root. Used as the canonical form for every
    /// filesystem path the daemon hands to the conflict engine or the
    /// filesystem — guarantees `/private/tmp/...` from the watcher and
    /// `/tmp/...` from `/push` hash into the same key.
    pub canonical_project: Arc<PathBuf>,
    pub events: broadcast::Sender<String>,
    pub conflict: Arc<ConflictEngine>,
    pub watch_tx: broadcast::Sender<Op>,
    pub project_name: Arc<RwLock<String>>,
    pub game_id: Arc<RwLock<Option<String>>>,
    pub place_ids: Arc<RwLock<Vec<String>>>,
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
}

/// Duration of the per-path quiet window after a `/push` write.
pub const PUSH_QUIET_MS: u64 = 200;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Query(args)) => run_query(args),
        Some(Command::Serve(args)) => run_serve(args).await,
        Some(Command::Get(args)) => run_get(args).await,
        Some(Command::Set(args)) => run_set(args).await,
        Some(Command::Ls(args)) => run_ls(args).await,
        Some(Command::Tree(args)) => run_tree(args).await,
        Some(Command::Find(args)) => run_find(args).await,
        Some(Command::Eval(args)) => run_eval(args).await,
        Some(Command::Logs(args)) => run_logs(args).await,
        Some(Command::Save(args)) => run_save(args).await,
        Some(Command::Undo(args)) => run_undo(args).await,
        Some(Command::Redo(args)) => run_redo(args).await,
        Some(Command::Waypoint(args)) => run_waypoint(args).await,
        Some(Command::Ping(args)) => run_ping(args).await,
        Some(Command::Version(args)) => run_version(args).await,
        Some(Command::New(args)) => run_new(args).await,
        Some(Command::Rm(args)) => run_rm(args).await,
        Some(Command::Mv(args)) => run_mv(args).await,
        Some(Command::Attr(args)) => run_attr(args).await,
        Some(Command::Tag(args)) => run_tag(args).await,
        Some(Command::Call(args)) => run_call(args).await,
        Some(Command::Select(args)) => run_select(args).await,
        Some(Command::Classinfo(args)) => run_classinfo(args).await,
        Some(Command::Enums(args)) => run_enums(args).await,
        Some(Command::Enum(args)) => run_enum(args).await,
        Some(Command::FindAttr(args)) => run_find_attr(args).await,
        None => {
            // Back-compat: bare invocation runs the daemon using top-level flags.
            let project = cli.project.ok_or_else(|| -> Box<dyn std::error::Error> {
                "missing --project (required for daemon mode; use a subcommand for other modes)".into()
            })?;
            run_serve(ServeArgs {
                project,
                port: cli.port,
                game_id: cli.game_id,
                place_id: cli.place_id,
            })
            .await
        }
    }
}

async fn run_serve(args: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.project.exists() {
        std::fs::create_dir_all(&args.project)?;
    }

    if let Err(e) = snapshot::write_ro_sync_md_if_missing(&args.project) {
        eprintln!("rosync: failed to write ro-sync.md: {e}");
    }
    if let Err(e) = snapshot::write_claude_md_if_missing_or_merge(&args.project) {
        eprintln!("rosync: failed to write CLAUDE.md: {e}");
    }

    // Project config: load or create, then apply CLI overrides (persist if anything changed).
    let mut cfg = match project_config::load_or_create(&args.project) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rosync: failed to load ro-sync.json: {e}");
            project_config::ProjectConfig::default_for(&args.project)
        }
    };
    let place_ids_override = if args.place_id.is_empty() { None } else { Some(args.place_id.clone()) };
    let changed = project_config::apply_overrides(&mut cfg, args.game_id.clone(), place_ids_override);
    if changed {
        if let Err(e) = project_config::write(&args.project, &cfg) {
            eprintln!("rosync: failed to write ro-sync.json: {e}");
        }
    }

    let (tx, _rx) = broadcast::channel::<String>(1024);

    let watcher = Watch::new(args.project.clone())?;
    let canonical_project = watcher.root().to_path_buf();
    let watch_tx = watcher.sender();
    let conflict_engine = Arc::new(ConflictEngine::new());
    let push_quiet: Arc<Mutex<HashMap<PathBuf, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let (request_tx, _) = broadcast::channel::<RequestEnvelope>(256);

    let state = AppState {
        project: Arc::new(args.project.clone()),
        canonical_project: Arc::new(canonical_project.clone()),
        events: tx.clone(),
        conflict: conflict_engine.clone(),
        watch_tx: watch_tx.clone(),
        project_name: Arc::new(RwLock::new(cfg.name.clone())),
        game_id: Arc::new(RwLock::new(cfg.game_id.clone())),
        place_ids: Arc::new(RwLock::new(cfg.place_ids.clone())),
        pending_initial: Arc::new(Mutex::new(None)),
        push_quiet: push_quiet.clone(),
        request_tx,
        pending_routes: Arc::new(Mutex::new(HashMap::new())),
    };

    spawn_watch_bridge(watcher, tx.clone(), conflict_engine.clone(), push_quiet.clone());
    spawn_config_hot_reload(state.clone());

    let addr = format!("127.0.0.1:{}", args.port);
    eprintln!("rosync listening on http://{} (project: {})", addr, args.project.display());

    let app = http::router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn run_query(args: QueryArgs) -> Result<(), Box<dyn std::error::Error>> {
    let tree_path = args.project.join(snapshot::TREE_JSON);
    let text = match std::fs::read_to_string(&tree_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "no tree.json at {}. Connect Studio via the Ro Sync plugin to generate it.",
                tree_path.display()
            )
            .into());
        }
        Err(e) => return Err(format!("read {}: {e}", tree_path.display()).into()),
    };
    let tree: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("parse {}: {e}", tree_path.display()))?;

    let matches = query::query(&tree, &args.selector);

    match args.format {
        QueryFormat::Json => {
            let arr: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "path": m.path,
                        "class": m.class,
                        "name": m.name,
                        "childrenCount": m.children_count,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&serde_json::Value::Array(arr))?);
        }
        QueryFormat::Paths => {
            for m in matches {
                println!("{}", m.path.join("/"));
            }
        }
        QueryFormat::Classes => {
            for m in matches {
                println!("{}\t{}", m.class, m.path.join("/"));
            }
        }
    }
    Ok(())
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
    if !args.yes {
        return Err("set: --yes is required for single-op writes (or pass --batch <file.json>)".into());
    }
    let path = args.path.clone().ok_or("set: --path is required")?;
    let prop = args.prop.clone().ok_or("set: --prop is required")?;
    if prop == "Parent" && !args.force_parent {
        eprintln!("========================================================");
        eprintln!("  rosync set: refusing to assign .Parent from the CLI.");
        eprintln!("");
        eprintln!("  Reparenting via raw property writes is the single most");
        eprintln!("  common way to corrupt a DataModel. Use `rosync mv` to");
        eprintln!("  reparent safely, or re-run with `--force-parent` if");
        eprintln!("  you really need the raw write.");
        eprintln!("========================================================");
        return Err(format!(
            "set: refusing to set .Parent on {} without --force-parent (use `rosync mv` instead)",
            path
        ).into());
    }
    let value_raw = args.value.clone().ok_or("set: --value is required (JSON literal)")?;
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
                let err = resp
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>");
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
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text)
        .map_err(|e| format!("parse {}: {e} (expected a JSON array)", batch_path.display()))?;
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
        let value = entry.get("value").cloned().unwrap_or(serde_json::Value::Null);
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
                    let err = resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("<no error>");
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
    print_response(&resp, args.raw, |v| print_ls(v));
    ok_or_err(&resp)
}

async fn run_tree(args: TreeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let req_args = serde_json::json!({ "path": args.path, "depth": args.depth });
    let resp = remote::request(args.port, "tree", req_args).await?;
    print_response(&resp, args.raw, |v| print_tree(v, 0));
    ok_or_err(&resp)
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
    print_response(&resp, args.raw, |v| print_find(v));
    ok_or_err(&resp)
}

async fn run_eval(args: EvalArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.yes {
        return Err("eval: --yes is required (eval is an unrestricted escape hatch)".into());
    }
    let req_args = serde_json::json!({ "source": args.source });
    let resp = remote::request(args.port, "eval", req_args).await?;
    print_response(&resp, args.raw, |v| {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    });
    ok_or_err(&resp)
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
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        eprintln!("error: {err}");
        return Err(err.to_string().into());
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
                    let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("<unknown>");
                    return Err(err.to_string().into());
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
            println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
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

/// Format a Unix timestamp as `HH:MM:SS` in the process's local timezone.
/// Uses libc `localtime_r` to avoid pulling a time crate.
fn format_hms_local(ts: i64) -> String {
    if ts == 0 {
        return "--:--:--".into();
    }
    // SAFETY: `localtime_r` is thread-safe; we pass valid pointers.
    unsafe {
        let mut tm: libc_tm = std::mem::zeroed();
        let t: i64 = ts;
        if localtime_r(&t, &mut tm).is_null() {
            return "--:--:--".into();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

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

extern "C" {
    fn localtime_r(time: *const i64, tm: *mut libc_tm) -> *mut libc_tm;
}

async fn run_save(args: SaveArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.yes {
        return Err("save: --yes is required (writes the place file)".into());
    }
    let resp = remote::request(args.port, "save", serde_json::json!({})).await?;
    print_response(&resp, args.raw, |_v| println!("ok: save started"));
    ok_or_err(&resp)
}

async fn run_undo(args: UndoArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.yes {
        return Err("undo: --yes is required (mutates Studio state)".into());
    }
    let resp = remote::request(args.port, "undo", serde_json::json!({})).await?;
    print_response(&resp, args.raw, |_v| println!("ok: undo"));
    ok_or_err(&resp)
}

async fn run_redo(args: RedoArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.yes {
        return Err("redo: --yes is required (mutates Studio state)".into());
    }
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
    let ok = ping_resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        let err = ping_resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        if args.raw {
            println!("{}", serde_json::to_string_pretty(&ping_resp).unwrap_or_default());
        }
        return Err(err.to_string().into());
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
        println!("{}", serde_json::to_string_pretty(&ping_resp).unwrap_or_default());
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
    // Plugin may be offline — treat failures as "no plugin connected" rather
    // than aborting the subcommand.
    let plugin_info = match remote::request(args.port, "version", serde_json::json!({})).await {
        Ok(v) => v,
        Err(e) => {
            if args.raw {
                println!(
                    "{}",
                    serde_json::json!({ "daemon": daemon, "plugin": null, "error": e }).to_string()
                );
            } else {
                println!("daemon: rosync {daemon}");
                println!("plugin: (not connected — {e})");
            }
            return Ok(());
        }
    };
    let value = plugin_info.get("value").cloned().unwrap_or(serde_json::Value::Null);
    if args.raw {
        println!(
            "{}",
            serde_json::json!({ "daemon": daemon, "plugin": value }).to_string()
        );
        return Ok(());
    }
    println!("daemon: rosync {daemon}");
    let pv = value
        .get("plugin_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let proto = value
        .get("protocol")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let sv = value
        .get("studio_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    println!("plugin: {pv} (protocol {proto}, Studio {sv})");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tier 1 runners. Every mutating op requires `--yes`; `mv` also requires
// `--force` to cross service boundaries (enforced plugin-side).
// ---------------------------------------------------------------------------

async fn run_new(args: NewArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.yes {
        return Err("new: --yes is required".into());
    }
    let mut req = serde_json::Map::new();
    req.insert("parent".into(), serde_json::Value::String(args.path.clone()));
    req.insert("class".into(), serde_json::Value::String(args.class.clone()));
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
        let class = v.get("class").and_then(|v| v.as_str()).unwrap_or(&class_label);
        println!("ok: created {class} at {path}");
    });
    ok_or_err(&resp)
}

async fn run_rm(args: RmArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.yes {
        return Err("rm: --yes is required (calls :Destroy())".into());
    }
    let req = serde_json::json!({ "path": args.path });
    let resp = remote::request(args.port, "rm", req).await?;
    let fallback_path = args.path.clone();
    print_response(&resp, args.raw, |v| {
        let path = v.get("path").and_then(|v| v.as_str()).unwrap_or(&fallback_path);
        println!("ok: destroyed {path}");
    });
    ok_or_err(&resp)
}

async fn run_mv(args: MvArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.yes {
        return Err("mv: --yes is required".into());
    }
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
    if !args.yes {
        return Err("attr set: --yes is required".into());
    }
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
    if !args.yes {
        return Err("attr rm: --yes is required".into());
    }
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
    if !args.yes {
        return Err(format!("tag {op}: --yes is required").into());
    }
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
    if !args.yes {
        return Err("call: --yes is required (invokes arbitrary methods)".into());
    }
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
    if !args.yes {
        return Err("select set: --yes is required".into());
    }
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

fn print_response<F: FnOnce(&serde_json::Value)>(
    resp: &serde_json::Value,
    raw: bool,
    pretty: F,
) {
    if raw {
        println!(
            "{}",
            serde_json::to_string_pretty(resp).unwrap_or_else(|_| resp.to_string())
        );
        return;
    }
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown error>");
        eprintln!("error: {err}");
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
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("request failed")
            .to_string();
        Err(err.into())
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
            println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
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
            let labels: Vec<String> = tags.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
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
                "Vector3" => return format!("Vector3({:.3}, {:.3}, {:.3})", num("x"), num("y"), num("z")),
                "Vector2" => return format!("Vector2({:.3}, {:.3})", num("x"), num("y")),
                "Color3" => return format!("Color3({:.3}, {:.3}, {:.3})", num("r"), num("g"), num("b")),
                "UDim" => return format!("UDim({:.3}, {})", num("scale"), num("offset") as i64),
                "UDim2" => return format!(
                    "UDim2({:.3}, {}, {:.3}, {})",
                    num("xScale"), num("xOffset") as i64, num("yScale"), num("yOffset") as i64
                ),
                "BrickColor" => if let Some(n) = obj.get("name").and_then(|v| v.as_str()) { return format!("BrickColor({n})"); },
                "EnumItem" => {
                    let e = obj.get("enumType").and_then(|v| v.as_str()).unwrap_or("?");
                    let n = obj.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    return format!("Enum.{e}.{n}");
                }
                "Instance" => if let Some(p) = obj.get("path").and_then(|v| v.as_str()) { return format!("→ {p}"); },
                "CFrame" => return format!("CFrame(pos=({:.3}, {:.3}, {:.3}))", num("x"), num("y"), num("z")),
                "NumberRange" => return format!("NumberRange({:.3}..{:.3})", num("min"), num("max")),
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
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
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
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
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

/// Bridges the filesystem-watcher's `broadcast::Sender<Op>` into the shared
/// `broadcast::Sender<String>` that `/events` streams. Each Op is first run
/// through `ConflictEngine::on_fs_change` so that echoes of our own writes
/// (baseline matches) are dropped and conflicts are surfaced as their own
/// event type rather than a propagation op.
fn spawn_watch_bridge(
    watcher: Watch,
    events: broadcast::Sender<String>,
    conflicts: Arc<ConflictEngine>,
    push_quiet: Arc<Mutex<HashMap<PathBuf, Instant>>>,
) {
    let mut rx = watcher.subscribe();
    // Move the Watch into the task so the debouncer stays alive for the lifetime
    // of the daemon.
    tokio::spawn(async move {
        let _watcher = watcher;
        loop {
            match rx.recv().await {
                Ok(op) => {
                    if is_push_quiet(&push_quiet, &op.path) {
                        continue;
                    }
                    // For renames, also suppress if the source side was a recent
                    // /push write — otherwise daemon-initiated renames echo back.
                    if let Some(from) = &op.from {
                        if is_push_quiet(&push_quiet, from) {
                            continue;
                        }
                    }
                    handle_op(op, &events, &conflicts)
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn is_push_quiet(
    push_quiet: &Arc<Mutex<HashMap<PathBuf, Instant>>>,
    path: &std::path::Path,
) -> bool {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let now = Instant::now();
    let mut guard = push_quiet.lock().unwrap();
    // Prune any expired entries as we go — cheap since the map is small in
    // steady state (one entry per in-flight push target).
    guard.retain(|_, deadline| *deadline > now);
    if let Some(deadline) = guard.get(&canon) {
        return *deadline > now;
    }
    // Also check the raw path for robustness against pre-canonicalize inserts.
    if let Some(deadline) = guard.get(path) {
        return *deadline > now;
    }
    false
}

/// Watch `<project>/ro-sync.json` itself. On change, re-parse and if gameId or
/// placeIds differ from AppState's current snapshot, update state and broadcast
/// a `{"type":"config-changed","gameId":...,"placeIds":[...]}` event.
fn spawn_config_hot_reload(state: AppState) {
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

fn reload_config(state: &AppState, _config_path: &std::path::Path) -> Option<()> {
    let cfg = match project_config::read_from_disk(state.canonical_project.as_path()) {
        Ok(Some(c)) => c,
        _ => return None,
    };
    let prev_game_id = state.game_id.read().unwrap().clone();
    let prev_place_ids = state.place_ids.read().unwrap().clone();
    let prev_name = state.project_name.read().unwrap().clone();

    let changed = prev_game_id != cfg.game_id
        || prev_place_ids != cfg.place_ids
        || prev_name != cfg.name;
    if !changed {
        return Some(());
    }

    *state.project_name.write().unwrap() = cfg.name.clone();
    *state.game_id.write().unwrap() = cfg.game_id.clone();
    *state.place_ids.write().unwrap() = cfg.place_ids.clone();

    let evt = serde_json::json!({
        "type": "config-changed",
        "name": cfg.name,
        "gameId": cfg.game_id,
        "placeIds": cfg.place_ids,
    });
    if let Ok(s) = serde_json::to_string(&evt) {
        let _ = state.events.send(s);
    }
    Some(())
}

fn handle_op(op: Op, events: &broadcast::Sender<String>, conflicts: &ConflictEngine) {
    match op.kind {
        OpKind::Add | OpKind::Update => {
            let bytes = match &op.content {
                Some(b) => b.clone(),
                None => {
                    // Directory or unreadable file — forward as-is.
                    emit_op(events, &op);
                    return;
                }
            };
            // Normalize line endings so CRLF-on-disk vs LF-from-Studio don't
            // show up as divergent content.
            let normalized = fs_map::normalize_line_endings(&bytes).into_owned();
            let mtime = fs_mtime(&op.path);
            match conflicts.on_fs_change(&op.path, &normalized, mtime) {
                FsDecision::NoChange => {}
                FsDecision::Propagate => emit_op(events, &op),
                FsDecision::Conflict => emit_conflict(events, &op.path),
            }
        }
        OpKind::Delete | OpKind::Rename => {
            // Deletes and renames bypass the hash-baseline check (no bytes to
            // compare) and are always forwarded. Conflict resolution for
            // concurrent delete-edit or rename-edit is handled at the
            // studio-push boundary.
            emit_op(events, &op);
        }
    }
}

fn emit_op(events: &broadcast::Sender<String>, op: &Op) {
    let payload = serde_json::json!({ "type": "op", "op": op });
    if let Ok(s) = serde_json::to_string(&payload) {
        let _ = events.send(s);
    }
}

fn emit_conflict(events: &broadcast::Sender<String>, path: &std::path::Path) {
    let payload = serde_json::json!({ "type": "conflict", "path": path });
    if let Ok(s) = serde_json::to_string(&payload) {
        let _ = events.send(s);
    }
}

fn fs_mtime(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tier 3 — class introspection, enum listing, attribute-scoped find.
// ---------------------------------------------------------------------------

#[derive(ClapArgs, Debug)]
pub struct ClassInfoArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Class name to introspect, e.g. `BasePart`, `TextLabel`, `Model`.
    #[arg(long = "class")]
    pub class_name: String,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct EnumsArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct EnumArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Enum type name, e.g. `Material`, `Font`, `KeyCode`.
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct FindAttrArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Attribute name to search for.
    #[arg(long)]
    pub name: String,
    /// Restrict traversal to this instance's descendants (omit for whole DataModel).
    #[arg(long)]
    pub under: Option<String>,
    /// Optional JSON literal — only match instances where the attribute equals
    /// this value (decoded the same way `set --value` decodes).
    #[arg(long)]
    pub value: Option<String>,
    #[arg(long)]
    pub raw: bool,
}

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
    print_response(&resp, args.raw, |v| print_find(v));
    ok_or_err(&resp)
}

fn print_classinfo(class_name: &str, value: &serde_json::Value) {
    let Some(obj) = value.as_object() else {
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
        return;
    };
    println!("{class_name}");
    if let Some(props) = obj.get("properties").and_then(|v| v.as_array()) {
        // Group by category. Preserve first-seen order per category so the
        // output is deterministic without requiring stable group ordering.
        let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for p in props {
            let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let cat = p.get("category").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let ty = p.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if name.is_empty() { continue; }
            let cat_label = if cat.is_empty() { "(uncategorized)".to_string() } else { cat };
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
}

fn print_enum_items(enum_name: &str, value: &serde_json::Value) {
    let Some(arr) = value.as_array() else {
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
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
mod tier2_tests {
    use super::*;

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
}
