use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch as tokio_watch};

mod artifact;
mod conflict;
mod diff;
mod fs_map;
mod fs_safety;
mod http;
mod img_upload;
mod initial_sync;
mod lifecycle;
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
mod watch;
mod workflow;
mod ws;

use conflict::{ConflictEngine, FsDecision};
use initial_sync::PendingInitial;
use watch::{Op, OpKind, Watch};
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

#[derive(Parser, Debug)]
#[command(
    name = "rosync",
    version,
    about = "Ro Sync — Roblox Studio sync daemon"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Project directory. Required when no subcommand is given (back-compat
    /// daemon mode); subcommands accept their own `--project`.
    #[arg(long)]
    pub project: Option<PathBuf>,

    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,

    /// Roblox GameId (Int64 — stored as string to avoid JSON precision loss).
    #[arg(long = "game-id")]
    pub game_id: Option<String>,

    /// Roblox GroupId associated with this project.
    #[arg(long = "group-id")]
    pub group_id: Option<String>,

    /// Roblox PlaceId — may be repeated.
    #[arg(long = "place-id")]
    pub place_id: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize a directory as a Ro Sync project without starting a daemon.
    Init(InitArgs),
    /// Install or inspect the bundled Roblox Studio plugin.
    Plugin(PluginArgs),
    /// Manage the CLI's Roblox Open Cloud credential.
    Auth(AuthArgs),
    /// Run the HTTP/WebSocket sync daemon.
    Serve(ServeArgs),
    /// Start, inspect, stop, restart, or read logs from a managed background daemon.
    Daemon(DaemonArgs),
    /// Print machine-readable command docs from the generated command registry.
    Commands(CommandsArgs),
    /// Print an LLM-oriented project context snapshot as JSON.
    Context(ContextArgs),
    /// Execute a validated, reference-aware JSON workflow over one persistent session.
    Run(RunWorkflowArgs),
    /// Report negotiated daemon, plugin, Studio, and runtime capabilities.
    Capabilities(CapabilitiesArgs),
    /// Capture an arbitrary Studio viewport/screen region as a PNG artifact.
    Capture(CaptureArgs),
    /// Start and control Studio playtests and their server/client runtime agents.
    Playtest(PlaytestArgs),
    /// Build a read-only JSON plan for a mutating command.
    Plan(PlanArgs),
    /// Match a selector against the live Studio tree.
    Query(QueryArgs),
    /// Translate between Studio instance paths and syncable filesystem paths.
    Path(PathArgs),
    /// Read an instance (or a single property) from the live Studio session
    /// via the plugin.
    Get(GetArgs),
    /// Set a property on a Studio instance.
    Set(SetArgs),
    /// List the children of a Studio instance.
    Ls(LsArgs),
    /// Print a subtree rooted at a Studio instance.
    Tree(TreeArgs),
    /// Export the live Studio tree and inspectable properties to JSON.
    Snapshot(SnapshotArgs),
    /// Compare local scripts/folders against the live Studio syncable tree.
    Diff(DiffArgs),
    /// Alias for `diff`, with wording aimed at resync reviews.
    Changes(DiffArgs),
    /// Select one or more Studio instances and print the resulting selection count.
    Open(OpenArgs),
    /// Locate matching instances in Studio by name, and optionally translate a path.
    Where(WhereArgs),
    /// Print properties for one live Studio instance.
    Props(PropsArgs),
    /// Print script source from Studio or disk.
    Source(SourceArgs),
    /// Show sync metadata for a Studio or filesystem path.
    Meta(MetaArgs),
    /// List synced services and whether they exist locally / in Studio.
    Services(ServicesArgs),
    /// List parked two-way source conflicts.
    Conflicts(ConflictsArgs),
    /// Resolve a parked conflict with either the disk or Studio version.
    Resolve(ResolveArgs),
    /// Inspect or answer the pending initial sync decision.
    #[command(alias = "decide")]
    Decision(DecisionArgs),
    /// Alias for `logs --tail`.
    Tail(TailArgs),
    /// Stream raw daemon WebSocket frames.
    Watch(WatchArgs),
    /// Rebuild generated sync metadata.
    Repair(RepairArgs),
    /// Upload assets through Roblox Open Cloud Assets.
    Upload(UploadArgs),
    /// Create, edit, list, and upload images for Roblox game passes / developer products.
    Monetization(MonetizationArgs),
    /// Upload an image through Roblox Open Cloud Assets.
    #[command(hide = true)]
    Img(ImgArgs),
    /// Bulk upload image files through Roblox Open Cloud Assets.
    #[command(hide = true)]
    Imgs(ImgsArgs),
    /// Find instances matching a class and/or name.
    Find(FindArgs),
    /// Execute Luau source inside Studio. Escape hatch for anything the
    /// structured ops don't cover.
    Eval(EvalArgs),
    /// Render/read EditableImages from Studio and write them as local PNG files.
    Transmit(TransmitArgs),
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
    /// Summarize daemon, plugin, project, tree, sourcemap, and write-log status.
    Status(StatusArgs),
    /// Check local Ro-Sync health: project files, daemon, plugin, linter, and sourcemap.
    Doctor(DoctorArgs),
    /// Refresh generated Ro Sync agent docs without starting the daemon.
    Refresh(RefreshArgs),
    /// Construct a new instance under a parent path.
    New(NewArgs),
    /// Destroy an instance.
    Rm(RmArgs),
    /// Reparent an instance. Cross-service moves require `--force`.
    Mv(MvArgs),
    /// Attribute ops: `attr set|rm|ls`.
    Attr(AttrArgs),
    /// CollectionService tag ops: `tag add|rm`.
    Tag(TagArgs),
    /// Invoke a method on an instance (`inst:Method(args...)`).
    Call(CallArgs),
    /// Selection service: `select get|set`.
    Select(SelectArgs),
    /// Copy arbitrary Studio instances into Ro Sync's cross-project clipboard.
    Copy(studio_clipboard::CopyArgs),
    /// Paste Ro Sync's cross-project clipboard into the connected Studio.
    Paste(studio_clipboard::PasteArgs),
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
    /// Run luau-lsp's standalone analyzer against the project or a file path.
    Lint(LintArgs),
}

#[derive(ClapArgs, Debug)]
pub struct CapabilitiesArgs {
    /// Project directory. Used for daemon port discovery.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Print the complete JSON capability document.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct CaptureArgs {
    #[command(subcommand)]
    pub command: CaptureCommand,
}

#[derive(Subcommand, Debug)]
pub enum CaptureCommand {
    /// Check screenshot API availability and current permission without prompting.
    Status(CaptureStatusArgs),
    /// Ask Studio for screenshot permission. This may show a user prompt.
    Authorize(CaptureAuthorizeArgs),
    /// Capture the requested screen rectangle and write a PNG file.
    Screen(CaptureScreenArgs),
    /// Capture the Studio viewport through Ro Sync's locally packaged Photo engine.
    Photo(CapturePhotoArgs),
    /// Frame a Studio instance in the 3D camera and capture the viewport.
    Scene(CaptureSceneArgs),
}

#[derive(ClapArgs, Debug)]
pub struct CaptureStatusArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct CaptureAuthorizeArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureUiMode {
    All,
    None,
}

impl CaptureUiMode {
    fn as_plugin_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::None => "none",
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureResampleMode {
    Default,
    Pixelated,
}

impl CaptureResampleMode {
    fn as_plugin_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Pixelated => "pixelated",
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct CaptureScreenArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Capture rectangle as x,y,width,height. Omit to capture the full Studio surface.
    #[arg(long)]
    pub region: Option<String>,
    /// Output dimensions as WIDTHxHEIGHT. Omit to preserve the capture size.
    #[arg(long = "output-size")]
    pub output_size: Option<String>,
    /// Include all Studio UI or capture only the 3D viewport.
    #[arg(long, value_enum, default_value_t = CaptureUiMode::All)]
    pub ui: CaptureUiMode,
    /// Scaling algorithm used when --output-size differs from the capture size.
    #[arg(long, value_enum, default_value_t = CaptureResampleMode::Default)]
    pub resample: CaptureResampleMode,
    /// Destination PNG. Parent directories are created automatically.
    #[arg(long, default_value = "rosync-capture.png")]
    pub output: PathBuf,
    /// Overall Studio capture timeout in seconds.
    #[arg(long, default_value_t = 30.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
    #[arg(long, hide = true)]
    pub focus: Option<String>,
    #[arg(long, hide = true)]
    pub view: Option<CaptureView>,
    #[arg(long, hide = true)]
    pub padding: Option<f64>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureView {
    Isometric,
    Front,
    Back,
    Left,
    Right,
    Top,
    Bottom,
}

impl CaptureView {
    fn as_plugin_str(self) -> &'static str {
        match self {
            Self::Isometric => "isometric",
            Self::Front => "front",
            Self::Back => "back",
            Self::Left => "left",
            Self::Right => "right",
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePhotoBackground {
    /// Reconstruct transparency by capturing against black and white backgrounds.
    Transparent,
    /// Preserve the viewport, sky, and world background exactly as rendered.
    Scene,
}

impl CapturePhotoBackground {
    fn as_wire_str(self) -> &'static str {
        match self {
            Self::Transparent => "transparent",
            Self::Scene => "scene",
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePhotoUiMode {
    /// Exclude in-game ScreenGui layers from the capture.
    None,
    /// Preserve in-game ScreenGui layers over the rendered 3D scene.
    Overlay,
    /// Capture only in-game ScreenGui layers as a transparent RGBA image.
    Only,
}

impl CapturePhotoUiMode {
    fn as_wire_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Overlay => "overlay",
            Self::Only => "only",
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct CapturePhotoArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Optional Studio instance to clone, isolate, and frame before capture.
    #[arg(long)]
    pub focus: Option<String>,
    /// Native viewport-image rectangle as x,y,width,height. Coordinates start at the viewport's top-left; combine with --size to resize the crop.
    #[arg(long)]
    pub region: Option<String>,
    /// Exact output dimensions as WIDTHxHEIGHT. Full UI-only captures preserve the native aspect ratio with transparent padding; explicit regions fill the canvas. With --focus, defaults to 1024x1024.
    #[arg(long)]
    pub size: Option<String>,
    /// Preset camera direction used with --focus.
    #[arg(long, value_enum, default_value_t = CaptureView::Isometric)]
    pub view: CaptureView,
    /// Arbitrary camera direction x,y,z; overrides --view.
    #[arg(long, allow_hyphen_values = true)]
    pub direction: Option<String>,
    /// Exact camera transform as the 12 values returned by CFrame:GetComponents(). Requires --focus and replaces automatic view/direction/padding framing.
    #[arg(
        long,
        allow_hyphen_values = true,
        requires = "focus",
        conflicts_with_all = ["view", "direction", "padding"]
    )]
    pub camera_cframe: Option<String>,
    /// Extra camera framing multiplier (1.0-4.0).
    #[arg(long, default_value_t = 1.25)]
    pub padding: f64,
    /// Camera vertical field of view used while framing --focus (1-120 degrees).
    #[arg(long, default_value_t = 32.0)]
    pub fov: f64,
    /// Preserve the rendered scene or reconstruct a transparent background.
    #[arg(long, value_enum, default_value_t = CapturePhotoBackground::Transparent)]
    pub background: CapturePhotoBackground,
    /// Keep RGB color in transparent edge pixels for cleaner texture filtering.
    #[arg(long)]
    pub alpha_bleed: bool,
    /// Frame the original target in place instead of a script-free isolated clone.
    #[arg(long)]
    pub include_world: bool,
    /// Preserve the camera-framed canvas instead of tightly cropping an isolated transparent --focus capture.
    #[arg(long, requires = "focus")]
    pub no_tight_crop: bool,
    /// Choose whether in-game UI is excluded, overlaid on the scene, or captured alone with transparency.
    #[arg(long, value_enum)]
    pub ui: Option<CapturePhotoUiMode>,
    /// Capture one GuiObject and its descendants with transparency, hiding every unrelated UI element. Implies --ui only; --region may override its automatic bounds.
    #[arg(long)]
    pub ui_target: Option<String>,
    /// Legacy alias for --ui overlay.
    #[arg(long, conflicts_with = "ui")]
    pub include_ui: bool,
    /// Delay after camera/scene setup so streaming and rendering can settle.
    #[arg(long, default_value_t = 0.05)]
    pub delay: f64,
    /// Destination PNG. Parent directories are created automatically.
    #[arg(long, default_value = "rosync-photo.png")]
    pub output: PathBuf,
    /// Overall Photo capture timeout in seconds.
    #[arg(long, default_value_t = 120.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct CaptureSceneArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Studio instance to frame, such as Workspace/Map/Boss.
    #[arg(long)]
    pub focus: String,
    #[arg(long, value_enum, default_value_t = CaptureView::Isometric)]
    pub view: CaptureView,
    /// Extra camera framing multiplier (1.0-4.0).
    #[arg(long, default_value_t = 1.25)]
    pub padding: f64,
    #[arg(long = "size", default_value = "1024x1024")]
    pub size: String,
    #[arg(long, value_enum, default_value_t = CaptureResampleMode::Default)]
    pub resample: CaptureResampleMode,
    /// Preserve the camera-framed canvas instead of tightly cropping the isolated subject.
    #[arg(long)]
    pub no_tight_crop: bool,
    #[arg(long, default_value = "rosync-scene.png")]
    pub output: PathBuf,
    #[arg(long, default_value_t = 120.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestArgs {
    #[command(subcommand)]
    pub command: PlaytestCommand,
}

#[derive(Subcommand, Debug)]
pub enum PlaytestCommand {
    /// Run one playscript-owned playtest session to completion.
    Run(playtest_run::PlaytestRunArgs),
    /// Start Play, Run, or a local multiplayer test as an asynchronous job.
    Start(PlaytestStartArgs),
    /// Print a playtest job and its currently connected contexts.
    Status(PlaytestStatusArgs),
    /// List PlayServer and PlayClient runtime contexts.
    Contexts(PlaytestContextsArgs),
    /// Wait until the requested number of runtime contexts are connected.
    Wait(PlaytestWaitArgs),
    /// Stop the active playtest.
    Stop(PlaytestStopArgs),
    /// Execute Luau in a PlayServer or PlayClient context.
    Exec(PlaytestExecArgs),
    /// Read output from a PlayServer or PlayClient context.
    Logs(PlaytestLogsArgs),
    /// Inspect resolved GUI geometry/text in a PlayClient context.
    Ui(PlaytestUiArgs),
    /// Send a JSON action sequence through PlayClient VirtualInput.
    Input(PlaytestInputArgs),
    /// Capture a PlayServer/PlayClient screen through its runtime agent.
    Capture(PlaytestCaptureArgs),
    /// Send an advanced runtime operation directly.
    Request(PlaytestRequestArgs),
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum PlaytestMode {
    Play,
    Run,
    Multiplayer,
}

impl PlaytestMode {
    fn as_plugin_str(self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Run => "run",
            Self::Multiplayer => "multiplayer",
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestStartArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long, value_enum, default_value_t = PlaytestMode::Play)]
    pub mode: PlaytestMode,
    /// Number of clients for multiplayer mode (1-8).
    #[arg(long, default_value_t = 1)]
    pub players: u8,
    /// Optional StudioTestService arguments as a JSON object.
    #[arg(long = "test-args")]
    pub test_args: Option<String>,
    /// Wait for runtime contexts after starting (server + clients for multiplayer).
    #[arg(long)]
    pub wait: bool,
    #[arg(long, default_value_t = 45.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestStatusArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long = "job-id")]
    pub job_id: Option<String>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestContextsArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestWaitArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long, default_value_t = 1)]
    pub minimum: u8,
    #[arg(long, default_value_t = 45.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestStopArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long = "job-id")]
    pub job_id: Option<String>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum RuntimeIdentity {
    Game,
    Plugin,
}

impl RuntimeIdentity {
    fn as_plugin_str(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Plugin => "plugin",
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestExecArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Runtime context from `playtest contexts`, e.g. server or client:1.
    #[arg(long)]
    pub context: String,
    #[arg(long, conflicts_with = "source_file")]
    pub source: Option<String>,
    #[arg(long = "source-file", conflicts_with = "source")]
    pub source_file: Option<PathBuf>,
    /// Game runs through a temporary Script/LocalScript; plugin runs in plugin identity.
    #[arg(long, value_enum, default_value_t = RuntimeIdentity::Game)]
    pub identity: RuntimeIdentity,
    #[arg(long, default_value_t = 15.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestLogsArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub context: String,
    #[arg(long = "since-seq", default_value_t = 0)]
    pub since_seq: u64,
    #[arg(long, default_value_t = 200)]
    pub limit: usize,
    #[arg(long, default_value_t = 15.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestUiArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub context: String,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long = "class")]
    pub class_name: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, default_value_t = 1000)]
    pub limit: usize,
    #[arg(long, default_value_t = 15.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestInputArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub context: String,
    /// JSON object or array of input actions.
    #[arg(long, conflicts_with = "file")]
    pub actions: Option<String>,
    /// Read the action object/array from a JSON file.
    #[arg(long, conflicts_with = "actions")]
    pub file: Option<PathBuf>,
    #[arg(long, default_value_t = 30.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestCaptureArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub context: String,
    #[arg(long)]
    pub region: Option<String>,
    #[arg(long = "output-size")]
    pub output_size: Option<String>,
    #[arg(long, value_enum, default_value_t = CaptureUiMode::All)]
    pub ui: CaptureUiMode,
    #[arg(long, value_enum, default_value_t = CaptureResampleMode::Default)]
    pub resample: CaptureResampleMode,
    #[arg(long, default_value = "rosync-playtest-capture.png")]
    pub output: PathBuf,
    #[arg(long, default_value_t = 45.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlaytestRequestArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub context: String,
    #[arg(long)]
    pub op: String,
    /// Runtime operation arguments as a JSON object.
    #[arg(long, default_value = "{}")]
    pub args: String,
    #[arg(long, default_value_t = 30.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct CommandsArgs {
    /// Optional command name. If omitted, prints the full command registry.
    pub name: Option<String>,
    /// Print a compact LLM-oriented command index instead of full command docs.
    #[arg(long)]
    pub compact: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ContextArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Include the full command registry. Omitted by default to keep context compact.
    #[arg(long = "full-commands")]
    pub full_commands: bool,
}

#[derive(ClapArgs, Debug)]
pub struct RunWorkflowArgs {
    /// Workflow JSON file using schema version 1.
    #[arg(long)]
    pub file: PathBuf,
    /// Project directory. Used for daemon port discovery and upload steps.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Validate, resolve static structure, and print the normalized workflow without executing.
    #[arg(long)]
    pub dry_run: bool,
    /// Continue after a non-transactional failed step. References to failed steps remain available.
    #[arg(long)]
    pub keep_going: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlanArgs {
    #[command(subcommand)]
    pub command: PlanCommand,
}

#[derive(Subcommand, Debug)]
pub enum PlanCommand {
    /// Plan a Studio property write.
    Set(PlanSetArgs),
    /// Plan creating a new instance.
    New(PlanNewArgs),
    /// Plan destroying an instance.
    Rm(PlanRmArgs),
    /// Plan reparenting an instance.
    Mv(PlanMvArgs),
    /// Plan resolving a parked source conflict.
    Resolve(PlanResolveArgs),
}

#[derive(ClapArgs, Debug)]
pub struct PlanSetArgs {
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub prop: String,
    /// Value as a JSON literal.
    #[arg(long)]
    pub value: String,
}

#[derive(ClapArgs, Debug)]
pub struct PlanNewArgs {
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub class: String,
    #[arg(long)]
    pub name: Option<String>,
    /// JSON object of initial properties.
    #[arg(long)]
    pub props: Option<String>,
}

#[derive(ClapArgs, Debug)]
pub struct PlanRmArgs {
    #[arg(long)]
    pub path: String,
}

#[derive(ClapArgs, Debug)]
pub struct PlanMvArgs {
    #[arg(long)]
    pub from: String,
    #[arg(long)]
    pub to: String,
    #[arg(long)]
    pub force: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PlanResolveArgs {
    #[arg(long)]
    pub path: String,
    #[arg(long, conflicts_with = "studio")]
    pub disk: bool,
    #[arg(long, conflicts_with = "disk")]
    pub studio: bool,
}

#[derive(ClapArgs, Debug)]
pub struct OpenArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Studio path(s) to select.
    #[arg(required = true)]
    pub paths: Vec<String>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct WhereArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Name substring or path to inspect.
    pub target: String,
    /// Restrict live search to this subtree.
    #[arg(long)]
    pub under: Option<String>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PropsArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct SourceArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Studio path or filesystem path.
    #[arg(long)]
    pub path: String,
    /// Read from disk instead of live Studio.
    #[arg(long)]
    pub disk: bool,
    /// Print JSON instead of the source text.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct MetaArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Studio path or filesystem path.
    pub target: String,
    #[arg(long, value_enum, default_value_t = path_resolver::PathInputKind::Auto)]
    pub from: path_resolver::PathInputKind,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ServicesArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ConflictsArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ResolveArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    /// Keep disk/local bytes and push them to Studio.
    #[arg(long, conflicts_with = "studio")]
    pub disk: bool,
    /// Keep Studio bytes and write them to disk.
    #[arg(long, conflicts_with = "disk")]
    pub studio: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct TailArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    pub level: LogLevel,
    #[arg(long, default_value_t = 200)]
    pub limit: u32,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct WatchArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Print compact one-line summaries instead of JSON frames.
    #[arg(long)]
    pub compact: bool,
}

#[derive(ClapArgs, Debug)]
pub struct RepairArgs {
    #[command(subcommand)]
    pub command: RepairCommand,
}

#[derive(Subcommand, Debug)]
pub enum RepairCommand {
    /// Validate that the live Studio tree can be read.
    Tree(RepairTreeArgs),
    /// Regenerate luau-lsp sourcemap JSON.
    Sourcemap(RepairSourcemapArgs),
}

#[derive(ClapArgs, Debug)]
pub struct RepairTreeArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Max recursion depth for the live Studio tree request.
    #[arg(long, default_value_t = 128)]
    pub depth: u32,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct RepairSourcemapArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Output path. Defaults to `<project>/sourcemap.json`.
    #[arg(long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct GetArgs {
    /// Project directory (informational; daemon connection uses `--port`).
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Daemon port.
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    /// Deprecated no-op kept for old scripts.
    #[arg(long, hide = true)]
    pub yes: bool,
    /// Read a JSON array of `{path,prop,value}` from this file and execute
    /// each entry sequentially.
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
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
pub struct SnapshotArgs {
    /// Project directory used for the default output location.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Output file or existing directory. Defaults to
    /// `<project-or-cwd>/rosync-snapshot-<unix-seconds>.json`.
    #[arg(long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct DiffArgs {
    /// Project directory. Defaults to the current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Max recursion depth for the live Studio tree request.
    #[arg(long, default_value_t = 128)]
    pub depth: u32,
    /// Print JSON instead of human-readable output.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct UploadArgs {
    /// Asset files or directories to upload.
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,
    /// Project directory to read `groupId` from when `--creator` is omitted.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Creator target as `user:<id>` or `group:<id>`. Can also be provided by
    /// ROBLOX_CREATOR, project `groupId`, or the active widget project's Group ID.
    #[arg(long)]
    pub creator: Option<String>,
    /// Asset display name. Only valid when exactly one file is uploaded.
    #[arg(long)]
    pub name: Option<String>,
    /// Asset description.
    #[arg(long, default_value_t = String::new())]
    pub description: String,
    /// Roblox asset type to create. When omitted, Ro Sync infers it from the file extension.
    #[arg(long = "asset-type", value_enum)]
    pub asset_type: Option<UploadAssetType>,
    /// Override the multipart file content type.
    #[arg(long = "content-type")]
    pub content_type: Option<String>,
    /// Credential type: API key uses `x-api-key`; bearer uses OAuth access tokens.
    #[arg(long, value_enum, default_value_t = ImgAuth::ApiKey)]
    pub auth: ImgAuth,
    /// Optional env var override for the Roblox Open Cloud API key or OAuth token.
    /// When omitted, Ro Sync uses the saved Settings key first.
    #[arg(long = "api-key-env")]
    pub api_key_env: Option<String>,
    /// Return after Roblox accepts the operation instead of polling for the asset id.
    #[arg(long = "no-wait")]
    pub no_wait: bool,
    /// Maximum time to wait for the Roblox operation.
    #[arg(long = "timeout-seconds", default_value_t = 120)]
    pub timeout_seconds: u64,
    /// Poll interval while waiting for the Roblox operation.
    #[arg(long = "poll-seconds", default_value_t = 2)]
    pub poll_seconds: u64,
    /// Maximum number of simultaneous uploads.
    #[arg(long, default_value_t = 2)]
    pub concurrency: usize,
    /// Do not recurse into directories.
    #[arg(long = "no-recursive")]
    pub no_recursive: bool,
    /// Write a JSON manifest containing every per-file result.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Print JSON instead of human-readable output.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ImgArgs {
    /// Local image file to upload.
    pub path: PathBuf,
    /// Project directory to read `groupId` from when `--creator` is omitted.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Creator target as `user:<id>` or `group:<id>`. Can also be provided by
    /// ROBLOX_CREATOR, project `groupId`, or the active widget project's Group ID.
    #[arg(long)]
    pub creator: Option<String>,
    /// Asset display name. Defaults to the image file stem.
    #[arg(long)]
    pub name: Option<String>,
    /// Asset description.
    #[arg(long, default_value_t = String::new())]
    pub description: String,
    /// Roblox asset type to create.
    #[arg(long = "asset-type", value_enum, default_value_t = UploadAssetType::Image)]
    pub asset_type: UploadAssetType,
    /// Credential type: API key uses `x-api-key`; bearer uses OAuth access tokens.
    #[arg(long, value_enum, default_value_t = ImgAuth::ApiKey)]
    pub auth: ImgAuth,
    /// Optional env var override for the Roblox Open Cloud API key or OAuth token.
    /// When omitted, Ro Sync uses the saved Settings key first.
    #[arg(long = "api-key-env")]
    pub api_key_env: Option<String>,
    /// Return after Roblox accepts each operation instead of polling for asset ids.
    #[arg(long = "no-wait")]
    pub no_wait: bool,
    /// Maximum time to wait for each Roblox operation.
    #[arg(long = "timeout-seconds", default_value_t = 120)]
    pub timeout_seconds: u64,
    /// Poll interval while waiting for Roblox operations.
    #[arg(long = "poll-seconds", default_value_t = 2)]
    pub poll_seconds: u64,
    /// Print JSON instead of human-readable output.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ImgsArgs {
    /// Image files or directories to upload.
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,
    /// Project directory to read `groupId` from when `--creator` is omitted.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Creator target as `user:<id>` or `group:<id>`. Can also be provided by
    /// ROBLOX_CREATOR, project `groupId`, or the active widget project's Group ID.
    #[arg(long)]
    pub creator: Option<String>,
    /// Asset description applied to every upload.
    #[arg(long, default_value_t = String::new())]
    pub description: String,
    /// Roblox asset type to create.
    #[arg(long = "asset-type", value_enum, default_value_t = UploadAssetType::Image)]
    pub asset_type: UploadAssetType,
    /// Credential type: API key uses `x-api-key`; bearer uses OAuth access tokens.
    #[arg(long, value_enum, default_value_t = ImgAuth::ApiKey)]
    pub auth: ImgAuth,
    /// Optional env var override for the Roblox Open Cloud API key or OAuth token.
    /// When omitted, Ro Sync uses the saved Settings key first.
    #[arg(long = "api-key-env")]
    pub api_key_env: Option<String>,
    /// Return after Roblox accepts each operation instead of polling for asset ids.
    #[arg(long = "no-wait")]
    pub no_wait: bool,
    /// Maximum time to wait for each Roblox operation.
    #[arg(long = "timeout-seconds", default_value_t = 120)]
    pub timeout_seconds: u64,
    /// Poll interval while waiting for Roblox operations.
    #[arg(long = "poll-seconds", default_value_t = 2)]
    pub poll_seconds: u64,
    /// Maximum number of simultaneous uploads.
    #[arg(long, default_value_t = 2)]
    pub concurrency: usize,
    /// Do not recurse into directories.
    #[arg(long = "no-recursive")]
    pub no_recursive: bool,
    /// Write a JSON manifest containing every per-file result.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Print JSON instead of human-readable output.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImgAuth {
    ApiKey,
    Bearer,
}

impl ImgAuth {
    fn as_upload_mode(self) -> img_upload::AuthMode {
        match self {
            ImgAuth::ApiKey => img_upload::AuthMode::ApiKey,
            ImgAuth::Bearer => img_upload::AuthMode::Bearer,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadAssetType {
    Animation,
    Audio,
    Image,
    Decal,
    Mesh,
    Model,
    Video,
}

impl UploadAssetType {
    fn as_cloud_str(self) -> &'static str {
        match self {
            UploadAssetType::Animation => "Animation",
            UploadAssetType::Audio => "Audio",
            UploadAssetType::Image => "Image",
            UploadAssetType::Decal => "Decal",
            UploadAssetType::Mesh => "Mesh",
            UploadAssetType::Model => "Model",
            UploadAssetType::Video => "Video",
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct MonetizationArgs {
    #[command(subcommand)]
    pub command: MonetizationCommand,
}

#[derive(Subcommand, Debug)]
pub enum MonetizationCommand {
    /// Game pass operations.
    #[command(alias = "gamepasses", alias = "gp", alias = "pass")]
    Gamepass(MonetizationAssetArgs),
    /// Developer product operations.
    #[command(alias = "products", alias = "dp", alias = "devproduct")]
    Product(MonetizationAssetArgs),
}

#[derive(ClapArgs, Debug)]
pub struct MonetizationAssetArgs {
    #[command(subcommand)]
    pub command: MonetizationAction,
}

#[derive(Subcommand, Debug)]
pub enum MonetizationAction {
    /// Discover project monetization config references.
    Discover(MonetizationDiscoverArgs),
    /// List assets from Roblox Open Cloud.
    List(MonetizationCommonArgs),
    /// Create one or more assets.
    Create(MonetizationCreateArgs),
    /// Edit an asset by id or resolved name.
    Edit(MonetizationEditArgs),
    /// Upload one explicit image to one asset.
    Image(MonetizationImageArgs),
    /// Upload every supported image in a directory, matched by normalized filename.
    Images(MonetizationImagesArgs),
}

#[derive(ClapArgs, Debug, Clone)]
pub struct MonetizationCommonArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long = "universe-id")]
    pub universe_id: Option<String>,
    /// Optional env var override for the Roblox Open Cloud API key.
    /// When omitted, Ro Sync uses the saved Settings key first.
    #[arg(long = "api-key-env")]
    pub api_key_env: Option<String>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct MonetizationDiscoverArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct MonetizationCreateArgs {
    #[command(flatten)]
    pub common: MonetizationCommonArgs,
    /// Entry like `VIP 499 robux`; can contain comma-separated entries.
    pub entries: Vec<String>,
    /// Explicit single-asset name.
    #[arg(long)]
    pub name: Option<String>,
    /// Explicit single-asset price in Robux.
    #[arg(long)]
    pub price: Option<u64>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub image: Option<PathBuf>,
    #[arg(long = "not-for-sale")]
    pub not_for_sale: bool,
}

#[derive(ClapArgs, Debug)]
pub struct MonetizationEditArgs {
    #[command(flatten)]
    pub common: MonetizationCommonArgs,
    #[arg(long)]
    pub id: Option<String>,
    /// Existing asset name to resolve through the list API when --id is omitted.
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long = "new-name")]
    pub new_name: Option<String>,
    #[arg(long)]
    pub price: Option<u64>,
    #[arg(long)]
    pub description: Option<String>,
    /// Set sale state. Example: `--for-sale true`.
    #[arg(long = "for-sale")]
    pub for_sale: Option<bool>,
}

#[derive(ClapArgs, Debug)]
pub struct MonetizationImageArgs {
    #[command(flatten)]
    pub common: MonetizationCommonArgs,
    #[arg(long)]
    pub id: Option<String>,
    /// Existing asset name to resolve through the list API when --id is omitted.
    #[arg(long)]
    pub name: Option<String>,
    pub file: PathBuf,
}

#[derive(ClapArgs, Debug)]
pub struct MonetizationImagesArgs {
    #[command(flatten)]
    pub common: MonetizationCommonArgs,
    pub dir: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonetizationKind {
    Gamepass,
    Product,
}

impl MonetizationKind {
    fn label(self) -> &'static str {
        match self {
            Self::Gamepass => "gamepass",
            Self::Product => "product",
        }
    }

    fn id_field(self) -> &'static str {
        match self {
            Self::Gamepass => "gamePassId",
            Self::Product => "productId",
        }
    }

    fn create_image_field(self) -> &'static str {
        "imageFile"
    }

    fn update_image_field(self) -> &'static str {
        match self {
            Self::Gamepass => "file",
            Self::Product => "imageFile",
        }
    }

    fn base_url(self, universe_id: &str) -> String {
        match self {
            Self::Gamepass => format!(
                "https://apis.roblox.com/game-passes/v1/universes/{universe_id}/game-passes"
            ),
            Self::Product => format!(
                "https://apis.roblox.com/developer-products/v2/universes/{universe_id}/developer-products"
            ),
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct FindArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Deprecated no-op kept for old scripts.
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct UndoArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct RedoArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct WaypointArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct VersionArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct StatusArgs {
    /// Project directory. Defaults to the current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Print JSON instead of human-readable checks.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct DoctorArgs {
    /// Project directory. Defaults to the current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Print JSON instead of human-readable checks.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct RefreshArgs {
    /// Project directory. Defaults to the current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Print JSON instead of human-readable output.
    #[arg(long)]
    pub raw: bool,
}

// ---------------------------------------------------------------------------
// Tier 1 args — construction / destruction / reparent / attrs / tags / call /
// selection.
// ---------------------------------------------------------------------------

#[derive(ClapArgs, Debug)]
pub struct NewArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    /// Deprecated no-op kept for old scripts.
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct RmArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Instance path to destroy.
    #[arg(long)]
    pub path: String,
    /// Deprecated no-op kept for old scripts.
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct MvArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    /// Deprecated no-op kept for old scripts.
    #[arg(long, hide = true)]
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
    /// Set an attribute.
    Set(AttrSetArgs),
    /// Clear an attribute.
    Rm(AttrRmArgs),
    /// List attributes on an instance.
    Ls(AttrLsArgs),
}

#[derive(ClapArgs, Debug)]
pub struct AttrSetArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub name: String,
    /// Value as a JSON literal. Same codec as `rosync set --value`.
    #[arg(long)]
    pub value: String,
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct AttrRmArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub name: String,
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct AttrLsArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    /// Add a CollectionService tag.
    Add(TagMutArgs),
    /// Remove a CollectionService tag.
    Rm(TagMutArgs),
}

#[derive(ClapArgs, Debug)]
pub struct TagMutArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub tag: String,
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct CallArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    /// Deprecated no-op kept for old scripts.
    #[arg(long, hide = true)]
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
    /// Replace the Studio Selection with the given paths.
    Set(SelectSetArgs),
}

#[derive(ClapArgs, Debug)]
pub struct SelectGetArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct SelectSetArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// JSON array of instance paths.
    #[arg(long)]
    pub paths: String,
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct EvalArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Luau source to execute. Wrap in `return ...` to get a return value.
    #[arg(long)]
    pub source: String,
    /// Deprecated no-op kept for old scripts.
    #[arg(long, hide = true)]
    pub yes: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct TransmitArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Luau source to execute before collecting images.
    #[arg(long, conflicts_with = "source_file")]
    pub source: Option<String>,
    /// Luau source file to execute before collecting images.
    #[arg(long = "source-file", conflicts_with = "source")]
    pub source_file: Option<PathBuf>,
    /// Collect EditableImages under this Studio path after source runs.
    #[arg(long = "from")]
    pub from_path: Option<String>,
    /// Collect one existing EditableImage path. May be repeated.
    #[arg(long = "path")]
    pub paths: Vec<String>,
    /// Output PNG file or directory. Direct file output is only valid for one image.
    #[arg(long, default_value = "rosync-transmit")]
    pub output: PathBuf,
    /// Request timeout in seconds.
    #[arg(long, default_value_t = 60.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct ServeArgs {
    #[arg(long)]
    pub project: PathBuf,

    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,

    #[arg(long = "game-id")]
    pub game_id: Option<String>,

    #[arg(long = "group-id")]
    pub group_id: Option<String>,

    #[arg(long = "place-id")]
    pub place_id: Vec<String>,

    /// Desktop-authorized parent directory for plugin-created projects.
    #[arg(long = "projects-root")]
    pub projects_root: Option<PathBuf>,

    /// Mark this daemon as owned by the Terminal 64 widget lifecycle.
    #[arg(long = "widget-owned", hide = true)]
    pub widget_owned: bool,

    /// Token required for widget lifecycle heartbeat and close requests.
    #[arg(
        long = "owner-token",
        hide = true,
        conflicts_with = "owner_token_state_file"
    )]
    pub owner_token: Option<String>,

    /// Read the widget owner token from state.daemonOwnerToken in a Terminal 64
    /// widget state file. This keeps the capability out of both shell command
    /// strings and child-process argv.
    #[arg(long = "owner-token-state-file", hide = true)]
    pub owner_token_state_file: Option<PathBuf>,

    /// Mark this daemon as lifecycle-managed without tying it to the legacy widget heartbeat.
    #[arg(long, hide = true)]
    pub managed: bool,

    /// Name of the lifecycle manager that launched this process.
    #[arg(long = "managed-by", hide = true)]
    pub managed_by: Option<String>,

    /// Token required for generic lifecycle heartbeat and stop requests.
    #[arg(
        long = "control-token",
        hide = true,
        conflicts_with = "control_token_env"
    )]
    pub control_token: Option<String>,

    /// Environment variable containing the generic lifecycle control token.
    #[arg(long = "control-token-env", hide = true)]
    pub control_token_env: Option<String>,

    /// Per-process identity returned by /hello and recorded by the lifecycle manager.
    #[arg(long = "boot-id", hide = true)]
    pub boot_id: Option<String>,

    /// Runtime record written atomically after the listener binds.
    #[arg(long = "runtime-record", hide = true)]
    pub runtime_record: Option<PathBuf>,

    /// Log file associated with the runtime record.
    #[arg(long = "log-path", hide = true)]
    pub log_path: Option<PathBuf>,

    /// Unix timestamp supplied by the lifecycle manager.
    #[arg(long = "started-at", hide = true)]
    pub started_at: Option<u64>,
}

#[derive(ClapArgs, Debug)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start a detached daemon for one project, or return the matching running daemon.
    Start(DaemonStartArgs),
    /// Report the managed daemon recorded for one project.
    Status(DaemonStatusArgs),
    /// Gracefully stop the exact managed daemon recorded for one project.
    Stop(DaemonStopArgs),
    /// Gracefully stop and then start the managed daemon for one project.
    Restart(DaemonRestartArgs),
    /// Print or follow the managed daemon's log file.
    Logs(DaemonLogsArgs),
}

#[derive(ClapArgs, Debug, Clone)]
pub struct DaemonStartArgs {
    #[arg(long)]
    pub project: PathBuf,
    /// Exact port to use. Without this flag, ports 7878-7890 are tried in order.
    #[arg(long)]
    pub port: Option<u16>,
    /// Lifecycle manager label recorded for diagnostics (for example cli or desktop).
    #[arg(long = "managed-by", default_value = "cli")]
    pub managed_by: String,
    /// Browser/control capability supplied by a trusted desktop manager.
    /// Hidden because it is a secret and never appears in lifecycle JSON.
    #[arg(long = "owner-token", hide = true, conflicts_with = "owner_token_env")]
    pub owner_token: Option<String>,
    /// Read the trusted desktop/browser capability from this environment variable.
    #[arg(long = "owner-token-env")]
    pub owner_token_env: Option<String>,
    /// Roblox GameId override persisted to ro-sync.json before launch.
    #[arg(long = "game-id")]
    pub game_id: Option<String>,
    /// Roblox GroupId override persisted to ro-sync.json before launch.
    #[arg(long = "group-id")]
    pub group_id: Option<String>,
    /// Roblox PlaceId override; repeat for multiple place IDs.
    #[arg(long = "place-id")]
    pub place_id: Vec<String>,
    /// Desktop-authorized parent directory for plugin-created projects.
    #[arg(long = "projects-root")]
    pub projects_root: Option<PathBuf>,
    /// Override the platform-native Ro Sync state directory.
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
    /// Seconds to wait for the exact boot-ID handshake.
    #[arg(long, default_value_t = 10.0)]
    pub timeout: f64,
    /// Keep this short-lived lifecycle process alive only while its parent
    /// keeps the inherited stdin pipe open.
    #[arg(long = "parent-stdin-lease", hide = true)]
    pub parent_stdin_lease: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct DaemonStatusArgs {
    #[arg(long)]
    pub project: PathBuf,
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
    /// Keep this short-lived lifecycle process alive only while its parent
    /// keeps the inherited stdin pipe open.
    #[arg(long = "parent-stdin-lease", hide = true)]
    pub parent_stdin_lease: bool,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct DaemonStopArgs {
    #[arg(long)]
    pub project: PathBuf,
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
    /// Seconds to wait for graceful shutdown. Ro Sync never kills a PID from a stale record.
    #[arg(long, default_value_t = 10.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct DaemonRestartArgs {
    #[arg(long)]
    pub project: PathBuf,
    /// Exact replacement port. Without this flag, the previous port is reused when possible.
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long = "managed-by", default_value = "cli")]
    pub managed_by: String,
    #[arg(long = "owner-token", hide = true, conflicts_with = "owner_token_env")]
    pub owner_token: Option<String>,
    /// Read the trusted manager/browser capability from this environment variable.
    #[arg(long = "owner-token-env")]
    pub owner_token_env: Option<String>,
    #[arg(long = "game-id")]
    pub game_id: Option<String>,
    #[arg(long = "group-id")]
    pub group_id: Option<String>,
    #[arg(long = "place-id")]
    pub place_id: Vec<String>,
    /// Desktop-authorized parent directory for plugin-created projects.
    #[arg(long = "projects-root")]
    pub projects_root: Option<PathBuf>,
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 10.0)]
    pub timeout: f64,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct DaemonLogsArgs {
    #[arg(long)]
    pub project: PathBuf,
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 100)]
    pub lines: usize,
    #[arg(long)]
    pub follow: bool,
    #[arg(long, conflicts_with = "follow")]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct InitArgs {
    #[arg(long)]
    pub project: PathBuf,
    /// Project display name written to ro-sync.json.
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long = "game-id")]
    pub game_id: Option<String>,
    #[arg(long = "group-id")]
    pub group_id: Option<String>,
    #[arg(long = "place-id")]
    pub place_id: Vec<String>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(Subcommand, Debug)]
pub enum PluginCommand {
    /// Atomically install the bundled Plugin.rbxm into Roblox Studio.
    Install(PluginInstallArgs),
    /// Compare the installed plugin with the bundled Plugin.rbxm.
    Status(PluginStatusArgs),
}

#[derive(ClapArgs, Debug)]
pub struct PluginInstallArgs {
    /// Override the bundled Plugin.rbxm source (primarily for packagers/tests).
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Override Roblox Studio's per-user Plugins directory.
    #[arg(long = "plugin-dir")]
    pub plugin_dir: Option<PathBuf>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PluginStatusArgs {
    #[arg(long)]
    pub source: Option<PathBuf>,
    #[arg(long = "plugin-dir")]
    pub plugin_dir: Option<PathBuf>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Store a Roblox Open Cloud credential from stdin, a file, or an environment variable.
    Set(AuthSetArgs),
    /// Report whether a CLI credential is stored (never prints the credential).
    Status(AuthStatusArgs),
    /// Remove the stored CLI credential.
    Clear(AuthClearArgs),
}

#[derive(ClapArgs, Debug)]
pub struct AuthSetArgs {
    /// Read the credential from stdin. The credential itself is never accepted as an argument.
    #[arg(long = "from-stdin", conflicts_with_all = ["file", "from_env"])]
    pub from_stdin: bool,
    /// Read the credential from a file.
    #[arg(long, conflicts_with_all = ["from_stdin", "from_env"])]
    pub file: Option<PathBuf>,
    /// Read the credential from an environment variable.
    #[arg(long = "from-env", conflicts_with_all = ["from_stdin", "file"])]
    pub from_env: Option<String>,
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct AuthStatusArgs {
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct AuthClearArgs {
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct QueryArgs {
    /// Project directory. Used for daemon port discovery.
    #[arg(long)]
    pub project: Option<PathBuf>,

    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,

    /// Selector. `/`-separated; `*` matches one segment, `**` matches zero or more.
    pub selector: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = QueryFormat::Json)]
    pub format: QueryFormat,

    /// Include an inspectable property in each match. Repeat for more properties.
    #[arg(long = "prop")]
    pub props: Vec<String>,

    /// Include attributes in each match.
    #[arg(long)]
    pub attributes: bool,

    /// Include CollectionService tags in each match.
    #[arg(long)]
    pub tags: bool,

    /// Maximum matches returned by Studio (1..=10000).
    #[arg(long, default_value_t = 5000)]
    pub limit: usize,
}

#[derive(ClapArgs, Debug)]
pub struct PathArgs {
    /// Project directory. Used for filesystem mapping and daemon port discovery.
    #[arg(long)]
    pub project: PathBuf,

    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,

    /// Interpret input as `studio`, `fs`, or try `auto`.
    #[arg(long, value_enum, default_value_t = path_resolver::PathInputKind::Auto)]
    pub from: path_resolver::PathInputKind,

    /// Studio path (`Workspace/Foo`) or filesystem path (`Workspace/Foo.luau`).
    pub target: String,

    /// Print JSON instead of the resolved path.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct DecisionArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// Pending choice id. If omitted, the single current pending choice is used.
    #[arg(long = "choice-id")]
    pub choice_id: Option<String>,
    /// Keep disk/local files and push them to Studio.
    #[arg(long, conflicts_with_all = ["studio", "cancel"])]
    pub disk: bool,
    /// Keep Studio and pull it to disk.
    #[arg(long, conflicts_with_all = ["disk", "cancel"])]
    pub studio: bool,
    /// Cancel the initial sync.
    #[arg(long, conflicts_with_all = ["disk", "studio"])]
    pub cancel: bool,
    /// Print JSON instead of human-readable output.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct LintArgs {
    /// Project directory. Defaults to the current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Daemon port used for live Studio DataModel discovery.
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    /// File or directory to analyze. May be repeated. Relative paths are
    /// resolved from `--project` when provided, otherwise from the current
    /// working directory.
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    /// Additional analyzer/compiler ignore glob. May be repeated.
    #[arg(long = "ignore")]
    pub ignores: Vec<String>,
    /// Disable Ro Sync's default dependency/vendor diagnostic ignores.
    #[arg(long = "no-vendor-ignores")]
    pub no_vendor_ignores: bool,
    /// Only print diagnostics for the requested --path scopes. Alias:
    /// --owned-only.
    #[arg(long = "scope-only", alias = "owned-only")]
    pub scope_only: bool,
    /// Print diagnostic counts by category and file after analysis.
    #[arg(long)]
    pub summary: bool,
    /// Path to a luau-lsp executable. If omitted, `ROSYNC_LUAU_LSP`, bundled
    /// and Aftman locations, then PATH are checked.
    #[arg(long = "luau-lsp")]
    pub luau_lsp: Option<PathBuf>,
    /// Run the Luau bytecode compiler in addition to static analysis. `auto`
    /// checks when luau-compile is available, `required` fails when it is not,
    /// and `off` disables the compiler stage.
    #[arg(long = "compile", value_enum, default_value_t = LintCompileMode::Auto)]
    pub compile: LintCompileMode,
    /// Path to a luau-compile executable. If omitted,
    /// `ROSYNC_LUAU_COMPILE`, `LUAU_COMPILE`, the bundled compiler, Aftman,
    /// and PATH are checked.
    #[arg(long = "luau-compile")]
    pub luau_compile: Option<PathBuf>,
    /// Do not generate/pass a Ro-Sync sourcemap to luau-lsp.
    #[arg(long = "no-sourcemap")]
    pub no_sourcemap: bool,
    /// DataModel typing source. `auto` uses the complete live Studio tree when
    /// connected and safely falls back to relaxed filesystem-only analysis.
    #[arg(long = "data-model", value_enum, default_value_t = LintDataModelMode::Auto)]
    pub data_model: LintDataModelMode,
    /// Print machine-readable diagnostics and coverage metadata as JSON.
    #[arg(long)]
    pub raw: bool,
    /// Extra arguments passed to `luau-lsp analyze` after `--`.
    #[arg(last = true)]
    pub extra_args: Vec<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintDataModelMode {
    /// Prefer the complete live Studio tree; fall back to relaxed disk types.
    Auto,
    /// Require the complete live Studio tree and strict DataModel diagnostics.
    Studio,
    /// Use the filesystem sourcemap with strict DataModel diagnostics. This can
    /// report false unknown-child errors for Studio-only instances.
    Filesystem,
    /// Use the filesystem sourcemap with relaxed DataModel diagnostics.
    Loose,
}

impl LintDataModelMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Studio => "studio",
            Self::Filesystem => "filesystem",
            Self::Loose => "loose",
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintCompileMode {
    /// Check bytecode compilation when luau-compile is available.
    Auto,
    /// Require luau-compile and fail if the compiler cannot be run.
    Required,
    /// Disable bytecode compilation checks.
    Off,
}

impl LintCompileMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Required => "required",
            Self::Off => "off",
        }
    }
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
    /// Saved full overwrite decision, mirrored from `ro-sync.json` and
    /// advertised via `/hello` for plugin auto-answer.
    pub initial_choice_default: Arc<RwLock<Option<String>>>,
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
    let requested_hello = fetch_daemon_hello(requested_port).ok();
    if requested_hello
        .as_ref()
        .is_some_and(|hello| daemon_hello_matches_project(hello, &canonical_project))
    {
        return Ok(Some(requested_port));
    }

    for port in ports {
        if port == requested_port {
            continue;
        }

        if fetch_daemon_hello(port)
            .ok()
            .as_ref()
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
                }
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("plugin").join("Plugin.rbxm"));
            candidates.push(cwd.join("..").join("plugin").join("Plugin.rbxm"));
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DaemonLifecycleStatus {
    ok: bool,
    running: bool,
    managed: bool,
    managed_by: Option<String>,
    project: String,
    canonical_project: String,
    pid: Option<u32>,
    port: Option<u16>,
    base_url: Option<String>,
    boot_id: Option<String>,
    log_path: Option<String>,
    started_at: Option<u64>,
    plugin_connected: Option<bool>,
    stale: bool,
    externally_managed: bool,
}

fn arm_parent_stdin_lease() -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("rosync-parent-stdin-lease".to_string())
        .spawn(|| {
            let stdin = std::io::stdin();
            monitor_parent_stdin(stdin.lock(), || -> () {
                terminate_lifecycle_after_parent_disconnect()
            });
        })?;
    Ok(())
}

fn monitor_parent_stdin<R, F>(mut reader: R, on_disconnect: F)
where
    R: std::io::Read,
    F: FnOnce(),
{
    let mut buffer = [0_u8; 64];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    on_disconnect();
}

#[cfg(unix)]
fn terminate_lifecycle_after_parent_disconnect() -> ! {
    // SAFETY: `_exit` immediately terminates this short-lived lifecycle
    // process without running locks or cleanup handlers that may be blocked on
    // another thread. The OS releases its start-lock and pipe handles.
    unsafe { libc::_exit(1) }
}

#[cfg(not(unix))]
fn terminate_lifecycle_after_parent_disconnect() -> ! {
    std::process::exit(1)
}

async fn run_daemon(args: DaemonArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        DaemonCommand::Start(args) => {
            let raw = args.raw;
            let status = daemon_start(args).await?;
            print_daemon_status(&status, raw)?;
        }
        DaemonCommand::Status(args) => {
            if args.parent_stdin_lease {
                arm_parent_stdin_lease()?;
            }
            let canonical_project =
                lifecycle::canonical_project(&args.project).map_err(|error| {
                    format!(
                        "daemon status: canonicalize {}: {error}",
                        args.project.display()
                    )
                })?;
            let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
            let status = daemon_status(&canonical_project, &paths, true)?;
            print_daemon_status(&status, args.raw)?;
        }
        DaemonCommand::Stop(args) => {
            validate_lifecycle_timeout(args.timeout, "daemon stop")?;
            let canonical_project =
                lifecycle::canonical_project(&args.project).map_err(|error| {
                    format!(
                        "daemon stop: canonicalize {}: {error}",
                        args.project.display()
                    )
                })?;
            let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
            let status = daemon_stop(
                &canonical_project,
                &paths,
                Duration::from_secs_f64(args.timeout),
            )
            .await?;
            print_daemon_status(&status, args.raw)?;
        }
        DaemonCommand::Restart(args) => {
            validate_lifecycle_timeout(args.timeout, "daemon restart")?;
            let canonical_project =
                lifecycle::canonical_project(&args.project).map_err(|error| {
                    format!(
                        "daemon restart: canonicalize {}: {error}",
                        args.project.display()
                    )
                })?;
            let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
            let previous_port = lifecycle::read_record(&paths.record)?.map(|record| record.port);
            let existing = daemon_status(&canonical_project, &paths, false)?;
            if existing.running || existing.stale {
                daemon_stop(
                    &canonical_project,
                    &paths,
                    Duration::from_secs_f64(args.timeout),
                )
                .await?;
            }
            let raw = args.raw;
            let status = daemon_start(DaemonStartArgs {
                project: canonical_project,
                port: args.port.or(previous_port),
                managed_by: args.managed_by,
                owner_token: args.owner_token,
                owner_token_env: args.owner_token_env,
                game_id: args.game_id,
                group_id: args.group_id,
                place_id: args.place_id,
                projects_root: args.projects_root,
                data_dir: args.data_dir,
                timeout: args.timeout,
                parent_stdin_lease: false,
                raw,
            })
            .await?;
            print_daemon_status(&status, raw)?;
        }
        DaemonCommand::Logs(args) => daemon_logs(args).await?,
    }
    Ok(())
}

fn daemon_runtime_paths(
    data_dir: Option<&std::path::Path>,
    canonical_project: &std::path::Path,
) -> Result<lifecycle::RuntimePaths, Box<dyn std::error::Error>> {
    let state_dir = lifecycle::state_dir(data_dir)
        .map_err(|error| format!("resolve Ro Sync state directory: {error}"))?;
    Ok(lifecycle::runtime_paths(state_dir, canonical_project))
}

fn validate_lifecycle_timeout(
    timeout: f64,
    context: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !timeout.is_finite() || !(0.1..=300.0).contains(&timeout) {
        return Err(format!("{context}: --timeout must be between 0.1 and 300 seconds").into());
    }
    Ok(())
}

fn read_named_secret_env(name: &str, context: &str) -> Result<String, Box<dyn std::error::Error>> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!("{context}: invalid environment variable name").into());
    }
    let value = std::env::var(name)
        .map_err(|_| format!("{context}: environment variable {name} is missing or not UTF-8"))?;
    if value.is_empty() {
        return Err(format!("{context}: environment variable {name} is empty").into());
    }
    Ok(value)
}

fn resolve_optional_secret(
    direct: Option<String>,
    env_name: Option<&str>,
    context: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match (direct, env_name) {
        (Some(value), None) if !value.is_empty() => Ok(Some(value)),
        (Some(_), None) => Err(format!("{context}: secret cannot be empty").into()),
        (None, Some(name)) => Ok(Some(read_named_secret_env(name, context)?)),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(format!("{context}: choose one secret source").into()),
    }
}

fn read_widget_owner_token_state_file(
    path: &std::path::Path,
) -> Result<String, Box<dyn std::error::Error>> {
    const MAX_WIDGET_STATE_BYTES: u64 = 4 * 1024 * 1024;
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "serve widget owner token: inspect Terminal 64 state file {}: {error}",
            path.display()
        )
    })?;
    if metadata.len() > MAX_WIDGET_STATE_BYTES {
        return Err(format!(
            "serve widget owner token: Terminal 64 state file {} exceeds the 4 MiB limit",
            path.display()
        )
        .into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "serve widget owner token: Terminal 64 state file {} must have mode 0600",
                path.display()
            )
            .into());
        }
    }
    let bytes = std::fs::read(path).map_err(|error| {
        format!(
            "serve widget owner token: read Terminal 64 state file {}: {error}",
            path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "serve widget owner token: parse Terminal 64 state file {}: {error}",
            path.display()
        )
    })?;
    let token = value
        .get("state")
        .and_then(|state| state.get("daemonOwnerToken"))
        .and_then(serde_json::Value::as_str)
        .filter(|token| {
            (16..=512).contains(&token.len())
                && token.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
                })
        })
        .ok_or_else(|| {
            format!(
                "serve widget owner token: {} has no valid state.daemonOwnerToken",
                path.display()
            )
        })?;
    Ok(token.to_owned())
}

fn resolve_widget_owner_token(
    direct: Option<String>,
    state_file: Option<&std::path::Path>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match (direct, state_file) {
        (Some(value), None) if !value.is_empty() => Ok(Some(value)),
        (Some(_), None) => Err("serve widget owner token cannot be empty".into()),
        (None, Some(path)) => read_widget_owner_token_state_file(path).map(Some),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err("serve widget owner token: choose one secret source".into()),
    }
}

fn normalize_optional_metadata(
    value: Option<&str>,
    flag: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match value {
        Some(value) if value.trim().is_empty() => Err(format!("{flag} cannot be empty").into()),
        Some(value) => Ok(Some(value.trim().to_string())),
        None => Ok(None),
    }
}

fn persist_daemon_start_metadata(
    canonical_project: &std::path::Path,
    game_id: Option<String>,
    group_id: Option<String>,
    place_ids: Option<Vec<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if game_id.is_none() && group_id.is_none() && place_ids.is_none() {
        return Ok(());
    }
    let mut config = project_config::load_or_create(canonical_project)
        .map_err(|error| format!("daemon start: load ro-sync.json: {error}"))?;
    if project_config::apply_overrides(&mut config, game_id, group_id, place_ids) {
        project_config::write(canonical_project, &config)
            .map_err(|error| format!("daemon start: write ro-sync.json: {error}"))?;
    }
    Ok(())
}

fn validate_existing_daemon_owner(
    record: &lifecycle::RuntimeRecord,
    supplied_owner_token: Option<&str>,
) -> Result<(), &'static str> {
    let Some(supplied) = supplied_owner_token else {
        return Ok(());
    };
    let expected = record.control_token.as_bytes();
    let supplied = supplied.as_bytes();
    if expected.len() != supplied.len() {
        return Err("matching managed daemon is owned by a different lifecycle capability");
    }
    let difference = expected
        .iter()
        .zip(supplied)
        .fold(0_u8, |difference, (expected, supplied)| {
            difference | (expected ^ supplied)
        });
    if difference == 0 {
        Ok(())
    } else {
        Err("matching managed daemon is owned by a different lifecycle capability")
    }
}

fn classify_running_daemon_for_manager(
    status: &mut DaemonLifecycleStatus,
    requested_manager: &str,
) {
    if status.running && status.managed && status.managed_by.as_deref() != Some(requested_manager) {
        // A live daemon owned by another manager is useful discovery, not an
        // ownership failure. Do not compare the caller's secret with the other
        // manager's runtime capability, and expose only ordinary /hello state.
        status.externally_managed = true;
        status.log_path = None;
    }
}

fn daemon_record_matches_hello(
    record: &lifecycle::RuntimeRecord,
    hello: &serde_json::Value,
    canonical_project: &std::path::Path,
) -> bool {
    daemon_hello_matches_project(hello, canonical_project)
        && hello.get("bootId").and_then(serde_json::Value::as_str) == Some(record.boot_id.as_str())
        && hello.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(record.pid))
        && hello.get("port").and_then(serde_json::Value::as_u64) == Some(u64::from(record.port))
}

fn daemon_status_from_record(
    record: &lifecycle::RuntimeRecord,
    hello: Option<&serde_json::Value>,
    running: bool,
    stale: bool,
) -> DaemonLifecycleStatus {
    DaemonLifecycleStatus {
        ok: true,
        running,
        managed: true,
        managed_by: Some(record.managed_by.clone()),
        project: record.project.clone(),
        canonical_project: record.canonical_project.clone(),
        pid: Some(record.pid),
        port: Some(record.port),
        base_url: Some(format!("http://127.0.0.1:{}", record.port)),
        boot_id: Some(record.boot_id.clone()),
        log_path: Some(record.log_path.clone()),
        started_at: Some(record.started_at),
        plugin_connected: hello.and_then(|value| {
            value
                .get("pluginConnected")
                .and_then(serde_json::Value::as_bool)
        }),
        stale,
        externally_managed: false,
    }
}

fn external_daemon_status(
    canonical_project: &std::path::Path,
    port: u16,
    hello: &serde_json::Value,
) -> DaemonLifecycleStatus {
    let managed = hello
        .get("managed")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            hello
                .get("widgetOwned")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false);
    DaemonLifecycleStatus {
        ok: true,
        running: true,
        managed,
        managed_by: hello
            .get("managedBy")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        project: hello
            .get("project")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| canonical_project.to_str().unwrap_or(""))
            .to_string(),
        canonical_project: canonical_project.display().to_string(),
        pid: hello
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
        port: Some(port),
        base_url: Some(format!("http://127.0.0.1:{port}")),
        boot_id: hello
            .get("bootId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        log_path: None,
        started_at: hello.get("startedAt").and_then(serde_json::Value::as_u64),
        plugin_connected: hello
            .get("pluginConnected")
            .and_then(serde_json::Value::as_bool),
        stale: false,
        externally_managed: true,
    }
}

fn matching_external_daemon_status(
    canonical_project: &std::path::Path,
    port: u16,
    hello: &serde_json::Value,
) -> Option<DaemonLifecycleStatus> {
    daemon_hello_matches_project(hello, canonical_project)
        .then(|| external_daemon_status(canonical_project, port, hello))
}

fn stopped_daemon_status(canonical_project: &std::path::Path) -> DaemonLifecycleStatus {
    DaemonLifecycleStatus {
        ok: true,
        running: false,
        managed: false,
        managed_by: None,
        project: canonical_project.display().to_string(),
        canonical_project: canonical_project.display().to_string(),
        pid: None,
        port: None,
        base_url: None,
        boot_id: None,
        log_path: None,
        started_at: None,
        plugin_connected: None,
        stale: false,
        externally_managed: false,
    }
}

fn find_daemon_for_project_in_range(
    canonical_project: &std::path::Path,
    ports: std::ops::RangeInclusive<u16>,
) -> Option<(u16, serde_json::Value)> {
    ports.into_iter().find_map(|port| {
        let hello = fetch_daemon_hello(port).ok()?;
        daemon_hello_matches_project(&hello, canonical_project).then_some((port, hello))
    })
}

fn find_daemon_for_project(
    canonical_project: &std::path::Path,
) -> Option<(u16, serde_json::Value)> {
    find_daemon_for_project_in_range(
        canonical_project,
        DEFAULT_DAEMON_PORT..=DAEMON_PORT_SCAN_MAX,
    )
}

fn daemon_status(
    canonical_project: &std::path::Path,
    paths: &lifecycle::RuntimePaths,
    clean_stale: bool,
) -> Result<DaemonLifecycleStatus, Box<dyn std::error::Error>> {
    if let Some(record) = lifecycle::read_record(&paths.record)? {
        if canonicalize_project_path(std::path::Path::new(&record.canonical_project))
            != canonical_project
        {
            return Err(format!(
                "daemon runtime record {} belongs to {}, not {}",
                paths.record.display(),
                record.canonical_project,
                canonical_project.display()
            )
            .into());
        }
        let hello = fetch_daemon_hello(record.port).ok();
        if hello
            .as_ref()
            .is_some_and(|hello| daemon_record_matches_hello(&record, hello, canonical_project))
        {
            return Ok(daemon_status_from_record(
                &record,
                hello.as_ref(),
                true,
                false,
            ));
        }
        if clean_stale {
            lifecycle::remove_record_if_boot(&paths.record, &record.boot_id)?;
        }

        // A stale record only proves that its exact boot is gone. A manual
        // daemon or a daemon adopted by another host may still be serving the
        // same project, either on the recorded port or elsewhere in the
        // discovery range. Prefer that live identity so `daemon start` stays
        // idempotent instead of launching a duplicate.
        if let Some(status) = hello.as_ref().and_then(|hello| {
            matching_external_daemon_status(canonical_project, record.port, hello)
        }) {
            return Ok(status);
        }
        if let Some((port, hello)) = find_daemon_for_project(canonical_project) {
            return Ok(external_daemon_status(canonical_project, port, &hello));
        }
        return Ok(daemon_status_from_record(
            &record,
            hello.as_ref(),
            false,
            true,
        ));
    }

    if let Some((port, hello)) = find_daemon_for_project(canonical_project) {
        return Ok(external_daemon_status(canonical_project, port, &hello));
    }
    Ok(stopped_daemon_status(canonical_project))
}

async fn daemon_start(
    args: DaemonStartArgs,
) -> Result<DaemonLifecycleStatus, Box<dyn std::error::Error>> {
    if args.parent_stdin_lease {
        arm_parent_stdin_lease()?;
    }
    validate_lifecycle_timeout(args.timeout, "daemon start")?;
    let timeout = Duration::from_secs_f64(args.timeout);
    if args.managed_by.trim().is_empty() {
        return Err("daemon start: --managed-by cannot be empty".into());
    }
    let supplied_owner_token = resolve_optional_secret(
        args.owner_token.clone(),
        args.owner_token_env.as_deref(),
        "daemon start owner token",
    )?;
    let canonical_project = lifecycle::canonical_project(&args.project).map_err(|error| {
        format!(
            "daemon start: canonicalize {}: {error}",
            args.project.display()
        )
    })?;
    let projects_root = args
        .projects_root
        .as_deref()
        .map(project_init::resolve_projects_root)
        .transpose()
        .map_err(|error| format!("daemon start: {error}"))?;
    let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
    let _lock = lifecycle::StartLock::acquire(&paths.start_lock)?;

    let game_id = normalize_optional_metadata(args.game_id.as_deref(), "--game-id")?;
    let group_id = normalize_optional_metadata(args.group_id.as_deref(), "--group-id")?;
    let place_ids = if args.place_id.is_empty() {
        None
    } else {
        Some(
            args.place_id
                .iter()
                .map(|value| {
                    normalize_optional_metadata(Some(value), "--place-id")?
                        .ok_or_else(|| "--place-id cannot be empty".into())
                })
                .collect::<Result<Vec<String>, Box<dyn std::error::Error>>>()?,
        )
    };
    let mut current = daemon_status(&canonical_project, &paths, true)?;
    classify_running_daemon_for_manager(&mut current, args.managed_by.trim());
    if current.running {
        if current.externally_managed {
            return Ok(current);
        }
        // An idempotent start is only idempotent for the same lifecycle
        // capability. Returning an existing boot to a caller that supplied a
        // different token makes that caller believe it owns a daemon it can
        // neither authenticate nor safely stop.
        if !current.externally_managed {
            let record = lifecycle::read_record(&paths.record)?.ok_or_else(|| {
                format!(
                    "daemon start: managed daemon is running but runtime record {} is missing",
                    paths.record.display()
                )
            })?;
            validate_existing_daemon_owner(&record, supplied_owner_token.as_deref())
                .map_err(|error| format!("daemon start: {error}"))?;
        }
        if let Some(requested_root) = projects_root.as_deref() {
            let advertised_root = current
                .port
                .and_then(|port| fetch_daemon_hello(port).ok())
                .and_then(|hello| {
                    hello
                        .pointer("/projectInit/projectsRoot")
                        .and_then(serde_json::Value::as_str)
                        .map(PathBuf::from)
                });
            if advertised_root.as_deref() != Some(requested_root) {
                return Err(format!(
                    "daemon start: the matching daemon is already running without the requested projects root {}; restart it to enable Studio project creation",
                    requested_root.display()
                )
                .into());
            }
        }
        if let Some(requested_port) = args.port {
            if current.port != Some(requested_port) {
                if let Ok(hello) = fetch_daemon_hello(requested_port) {
                    let daemon_project = hello
                        .get("project")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown project");
                    return Err(format!(
                        "daemon start: requested port {requested_port} serves {daemon_project}; matching project already runs on port {}",
                        current.port.unwrap_or_default()
                    )
                    .into());
                }
            }
        }
        // Metadata belongs to the lifecycle owner just as shutdown authority
        // does. Defer disk writes until the exact live boot has accepted this
        // caller's capability and all idempotent-start checks will succeed.
        persist_daemon_start_metadata(&canonical_project, game_id, group_id, place_ids)?;
        return Ok(current);
    }

    if let Some(requested_port) = args.port {
        if let Ok(hello) = fetch_daemon_hello(requested_port) {
            if let Some(status) =
                matching_external_daemon_status(&canonical_project, requested_port, &hello)
            {
                return Ok(status);
            }
            let daemon_project = hello
                .get("project")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown project");
            return Err(format!(
                "daemon start: port {requested_port} is already serving {daemon_project}, not {}",
                canonical_project.display()
            )
            .into());
        }
    }

    // Project start locks are intentionally independent, but TCP ports are
    // process-global. Serialize selection through the shared daemon state
    // directory and hold this lock until the child has bound and completed its
    // exact boot handshake. Without this guard, two different projects can
    // both probe the same fallback port before either child starts.
    let port_lock_path = daemon_port_allocation_lock_path(&paths)?;
    let _port_lock = acquire_daemon_port_allocation_lock(&port_lock_path, timeout)
        .await
        .map_err(|error| format!("daemon start: {error}"))?;

    // No matching live daemon exists after the locked status/port probes, so
    // these explicit launch overrides can safely seed the process about to be
    // created. A foreign live daemon always returned above without mutation.
    persist_daemon_start_metadata(&canonical_project, game_id, group_id, place_ids)?;

    let port = match args.port {
        Some(0) => reserve_ephemeral_port()?,
        Some(port) => {
            ensure_daemon_port_available(port)?;
            port
        }
        None => find_available_daemon_port().ok_or_else(|| {
            format!(
                "daemon start: no available port in {DEFAULT_DAEMON_PORT}-{DAEMON_PORT_SCAN_MAX}"
            )
        })?,
    };
    let control_token = match supplied_owner_token {
        Some(token) => token,
        None => artifact::random_hex(32)?,
    };
    let boot_id = artifact::random_hex(32)?;
    let started_at = unix_secs();
    spawn_managed_daemon(ManagedDaemonLaunch {
        canonical_project: &canonical_project,
        paths: &paths,
        port,
        managed_by: &args.managed_by,
        control_token: &control_token,
        boot_id: &boot_id,
        started_at,
        timeout,
        owner_token_env: args.owner_token_env.as_deref(),
        projects_root: projects_root.as_deref(),
    })
    .await
}

fn daemon_port_allocation_lock_path(
    paths: &lifecycle::RuntimePaths,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let daemon_dir = paths
        .start_lock
        .parent()
        .ok_or("daemon start: invalid per-project lock path")?;
    Ok(daemon_dir.join("ports.start.lock"))
}

async fn acquire_daemon_port_allocation_lock(
    path: &std::path::Path,
    timeout: Duration,
) -> std::io::Result<lifecycle::StartLock> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon port-allocation timeout overflow",
        )
    })?;
    loop {
        match lifecycle::StartLock::acquire_named(path, "daemon port allocation") {
            Ok(lock) => return Ok(lock),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for another daemon to finish port allocation (lock {})",
                        path.display()
                    ),
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

fn ensure_daemon_port_available(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .map_err(|error| format!("daemon start: requested port {port} is unavailable: {error}"))?;
    drop(listener);
    Ok(())
}

fn reserve_ephemeral_port() -> Result<u16, Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

fn find_available_daemon_port() -> Option<u16> {
    (DEFAULT_DAEMON_PORT..=DAEMON_PORT_SCAN_MAX)
        .find(|port| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port)).is_ok())
}

struct ManagedDaemonLaunch<'a> {
    canonical_project: &'a std::path::Path,
    paths: &'a lifecycle::RuntimePaths,
    port: u16,
    managed_by: &'a str,
    control_token: &'a str,
    boot_id: &'a str,
    started_at: u64,
    timeout: Duration,
    owner_token_env: Option<&'a str>,
    projects_root: Option<&'a std::path::Path>,
}

async fn spawn_managed_daemon(
    launch: ManagedDaemonLaunch<'_>,
) -> Result<DaemonLifecycleStatus, Box<dyn std::error::Error>> {
    let ManagedDaemonLaunch {
        canonical_project,
        paths,
        port,
        managed_by,
        control_token,
        boot_id,
        started_at,
        timeout,
        owner_token_env,
        projects_root,
    } = launch;
    let executable = std::env::current_exe()?;
    let stdout = lifecycle::open_private_log(&paths.log)?;
    let stderr = stdout.try_clone()?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("serve")
        .arg("--project")
        .arg(canonical_project)
        .arg("--port")
        .arg(port.to_string())
        .arg("--managed")
        .arg("--managed-by")
        .arg(managed_by)
        .arg("--control-token-env")
        .arg("ROSYNC_DAEMON_CONTROL_TOKEN")
        .arg("--boot-id")
        .arg(boot_id)
        .arg("--runtime-record")
        .arg(&paths.record)
        .arg("--log-path")
        .arg(&paths.log)
        .arg("--started-at")
        .arg(started_at.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(projects_root) = projects_root {
        command.arg("--projects-root").arg(projects_root);
    }
    // The short-lived lifecycle process receives the manager secret through
    // an environment variable, but the long-lived daemon needs only its
    // dedicated control-token copy. Do not retain or propagate the source
    // variable into daemon-launched tools.
    command.env_remove("ROSYNC_OWNER_TOKEN");
    if let Some(owner_token_env) = owner_token_env {
        command.env_remove(owner_token_env);
    }
    command.env("ROSYNC_DAEMON_CONTROL_TOKEN", control_token);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `setsid` is async-signal-safe and the closure performs no
        // allocation or shared-memory access between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = command.spawn().map_err(|error| {
        format!(
            "daemon start: launch managed daemon for {}: {error}",
            canonical_project.display()
        )
    })?;
    let child_pid = child.id();
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("daemon start: timeout overflow")?;
    loop {
        if let Ok(hello) = fetch_daemon_hello(port) {
            let expected = lifecycle::RuntimeRecord {
                version: lifecycle::RUNTIME_RECORD_VERSION,
                project: canonical_project.display().to_string(),
                canonical_project: canonical_project.display().to_string(),
                pid: child_pid,
                port,
                boot_id: boot_id.to_string(),
                control_token: control_token.to_string(),
                managed_by: managed_by.to_string(),
                log_path: paths.log.display().to_string(),
                started_at,
            };
            if daemon_record_matches_hello(&expected, &hello, canonical_project) {
                let record = lifecycle::read_record(&paths.record)?.ok_or_else(|| {
                    format!(
                        "daemon start: exact daemon answered but runtime record {} is missing",
                        paths.record.display()
                    )
                })?;
                if record.boot_id != boot_id || record.pid != child_pid || record.port != port {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        "daemon start: runtime record did not match the spawned process".into(),
                    );
                }
                return Ok(daemon_status_from_record(
                    &record,
                    Some(&hello),
                    true,
                    false,
                ));
            }
        }
        if let Some(exit) = child.try_wait()? {
            lifecycle::remove_record_if_boot(&paths.record, boot_id)?;
            let tail = read_log_tail(&paths.log, 20).unwrap_or_default();
            return Err(format!(
                "daemon start: child {child_pid} exited with {exit} before the exact handshake{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\n{tail}")
                }
            )
            .into());
        }
        if Instant::now() >= deadline {
            // This is the exact child handle created above, never a PID read
            // from disk. It is therefore safe to terminate on failed startup.
            let _ = child.kill();
            let _ = child.wait();
            lifecycle::remove_record_if_boot(&paths.record, boot_id)?;
            let tail = read_log_tail(&paths.log, 20).unwrap_or_default();
            return Err(format!(
                "daemon start: timed out waiting for project/boot handshake on port {port}{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\n{tail}")
                }
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn managed_daemon_close_request(
    record: &lifecycle::RuntimeRecord,
    reason: &str,
) -> serde_json::Value {
    serde_json::json!({
        "token": record.control_token,
        "reason": reason,
        "expectedBootId": record.boot_id,
        "expectedPid": record.pid,
        "expectedPort": record.port,
        "expectedCanonicalProject": record.canonical_project,
    })
}

async fn daemon_stop(
    canonical_project: &std::path::Path,
    paths: &lifecycle::RuntimePaths,
    timeout: Duration,
) -> Result<DaemonLifecycleStatus, Box<dyn std::error::Error>> {
    let Some(record) = lifecycle::read_record(&paths.record)? else {
        if let Some((port, _)) = find_daemon_for_project(canonical_project) {
            return Err(format!(
                "daemon stop: project is running on port {port}, but no matching managed runtime record exists; refusing PID-only or unauthenticated shutdown"
            )
            .into());
        }
        return Ok(stopped_daemon_status(canonical_project));
    };
    if canonicalize_project_path(std::path::Path::new(&record.canonical_project))
        != canonical_project
    {
        return Err(format!(
            "daemon stop: runtime record belongs to {}, not {}",
            record.canonical_project,
            canonical_project.display()
        )
        .into());
    }
    let hello = fetch_daemon_hello(record.port).ok();
    if !hello
        .as_ref()
        .is_some_and(|hello| daemon_record_matches_hello(&record, hello, canonical_project))
    {
        lifecycle::remove_record_if_boot(&paths.record, &record.boot_id)?;
        return Ok(daemon_status_from_record(
            &record,
            hello.as_ref(),
            false,
            true,
        ));
    }

    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("daemon stop: timeout overflow")?;
    let response = http_post_json_until(
        record.port,
        "/manager-close",
        &managed_daemon_close_request(&record, "managed daemon stop requested"),
        deadline,
    )
    .await
    .map_err(|error| format!("daemon stop: authenticated shutdown request: {error}"))?;
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!("daemon stop: daemon rejected shutdown: {response}").into());
    }

    loop {
        let exact_still_running = fetch_daemon_hello(record.port)
            .ok()
            .is_some_and(|hello| daemon_record_matches_hello(&record, &hello, canonical_project));
        if !exact_still_running {
            lifecycle::remove_record_if_boot(&paths.record, &record.boot_id)?;
            let mut status = daemon_status_from_record(&record, None, false, false);
            status.plugin_connected = response
                .get("pluginConnected")
                .and_then(serde_json::Value::as_bool);
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "daemon stop: timed out waiting for boot {} on port {} to stop; no PID signal was sent",
                record.boot_id, record.port
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn print_daemon_status(
    status: &DaemonLifecycleStatus,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if raw {
        println!("{}", serde_json::to_string(status)?);
        return Ok(());
    }
    if status.running {
        let ownership = if status.externally_managed {
            "external"
        } else if status.managed {
            "managed"
        } else {
            "manual"
        };
        println!(
            "Ro Sync is running for {} on {} ({ownership}).",
            status.canonical_project,
            status.base_url.as_deref().unwrap_or("unknown address")
        );
    } else if status.stale {
        println!(
            "Ro Sync is not running for {} (removed stale runtime state).",
            status.canonical_project
        );
    } else {
        println!("Ro Sync is not running for {}.", status.canonical_project);
    }
    Ok(())
}

fn read_log_tail(path: &std::path::Path, lines: usize) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    if lines == 0 {
        return Ok(String::new());
    }
    let mut selected = text.lines().rev().take(lines).collect::<Vec<_>>();
    selected.reverse();
    Ok(selected.join("\n"))
}

async fn daemon_logs(args: DaemonLogsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let canonical_project = lifecycle::canonical_project(&args.project).map_err(|error| {
        format!(
            "daemon logs: canonicalize {}: {error}",
            args.project.display()
        )
    })?;
    let paths = daemon_runtime_paths(args.data_dir.as_deref(), &canonical_project)?;
    let path = lifecycle::read_record(&paths.record)?
        .map(|record| PathBuf::from(record.log_path))
        .unwrap_or(paths.log);
    let content = match read_log_tail(&path, args.lines) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("daemon logs: read {}: {error}", path.display()).into()),
    };
    if args.raw {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "lines": args.lines,
                "content": content,
            }))?
        );
        return Ok(());
    }
    if !content.is_empty() {
        println!("{content}");
    }
    if !args.follow {
        return Ok(());
    }

    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|error| format!("daemon logs: follow {}: {error}", path.display()))?;
    let mut offset = file.metadata()?.len();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                let length = file.metadata()?.len();
                if length < offset {
                    offset = 0;
                }
                if length > offset {
                    file.seek(SeekFrom::Start(offset))?;
                    let mut appended = String::new();
                    file.read_to_string(&mut appended)?;
                    offset = length;
                    print!("{appended}");
                    std::io::stdout().flush()?;
                }
            }
        }
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

    // Documentation/config merges are best-effort conveniences; run them off
    // the readiness path so the daemon starts serving immediately.
    {
        let docs_project = canonical_project.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = snapshot::write_ro_sync_md_if_missing(&docs_project) {
                eprintln!("rosync: failed to write ro-sync.md: {e}");
            }
            if let Err(e) = snapshot::write_claude_md_if_missing_or_merge(&docs_project) {
                eprintln!("rosync: failed to write CLAUDE.md: {e}");
            }
            if let Err(e) = snapshot::write_codex_context_if_missing_or_merge(&docs_project) {
                eprintln!("rosync: failed to write Codex context: {e}");
            }
            if let Err(e) = snapshot::write_project_tooling_if_missing_or_merge(&docs_project) {
                eprintln!("rosync: failed to write project tooling files: {e}");
            }
        });
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

    let (tx, _rx) = broadcast::channel::<String>(1024);

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
        initial_choice_default: Arc::new(RwLock::new(cfg.initial_choice_default.clone())),
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

async fn run_workflow(args: RunWorkflowArgs) -> Result<(), Box<dyn std::error::Error>> {
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

fn workflow_idempotency_path(project: &std::path::Path, key: &str) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
    project
        .join(".rosync-workflows")
        .join(format!("{digest}.json"))
}

fn workflow_content_hash(
    workflow: &workflow::Workflow,
) -> Result<String, Box<dyn std::error::Error>> {
    use sha2::{Digest as _, Sha256};
    let normalized = serde_json::to_vec(workflow)?;
    Ok(format!("{:x}", Sha256::digest(normalized)))
}

fn workflow_replay_idempotency(
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

struct WorkflowIdempotencyLock {
    path: PathBuf,
}

impl WorkflowIdempotencyLock {
    fn acquire(record_path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
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

fn write_json_atomic(path: &std::path::Path, value: &serde_json::Value) -> std::io::Result<()> {
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

fn workflow_step_report(
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

fn workflow_error_response(code: &str, message: String, retryable: bool) -> serde_json::Value {
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

async fn workflow_finish_transaction(
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

async fn workflow_finish_transaction_recorded(
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

async fn workflow_check_environment(
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

async fn workflow_execute_step(
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

fn workflow_deadline_remaining(
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

fn workflow_target_path(operation: &workflow::StepOperation) -> Option<&str> {
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

async fn workflow_check_target_precondition(
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

fn workflow_wire_request(operation: &workflow::StepOperation) -> (&'static str, serde_json::Value) {
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

fn workflow_assertion_matches(actual: &serde_json::Value, check: &workflow::Assertion) -> bool {
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

async fn workflow_wait(
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

async fn workflow_verify_step(
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

fn workflow_playtest_request(
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

async fn workflow_capture(
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

async fn workflow_upload(
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

async fn run_upload(args: UploadArgs) -> Result<(), Box<dyn std::error::Error>> {
    run_upload_inner(args).await
}

async fn run_img(args: ImgArgs) -> Result<(), Box<dyn std::error::Error>> {
    run_upload_inner(UploadArgs {
        inputs: vec![args.path],
        project: args.project,
        creator: args.creator,
        name: args.name,
        description: args.description,
        asset_type: Some(args.asset_type),
        content_type: None,
        auth: args.auth,
        api_key_env: args.api_key_env,
        no_wait: args.no_wait,
        timeout_seconds: args.timeout_seconds,
        poll_seconds: args.poll_seconds,
        concurrency: 1,
        no_recursive: true,
        manifest: None,
        raw: args.raw,
    })
    .await
}

async fn run_imgs(args: ImgsArgs) -> Result<(), Box<dyn std::error::Error>> {
    run_upload_inner(UploadArgs {
        inputs: args.inputs,
        project: args.project,
        creator: args.creator,
        name: None,
        description: args.description,
        asset_type: Some(args.asset_type),
        content_type: None,
        auth: args.auth,
        api_key_env: args.api_key_env,
        no_wait: args.no_wait,
        timeout_seconds: args.timeout_seconds,
        poll_seconds: args.poll_seconds,
        concurrency: args.concurrency,
        no_recursive: args.no_recursive,
        manifest: args.manifest,
        raw: args.raw,
    })
    .await
}

async fn run_monetization(args: MonetizationArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        MonetizationCommand::Gamepass(args) => {
            run_monetization_asset(MonetizationKind::Gamepass, args).await
        }
        MonetizationCommand::Product(args) => {
            run_monetization_asset(MonetizationKind::Product, args).await
        }
    }
}

async fn run_monetization_asset(
    kind: MonetizationKind,
    args: MonetizationAssetArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        MonetizationAction::Discover(args) => run_monetization_discover(kind, args),
        MonetizationAction::List(args) => run_monetization_list(kind, args).await,
        MonetizationAction::Create(args) => run_monetization_create(kind, args).await,
        MonetizationAction::Edit(args) => run_monetization_edit(kind, args).await,
        MonetizationAction::Image(args) => run_monetization_image(kind, args).await,
        MonetizationAction::Images(args) => run_monetization_images(kind, args).await,
    }
}

fn run_monetization_discover(
    kind: MonetizationKind,
    args: MonetizationDiscoverArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = project_or_cwd(args.project.as_deref(), "monetization discover")?;
    let hits = discover_monetization_files(&project)?;
    let value = serde_json::json!({
        "ok": true,
        "kind": kind.label(),
        "project": project.display().to_string(),
        "universeId": resolve_monetization_universe_id(args.project.as_deref()).ok(),
        "credentialSources": monetization_credential_sources(args.project.as_deref()),
        "matches": hits,
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn run_monetization_list(
    kind: MonetizationKind,
    args: MonetizationCommonArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args)?;
    let value = monetization_list_api(kind, &universe_id, &api_key).await?;
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        let items = monetization_items_from_response(kind, &value);
        if items.is_empty() {
            println!("no {} entries returned", kind.label());
        } else {
            for item in items {
                println!(
                    "{}\t{}\t{}",
                    item.id.unwrap_or_else(|| "?".into()),
                    item.price
                        .map(|price| price.to_string())
                        .unwrap_or_else(|| "-".into()),
                    item.name.unwrap_or_else(|| "?".into())
                );
            }
        }
    }
    Ok(())
}

async fn run_monetization_create(
    kind: MonetizationKind,
    args: MonetizationCreateArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args.common)?;
    let specs = monetization_create_specs(&args)?;
    let existing = monetization_list_api(kind, &universe_id, &api_key).await?;
    let existing_names = monetization_items_from_response(kind, &existing)
        .into_iter()
        .filter_map(|item| item.name)
        .map(|name| normalize_monetization_name(&name))
        .collect::<std::collections::HashSet<_>>();
    let mut results = Vec::new();
    for spec in specs {
        if existing_names.contains(&normalize_monetization_name(&spec.name)) {
            results.push(serde_json::json!({
                "ok": false,
                "kind": kind.label(),
                "name": spec.name,
                "price": spec.price,
                "error": "asset with this normalized name already exists",
            }));
            continue;
        }
        match monetization_create_one(kind, &universe_id, &api_key, &args, &spec).await {
            Ok(value) => results.push(serde_json::json!({
                "ok": true,
                "kind": kind.label(),
                "name": spec.name,
                "price": spec.price,
                "id": monetization_id_from_value(kind, &value),
                "response": value,
            })),
            Err(e) => results.push(serde_json::json!({
                "ok": false,
                "kind": kind.label(),
                "name": spec.name,
                "price": spec.price,
                "error": e.to_string(),
            })),
        }
    }
    let ok = results.iter().all(|value| value["ok"] == true);
    let out = serde_json::json!({ "ok": ok, "results": results });
    println!("{}", serde_json::to_string_pretty(&out)?);
    if !ok {
        return Err("monetization create: one or more entries failed".into());
    }
    Ok(())
}

async fn run_monetization_edit(
    kind: MonetizationKind,
    args: MonetizationEditArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args.common)?;
    let id =
        resolve_monetization_asset_id(kind, &universe_id, &api_key, args.id, args.name).await?;
    let value = monetization_update_one(kind, &universe_id, &api_key, &id, |mut form| {
        if let Some(name) = &args.new_name {
            form = form.text("name", name.clone());
        }
        if let Some(price) = args.price {
            form = form.text("price", price.to_string());
        }
        if let Some(description) = &args.description {
            form = form.text("description", description.clone());
        }
        if let Some(for_sale) = args.for_sale {
            form = form.text("isForSale", for_sale.to_string());
        }
        form
    })
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": true,
            "kind": kind.label(),
            "id": id,
            "response": value,
        }))?
    );
    Ok(())
}

async fn run_monetization_image(
    kind: MonetizationKind,
    args: MonetizationImageArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args.common)?;
    let id =
        resolve_monetization_asset_id(kind, &universe_id, &api_key, args.id, args.name).await?;
    let value = monetization_update_image(kind, &universe_id, &api_key, &id, &args.file).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": true,
            "kind": kind.label(),
            "id": id,
            "file": args.file,
            "response": value,
        }))?
    );
    Ok(())
}

async fn run_monetization_images(
    kind: MonetizationKind,
    args: MonetizationImagesArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args.common)?;
    let list = monetization_list_api(kind, &universe_id, &api_key).await?;
    let items = monetization_items_from_response(kind, &list);
    let mut by_name = HashMap::new();
    for item in items {
        if let (Some(id), Some(name)) = (item.id, item.name) {
            by_name.insert(normalize_monetization_name(&name), id);
        }
    }
    let mut results = Vec::new();
    let mut entries = std::fs::read_dir(&args.dir)
        .map_err(|e| format!("monetization images: read {}: {e}", args.dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if !path.is_file() || !is_supported_image_path(&path) {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let key = normalize_monetization_name(stem);
        let Some(id) = by_name.get(&key).cloned() else {
            results.push(serde_json::json!({
                "ok": false,
                "file": path,
                "error": "no asset matched normalized filename",
            }));
            continue;
        };
        match monetization_update_image(kind, &universe_id, &api_key, &id, &path).await {
            Ok(value) => results.push(serde_json::json!({
                "ok": true,
                "id": id,
                "file": path,
                "response": value,
            })),
            Err(e) => results.push(serde_json::json!({
                "ok": false,
                "id": id,
                "file": path,
                "error": e.to_string(),
            })),
        }
    }
    let ok = results.iter().all(|value| value["ok"] == true);
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "ok": ok, "results": results }))?
    );
    if !ok {
        return Err("monetization images: one or more images failed".into());
    }
    Ok(())
}

async fn run_upload_inner(args: UploadArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.no_wait && args.timeout_seconds == 0 {
        return Err(
            "upload: --timeout-seconds must be greater than 0 unless --no-wait is used".into(),
        );
    }
    if !args.no_wait && args.poll_seconds == 0 {
        return Err(
            "upload: --poll-seconds must be greater than 0 unless --no-wait is used".into(),
        );
    }
    if args.concurrency == 0 {
        return Err("upload: --concurrency must be greater than 0".into());
    }
    if args
        .content_type
        .as_deref()
        .is_some_and(|content_type| content_type.trim().is_empty())
    {
        return Err("upload: --content-type cannot be empty".into());
    }

    let mut failures = Vec::new();
    let jobs = collect_upload_jobs(
        &args.inputs,
        !args.no_recursive,
        args.asset_type,
        args.content_type.as_deref(),
        &mut failures,
    )?;
    let attempted = jobs.len() + failures.len();
    if args.name.is_some() && attempted != 1 {
        return Err("upload: --name can only be used when exactly one file is uploaded".into());
    }
    if jobs.is_empty() && failures.is_empty() {
        return Err("upload: no supported asset files found".into());
    }

    let mut results = failures;
    if !jobs.is_empty() {
        let creator_text = args
            .creator
            .or_else(|| std::env::var("ROBLOX_CREATOR").ok())
            .or_else(|| resolve_img_creator(&args.project))
            .ok_or("upload: missing creator. Pass --creator user:<id> or group:<id>, set ROBLOX_CREATOR, or set a project Group ID.")?;
        let creator = img_upload::parse_creator(&creator_text)
            .map_err(|e| format!("upload: invalid creator {creator_text:?}: {e}"))?;
        let api_key = resolve_img_api_key(args.api_key_env.as_deref())?;

        let semaphore = Arc::new(tokio::sync::Semaphore::new(args.concurrency));
        let mut tasks = futures::stream::FuturesUnordered::new();
        for job in jobs {
            let permit = semaphore.clone().acquire_owned().await?;
            let api_key = api_key.clone();
            let creator = creator.clone();
            let description = args.description.clone();
            let auth_mode = args.auth.as_upload_mode();
            let wait = !args.no_wait;
            let timeout = Duration::from_secs(args.timeout_seconds);
            let poll = Duration::from_secs(args.poll_seconds);
            let display_name = args.name.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = permit;
                upload_asset_job(
                    job,
                    api_key,
                    auth_mode,
                    creator,
                    description,
                    display_name,
                    wait,
                    timeout,
                    poll,
                )
                .await
            }));
        }

        while let Some(result) = tasks.next().await {
            match result {
                Ok(result) => results.push(result),
                Err(e) => results.push(UploadBulkResult {
                    index: usize::MAX,
                    file: String::new(),
                    display_name: None,
                    asset_type: None,
                    content_type: None,
                    ok: false,
                    asset_id: None,
                    asset_uri: None,
                    operation_path: None,
                    error: Some(format!("task failed: {e}")),
                }),
            }
        }
    }
    results.sort_by_key(|result| result.index);

    if let Some(path) = &args.manifest {
        write_upload_manifest(path, &results)?;
    }

    if args.raw && results.len() == 1 && results[0].ok {
        println!("{}", serde_json::to_string_pretty(&results[0])?);
    } else if args.raw {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print_upload_results(&results);
    }

    let failed = results.iter().filter(|result| !result.ok).count();
    if failed > 0 {
        return Err(format!("upload: {failed} upload(s) failed").into());
    }
    Ok(())
}

#[derive(Clone)]
struct UploadJob {
    index: usize,
    file: PathBuf,
    media: UploadMedia,
}

#[derive(Clone)]
struct UploadMedia {
    asset_type: UploadAssetType,
    content_type: String,
}

#[derive(Clone, Debug)]
struct MonetizationCreateSpec {
    name: String,
    price: u64,
}

#[derive(Clone, Debug)]
struct MonetizationListedItem {
    id: Option<String>,
    name: Option<String>,
    price: Option<u64>,
}

fn monetization_context(
    args: &MonetizationCommonArgs,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let universe_id = args
        .universe_id
        .clone()
        .or_else(|| resolve_monetization_universe_id(args.project.as_deref()).ok())
        .ok_or("monetization: missing universe id. Pass --universe-id, set ROBLOX_UNIVERSE_ID/GAMEID, or set ro-sync.json gameId.")?;
    let api_key =
        resolve_monetization_api_key(args.project.as_deref(), args.api_key_env.as_deref())?;
    Ok((universe_id, api_key))
}

fn resolve_monetization_universe_id(project: Option<&std::path::Path>) -> Result<String, String> {
    for env_name in ["ROBLOX_UNIVERSE_ID", "UNIVERSE_ID", "GAMEID", "GAME_ID"] {
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    for (key, value) in read_project_env_values(project) {
        if matches!(
            key.as_str(),
            "ROBLOX_UNIVERSE_ID" | "UNIVERSE_ID" | "GAMEID" | "GAME_ID"
        ) && !value.trim().is_empty()
        {
            return Ok(value.trim().to_string());
        }
    }

    let root = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    project_config::read_from_disk(&root)
        .map_err(|e| e.to_string())?
        .and_then(|cfg| cfg.game_id)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "no universe id found".to_string())
}

fn resolve_monetization_api_key(
    project: Option<&std::path::Path>,
    preferred_env: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut env_names = Vec::new();
    if let Some(env_name) = preferred_env {
        env_names.push(env_name.to_string());
    }
    for env_name in [
        "ROBLOX_API_KEY",
        "CLOUD_API_KEY",
        "ROBLOX_OPEN_CLOUD_API_KEY",
    ] {
        if !env_names.iter().any(|existing| existing == env_name) {
            env_names.push(env_name.to_string());
        }
    }

    if let Some(env_name) = preferred_env {
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    } else if let Some(value) = find_widget_secret("robloxCloudApiKey") {
        return Ok(value);
    }

    let env_values = read_project_env_values(project);
    if let Some(env_name) = preferred_env {
        if let Some((_, value)) = env_values
            .iter()
            .find(|(key, value)| key == env_name && !value.trim().is_empty())
        {
            return Ok(value.trim().to_string());
        }
    }

    for env_name in &env_names {
        if Some(env_name.as_str()) == preferred_env {
            continue;
        }
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    for env_name in &env_names {
        if Some(env_name.as_str()) == preferred_env {
            continue;
        }
        if let Some((_, value)) = env_values
            .iter()
            .find(|(key, value)| key == env_name && !value.trim().is_empty())
        {
            return Ok(value.trim().to_string());
        }
    }

    if let Some(value) = find_widget_secret("robloxCloudApiKey") {
        return Ok(value);
    }

    Err(format!(
        "monetization: missing Roblox Open Cloud API key. Save one in Ro Sync Settings, set one of {}, or add it to a project env file.",
        env_names.join(", ")
    )
    .into())
}

fn monetization_credential_sources(project: Option<&std::path::Path>) -> Vec<String> {
    let env_values = read_project_env_values(project);
    let mut sources = Vec::new();
    for env_name in [
        "ROBLOX_API_KEY",
        "CLOUD_API_KEY",
        "ROBLOX_OPEN_CLOUD_API_KEY",
        "ROBLOX_UNIVERSE_ID",
        "UNIVERSE_ID",
        "GAMEID",
        "GAME_ID",
    ] {
        if std::env::var(env_name)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
        {
            sources.push(format!("env:{env_name}"));
        }
        if env_values
            .iter()
            .any(|(key, value)| key == env_name && !value.trim().is_empty())
        {
            sources.push(format!("project-env:{env_name}"));
        }
    }
    if find_widget_secret("robloxCloudApiKey").is_some() {
        sources.push("rosync-secret:robloxCloudApiKey".to_string());
    }
    sources
}

fn read_project_env_values(project: Option<&std::path::Path>) -> Vec<(String, String)> {
    let root = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let mut values = Vec::new();
    for file_name in ["info.env", ".env", ".env.local"] {
        let path = root.join(file_name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().trim_start_matches("export ").to_string();
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if !key.is_empty() {
                values.push((key, value));
            }
        }
    }
    values
}

fn monetization_create_specs(
    args: &MonetizationCreateArgs,
) -> Result<Vec<MonetizationCreateSpec>, Box<dyn std::error::Error>> {
    if let Some(name) = &args.name {
        let price = args
            .price
            .ok_or("monetization create: --price is required with --name")?;
        if !args.entries.is_empty() {
            return Err(
                "monetization create: use either entries or --name/--price, not both".into(),
            );
        }
        return Ok(vec![MonetizationCreateSpec {
            name: name.trim().to_string(),
            price,
        }]);
    }
    if args.price.is_some() {
        return Err("monetization create: --price requires --name".into());
    }

    let mut specs = Vec::new();
    for raw in &args.entries {
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            specs.push(parse_monetization_create_entry(entry)?);
        }
    }
    if specs.is_empty() {
        return Err("monetization create: provide at least one entry like `VIP 499 robux` or --name/--price".into());
    }
    Ok(specs)
}

fn parse_monetization_create_entry(
    entry: &str,
) -> Result<MonetizationCreateSpec, Box<dyn std::error::Error>> {
    let mut tokens: Vec<&str> = entry.split_whitespace().collect();
    while tokens
        .last()
        .is_some_and(|token| token.eq_ignore_ascii_case("robux"))
    {
        tokens.pop();
    }
    let Some(price_token) = tokens.pop() else {
        return Err(format!("invalid monetization entry {entry:?}: missing price").into());
    };
    let price = price_token
        .parse::<u64>()
        .map_err(|_| format!("invalid monetization entry {entry:?}: price must be a number"))?;
    let name = tokens.join(" ").trim().to_string();
    if name.is_empty() {
        return Err(format!("invalid monetization entry {entry:?}: missing name").into());
    }
    Ok(MonetizationCreateSpec { name, price })
}

async fn monetization_list_api(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let url = format!("{}/creator", kind.base_url(universe_id));
    let response = reqwest::Client::new()
        .get(url)
        .header("x-api-key", api_key)
        .send()
        .await?;
    monetization_response(response, "list").await
}

async fn monetization_create_one(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
    args: &MonetizationCreateArgs,
    spec: &MonetizationCreateSpec,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut form = reqwest::multipart::Form::new()
        .text("name", spec.name.clone())
        .text("price", spec.price.to_string())
        .text("isForSale", (!args.not_for_sale).to_string());
    if let Some(description) = &args.description {
        form = form.text("description", description.clone());
    }
    if let Some(image) = &args.image {
        form = add_file_part(form, kind.create_image_field(), image)?;
    }
    let response = reqwest::Client::new()
        .post(kind.base_url(universe_id))
        .header("x-api-key", api_key)
        .multipart(form)
        .send()
        .await?;
    monetization_response(response, "create").await
}

async fn monetization_update_one<F>(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
    id: &str,
    build_form: F,
) -> Result<serde_json::Value, Box<dyn std::error::Error>>
where
    F: FnOnce(reqwest::multipart::Form) -> reqwest::multipart::Form,
{
    let form = build_form(reqwest::multipart::Form::new());
    let response = reqwest::Client::new()
        .patch(format!("{}/{}", kind.base_url(universe_id), id))
        .header("x-api-key", api_key)
        .multipart(form)
        .send()
        .await?;
    monetization_response(response, "update").await
}

async fn monetization_update_image(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
    id: &str,
    file: &std::path::Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let form = add_file_part(
        reqwest::multipart::Form::new(),
        kind.update_image_field(),
        file,
    )?;
    let response = reqwest::Client::new()
        .patch(format!("{}/{}", kind.base_url(universe_id), id))
        .header("x-api-key", api_key)
        .multipart(form)
        .send()
        .await?;
    monetization_response(response, "image").await
}

fn add_file_part(
    form: reqwest::multipart::Form,
    field: &'static str,
    path: &std::path::Path,
) -> Result<reqwest::multipart::Form, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("monetization: read image {}: {e}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image")
        .to_string();
    let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
    Ok(form.part(field, part))
}

async fn monetization_response(
    response: reqwest::Response,
    label: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let status = response.status();
    let text = response.text().await?;
    if status.is_success() {
        if text.trim().is_empty() {
            return Ok(serde_json::json!({ "status": status.as_u16() }));
        }
        return serde_json::from_str(&text).map_err(|e| {
            format!("monetization {label}: expected JSON response, got {text:?}: {e}").into()
        });
    }
    let body = if text.trim().is_empty() {
        "<empty response>".to_string()
    } else {
        text
    };
    Err(format!("monetization {label}: Roblox API returned {status}: {body}").into())
}

async fn resolve_monetization_asset_id(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
    id: Option<String>,
    name: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(id) = id {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    let name = name.ok_or("monetization: pass --id or --name")?;
    let key = normalize_monetization_name(&name);
    let list = monetization_list_api(kind, universe_id, api_key).await?;
    let mut matches = monetization_items_from_response(kind, &list)
        .into_iter()
        .filter(|item| {
            item.name
                .as_deref()
                .map(normalize_monetization_name)
                .is_some_and(|item_key| item_key == key)
        })
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    if matches.len() > 1 {
        return Err(format!(
            "monetization: name {name:?} matched multiple {} entries; pass --id",
            kind.label()
        )
        .into());
    }
    matches
        .pop()
        .and_then(|item| item.id)
        .ok_or_else(|| format!("monetization: no {} named {name:?} found", kind.label()).into())
}

fn monetization_items_from_response(
    kind: MonetizationKind,
    value: &serde_json::Value,
) -> Vec<MonetizationListedItem> {
    let mut out = Vec::new();
    collect_monetization_items(kind, value, &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.id == b.id && a.name == b.name);
    out
}

fn collect_monetization_items(
    kind: MonetizationKind,
    value: &serde_json::Value,
    out: &mut Vec<MonetizationListedItem>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_monetization_items(kind, value, out);
            }
        }
        serde_json::Value::Object(map) => {
            let id = monetization_id_from_value(kind, value);
            let name = map
                .get("name")
                .or_else(|| map.get("displayName"))
                .and_then(json_scalar_to_string);
            if id.is_some() || name.is_some() {
                let price = map
                    .get("price")
                    .or_else(|| map.get("priceInRobux"))
                    .and_then(json_u64);
                out.push(MonetizationListedItem { id, name, price });
            }
            for child in map.values() {
                collect_monetization_items(kind, child, out);
            }
        }
        _ => {}
    }
}

fn monetization_id_from_value(kind: MonetizationKind, value: &serde_json::Value) -> Option<String> {
    let map = value.as_object()?;
    for key in [kind.id_field(), "id", "assetId"] {
        if let Some(id) = map.get(key).and_then(json_scalar_to_string) {
            if !id.trim().is_empty() {
                return Some(id);
            }
        }
    }
    None
}

fn json_scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(value) => value.as_u64(),
        serde_json::Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn normalize_monetization_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_supported_image_path(path: &std::path::Path) -> bool {
    matches!(
        upload_extension(path).as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "tga"
    )
}

fn discover_monetization_files(
    project: &std::path::Path,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    discover_monetization_files_inner(project, project, &mut out)?;
    out.sort_by_key(|value| value["path"].as_str().map(str::to_string));
    Ok(out)
}

fn discover_monetization_files_inner(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<serde_json::Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = std::fs::read_dir(dir)
        .map_err(|e| format!("monetization discover: read {}: {e}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| {
            matches!(
                name,
                ".git"
                    | "node_modules"
                    | "target"
                    | "tools"
                    | "dist"
                    | "build"
                    | ".cursor"
                    | ".vscode"
                    | ".DS_Store"
            )
        }) {
            continue;
        }
        if path.is_dir() {
            discover_monetization_files_inner(root, &path, out)?;
            continue;
        }
        let Some(ext) = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
        else {
            continue;
        };
        if !matches!(ext.as_str(), "luau" | "lua" | "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let matches = [
            "GamePass",
            "Gamepass",
            "DeveloperProduct",
            "ProductId",
            "GamePassId",
            "MarketplaceService",
            "ProcessReceipt",
            "PromptGamePassPurchase",
        ]
        .iter()
        .filter(|needle| text.contains(**needle))
        .copied()
        .collect::<Vec<_>>();
        if matches.is_empty() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        out.push(serde_json::json!({
            "path": rel,
            "matches": matches,
        }));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadBulkResult {
    index: usize,
    file: String,
    display_name: Option<String>,
    asset_type: Option<String>,
    content_type: Option<String>,
    ok: bool,
    asset_id: Option<String>,
    asset_uri: Option<String>,
    operation_path: Option<String>,
    error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn upload_asset_job(
    job: UploadJob,
    api_key: String,
    auth_mode: img_upload::AuthMode,
    creator: img_upload::Creator,
    description: String,
    display_name: Option<String>,
    wait: bool,
    timeout: Duration,
    poll: Duration,
) -> UploadBulkResult {
    let display_name = display_name.unwrap_or_else(|| img_upload::default_display_name(&job.file));
    let file = job.file.display().to_string();
    let asset_type = job.media.asset_type.as_cloud_str().to_string();
    let content_type = job.media.content_type;
    match img_upload::upload_asset(img_upload::AssetUploadRequest {
        file: job.file,
        api_key,
        auth_mode,
        creator,
        asset_type: asset_type.clone(),
        content_type: content_type.clone(),
        display_name: display_name.clone(),
        description,
        wait,
        timeout,
        poll,
    })
    .await
    {
        Ok(outcome) => UploadBulkResult {
            index: job.index,
            file,
            display_name: Some(display_name),
            asset_type: Some(asset_type),
            content_type: Some(content_type),
            ok: true,
            asset_id: outcome.asset_id,
            asset_uri: outcome.asset_uri,
            operation_path: outcome.operation_path,
            error: None,
        },
        Err(e) => UploadBulkResult {
            index: job.index,
            file,
            display_name: Some(display_name),
            asset_type: Some(asset_type),
            content_type: Some(content_type),
            ok: false,
            asset_id: None,
            asset_uri: None,
            operation_path: None,
            error: Some(e.to_string()),
        },
    }
}

fn collect_upload_jobs(
    inputs: &[PathBuf],
    recursive: bool,
    asset_type: Option<UploadAssetType>,
    content_type: Option<&str>,
    failures: &mut Vec<UploadBulkResult>,
) -> Result<Vec<UploadJob>, Box<dyn std::error::Error>> {
    let mut jobs = Vec::new();
    let mut index = 0;
    for input in inputs {
        collect_upload_input(
            input,
            recursive,
            true,
            asset_type,
            content_type,
            &mut index,
            &mut jobs,
            failures,
        )?;
    }
    jobs.sort_by(|a, b| a.file.cmp(&b.file));
    for (idx, job) in jobs.iter_mut().enumerate() {
        job.index = idx;
    }
    for (offset, failure) in failures.iter_mut().enumerate() {
        failure.index = jobs.len() + offset;
    }
    Ok(jobs)
}

#[allow(clippy::too_many_arguments)]
fn collect_upload_input(
    input: &std::path::Path,
    recursive: bool,
    explicit: bool,
    asset_type: Option<UploadAssetType>,
    content_type: Option<&str>,
    index: &mut usize,
    jobs: &mut Vec<UploadJob>,
    failures: &mut Vec<UploadBulkResult>,
) -> Result<(), Box<dyn std::error::Error>> {
    if input.is_file() {
        match resolve_upload_media(input, asset_type, content_type, explicit) {
            Ok(media) => {
                jobs.push(UploadJob {
                    index: *index,
                    file: input.to_path_buf(),
                    media,
                });
                *index += 1;
            }
            Err(e) if explicit => {
                failures.push(UploadBulkResult {
                    index: *index,
                    file: input.display().to_string(),
                    display_name: None,
                    asset_type: asset_type.map(|asset_type| asset_type.as_cloud_str().to_string()),
                    content_type: content_type.map(|content_type| content_type.to_string()),
                    ok: false,
                    asset_id: None,
                    asset_uri: None,
                    operation_path: None,
                    error: Some(e),
                });
                *index += 1;
            }
            Err(_) => {}
        }
        return Ok(());
    }
    if input.is_dir() {
        let mut entries = std::fs::read_dir(input)
            .map_err(|e| format!("upload: read directory {}: {e}", input.display()))?
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                if recursive {
                    collect_upload_input(
                        &path,
                        recursive,
                        false,
                        asset_type,
                        content_type,
                        index,
                        jobs,
                        failures,
                    )?;
                }
            } else {
                collect_upload_input(
                    &path,
                    recursive,
                    false,
                    asset_type,
                    content_type,
                    index,
                    jobs,
                    failures,
                )?;
            }
        }
        return Ok(());
    }
    failures.push(UploadBulkResult {
        index: *index,
        file: input.display().to_string(),
        display_name: None,
        asset_type: asset_type.map(|asset_type| asset_type.as_cloud_str().to_string()),
        content_type: content_type.map(|content_type| content_type.to_string()),
        ok: false,
        asset_id: None,
        asset_uri: None,
        operation_path: None,
        error: Some("path does not exist".to_string()),
    });
    *index += 1;
    Ok(())
}

fn resolve_upload_media(
    path: &std::path::Path,
    requested_asset_type: Option<UploadAssetType>,
    content_type_override: Option<&str>,
    explicit: bool,
) -> Result<UploadMedia, String> {
    let inferred = infer_upload_media(path);
    let asset_type = match requested_asset_type {
        Some(asset_type) => asset_type,
        None => inferred
            .as_ref()
            .map(|media| media.asset_type)
            .ok_or_else(|| {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("asset");
                format!(
                    "unsupported or ambiguous asset type for {name}; pass --asset-type and optionally --content-type"
                )
            })?,
    };
    let content_type = match content_type_override {
        Some(content_type) => content_type.trim().to_string(),
        None => match (requested_asset_type, inferred) {
            (None, Some(media)) => media.content_type,
            _ => content_type_for_asset_type(path, asset_type, explicit)?.to_string(),
        },
    };
    Ok(UploadMedia {
        asset_type,
        content_type,
    })
}

fn infer_upload_media(path: &std::path::Path) -> Option<UploadMedia> {
    let ext = upload_extension(path);
    let (asset_type, content_type) = match ext.as_str() {
        "png" => (UploadAssetType::Image, "image/png"),
        "jpg" | "jpeg" => (UploadAssetType::Image, "image/jpeg"),
        "bmp" => (UploadAssetType::Image, "image/bmp"),
        "tga" => (UploadAssetType::Image, "image/tga"),
        "mp3" => (UploadAssetType::Audio, "audio/mpeg"),
        "ogg" => (UploadAssetType::Audio, "audio/ogg"),
        "wav" => (UploadAssetType::Audio, "audio/wav"),
        "flac" => (UploadAssetType::Audio, "audio/flac"),
        "fbx" => (UploadAssetType::Model, "model/fbx"),
        "gltf" => (UploadAssetType::Model, "model/gltf+json"),
        "glb" => (UploadAssetType::Model, "model/gltf-binary"),
        "mesh" | "rbxmesh" => (UploadAssetType::Mesh, "model/x-file-mesh-data"),
        "mp4" => (UploadAssetType::Video, "video/mp4"),
        "mov" => (UploadAssetType::Video, "video/mov"),
        _ => return None,
    };
    Some(UploadMedia {
        asset_type,
        content_type: content_type.to_string(),
    })
}

fn content_type_for_asset_type(
    path: &std::path::Path,
    asset_type: UploadAssetType,
    explicit: bool,
) -> Result<&'static str, String> {
    let ext = upload_extension(path);
    match asset_type {
        UploadAssetType::Animation => match ext.as_str() {
            "rbxm" | "rbxmx" => Ok("model/x-rbxm"),
            _ => Err(format!(
                "unsupported file type for Animation; expected {}",
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Audio => match ext.as_str() {
            "mp3" => Ok("audio/mpeg"),
            "ogg" => Ok("audio/ogg"),
            "wav" => Ok("audio/wav"),
            "flac" => Ok("audio/flac"),
            _ => Err(format!(
                "unsupported file type for Audio; expected {}",
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Decal | UploadAssetType::Image => match ext.as_str() {
            "png" => Ok("image/png"),
            "jpg" | "jpeg" => Ok("image/jpeg"),
            "bmp" => Ok("image/bmp"),
            "tga" => Ok("image/tga"),
            _ => Err(format!(
                "unsupported file type for {}; expected {}",
                asset_type.as_cloud_str(),
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Mesh => match ext.as_str() {
            "mesh" | "rbxmesh" => Ok("model/x-file-mesh-data"),
            _ if explicit => Ok("model/x-file-mesh-data"),
            _ => Err(format!(
                "unsupported file type for Mesh; expected {}",
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Model => match ext.as_str() {
            "fbx" => Ok("model/fbx"),
            "gltf" => Ok("model/gltf+json"),
            "glb" => Ok("model/gltf-binary"),
            "rbxm" | "rbxmx" => Ok("model/x-rbxm"),
            _ => Err(format!(
                "unsupported file type for Model; expected {}",
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Video => match ext.as_str() {
            "mp4" => Ok("video/mp4"),
            "mov" => Ok("video/mov"),
            _ => Err(format!(
                "unsupported file type for Video; expected {}",
                expected_extensions(asset_type)
            )),
        },
    }
}

fn expected_extensions(asset_type: UploadAssetType) -> &'static str {
    match asset_type {
        UploadAssetType::Animation => ".rbxm or .rbxmx",
        UploadAssetType::Audio => ".mp3, .ogg, .wav, or .flac",
        UploadAssetType::Decal | UploadAssetType::Image => ".png, .jpg, .jpeg, .bmp, or .tga",
        UploadAssetType::Mesh => ".mesh or .rbxmesh, or pass --content-type model/x-file-mesh-data",
        UploadAssetType::Model => ".fbx, .gltf, .glb, .rbxm, or .rbxmx",
        UploadAssetType::Video => ".mp4 or .mov",
    }
}

fn upload_extension(path: &std::path::Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn write_upload_manifest(
    path: &std::path::Path,
    results: &[UploadBulkResult],
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(results)? + "\n")?;
    Ok(())
}

fn print_upload_results(results: &[UploadBulkResult]) {
    for result in results {
        if result.ok {
            let uri = result
                .asset_uri
                .as_deref()
                .or(result.operation_path.as_deref())
                .unwrap_or("uploaded");
            let asset_type = result.asset_type.as_deref().unwrap_or("Asset");
            println!(
                "uploaded  {:40} {:9} {}",
                truncate_middle(&result.file, 40),
                asset_type,
                uri
            );
        } else {
            println!(
                "failed    {:40} {}",
                truncate_middle(&result.file, 40),
                result.error.as_deref().unwrap_or("unknown error")
            );
        }
    }
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let head_len = (max_chars - 3) / 2;
    let tail_len = max_chars - 3 - head_len;
    let head: String = value.chars().take(head_len).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

async fn run_lint(args: LintArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = match args.project {
        Some(p) => p,
        None => std::env::current_dir().map_err(|e| format!("lint: current directory: {e}"))?,
    };
    let project = lifecycle::canonical_project(&project)
        .map_err(|e| format!("lint: validate project {}: {e}", project.display()))?;

    if args.scope_only && args.paths.is_empty() {
        return Err("lint: --scope-only requires at least one --path".into());
    }
    if extra_args_use_plain_formatter(&args.extra_args) {
        return Err(
            "lint: --formatter=plain does not preserve analyzer failure exit codes; use the default or GNU formatter"
                .into(),
        );
    }

    let explicit_targets = !args.paths.is_empty();
    let mut targets = if args.paths.is_empty() {
        vec![project.clone()]
    } else {
        args.paths
            .iter()
            .map(|path| lint_target_path(&project, path))
            .collect()
    };
    targets = targets
        .into_iter()
        .map(|target| validate_lint_target(&project, &target))
        .collect::<Result<Vec<_>, _>>()?;

    let compile_report = run_lint_compiler(
        &project,
        &targets,
        explicit_targets,
        args.compile,
        args.luau_compile.clone(),
        args.no_vendor_ignores,
        &args.ignores,
    )?;
    report_lint_compiler(&compile_report, args.raw, args.summary);

    let luau_lsp = resolve_luau_lsp(args.luau_lsp);
    warn_if_old_luau_lsp(&luau_lsp, &project);
    let user_sourcemap = extra_args_include_sourcemap(&args.extra_args);
    if (args.no_sourcemap || user_sourcemap)
        && matches!(
            args.data_model,
            LintDataModelMode::Studio | LintDataModelMode::Filesystem
        )
    {
        return Err(format!(
            "lint: --data-model {} requires Ro Sync's generated sourcemap; remove --no-sourcemap/custom --sourcemap",
            args.data_model.as_str()
        )
        .into());
    }

    let (sourcemap, mut coverage) = if args.no_sourcemap || user_sourcemap {
        (
            None,
            LintDataModelCoverage::external(args.data_model, user_sourcemap),
        )
    } else {
        let (map, coverage) = prepare_lint_sourcemap(&project, args.port, args.data_model).await?;
        (Some(write_temp_sourcemap_value(&map)?), coverage)
    };
    let definitions = if extra_args_include_roblox_definitions(&args.extra_args) {
        None
    } else {
        find_luau_definitions(&project)
            .map_err(|error| format!("lint: locate Roblox definitions: {error}"))?
    };
    let strict_settings = if coverage.strict
        && !extra_args_include_settings(&args.extra_args)
        && !extra_args_disable_strict_datamodel(&args.extra_args)
    {
        Some(write_temp_lint_settings()?)
    } else {
        if coverage.strict && extra_args_include_settings(&args.extra_args) {
            coverage.note = Some(
                "A caller-supplied --settings file controls strict DataModel diagnostics."
                    .to_string(),
            );
            coverage.strict = false;
        }
        if extra_args_disable_strict_datamodel(&args.extra_args) {
            coverage.note = Some(
                "Strict DataModel diagnostics were disabled by --no-strict-dm-types.".to_string(),
            );
            coverage.strict = false;
        }
        None
    };

    report_lint_coverage(&coverage, args.raw);
    let mut cmd = std::process::Command::new(&luau_lsp);
    cmd.arg("analyze");
    if !extra_args_include_platform(&args.extra_args) {
        cmd.arg("--platform=roblox");
    }
    if let Some(path) = &sourcemap {
        cmd.arg(format!("--sourcemap={}", path.display()));
    }
    if let Some(path) = &strict_settings {
        cmd.arg(format!("--settings={}", path.display()));
    }
    if let Some(path) = &definitions {
        cmd.arg(format!("--definitions=@roblox={}", path.display()));
    }

    // An explicit --path is an explicit ownership boundary and must never be
    // silently swallowed by the default vendor filters.
    if !args.no_vendor_ignores && !explicit_targets {
        for pattern in DEFAULT_LINT_VENDOR_IGNORES {
            cmd.arg(format!("--ignore={pattern}"));
        }
    }
    for pattern in &args.ignores {
        cmd.arg(format!("--ignore={pattern}"));
    }

    cmd.args(&args.extra_args)
        .args(&targets)
        .current_dir(&project)
        .stdin(Stdio::null());

    let capture_output = args.scope_only || args.summary || args.raw;
    let (status, effective_success) = if capture_output {
        let output = match cmd.output() {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cleanup_temp_file(&sourcemap);
                cleanup_temp_file(&strict_settings);
                print_luau_lsp_missing(&luau_lsp);
                std::process::exit(127);
            }
            Err(e) => {
                cleanup_temp_file(&sourcemap);
                cleanup_temp_file(&strict_settings);
                return Err(
                    format!("lint: failed to run {}: {e}", luau_lsp.to_string_lossy()).into(),
                );
            }
        };
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        let all_diagnostics = lint_diagnostics(&project, &combined);
        let rendered = if args.scope_only {
            filter_lint_output_to_targets(&project, &targets, &combined)
        } else {
            combined
        };
        let shown_diagnostics = lint_diagnostics(&project, &rendered);
        let suppressed = all_diagnostics
            .len()
            .saturating_sub(shown_diagnostics.len());
        let retained_unparsed_failure =
            args.scope_only && lint_has_unparsed_failure(&project, &rendered);
        let effective_success = lint_analyzer_effective_success(
            args.scope_only,
            output.status.success(),
            all_diagnostics.len(),
            shown_diagnostics.len(),
            retained_unparsed_failure,
        );
        if args.raw {
            print_lint_json(
                &project,
                &coverage,
                &compile_report,
                LintAnalyzerJson {
                    output: &rendered,
                    diagnostics: &shown_diagnostics,
                    suppressed,
                    exit_code: output.status.code(),
                    ok: effective_success && compile_report.is_success(),
                },
            )?;
        } else {
            print!("{rendered}");
            if args.summary {
                print_lint_summary(&project, &rendered, &compile_report, suppressed);
            }
        }
        (output.status, effective_success)
    } else {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        let status = match cmd.status() {
            Ok(status) => status,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cleanup_temp_file(&sourcemap);
                cleanup_temp_file(&strict_settings);
                print_luau_lsp_missing(&luau_lsp);
                std::process::exit(127);
            }
            Err(e) => {
                cleanup_temp_file(&sourcemap);
                cleanup_temp_file(&strict_settings);
                return Err(
                    format!("lint: failed to run {}: {e}", luau_lsp.to_string_lossy()).into(),
                );
            }
        };
        let success = status.success();
        (status, success)
    };

    cleanup_temp_file(&sourcemap);
    cleanup_temp_file(&strict_settings);
    if !effective_success || !compile_report.is_success() {
        let exit_code = if effective_success {
            compile_report.exit_code().unwrap_or(1)
        } else {
            status.code().filter(|code| *code != 0).unwrap_or(1)
        };
        std::process::exit(exit_code);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LintDataModelCoverage {
    requested: String,
    source: String,
    strict: bool,
    live_nodes: Option<usize>,
    note: Option<String>,
}

impl LintDataModelCoverage {
    fn external(mode: LintDataModelMode, user_sourcemap: bool) -> Self {
        Self {
            requested: mode.as_str().to_string(),
            source: if user_sourcemap {
                "caller-supplied".to_string()
            } else {
                "disabled".to_string()
            },
            strict: false,
            live_nodes: None,
            note: Some(if user_sourcemap {
                "Ro Sync cannot determine strict DataModel coverage for a caller-supplied sourcemap."
                    .to_string()
            } else {
                "DataModel sourcemap generation was disabled.".to_string()
            }),
        }
    }
}

const LINT_COMPILE_OPTIMIZATIONS: &[u8] = &[0, 1, 2];
const LINT_COMPILE_BATCH_MAX_FILES: usize = 128;
const LINT_COMPILE_BATCH_MAX_ARG_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LintCompileReport {
    requested: String,
    status: String,
    executable: Option<String>,
    source_files: usize,
    optimizations_checked: Vec<u8>,
    failures: Vec<LintCompileFailure>,
    note: Option<String>,
}

impl LintCompileReport {
    fn disabled(mode: LintCompileMode) -> Self {
        Self {
            requested: mode.as_str().to_string(),
            status: "disabled".to_string(),
            executable: None,
            source_files: 0,
            optimizations_checked: Vec::new(),
            failures: Vec::new(),
            note: None,
        }
    }

    fn skipped(mode: LintCompileMode, executable: Option<&OsString>, note: String) -> Self {
        Self {
            requested: mode.as_str().to_string(),
            status: "skipped".to_string(),
            executable: executable.map(|value| value.to_string_lossy().into_owned()),
            source_files: 0,
            optimizations_checked: Vec::new(),
            failures: Vec::new(),
            note: Some(note),
        }
    }

    fn is_success(&self) -> bool {
        self.status != "failed"
    }

    fn exit_code(&self) -> Option<i32> {
        self.failures.iter().find_map(|failure| failure.exit_code)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LintCompileFailure {
    optimization: u8,
    batch: usize,
    exit_code: Option<i32>,
    output: String,
}

fn run_lint_compiler(
    project: &std::path::Path,
    targets: &[PathBuf],
    explicit_targets: bool,
    mode: LintCompileMode,
    explicit_executable: Option<PathBuf>,
    no_vendor_ignores: bool,
    ignores: &[String],
) -> Result<LintCompileReport, Box<dyn std::error::Error>> {
    if mode == LintCompileMode::Off {
        return Ok(LintCompileReport::disabled(mode));
    }

    let executable = resolve_luau_compile(explicit_executable);
    let Some(executable) = executable else {
        let note = "luau-compile was not found; install the Luau compiler, set ROSYNC_LUAU_COMPILE, or pass --luau-compile"
            .to_string();
        if mode == LintCompileMode::Required {
            return Err(format!("lint: {note}").into());
        }
        return Ok(LintCompileReport::skipped(mode, None, note));
    };

    match std::process::Command::new(&executable)
        .arg("--help")
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            return Err(format!(
                "lint: {} did not accept --help (exit {}); pass a valid luau-compile executable",
                executable.to_string_lossy(),
                status.code().unwrap_or(1)
            )
            .into());
        }
        Err(error) => {
            let note = format!(
                "could not run luau-compile at {}: {error}",
                executable.to_string_lossy()
            );
            if mode == LintCompileMode::Required {
                return Err(format!("lint: {note}").into());
            }
            return Ok(LintCompileReport::skipped(mode, Some(&executable), note));
        }
    }

    let use_default_ignores = !no_vendor_ignores && !explicit_targets;
    let sources = collect_lint_compile_sources(project, targets, use_default_ignores, ignores)?;
    let mut report = LintCompileReport {
        requested: mode.as_str().to_string(),
        status: "passed".to_string(),
        executable: Some(executable.to_string_lossy().into_owned()),
        source_files: sources.len(),
        optimizations_checked: Vec::new(),
        failures: Vec::new(),
        note: None,
    };
    if sources.is_empty() {
        report.note = Some("No executable .lua or .luau source files were in scope.".to_string());
        return Ok(report);
    }

    let batches = lint_compile_batches(project, &sources);
    for &optimization in LINT_COMPILE_OPTIMIZATIONS {
        report.optimizations_checked.push(optimization);
        for (batch_index, batch) in batches.iter().enumerate() {
            let output = std::process::Command::new(&executable)
                .arg("--null")
                .arg(format!("-O{optimization}"))
                .args(batch)
                .current_dir(project)
                .stdin(Stdio::null())
                .output()
                .map_err(|error| {
                    format!(
                        "lint: failed to run {} during bytecode compilation: {error}",
                        executable.to_string_lossy()
                    )
                })?;
            if output.status.success() {
                continue;
            }
            report.status = "failed".to_string();
            report.failures.push(LintCompileFailure {
                optimization,
                batch: batch_index + 1,
                exit_code: output.status.code(),
                output: lint_compile_failure_output(&output),
            });
        }
    }
    Ok(report)
}

fn report_lint_compiler(report: &LintCompileReport, raw: bool, summary: bool) {
    if raw {
        return;
    }
    match report.status.as_str() {
        "skipped" => {
            if let Some(note) = &report.note {
                eprintln!("[rosync lint] bytecode check skipped: {note}");
            }
        }
        "failed" => {
            for failure in &report.failures {
                eprintln!(
                    "[rosync lint] bytecode compilation failed at -O{} (batch {}):",
                    failure.optimization, failure.batch
                );
                eprint!("{}", failure.output);
                if !failure.output.ends_with('\n') {
                    eprintln!();
                }
            }
        }
        "passed" if summary => {
            let modes = report
                .optimizations_checked
                .iter()
                .map(|optimization| format!("O{optimization}"))
                .collect::<Vec<_>>()
                .join("/");
            if modes.is_empty() {
                eprintln!(
                    "[rosync lint] bytecode: {}",
                    report.note.as_deref().unwrap_or("nothing to compile")
                );
            } else {
                eprintln!(
                    "[rosync lint] bytecode: {} source file{} passed {modes}",
                    report.source_files,
                    plural_s(report.source_files)
                );
            }
        }
        _ => {}
    }
}

fn collect_lint_compile_sources(
    project: &std::path::Path,
    targets: &[PathBuf],
    use_default_ignores: bool,
    ignores: &[String],
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut sources = Vec::new();
    for target in targets {
        let metadata = crate::fs_safety::require_metadata_no_follow(target)
            .map_err(|error| format!("lint: inspect {}: {error}", target.display()))?;
        if metadata.is_file() {
            if is_lint_compile_source(project, target, true)
                && !lint_compile_path_ignored(project, target, use_default_ignores, ignores)
            {
                sources.push(validate_lint_target(project, target)?);
            }
            continue;
        }
        if !metadata.is_dir() {
            return Err(format!(
                "lint: target is not a regular file or directory: {}",
                target.display()
            )
            .into());
        }
        collect_lint_compile_directory(
            project,
            target,
            use_default_ignores,
            ignores,
            &mut sources,
        )?;
    }
    sources.sort();
    sources.dedup();
    Ok(sources)
}

fn collect_lint_compile_directory(
    project: &std::path::Path,
    directory: &std::path::Path,
    use_default_ignores: bool,
    ignores: &[String],
    sources: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut pending = vec![directory.to_path_buf()];
    let mut visited = 0usize;
    while let Some(current) = pending.pop() {
        let relative = current.strip_prefix(project).unwrap_or(&current);
        let first = relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str());
        let index = if current == project {
            crate::fs_safety::PortableDirectoryIndex::read_project_root(&current)
        } else if first.is_some_and(|service| crate::fs_safety::SYNCED_SERVICES.contains(&service))
        {
            crate::fs_safety::PortableDirectoryIndex::read(&current)
        } else {
            crate::fs_safety::PortableDirectoryIndex::read_raw(&current)
        }
        .map_err(|error| format!("lint: scan {}: {error}", current.display()))?;
        visited = visited.saturating_add(index.entries().len());
        if visited > crate::fs_safety::MAX_SERVICE_TREE_NODES {
            return Err(format!(
                "lint: source scan exceeds the {} node safety limit",
                crate::fs_safety::MAX_SERVICE_TREE_NODES
            )
            .into());
        }

        for entry in index.entries().iter().rev() {
            let path = &entry.path;
            if lint_compile_path_ignored(project, path, use_default_ignores, ignores) {
                continue;
            }
            match entry.kind {
                crate::fs_safety::SafeEntryKind::Directory => pending.push(path.clone()),
                crate::fs_safety::SafeEntryKind::File
                    if is_lint_compile_source(project, path, false) =>
                {
                    sources.push(validate_lint_target(project, path)?);
                }
                crate::fs_safety::SafeEntryKind::File => {}
            }
        }
    }
    Ok(())
}

fn is_lint_compile_source(
    project: &std::path::Path,
    path: &std::path::Path,
    explicit_file: bool,
) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if fs_map::classify_script_file(name).is_none() {
        return false;
    }
    if name.ends_with(".d.luau") || name.ends_with(".d.lua") {
        // Declaration files outside the mirrored DataModel are analyzer inputs,
        // not executable chunks. Inside a synced service, however, `Foo.d.luau`
        // is a perfectly valid ModuleScript named `Foo.d`; an explicit file is
        // likewise an unambiguous request to run the compiler.
        if explicit_file {
            return true;
        }
        let relative = path.strip_prefix(project).unwrap_or(path);
        return relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .is_some_and(|service| snapshot::SYNCED_SERVICES.contains(&service));
    }
    true
}

fn lint_compile_path_ignored(
    project: &std::path::Path,
    path: &std::path::Path,
    use_default_ignores: bool,
    ignores: &[String],
) -> bool {
    let relative = path.strip_prefix(project).unwrap_or(path);
    let relative = relative.to_string_lossy().replace('\\', "/");
    let absolute = path.to_string_lossy().replace('\\', "/");
    let matches = |pattern: &str| {
        lint_glob_matches(pattern, &relative)
            || lint_glob_matches(pattern, &absolute)
            || (!pattern.contains('/')
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| lint_glob_matches(pattern, name)))
    };
    (use_default_ignores
        && DEFAULT_LINT_VENDOR_IGNORES
            .iter()
            .any(|pattern| matches(pattern)))
        || ignores.iter().any(|pattern| matches(pattern))
}

fn lint_glob_matches(pattern: &str, value: &str) -> bool {
    fn recurse(
        pattern: &[u8],
        value: &[u8],
        pattern_index: usize,
        value_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, value_index)) {
            return *result;
        }
        let result = if pattern_index == pattern.len() {
            value_index == value.len()
        } else if pattern[pattern_index..].starts_with(b"**/") {
            recurse(pattern, value, pattern_index + 3, value_index, memo)
                || (value_index < value.len()
                    && recurse(pattern, value, pattern_index, value_index + 1, memo))
        } else if pattern[pattern_index..].starts_with(b"**") {
            recurse(pattern, value, pattern_index + 2, value_index, memo)
                || (value_index < value.len()
                    && recurse(pattern, value, pattern_index, value_index + 1, memo))
        } else if pattern[pattern_index] == b'*' {
            recurse(pattern, value, pattern_index + 1, value_index, memo)
                || (value_index < value.len()
                    && value[value_index] != b'/'
                    && recurse(pattern, value, pattern_index, value_index + 1, memo))
        } else if pattern[pattern_index] == b'?' {
            value_index < value.len()
                && value[value_index] != b'/'
                && recurse(pattern, value, pattern_index + 1, value_index + 1, memo)
        } else {
            value_index < value.len()
                && pattern[pattern_index] == value[value_index]
                && recurse(pattern, value, pattern_index + 1, value_index + 1, memo)
        };
        memo.insert((pattern_index, value_index), result);
        result
    }

    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    recurse(
        pattern.as_bytes(),
        value.as_bytes(),
        0,
        0,
        &mut HashMap::new(),
    )
}

fn lint_compile_batches(project: &std::path::Path, sources: &[PathBuf]) -> Vec<Vec<OsString>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut argument_bytes = 0usize;
    for source in sources {
        let argument_path = source.strip_prefix(project).unwrap_or(source);
        let argument = argument_path.as_os_str().to_os_string();
        let bytes = argument.to_string_lossy().len() + 1;
        if !batch.is_empty()
            && (batch.len() >= LINT_COMPILE_BATCH_MAX_FILES
                || argument_bytes.saturating_add(bytes) > LINT_COMPILE_BATCH_MAX_ARG_BYTES)
        {
            batches.push(std::mem::take(&mut batch));
            argument_bytes = 0;
        }
        argument_bytes = argument_bytes.saturating_add(bytes);
        batch.push(argument);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

fn lint_compile_failure_output(output: &std::process::Output) -> String {
    let mut rendered = String::new();
    rendered.push_str(&String::from_utf8_lossy(&output.stderr));
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if !line.starts_with("Compiled ") {
            rendered.push_str(line);
            rendered.push('\n');
        }
    }
    if rendered.trim().is_empty() {
        rendered = format!(
            "luau-compile exited with status {} without an error message\n",
            output.status.code().unwrap_or(1)
        );
    }
    rendered
}

async fn prepare_lint_sourcemap(
    project: &std::path::Path,
    port: u16,
    mode: LintDataModelMode,
) -> Result<(serde_json::Value, LintDataModelCoverage), Box<dyn std::error::Error>> {
    let mut map = sourcemap::generate(project)?;
    let mut coverage = LintDataModelCoverage {
        requested: mode.as_str().to_string(),
        source: "filesystem".to_string(),
        strict: mode == LintDataModelMode::Filesystem,
        live_nodes: None,
        note: None,
    };

    match mode {
        LintDataModelMode::Loose => {
            coverage.note = Some(
                "DataModel-derived expressions remain gradual/any in diagnostics.".to_string(),
            );
            return Ok((map, coverage));
        }
        LintDataModelMode::Filesystem => {
            coverage.note = Some(
                "Strict filesystem types can report unknown children for Studio-only instances."
                    .to_string(),
            );
            return Ok((map, coverage));
        }
        LintDataModelMode::Auto | LintDataModelMode::Studio => {}
    }

    let hello = fetch_daemon_hello(port).ok();
    let matching_daemon = hello
        .as_ref()
        .is_some_and(|hello| daemon_hello_matches_project(hello, project));
    let plugin_connected = hello
        .as_ref()
        .and_then(|hello| hello.get("pluginConnected"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    if matching_daemon && plugin_connected {
        match live_tree(port, "lint").await {
            Ok(tree) => {
                if diff::has_truncated_tree(&tree) {
                    return Err("lint: Studio returned a truncated DataModel tree".into());
                }
                let live_nodes = count_json_tree_nodes(&tree);
                sourcemap::merge_live_tree(&mut map, &tree);
                coverage.source = "studio".to_string();
                coverage.strict = true;
                coverage.live_nodes = Some(live_nodes);
                coverage.note = Some(
                    "Strict DataModel diagnostics use live Studio classes plus disk file mappings."
                        .to_string(),
                );
                return Ok((map, coverage));
            }
            Err(error) if mode == LintDataModelMode::Studio => {
                return Err(format!("lint: live Studio DataModel request failed: {error}").into());
            }
            Err(error) => {
                coverage.note = Some(format!(
                    "Live Studio DataModel request failed ({error}); using relaxed filesystem types."
                ));
                return Ok((map, coverage));
            }
        }
    }

    if mode == LintDataModelMode::Studio {
        let reason = if !matching_daemon {
            format!("no matching Ro Sync daemon is reachable on port {port}")
        } else {
            "the Studio plugin is not connected".to_string()
        };
        return Err(format!("lint: --data-model studio requires live Studio: {reason}").into());
    }

    coverage.note = Some(if !matching_daemon {
        "Studio is unavailable; using relaxed filesystem types. Use --data-model filesystem for an offline strict audit."
            .to_string()
    } else {
        "Studio plugin is disconnected; using relaxed filesystem types. Use --data-model filesystem for an offline strict audit."
            .to_string()
    });
    Ok((map, coverage))
}

fn count_json_tree_nodes(node: &serde_json::Value) -> usize {
    1 + node
        .get("children")
        .and_then(serde_json::Value::as_array)
        .map(|children| children.iter().map(count_json_tree_nodes).sum::<usize>())
        .unwrap_or(0)
}

fn report_lint_coverage(coverage: &LintDataModelCoverage, raw: bool) {
    if raw {
        return;
    }
    let node_detail = coverage
        .live_nodes
        .map(|count| format!(", {count} live instances"))
        .unwrap_or_default();
    let strict = if coverage.strict { "strict" } else { "relaxed" };
    eprintln!(
        "[rosync lint] DataModel: {} ({strict}{node_detail})",
        coverage.source
    );
    if let Some(note) = &coverage.note {
        eprintln!("[rosync lint] {note}");
    }
}

const DEFAULT_LINT_VENDOR_IGNORES: &[&str] = &[
    "**/Packages/**",
    "**/_Index/**",
    "**/Madwork*/**",
    "**/PlayerModule/**",
    "**/node_modules/**",
    "**/.git/**",
    "**/.codex/**",
    "**/.vscode/**",
    "**/.rosync-artifacts/**",
    "**/.rosync-backups/**",
    "**/.rosync-workflows/**",
    "**/tools/**",
];

fn lint_target_path(project: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project.join(path)
    }
}

fn validate_lint_target(
    project: &std::path::Path,
    target: &std::path::Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if target == project {
        return Ok(project.to_path_buf());
    }

    let validated = if let Ok(relative) = target.strip_prefix(project) {
        let synced = relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .is_some_and(|service| crate::fs_safety::SYNCED_SERVICES.contains(&service));
        if synced {
            crate::fs_safety::validate_synced_path(project, target, false)
        } else {
            crate::fs_safety::validate_descendant_no_follow(project, relative, false)
        }
        .map_err(|error| format!("lint: validate target {}: {error}", target.display()))?
    } else {
        let metadata = crate::fs_safety::require_metadata_no_follow(target)
            .map_err(|error| format!("lint: inspect target {}: {error}", target.display()))?;
        if metadata.is_dir() {
            crate::fs_safety::stable_canonical_directory(target).map_err(|error| {
                format!(
                    "lint: validate target directory {}: {error}",
                    target.display()
                )
            })?
        } else if metadata.is_file() {
            target.to_path_buf()
        } else {
            return Err(format!(
                "lint: target is not a regular file or directory: {}",
                target.display()
            )
            .into());
        }
    };

    let metadata = crate::fs_safety::require_metadata_no_follow(&validated)
        .map_err(|error| format!("lint: inspect target {}: {error}", target.display()))?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(format!(
            "lint: target is not a regular file or directory: {}",
            target.display()
        )
        .into());
    }
    Ok(validated)
}

#[derive(Debug, Clone)]
struct LintDiagnostic {
    path: PathBuf,
    category: String,
    message: String,
    line: usize,
    column: usize,
    end_line: Option<usize>,
    end_column: Option<usize>,
}

fn filter_lint_output_to_targets(
    project: &std::path::Path,
    targets: &[PathBuf],
    output: &str,
) -> String {
    let scopes: Vec<PathBuf> = targets.to_vec();
    let mut filtered = String::new();
    for line in output.lines() {
        match parse_lint_diagnostic(project, line) {
            Some(diag) if lint_path_in_scopes(&diag.path, &scopes) => {
                filtered.push_str(line);
                filtered.push('\n');
            }
            Some(_) => {}
            None => {
                filtered.push_str(line);
                filtered.push('\n');
            }
        }
    }
    filtered
}

fn lint_path_in_scopes(path: &std::path::Path, scopes: &[PathBuf]) -> bool {
    scopes.iter().any(|scope| {
        if scope.is_dir() {
            path.starts_with(scope)
        } else {
            path == scope
        }
    })
}

#[cfg(test)]
fn normalize_existing_path(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn parse_lint_diagnostic(project: &std::path::Path, line: &str) -> Option<LintDiagnostic> {
    let (file_part, coordinates, message) = split_lint_diagnostic_line(line)?;
    let (category, diagnostic_message) = split_lint_diagnostic_message(message)?;
    // With a sourcemap, luau-lsp appends its virtual DataModel location to the
    // real filename: `Main.luau [game/ReplicatedStorage/Main]`. Keep the disk
    // path for ownership filtering and structured output.
    let file_label = strip_lint_virtual_path_suffix(file_part);
    let file_path = std::path::Path::new(file_label);
    let absolute = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        project.join(file_path)
    };
    Some(LintDiagnostic {
        path: validate_lint_target(project, &absolute).unwrap_or(absolute),
        category: category.trim().to_string(),
        message: diagnostic_message.trim().to_string(),
        line: coordinates[0],
        column: coordinates[1],
        end_line: coordinates.get(2).copied(),
        end_column: coordinates.get(3).copied(),
    })
}

fn split_lint_diagnostic_line(line: &str) -> Option<(&str, Vec<usize>, &str)> {
    // Search for a numeric `(line,column[,endLine,endColumn]): ` suffix rather
    // than splitting at the first `(`. Ro Sync's script-with-children files
    // intentionally contain parentheses, e.g. `init (MarketService).luau`.
    for (location_end, _) in line.rmatch_indices("): ") {
        let prefix = &line[..location_end];
        let Some(location_start) = prefix.rfind('(') else {
            continue;
        };
        let Ok(coordinates) = prefix[location_start + 1..]
            .split(',')
            .map(str::parse::<usize>)
            .collect::<Result<Vec<_>, _>>()
        else {
            continue;
        };
        let file_part = &prefix[..location_start];
        let message = &line[location_end + 3..];
        if (coordinates.len() == 2 || coordinates.len() == 4)
            && lint_file_part_is_plausible(file_part)
            && split_lint_diagnostic_message(message).is_some()
        {
            return Some((file_part, coordinates, message));
        }
    }

    // `--formatter=gnu` uses `path:line.column-endLine.endColumn: ...`, while
    // `--formatter=plain` uses `path:line:column-endColumn: (Wn) ...`.
    for (location_end, _) in line.rmatch_indices(": ") {
        let prefix = &line[..location_end];
        let message = &line[location_end + 2..];
        if split_lint_diagnostic_message(message).is_none() {
            continue;
        }
        if let Some((file_part, coordinates)) = split_gnu_lint_location(prefix) {
            if lint_file_part_is_plausible(file_part) {
                return Some((file_part, coordinates, message));
            }
        }
        if let Some((file_part, coordinates)) = split_plain_lint_location(prefix) {
            if lint_file_part_is_plausible(file_part) {
                return Some((file_part, coordinates, message));
            }
        }
    }
    None
}

fn lint_file_part_is_plausible(file_part: &str) -> bool {
    for marker in [" [game/", " [game]"] {
        if let Some(index) = file_part.rfind(marker) {
            let disk_label = &file_part[..index];
            if disk_label.ends_with(".lua") || disk_label.ends_with(".luau") {
                if !file_part.ends_with(']') {
                    return false;
                }
                break;
            }
        }
    }
    let disk_label = strip_lint_virtual_path_suffix(file_part);
    disk_label.ends_with(".lua") || disk_label.ends_with(".luau")
}

fn split_lint_diagnostic_message(message: &str) -> Option<(&str, &str)> {
    let mut message = message.trim();
    if let Some(after_open) = message.strip_prefix('(') {
        if let Some((severity, rest)) = after_open.split_once(") ") {
            let has_digit = severity.bytes().any(|byte| byte.is_ascii_digit());
            if has_digit
                && severity
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                message = rest;
            }
        }
    }
    let (category, diagnostic_message) = message.split_once(':')?;
    let category = category.trim();
    if category.is_empty()
        || !category
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some((category, diagnostic_message.trim()))
}

fn split_gnu_lint_location(prefix: &str) -> Option<(&str, Vec<usize>)> {
    let location_start = prefix.rfind(':')?;
    let coordinates = parse_gnu_lint_location(&prefix[location_start + 1..])?;
    Some((&prefix[..location_start], coordinates))
}

fn split_plain_lint_location(prefix: &str) -> Option<(&str, Vec<usize>)> {
    let (file_and_line, column_range) = prefix.rsplit_once(':')?;
    let (file_part, line) = file_and_line.rsplit_once(':')?;
    let line = line.parse::<usize>().ok()?;
    let (column, end_column) = match column_range.split_once('-') {
        Some((column, end_column)) => (
            column.parse::<usize>().ok()?,
            Some(end_column.parse::<usize>().ok()?),
        ),
        None => (column_range.parse::<usize>().ok()?, None),
    };
    let coordinates = match end_column {
        Some(end_column) => vec![line, column, line, end_column],
        None => vec![line, column],
    };
    Some((file_part, coordinates))
}

fn parse_gnu_lint_location(location: &str) -> Option<Vec<usize>> {
    fn point(value: &str) -> Option<(usize, usize)> {
        let (line, column) = value.split_once('.')?;
        Some((line.parse().ok()?, column.parse().ok()?))
    }

    if let Some((start, end)) = location.split_once('-') {
        let (line, column) = point(start)?;
        let (end_line, end_column) = point(end)?;
        Some(vec![line, column, end_line, end_column])
    } else {
        let (line, column) = point(location)?;
        Some(vec![line, column])
    }
}

fn strip_lint_virtual_path_suffix(label: &str) -> &str {
    if !label.ends_with(']') {
        return label;
    }
    for marker in [" [game/", " [game]"] {
        if let Some(index) = label.rfind(marker) {
            return &label[..index];
        }
    }
    label
}

fn lint_diagnostics(project: &std::path::Path, output: &str) -> Vec<LintDiagnostic> {
    output
        .lines()
        .filter_map(|line| parse_lint_diagnostic(project, line))
        .collect()
}

fn lint_summary_counts(
    project: &std::path::Path,
    analyzer_output: &str,
    compiler: &LintCompileReport,
) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let project = lifecycle::canonical_project(project).unwrap_or_else(|_| project.to_path_buf());
    let mut by_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_file: BTreeMap<String, usize> = BTreeMap::new();
    let analyzer_diagnostics = lint_diagnostics(&project, analyzer_output);
    let compiler_diagnostics = compiler
        .failures
        .iter()
        .flat_map(|failure| lint_diagnostics(&project, &failure.output))
        .collect::<Vec<_>>();
    for diag in analyzer_diagnostics.into_iter().chain(compiler_diagnostics) {
        *by_category.entry(diag.category).or_insert(0) += 1;
        let file = diag
            .path
            .strip_prefix(&project)
            .unwrap_or(&diag.path)
            .to_string_lossy()
            .replace('\\', "/");
        *by_file.entry(file).or_insert(0) += 1;
    }
    (by_category, by_file)
}

fn print_lint_summary(
    project: &std::path::Path,
    analyzer_output: &str,
    compiler: &LintCompileReport,
    suppressed: usize,
) {
    let (by_category, by_file) = lint_summary_counts(project, analyzer_output, compiler);
    let total: usize = by_category.values().sum();
    if total == 0 {
        println!("\nSummary: 0 diagnostics");
        if suppressed > 0 {
            println!("Suppressed outside requested scopes: {suppressed}");
        }
        return;
    }
    println!("\nSummary: {total} diagnostic{}", plural_s(total));
    println!("By category:");
    for (category, count) in by_category {
        println!("  {count:>4} {category}");
    }
    println!("By file:");
    for (file, count) in by_file {
        println!("  {count:>4} {file}");
    }
    if suppressed > 0 {
        println!("Suppressed outside requested scopes: {suppressed}");
    }
}

struct LintAnalyzerJson<'a> {
    output: &'a str,
    diagnostics: &'a [LintDiagnostic],
    suppressed: usize,
    exit_code: Option<i32>,
    ok: bool,
}

fn print_lint_json(
    project: &std::path::Path,
    coverage: &LintDataModelCoverage,
    compiler: &LintCompileReport,
    analyzer: LintAnalyzerJson<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let analyzer_messages = lint_unparsed_lines(project, analyzer.output);
    let analyzer_diagnostic_count = analyzer.diagnostics.len();
    let mut diagnostics = analyzer
        .diagnostics
        .iter()
        .map(|diagnostic| lint_diagnostic_json(project, diagnostic, "analyzer", None))
        .collect::<Vec<_>>();
    let mut compiler_diagnostic_count = 0usize;
    for failure in &compiler.failures {
        for diagnostic in lint_diagnostics(project, &failure.output) {
            compiler_diagnostic_count += 1;
            diagnostics.push(lint_diagnostic_json(
                project,
                &diagnostic,
                "compiler",
                Some(failure.optimization),
            ));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": analyzer.ok,
            "project": project,
            "analyzerExitCode": analyzer.exit_code,
            "dataModel": coverage,
            "compiler": compiler,
            "analyzerDiagnosticCount": analyzer_diagnostic_count,
            "analyzerMessages": analyzer_messages,
            "compilerDiagnosticCount": compiler_diagnostic_count,
            "diagnosticCount": diagnostics.len(),
            "suppressedDiagnostics": analyzer.suppressed,
            "diagnostics": diagnostics,
        }))?
    );
    Ok(())
}

fn lint_unparsed_lines(project: &std::path::Path, output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty() && parse_lint_diagnostic(project, line).is_none())
        .map(str::to_string)
        .collect()
}

fn lint_has_unparsed_failure(project: &std::path::Path, output: &str) -> bool {
    lint_unparsed_lines(project, output)
        .iter()
        .any(|line| !lint_unparsed_line_is_benign(line))
}

fn lint_analyzer_effective_success(
    scope_only: bool,
    process_success: bool,
    all_diagnostics: usize,
    shown_diagnostics: usize,
    retained_unparsed_failure: bool,
) -> bool {
    if process_success {
        return true;
    }
    scope_only && all_diagnostics > 0 && shown_diagnostics == 0 && !retained_unparsed_failure
}

fn lint_unparsed_line_is_benign(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("[INFO]")
        || line.starts_with("[WARN] client does not allow didChangeWatchedFiles registration")
}

fn lint_diagnostic_json(
    project: &std::path::Path,
    diagnostic: &LintDiagnostic,
    stage: &str,
    optimization: Option<u8>,
) -> serde_json::Value {
    let path = diagnostic
        .path
        .strip_prefix(project)
        .unwrap_or(&diagnostic.path)
        .to_string_lossy()
        .replace('\\', "/");
    serde_json::json!({
        "stage": stage,
        "optimization": optimization,
        "path": path,
        "line": diagnostic.line,
        "column": diagnostic.column,
        "endLine": diagnostic.end_line,
        "endColumn": diagnostic.end_column,
        "category": diagnostic.category,
        "message": diagnostic.message,
    })
}

fn plural_s(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn write_temp_sourcemap_value(
    map: &serde_json::Value,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rosync-sourcemap-{}-{}.json",
        std::process::id(),
        unix_nanos()
    ));
    let text = serde_json::to_string_pretty(&map)?;
    std::fs::write(&path, text).map_err(|e| format!("lint: write {}: {e}", path.display()))?;
    Ok(path)
}

fn write_temp_lint_settings() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rosync-lint-settings-{}-{}.json",
        std::process::id(),
        unix_nanos()
    ));
    let text = serde_json::to_string_pretty(&serde_json::json!({
        "luau-lsp.diagnostics.strictDatamodelTypes": true,
        "luau-lsp.platform.type": "roblox",
    }))?;
    std::fs::write(&path, text).map_err(|e| format!("lint: write {}: {e}", path.display()))?;
    Ok(path)
}

fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn extra_args_include_sourcemap(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--sourcemap" || arg.starts_with("--sourcemap="))
}

fn extra_args_include_platform(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--platform" || arg.starts_with("--platform="))
}

fn extra_args_use_plain_formatter(args: &[String]) -> bool {
    for (index, arg) in args.iter().enumerate() {
        if arg == "--formatter" {
            if args.get(index + 1).is_some_and(|value| value == "plain") {
                return true;
            }
            continue;
        }
        for prefix in ["--formatter=", "--formatter:"] {
            if arg.strip_prefix(prefix) == Some("plain") {
                return true;
            }
        }
    }
    false
}

fn extra_args_include_settings(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--settings" || arg.starts_with("--settings="))
}

fn extra_args_disable_strict_datamodel(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--no-strict-dm-types")
}

fn extra_args_include_roblox_definitions(args: &[String]) -> bool {
    for (index, arg) in args.iter().enumerate() {
        if arg == "--definitions" || arg == "--defs" {
            if args
                .get(index + 1)
                .is_some_and(|value| definition_value_replaces_roblox(value))
            {
                return true;
            }
            continue;
        }
        for prefix in ["--definitions=", "--defs=", "--definitions:", "--defs:"] {
            if let Some(value) = arg.strip_prefix(prefix) {
                if definition_value_replaces_roblox(value) {
                    return true;
                }
            }
        }
    }
    false
}

fn definition_value_replaces_roblox(value: &str) -> bool {
    !value.starts_with('@') || value.starts_with("@roblox=")
}

fn cleanup_temp_file(path: &Option<PathBuf>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

fn resolve_luau_lsp(explicit: Option<PathBuf>) -> OsString {
    if let Some(path) = explicit {
        return path.into_os_string();
    }
    if let Ok(path) = std::env::var("ROSYNC_LUAU_LSP") {
        if !path.trim().is_empty() {
            return OsString::from(path);
        }
    }
    if let Some(path) = find_bundled_luau_lsp() {
        return path.into_os_string();
    }
    if let Some(path) = find_aftman_luau_lsp() {
        return path.into_os_string();
    }
    OsString::from("luau-lsp")
}

fn resolve_luau_compile(explicit: Option<PathBuf>) -> Option<OsString> {
    if let Some(path) = explicit {
        return Some(path.into_os_string());
    }
    for variable in ["ROSYNC_LUAU_COMPILE", "LUAU_COMPILE"] {
        if let Some(path) = std::env::var_os(variable) {
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    if let Some(path) = find_bundled_luau_compile() {
        return Some(path.into_os_string());
    }
    if let Some(path) = find_aftman_luau_compile() {
        return Some(path.into_os_string());
    }
    find_executable_on_path(if cfg!(windows) {
        "luau-compile.exe"
    } else {
        "luau-compile"
    })
    .map(PathBuf::into_os_string)
}

fn find_bundled_luau_compile() -> Option<PathBuf> {
    find_in_tool_bases(&bundled_luau_compile_relative_path())
}

fn bundled_luau_compile_relative_path() -> PathBuf {
    PathBuf::from("tools")
        .join("luau")
        .join(platform_tool_triple())
        .join(if cfg!(windows) {
            "luau-compile.exe"
        } else {
            "luau-compile"
        })
}

fn find_bundled_luau_lsp() -> Option<PathBuf> {
    let rel = PathBuf::from("tools")
        .join("luau-lsp")
        .join(platform_tool_triple())
        .join(if cfg!(windows) {
            "luau-lsp.exe"
        } else {
            "luau-lsp"
        });
    find_in_tool_bases(&rel)
}

fn find_aftman_luau_lsp() -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "luau-lsp.exe"
    } else {
        "luau-lsp"
    };
    let path = dirs::home_dir()?
        .join(".aftman")
        .join("bin")
        .join(executable);
    path.is_file().then_some(path)
}

fn find_aftman_luau_compile() -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "luau-compile.exe"
    } else {
        "luau-compile"
    };
    let path = dirs::home_dir()?
        .join(".aftman")
        .join("bin")
        .join(executable);
    path.is_file().then_some(path)
}

fn find_executable_on_path(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(executable);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        if std::path::Path::new(executable).extension().is_none() {
            let extensions = std::env::var_os("PATHEXT")
                .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
            for extension in extensions.to_string_lossy().split(';') {
                let candidate = directory.join(format!("{executable}{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn find_luau_definitions(project: &std::path::Path) -> Result<Option<PathBuf>, String> {
    // The widget snapshot is paired with the analyzer version Ro Sync tests.
    // A project copy exists for editor tooling and as a standalone fallback,
    // but it can be stale until the next `rosync refresh`.
    if let Some(definitions) = find_bundled_luau_definitions() {
        let metadata = crate::fs_safety::require_metadata_no_follow(&definitions)
            .map_err(|error| format!("inspect bundled definitions: {error}"))?;
        if !metadata.is_file() {
            return Err(format!(
                "bundled definitions are not a regular file: {}",
                definitions.display()
            ));
        }
        return Ok(Some(definitions));
    }
    let project_definitions = project.join(snapshot::ROBLOX_DEFINITIONS_PATH);
    if snapshot::project_tool_file_exists(project, &project_definitions)
        .map_err(|error| format!("inspect project definitions: {error}"))?
    {
        return Ok(Some(project_definitions));
    }
    Ok(None)
}

fn find_bundled_luau_definitions() -> Option<PathBuf> {
    let rel = PathBuf::from("tools")
        .join("luau-lsp")
        .join("roblox")
        .join("globalTypes.d.luau");
    find_in_tool_bases(&rel)
}

fn warn_if_old_luau_lsp(executable: &OsString, project: &std::path::Path) {
    let Ok(output) = std::process::Command::new(executable)
        .arg("--version")
        .current_dir(project)
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let Some(parsed) = parse_semver_triplet(&version) else {
        return;
    };
    if parsed < RECOMMENDED_LUAU_LSP_VERSION {
        eprintln!(
            "[rosync lint] warning: luau-lsp {version} is older than tested {}.{}.{}; run `aftman install` after `rosync refresh`",
            RECOMMENDED_LUAU_LSP_VERSION.0,
            RECOMMENDED_LUAU_LSP_VERSION.1,
            RECOMMENDED_LUAU_LSP_VERSION.2,
        );
    }
}

fn parse_semver_triplet(value: &str) -> Option<(u64, u64, u64)> {
    let version = value.trim().trim_start_matches('v');
    let mut parts = version.split(|character: char| !character.is_ascii_digit());
    let major = parts.find(|part| !part.is_empty())?.parse().ok()?;
    let minor = parts.find(|part| !part.is_empty())?.parse().ok()?;
    let patch = parts.find(|part| !part.is_empty())?.parse().ok()?;
    Some((major, minor, patch))
}

fn resolve_img_api_key(preferred_env: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(env_name) = preferred_env {
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    } else if let Some(value) = find_widget_secret("robloxCloudApiKey") {
        return Ok(value);
    }

    let mut env_names = Vec::new();
    if let Some(env_name) = preferred_env {
        env_names.push(env_name.to_string());
    }
    for env_name in [
        "ROBLOX_API_KEY",
        "CLOUD_API_KEY",
        "ROBLOX_OPEN_CLOUD_API_KEY",
    ] {
        if !env_names.iter().any(|existing| existing == env_name) {
            env_names.push(env_name.to_string());
        }
    }

    for env_name in &env_names {
        if Some(env_name.as_str()) == preferred_env {
            continue;
        }
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    if preferred_env.is_some() {
        if let Some(value) = find_widget_secret("robloxCloudApiKey") {
            return Ok(value);
        }
    }

    Err(format!(
        "upload: missing Roblox Open Cloud credential. Save one in Ro Sync Settings, set one of {}, or pass --api-key-env with a populated environment variable.",
        env_names.join(", ")
    )
    .into())
}

fn resolve_img_creator(project: &Option<PathBuf>) -> Option<String> {
    if let Some(group_id) = project_group_id(project.as_deref()) {
        return Some(format!("group:{group_id}"));
    }
    if let Some(group_id) = active_widget_project_group_id() {
        return Some(format!("group:{group_id}"));
    }
    None
}

fn project_group_id(project: Option<&std::path::Path>) -> Option<String> {
    let root = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    project_config::read_from_disk(&root)
        .ok()
        .flatten()
        .and_then(|cfg| cfg.group_id)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

fn active_widget_project_group_id() -> Option<String> {
    for state_file in widget_state_file_candidates() {
        let Ok(text) = std::fs::read_to_string(&state_file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(group_id) = group_id_from_widget_state(&value) {
            return Some(group_id);
        }
    }
    None
}

fn group_id_from_widget_state(value: &serde_json::Value) -> Option<String> {
    let state = value.get("state").unwrap_or(value);
    let active_id = state
        .get("activeProjectId")
        .and_then(serde_json::Value::as_str)?;
    let projects = state
        .get("projects")
        .and_then(serde_json::Value::as_array)?;
    projects
        .iter()
        .find(|project| {
            project
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id == active_id)
        })
        .and_then(|project| project.get("groupId"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn find_widget_secret(key: &str) -> Option<String> {
    if let Ok(state_dir) = lifecycle::state_dir(None) {
        let path = lifecycle::credentials_path(&state_dir);
        if let Ok(Some(secret)) = lifecycle::read_credential(&path, key) {
            let secret = secret.trim();
            if !secret.is_empty() {
                return Some(secret.to_string());
            }
        }
    }
    for state_file in widget_state_file_candidates() {
        let Ok(text) = std::fs::read_to_string(&state_file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str(&text) else {
            continue;
        };
        if let Some(secret) = secret_from_widget_state(&value, key) {
            return Some(secret);
        }
    }
    None
}

fn secret_from_widget_state(value: &serde_json::Value, key: &str) -> Option<String> {
    for pointer in [
        format!("/state/secrets/{key}"),
        format!("/secrets/{key}"),
        format!("/{key}"),
    ] {
        if let Some(secret) = value
            .pointer(&pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|secret| !secret.is_empty())
        {
            return Some(secret.to_string());
        }
    }
    None
}

fn widget_state_file_candidates() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    let mut files = Vec::new();

    if let Ok(path) = std::env::var("ROSYNC_WIDGET_STATE") {
        push_unique_path(&mut files, PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        push_unique_path(
            &mut files,
            home.join(".terminal64")
                .join("widgets")
                .join("ro-sync")
                .join("state.json"),
        );
    }

    if let Ok(cwd) = std::env::current_dir() {
        push_ancestors(&mut bases, cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        push_exe_ancestors(&mut bases, &exe);
        if let Ok(canonical) = std::fs::canonicalize(&exe) {
            push_exe_ancestors(&mut bases, &canonical);
        }
        if let Ok(target) = std::fs::read_link(&exe) {
            let resolved = if target.is_absolute() {
                target
            } else {
                exe.parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join(target)
            };
            push_exe_ancestors(&mut bases, &resolved);
            if let Ok(canonical) = std::fs::canonicalize(&resolved) {
                push_exe_ancestors(&mut bases, &canonical);
            }
        }
    }

    for base in bases {
        push_unique_path(&mut files, base.join("state.json"));
    }
    files
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn push_exe_ancestors(paths: &mut Vec<PathBuf>, exe: &std::path::Path) {
    if let Some(parent) = exe.parent() {
        push_ancestors(paths, parent.to_path_buf());
    }
}

fn push_ancestors(paths: &mut Vec<PathBuf>, start: PathBuf) {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if !paths.contains(&dir) {
            paths.push(dir.clone());
        }
        cur = dir.parent().map(std::path::Path::to_path_buf);
    }
}

fn find_in_tool_bases(rel: &std::path::Path) -> Option<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent();
        while let Some(dir) = cur {
            bases.push(dir.to_path_buf());
            cur = dir.parent();
        }
    }

    for base in bases {
        let candidate = base.join(rel);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn platform_tool_triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x86_64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else {
        "unknown"
    }
}

fn print_luau_lsp_missing(luau_lsp: &OsString) {
    eprintln!("luau-lsp not found: {}", luau_lsp.to_string_lossy());
    eprintln!();
    eprintln!("Install luau-lsp and make it available on PATH:");
    eprintln!("https://github.com/JohnnyMorganz/luau-lsp");
    eprintln!();
    eprintln!("Ro-Sync also checks this bundled tool path:");
    eprintln!(
        "tools/luau-lsp/{}/{}",
        platform_tool_triple(),
        if cfg!(windows) {
            "luau-lsp.exe"
        } else {
            "luau-lsp"
        }
    );
    eprintln!();
    eprintln!("Or pass an explicit executable path:");
    eprintln!("rosync lint --luau-lsp /path/to/luau-lsp");
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
    let timestamp = unix_secs();
    let output = snapshot_output_path(args.output.as_deref(), args.project.as_deref(), timestamp)?;
    let tree_resp = remote::request(
        args.port,
        "tree",
        serde_json::json!({ "path": "", "depth": u32::MAX }),
    )
    .await
    .map_err(|e| format!("snapshot: tree request failed: {e}"))?;
    let tree = response_value_or_err(&tree_resp, "snapshot tree")?;

    let mut paths = Vec::new();
    collect_snapshot_paths(&tree, "", &mut paths);
    let mut inspections = BTreeMap::new();
    for path in &paths {
        let resp = remote::request(args.port, "get", serde_json::json!({ "path": path }))
            .await
            .map_err(|e| format!("snapshot: get {} failed: {e}", snapshot_path_label(path)))?;
        let value = response_value_or_err(
            &resp,
            &format!("snapshot get {}", snapshot_path_label(path)),
        )?;
        inspections.insert(path.clone(), value);
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
            }))?
        );
    } else {
        println!(
            "snapshot: wrote {} ({} nodes)",
            output.display(),
            paths.len()
        );
    }
    Ok(())
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
    for (path, source_path) in diff::studio_script_paths(&studio) {
        let resp = remote::request(
            args.port,
            "get",
            serde_json::json!({ "path": source_path, "prop": "Source" }),
        )
        .await?;
        let source_value = response_value_or_err(&resp, &format!("diff get {source_path}.Source"))?;
        let source = source_value.as_str().unwrap_or("").to_string();
        diff::set_node_source(&mut studio, &path, source);
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
    // The daemon gates a full-disk overwrite behind mode="all" (the widget's
    // selective picker sends explicit path selections instead). Without this,
    // `--disk` always 409'd — making "studio" the only answer the CLI could
    // ever give, which silently biased every scripted/automated decision.
    let body = if choice == "disk" {
        serde_json::json!({ "choiceId": choice_id, "choice": choice, "mode": "all" })
    } else {
        serde_json::json!({ "choiceId": choice_id, "choice": choice })
    };
    let value = http_post_json(args.port, "/initial-choice", &body)
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
        serde_json::json!({ "type": "hello", "clientId": "rosync-watch", "role": "watch" })
            .to_string(),
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

async fn run_capture(args: CaptureArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        CaptureCommand::Status(args) => run_capture_status(args).await,
        CaptureCommand::Authorize(args) => run_capture_authorize(args).await,
        CaptureCommand::Screen(args) => run_capture_screen(args).await,
        CaptureCommand::Photo(args) => run_capture_photo(args).await,
        CaptureCommand::Scene(args) => run_capture_scene(args).await,
    }
}

async fn run_capture_scene(args: CaptureSceneArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !(1.0..=4.0).contains(&args.padding) || !args.padding.is_finite() {
        return Err("capture scene: --padding must be between 1.0 and 4.0".into());
    }
    parse_capture_size(&args.size)?;
    if args.resample != CaptureResampleMode::Default {
        return Err("capture scene: --resample pixelated is not supported by the Photo engine; use `capture photo` and resize the PNG after capture".into());
    }
    run_capture_photo(capture_scene_photo_args(args)).await
}

fn capture_scene_photo_args(args: CaptureSceneArgs) -> CapturePhotoArgs {
    CapturePhotoArgs {
        project: args.project,
        port: args.port,
        focus: Some(args.focus),
        region: None,
        size: Some(args.size),
        view: args.view,
        direction: None,
        camera_cframe: None,
        padding: args.padding,
        fov: 32.0,
        background: CapturePhotoBackground::Transparent,
        alpha_bleed: true,
        include_world: false,
        no_tight_crop: args.no_tight_crop,
        ui: None,
        ui_target: None,
        include_ui: false,
        delay: 0.05,
        output: args.output,
        timeout: args.timeout,
        raw: args.raw,
    }
}

#[derive(Debug, Deserialize)]
struct PhotoPrepared {
    #[serde(rename = "sessionId")]
    session_id: String,
    width: u32,
    height: u32,
    #[serde(rename = "byteLength")]
    byte_length: usize,
    #[serde(default)]
    background: Option<String>,
    #[serde(default, rename = "uiMode")]
    ui_mode: Option<String>,
    #[serde(default, rename = "cameraCFrame")]
    camera_cframe: Option<serde_json::Value>,
    #[serde(default, rename = "uiTarget")]
    ui_target: Option<String>,
    #[serde(default, rename = "uiTargetClass")]
    ui_target_class: Option<String>,
    #[serde(default, rename = "fieldOfView")]
    field_of_view: Option<f64>,
    #[serde(default)]
    isolated: Option<bool>,
    #[serde(default, rename = "tightCrop")]
    tight_crop: Option<bool>,
    #[serde(default, rename = "fullSize")]
    full_size: Option<serde_json::Value>,
    #[serde(default)]
    region: Option<serde_json::Value>,
    #[serde(default, rename = "regionSource")]
    region_source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PhotoChunk {
    offset: usize,
    #[serde(rename = "nextOffset")]
    next_offset: usize,
    eof: bool,
    #[serde(rename = "bytesBase64")]
    bytes_base64: String,
}

fn parse_capture_direction(value: &str) -> Result<[f64; 3], Box<dyn std::error::Error>> {
    let fields = value.split(',').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err("capture photo: --direction must be x,y,z".into());
    }
    let mut direction = [0.0; 3];
    for (index, field) in fields.iter().enumerate() {
        direction[index] = field.parse::<f64>().map_err(|error| {
            format!(
                "capture photo: invalid direction component {}: {error}",
                index + 1
            )
        })?;
        if !direction[index].is_finite() {
            return Err("capture photo: --direction components must be finite".into());
        }
    }
    let magnitude = direction[0].hypot(direction[1]).hypot(direction[2]);
    if !magnitude.is_finite() {
        return Err("capture photo: --direction magnitude must be finite".into());
    }
    if magnitude <= 1e-6 {
        return Err("capture photo: --direction cannot be the zero vector".into());
    }
    for component in &mut direction {
        *component /= magnitude;
    }
    Ok(direction)
}

fn parse_capture_camera_cframe(value: &str) -> Result<[f64; 12], Box<dyn std::error::Error>> {
    const ORTHONORMAL_EPSILON: f64 = 1e-3;

    let fields = value.split(',').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 12 {
        return Err(
            "capture photo: --camera-cframe must contain the 12 comma-separated values returned by CFrame:GetComponents()"
                .into(),
        );
    }

    let mut components = [0.0; 12];
    for (index, field) in fields.iter().enumerate() {
        components[index] = field.parse::<f64>().map_err(|error| {
            format!(
                "capture photo: invalid --camera-cframe component {}: {error}",
                index + 1
            )
        })?;
        if !components[index].is_finite() {
            return Err("capture photo: --camera-cframe components must be finite".into());
        }
    }

    let rows = [
        [components[3], components[4], components[5]],
        [components[6], components[7], components[8]],
        [components[9], components[10], components[11]],
    ];
    let dot = |left: [f64; 3], right: [f64; 3]| {
        left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
    };
    let rows_are_unit = rows
        .iter()
        .all(|row| (dot(*row, *row) - 1.0).abs() <= ORTHONORMAL_EPSILON);
    let rows_are_orthogonal = dot(rows[0], rows[1]).abs() <= ORTHONORMAL_EPSILON
        && dot(rows[0], rows[2]).abs() <= ORTHONORMAL_EPSILON
        && dot(rows[1], rows[2]).abs() <= ORTHONORMAL_EPSILON;
    let determinant = rows[0][0] * (rows[1][1] * rows[2][2] - rows[1][2] * rows[2][1])
        - rows[0][1] * (rows[1][0] * rows[2][2] - rows[1][2] * rows[2][0])
        + rows[0][2] * (rows[1][0] * rows[2][1] - rows[1][1] * rows[2][0]);
    if !rows_are_unit
        || !rows_are_orthogonal
        || !determinant.is_finite()
        || (determinant - 1.0).abs() > ORTHONORMAL_EPSILON
    {
        return Err(
            "capture photo: --camera-cframe rotation must be an orthonormal right-handed matrix from CFrame:GetComponents()"
                .into(),
        );
    }

    Ok(components)
}

fn build_capture_photo_request(
    args: &CapturePhotoArgs,
    ui_mode: CapturePhotoUiMode,
    region: Option<CaptureRegion>,
    size: Option<[u32; 2]>,
    direction: Option<[f64; 3]>,
    camera_cframe: Option<[f64; 12]>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut request = serde_json::Map::new();
    request.insert(
        "background".into(),
        serde_json::Value::String(args.background.as_wire_str().to_string()),
    );
    request.insert("alphaBleed".into(), serde_json::json!(args.alpha_bleed));
    request.insert(
        "tightCrop".into(),
        serde_json::json!(capture_photo_uses_tight_crop(args)),
    );
    request.insert(
        "uiMode".into(),
        serde_json::Value::String(ui_mode.as_wire_str().to_string()),
    );
    request.insert(
        "hideUI".into(),
        serde_json::json!(ui_mode == CapturePhotoUiMode::None),
    );
    request.insert("delay".into(), serde_json::json!(args.delay));
    request.insert("timeoutSeconds".into(), serde_json::json!(args.timeout));
    if let Some(ui_target) = &args.ui_target {
        request.insert(
            "uiTarget".into(),
            serde_json::Value::String(ui_target.clone()),
        );
    }
    if let Some(focus) = &args.focus {
        request.insert("focus".into(), serde_json::Value::String(focus.clone()));
        request.insert("fieldOfView".into(), serde_json::json!(args.fov));
        request.insert("isolate".into(), serde_json::json!(!args.include_world));
        if let Some(components) = camera_cframe {
            request.insert(
                "cameraCFrame".into(),
                serde_json::json!({
                    "__type": "CFrame",
                    "components": components,
                }),
            );
        } else {
            request.insert(
                "view".into(),
                serde_json::Value::String(args.view.as_plugin_str().to_string()),
            );
            request.insert("padding".into(), serde_json::json!(args.padding));
            if let Some(direction) = direction {
                request.insert(
                    "direction".into(),
                    serde_json::json!({ "x": direction[0], "y": direction[1], "z": direction[2] }),
                );
            }
        }
    }
    if let Some(region) = region {
        request.insert(
            "nativeRect".into(),
            serde_json::json!({
                "x": region.x,
                "y": region.y,
                "width": region.width,
                "height": region.height,
            }),
        );
    }
    if let Some([width, height]) = size {
        request.insert(
            "outputSize".into(),
            serde_json::json!({ "x": width, "y": height }),
        );
    }
    request
}

fn capture_photo_uses_tight_crop(args: &CapturePhotoArgs) -> bool {
    args.focus.is_some()
        && !args.include_world
        && !args.no_tight_crop
        && args.background == CapturePhotoBackground::Transparent
}

fn validate_photo_dimensions(width: u32, height: u32) -> Result<(), Box<dyn std::error::Error>> {
    if width == 0 || height == 0 {
        return Err("capture photo: dimensions must be positive".into());
    }
    validate_capture_dimensions(width, height)?;
    if width > PHOTO_MAX_DIMENSION || height > PHOTO_MAX_DIMENSION {
        return Err(format!(
            "capture photo: dimensions {width}x{height} exceed the {PHOTO_MAX_DIMENSION}px Photo limit"
        )
        .into());
    }
    if u64::from(width) * u64::from(height) > PHOTO_MAX_PIXELS {
        return Err(format!(
            "capture photo: dimensions {width}x{height} exceed the {PHOTO_MAX_PIXELS}-pixel Photo limit"
        )
        .into());
    }
    Ok(())
}

fn encode_photo_png(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let expected = usize::try_from(u64::from(width) * u64::from(height) * 4)
        .map_err(|_| "capture photo: RGBA byte length does not fit this platform")?;
    if rgba.len() != expected {
        return Err(format!(
            "capture photo: received {} RGBA bytes, expected {expected} for {width}x{height}",
            rgba.len()
        )
        .into());
    }
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("capture photo: encode PNG header: {error}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|error| format!("capture photo: encode PNG pixels: {error}"))?;
    }
    Ok(png_bytes)
}

async fn capture_remote_session_connect_until(
    port: u16,
    deadline: Instant,
    phase: &str,
) -> Result<remote::RemoteSession, String> {
    let remaining = capture_deadline_remaining(deadline, phase)?;
    tokio::time::timeout(remaining, remote::RemoteSession::connect(port))
        .await
        .map_err(|_| format!("capture deadline expired during {phase}"))?
}

async fn capture_remote_session_request_until(
    session: &mut remote::RemoteSession,
    op: &str,
    args: serde_json::Value,
    deadline: Instant,
    phase: &str,
) -> Result<serde_json::Value, String> {
    let remaining = capture_deadline_remaining(deadline, phase)?;
    tokio::time::timeout(remaining, session.request(op, args, remaining))
        .await
        .map_err(|_| format!("capture deadline expired during {phase}"))?
}

fn confirm_photo_close_response(response: &serde_json::Value) -> Result<(), String> {
    let value = response_value_or_err(response, "capture photo close")
        .map_err(|error| error.to_string())?;
    if value.as_bool() == Some(true) {
        Ok(())
    } else {
        Err("capture photo close: plugin did not confirm session cleanup".into())
    }
}

async fn close_photo_session_until(
    session: &mut remote::RemoteSession,
    session_id: &str,
    deadline: Instant,
) -> Result<(), String> {
    let response = capture_remote_session_request_until(
        session,
        "photo_close",
        serde_json::json!({ "sessionId": session_id }),
        deadline,
        "capture photo close",
    )
    .await?;
    confirm_photo_close_response(&response)
}

async fn run_capture_photo(args: CapturePhotoArgs) -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};

    if !args.timeout.is_finite() || !(1.0..=120.0).contains(&args.timeout) {
        return Err("capture photo: --timeout must be between 1 and 120 seconds".into());
    }
    let deadline = capture_deadline(args.timeout, "capture photo")?;
    if !(1.0..=4.0).contains(&args.padding) || !args.padding.is_finite() {
        return Err("capture photo: --padding must be between 1.0 and 4.0".into());
    }
    if !(1.0..=120.0).contains(&args.fov) || !args.fov.is_finite() {
        return Err("capture photo: --fov must be between 1 and 120 degrees".into());
    }
    if !(0.0..=5.0).contains(&args.delay) || !args.delay.is_finite() {
        return Err("capture photo: --delay must be between 0 and 5 seconds".into());
    }
    if args.delay >= args.timeout {
        return Err("capture photo: --delay must be shorter than --timeout".into());
    }
    if args.no_tight_crop && args.focus.is_none() {
        return Err("capture photo: --no-tight-crop requires --focus".into());
    }
    if let Some(ui_target) = &args.ui_target {
        if ui_target.trim().is_empty() {
            return Err(
                "capture photo: --ui-target must be a non-empty Studio instance path".into(),
            );
        }
        if let Some(mode) = args.ui {
            if mode != CapturePhotoUiMode::Only {
                return Err("capture photo: --ui-target implies --ui only and cannot be combined with --ui none or --ui overlay".into());
            }
        }
        if args.include_ui {
            return Err(
                "capture photo: --ui-target cannot be combined with the --include-ui overlay alias"
                    .into(),
            );
        }
        if args.focus.is_some() {
            return Err("capture photo: --ui-target cannot be combined with --focus".into());
        }
        if args.background != CapturePhotoBackground::Transparent {
            return Err("capture photo: --ui-target requires --background transparent".into());
        }
    }
    let ui_mode = if args.ui_target.is_some() {
        CapturePhotoUiMode::Only
    } else {
        args.ui.unwrap_or(if args.include_ui {
            CapturePhotoUiMode::Overlay
        } else {
            CapturePhotoUiMode::None
        })
    };
    if ui_mode == CapturePhotoUiMode::Only {
        if args.focus.is_some() {
            return Err(
                "capture photo: --ui only captures the current viewport and cannot be combined with --focus"
                    .into(),
            );
        }
        if args.background != CapturePhotoBackground::Transparent {
            return Err("capture photo: --ui only requires --background transparent".into());
        }
    }
    if args.camera_cframe.is_some() && args.focus.is_none() {
        return Err("capture photo: --camera-cframe requires --focus".into());
    }
    if args.camera_cframe.is_some()
        && (args.direction.is_some()
            || args.view != CaptureView::Isometric
            || (args.padding - 1.25).abs() > f64::EPSILON)
    {
        return Err(
            "capture photo: --camera-cframe cannot be combined with --view, --direction, or --padding"
                .into(),
        );
    }
    if args.focus.is_none()
        && (args.direction.is_some() || args.include_world || args.view != CaptureView::Isometric)
    {
        return Err(
            "capture photo: --view, --direction, and --include-world require --focus".into(),
        );
    }
    if args.focus.is_some() && args.region.is_some() {
        return Err(
            "capture photo: --region captures the current viewport and cannot be combined with --focus; use --size to frame a subject".into(),
        );
    }

    let region = args
        .region
        .as_deref()
        .map(parse_capture_region)
        .transpose()?;
    if let Some(region) = region {
        if region.x < 0 || region.y < 0 {
            return Err(
                "capture photo: viewport-native --region x and y must be non-negative".into(),
            );
        }
        validate_photo_dimensions(region.width, region.height)?;
    }
    let size = match args.size.as_deref() {
        Some(value) => Some(parse_capture_size(value)?),
        None if args.focus.is_some() => Some([1024, 1024]),
        None => None,
    };
    if let Some([width, height]) = size {
        validate_photo_dimensions(width, height)?;
    }
    let direction = args
        .direction
        .as_deref()
        .map(parse_capture_direction)
        .transpose()?;
    let camera_cframe = args
        .camera_cframe
        .as_deref()
        .map(parse_capture_camera_cframe)
        .transpose()?;
    let tight_crop = capture_photo_uses_tight_crop(&args);

    let request =
        build_capture_photo_request(&args, ui_mode, region, size, direction, camera_cframe);

    let work_deadline = capture_work_deadline(deadline);
    let mut photo_remote =
        capture_remote_session_connect_until(args.port, work_deadline, "capture photo connect")
            .await?;
    if ui_mode == CapturePhotoUiMode::Only
        || camera_cframe.is_some()
        || args.ui_target.is_some()
        || tight_crop
    {
        let capability_response = capture_remote_session_request_until(
            &mut photo_remote,
            "capabilities",
            serde_json::json!({}),
            work_deadline,
            "capture photo capabilities",
        )
        .await?;
        let capability_value =
            response_value_or_err(&capability_response, "capture photo capabilities")?;
        let features = capability_value
            .get("features")
            .and_then(serde_json::Value::as_object);
        let supported = |name: &str| {
            features
                .and_then(|features| features.get(name))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        };
        if camera_cframe.is_some() && !supported("photoCameraCFrame") {
            return Err(
                "capture photo: the connected Studio plugin does not support --camera-cframe; reinstall the current Ro Sync plugin and reload Studio"
                    .into(),
            );
        }
        if args.ui_target.is_some() && !supported("photoUiTarget") {
            return Err(
                "capture photo: the connected Studio plugin does not support --ui-target; reinstall the current Ro Sync plugin and reload Studio"
                    .into(),
            );
        }
        if ui_mode == CapturePhotoUiMode::Only && !supported("photoUiOnly") {
            return Err(
                "capture photo: the connected Studio plugin does not support --ui only; reinstall the current Ro Sync plugin and reload Studio"
                    .into(),
            );
        }
        if tight_crop && !supported("photoInstanceTightCrop") {
            return Err(
                "capture photo: the connected Studio plugin does not support automatic instance tight-cropping; reinstall the current Ro Sync plugin and reload Studio, or pass --no-tight-crop"
                    .into(),
            );
        }
    }
    let prepare_response = capture_remote_session_request_until(
        &mut photo_remote,
        "photo_prepare",
        serde_json::Value::Object(request),
        work_deadline,
        "capture photo prepare",
    )
    .await?;
    let prepared_value = response_value_or_err(&prepare_response, "capture photo prepare")?;
    let session_hint = prepared_value
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let prepared: PhotoPrepared = match serde_json::from_value(prepared_value) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(session_id) = session_hint {
                if let Err(cleanup) =
                    close_photo_session_until(&mut photo_remote, &session_id, deadline).await
                {
                    return Err(format!(
                        "capture photo: plugin returned invalid metadata: {error}; session cleanup also failed: {cleanup}"
                    )
                    .into());
                }
            }
            return Err(format!("capture photo: plugin returned invalid metadata: {error}").into());
        }
    };
    let session_id = prepared.session_id.clone();

    let flow: Result<(PathBuf, usize, String), Box<dyn std::error::Error>> = async {
        validate_photo_dimensions(prepared.width, prepared.height)?;
        let expected_u64 = u64::from(prepared.width)
            .checked_mul(u64::from(prepared.height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("capture photo: RGBA byte length overflowed")?;
        let expected = usize::try_from(expected_u64)
            .map_err(|_| "capture photo: RGBA byte length does not fit this platform")?;
        if expected_u64 > CAPTURE_MAX_ARTIFACT_BYTES || prepared.byte_length != expected {
            return Err(format!(
                "capture photo: plugin reported {} bytes for {}x{} RGBA; expected {expected}",
                prepared.byte_length, prepared.width, prepared.height
            )
            .into());
        }

        let mut rgba = Vec::with_capacity(expected);
        let mut offset = 0usize;
        while offset < expected {
            let response = capture_remote_session_request_until(
                &mut photo_remote,
                "photo_read",
                serde_json::json!({
                    "sessionId": session_id,
                    "offset": offset,
                    "maxBytes": 384 * 1024,
                }),
                capture_work_deadline(deadline),
                "capture photo read",
            )
            .await?;
            let value = response_value_or_err(&response, "capture photo read")?;
            let chunk: PhotoChunk = serde_json::from_value(value)
                .map_err(|error| format!("capture photo: invalid chunk metadata: {error}"))?;
            if chunk.offset != offset
                || chunk.next_offset <= chunk.offset
                || chunk.next_offset > expected
            {
                return Err(format!(
                    "capture photo: invalid chunk range {}..{} at expected offset {offset}",
                    chunk.offset, chunk.next_offset
                )
                .into());
            }
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&chunk.bytes_base64)
                .map_err(|error| format!("capture photo: decode RGBA chunk: {error}"))?;
            let declared = chunk.next_offset - chunk.offset;
            if decoded.len() != declared || decoded.len() > 384 * 1024 {
                return Err(format!(
                    "capture photo: chunk decoded to {} bytes, expected {declared}",
                    decoded.len()
                )
                .into());
            }
            rgba.extend_from_slice(&decoded);
            offset = chunk.next_offset;
            if chunk.eof != (offset == expected) {
                return Err(
                    "capture photo: plugin returned inconsistent end-of-file metadata".into(),
                );
            }
        }

        let png_bytes = encode_photo_png(prepared.width, prepared.height, &rgba)?;
        if u64::try_from(png_bytes.len()).unwrap_or(u64::MAX) > CAPTURE_MAX_ARTIFACT_BYTES {
            return Err("capture photo: encoded PNG exceeds the artifact byte limit".into());
        }
        verify_capture_png(
            &png_bytes,
            Some((prepared.width, prepared.height)),
            capture_work_deadline(deadline),
        )?;
        if let Some(parent) = args.output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!("capture photo: create {}: {error}", parent.display())
                })?;
            }
        }
        std::fs::write(&args.output, &png_bytes)
            .map_err(|error| format!("capture photo: write {}: {error}", args.output.display()))?;
        let absolute = std::fs::canonicalize(&args.output).unwrap_or_else(|_| args.output.clone());
        let sha256 = format!("{:x}", Sha256::digest(&png_bytes));
        Ok((absolute, png_bytes.len(), sha256))
    }
    .await;

    let close_result = close_photo_session_until(&mut photo_remote, &session_id, deadline).await;
    let ((absolute, png_size, sha256), consumed) = match (flow, close_result) {
        (Ok(result), Ok(())) => (result, true),
        (Ok(result), Err(cleanup)) => {
            eprintln!("capture photo: warning: session cleanup failed: {cleanup}");
            (result, false)
        }
        (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(cleanup)) => {
            return Err(format!("{error}; session cleanup also failed: {cleanup}").into());
        }
    };
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "artifact": {
                    "path": absolute,
                    "provider": "rosync-photo",
                    "source": "locally-packaged",
                    "mime": "image/png",
                    "size": png_size,
                    "sha256": sha256,
                    "width": prepared.width,
                    "height": prepared.height,
                    "background": prepared.background,
                    "uiMode": prepared.ui_mode,
                    "cameraCFrame": prepared.camera_cframe,
                    "uiTarget": prepared.ui_target,
                    "uiTargetClass": prepared.ui_target_class,
                    "fieldOfView": prepared.field_of_view,
                    "isolated": prepared.isolated,
                    "tightCrop": prepared.tight_crop,
                    "fullSize": prepared.full_size,
                    "region": prepared.region,
                    "regionSource": prepared.region_source,
                    "transport": {
                        "kind": "bounded-rgba-chunks",
                        "consumed": consumed,
                    },
                }
            }))?
        );
    } else {
        println!(
            "wrote {} ({}x{}, {} bytes, sha256 {}; locally packaged Photo engine)",
            absolute.display(),
            prepared.width,
            prepared.height,
            png_size,
            sha256
        );
    }
    Ok(())
}

async fn run_capture_status(args: CaptureStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut resp = remote::request(args.port, "capture_status", serde_json::json!({})).await?;
    let native = native_capture::screen_capture_permission_status();
    if resp.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        let mut value = resp
            .get("value")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        let studio_authorized = value
            .get("authorized")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let provider_unsupported = value
            .get("providerUnsupported")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        value.insert(
            "nativeFallback".into(),
            serde_json::json!({
                "available": native.available,
                "authorized": native.authorized,
                "scope": "screen-ui-all",
            }),
        );
        value.insert(
            "effectiveProvider".into(),
            serde_json::Value::String(
                capture_effective_provider(studio_authorized, provider_unsupported, native)
                    .to_string(),
            ),
        );
        resp["value"] = serde_json::Value::Object(value);
    }
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        let value = response_value_or_err(&resp, "capture status")?;
        let available = value
            .get("available")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let authorized = value
            .get("authorized")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let provider = value
            .get("effectiveProvider")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("none");
        let photo_available = value
            .get("photoAvailable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let photo_ui_only_available = value
            .get("photoUiOnlyAvailable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        println!(
            "capture API: {}; Studio permission: {}; effective provider: {}; packaged Photo: {}; UI-only: {}",
            if available {
                "available"
            } else {
                "unavailable"
            },
            if authorized { "granted" } else { "not granted" },
            provider,
            if photo_available {
                "available"
            } else {
                "unavailable"
            },
            if photo_ui_only_available {
                "available"
            } else {
                "unavailable"
            },
        );
    }
    ok_or_err(&resp)
}

fn capture_effective_provider(
    studio_authorized: bool,
    provider_unsupported: bool,
    native: native_capture::NativePermissionStatus,
) -> &'static str {
    if studio_authorized {
        "studio"
    } else if provider_unsupported && native.available && native.authorized {
        "macos-window"
    } else {
        "none"
    }
}

async fn run_capture_authorize(
    args: CaptureAuthorizeArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request_with_timeout(
        args.port,
        "capture_authorize",
        serde_json::json!({}),
        Duration::from_secs(120),
    )
    .await?;
    let value = match response_value_or_err(&resp, "capture authorize") {
        Ok(value) => value,
        Err(error) => {
            if args.raw {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            return Err(error);
        }
    };
    let studio_authorized = value
        .get("authorized")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let provider_unsupported = value
        .get("providerUnsupported")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let provider_error = value.get("providerError").cloned();
    let native_before = native_capture::screen_capture_permission_status();
    let mut native = native_before;
    let mut native_prompted = false;
    if provider_unsupported && native.available && !native.authorized {
        native_prompted = true;
        native = native_capture::request_screen_capture_permission()?;
    }
    let provider = capture_effective_provider(studio_authorized, provider_unsupported, native);
    let authorized = provider != "none";
    let aggregate = serde_json::json!({
        "ok": authorized,
        "provider": provider,
        "studio": {
            "available": true,
            "authorized": studio_authorized,
            "providerUnsupported": provider_unsupported,
            "providerError": provider_error,
        },
        "nativeFallback": {
            "available": native.available,
            "authorized": native.authorized,
            "prompted": native_prompted,
            "scope": "screen-ui-all",
        }
    });
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&aggregate)?);
    } else if studio_authorized {
        println!("screenshot permission: granted (Studio provider)");
    } else if provider_unsupported && native.authorized {
        println!(
            "screenshot permission: granted (macOS Roblox Studio window fallback; Studio provider unsupported)"
        );
    } else if provider_unsupported && native.available {
        println!(
            "screenshot permission: denied (Studio provider unsupported; macOS Screen & System Audio Recording permission not granted)"
        );
    } else {
        println!("screenshot permission: denied");
    }
    if authorized {
        Ok(())
    } else if provider_unsupported && !native.available {
        Err("capture authorize: Studio screenshot provider is unsupported and the native fallback is only available on macOS".into())
    } else if provider_unsupported {
        Err("capture authorize: macOS Screen & System Audio Recording permission was not granted; enable it for the app running rosync, then retry".into())
    } else {
        Err("capture authorize: Studio screenshot permission was denied".into())
    }
}

#[derive(Debug, Deserialize)]
struct CapturePrepared {
    #[serde(rename = "sessionId")]
    session_id: String,
    width: u32,
    height: u32,
    #[serde(rename = "byteLength")]
    byte_length: usize,
    #[serde(default)]
    position: Option<CapturePoint>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CapturePoint {
    x: f64,
    y: f64,
}

#[derive(Clone, Copy)]
struct CaptureRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn parse_capture_region(value: &str) -> Result<CaptureRegion, Box<dyn std::error::Error>> {
    let fields = value.split(',').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 4 {
        return Err("capture: --region must be x,y,width,height with positive dimensions".into());
    }
    let x = fields[0]
        .parse::<i32>()
        .map_err(|e| format!("capture: invalid region x coordinate: {e}"))?;
    let y = fields[1]
        .parse::<i32>()
        .map_err(|e| format!("capture: invalid region y coordinate: {e}"))?;
    let width = fields[2]
        .parse::<u32>()
        .map_err(|e| format!("capture: invalid region width: {e}"))?;
    let height = fields[3]
        .parse::<u32>()
        .map_err(|e| format!("capture: invalid region height: {e}"))?;
    if width == 0 || height == 0 {
        return Err("capture: region dimensions must be positive".into());
    }
    Ok(CaptureRegion {
        x,
        y,
        width,
        height,
    })
}

fn parse_capture_size(value: &str) -> Result<[u32; 2], Box<dyn std::error::Error>> {
    let normalized = value.trim().to_ascii_lowercase();
    let Some((width, height)) = normalized.split_once('x') else {
        return Err("capture: --output-size must be WIDTHxHEIGHT".into());
    };
    let width = width
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("capture: invalid output width: {e}"))?;
    let height = height
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("capture: invalid output height: {e}"))?;
    if width == 0 || height == 0 {
        return Err("capture: output dimensions must be positive".into());
    }
    Ok([width, height])
}

fn validate_capture_dimensions(width: u32, height: u32) -> Result<(), Box<dyn std::error::Error>> {
    if width > CAPTURE_MAX_DIMENSION || height > CAPTURE_MAX_DIMENSION {
        return Err(format!(
            "capture: dimensions {width}x{height} exceed the {CAPTURE_MAX_DIMENSION}px per-axis limit"
        )
        .into());
    }
    if u64::from(width) * u64::from(height) > CAPTURE_MAX_PIXELS {
        return Err(format!(
            "capture: dimensions {width}x{height} exceed the {CAPTURE_MAX_PIXELS}-pixel limit"
        )
        .into());
    }
    Ok(())
}

fn capture_deadline(
    timeout_seconds: f64,
    context: &str,
) -> Result<Instant, Box<dyn std::error::Error>> {
    if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 || timeout_seconds > 120.0 {
        return Err(format!(
            "{context}: timeout must be finite, greater than zero, and at most 120 seconds"
        )
        .into());
    }
    Instant::now()
        .checked_add(Duration::from_secs_f64(timeout_seconds))
        .ok_or_else(|| format!("{context}: timeout is too large").into())
}

fn capture_deadline_remaining(deadline: Instant, phase: &str) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(format!("capture deadline expired before {phase}"))
    } else {
        Ok(remaining)
    }
}

fn capture_work_deadline(deadline: Instant) -> Instant {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let reserve = CAPTURE_CLEANUP_RESERVE.min(remaining / 5);
    deadline.checked_sub(reserve).unwrap_or(deadline)
}

async fn capture_remote_request_until(
    port: u16,
    op: &str,
    args: serde_json::Value,
    deadline: Instant,
    phase: &str,
) -> Result<serde_json::Value, String> {
    let remaining = capture_deadline_remaining(deadline, phase)?;
    tokio::time::timeout(
        remaining,
        remote::request_with_timeout(port, op, args, remaining),
    )
    .await
    .map_err(|_| format!("capture deadline expired during {phase}"))?
}

fn validate_artifact_id(id: &str) -> Result<&str, String> {
    if id.len() == 48 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err("capture artifact id must be exactly 48 hexadecimal characters".into())
    }
}

fn plugin_artifact_id<'a>(
    artifact: &'a serde_json::Value,
    context: &str,
) -> Result<&'a str, String> {
    let id = artifact
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{context}: response omitted artifact id"))?;
    validate_artifact_id(id).map_err(|error| format!("{context}: {error}"))
}

async fn lookup_artifact_transport_until(
    port: u16,
    id: &str,
    deadline: Instant,
) -> Result<artifact::ArtifactMetadata, String> {
    validate_artifact_id(id)?;
    let response = http_get_json_until(port, &format!("/artifacts/{id}"), deadline).await?;
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!("artifact lookup rejected: {response}"));
    }
    let metadata: artifact::ArtifactMetadata = serde_json::from_value(
        response
            .get("artifact")
            .cloned()
            .ok_or_else(|| "artifact lookup omitted metadata".to_string())?,
    )
    .map_err(|error| format!("artifact lookup returned invalid metadata: {error}"))?;
    if metadata.id != id {
        return Err(format!(
            "artifact lookup returned id {}, expected {id}",
            metadata.id
        ));
    }
    if metadata.mime != "image/png" {
        return Err(format!(
            "artifact {id} has MIME {}, expected image/png",
            metadata.mime
        ));
    }
    if metadata.size == 0 || metadata.size > CAPTURE_MAX_ARTIFACT_BYTES {
        return Err(format!(
            "artifact {id} size {} is outside the capture limit",
            metadata.size
        ));
    }
    if metadata.sha256.len() != 64 || !metadata.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!("artifact {id} has an invalid SHA-256 digest"));
    }
    if !metadata.path.is_absolute() {
        return Err(format!("artifact {id} path is not absolute"));
    }
    Ok(metadata)
}

fn read_bounded_capture_file(
    metadata: &artifact::ArtifactMetadata,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    if metadata.size == 0 || metadata.size > CAPTURE_MAX_ARTIFACT_BYTES {
        return Err(format!(
            "artifact size {} is outside the capture limit",
            metadata.size
        ));
    }
    let expected = usize::try_from(metadata.size)
        .map_err(|_| "artifact size does not fit this platform".to_string())?;
    let file_metadata = std::fs::metadata(&metadata.path).map_err(|error| {
        format!(
            "read artifact metadata {}: {error}",
            metadata.path.display()
        )
    })?;
    if !file_metadata.is_file() {
        return Err(format!(
            "artifact path is not a regular file: {}",
            metadata.path.display()
        ));
    }
    if file_metadata.len() != metadata.size {
        return Err(format!(
            "artifact file size {} does not match daemon metadata {}",
            file_metadata.len(),
            metadata.size
        ));
    }
    let mut file = std::fs::File::open(&metadata.path)
        .map_err(|error| format!("open artifact {}: {error}", metadata.path.display()))?;
    let mut bytes = Vec::with_capacity(expected);
    let mut buffer = [0u8; 64 * 1024];
    let bounded_length = expected + 1;
    while bytes.len() < bounded_length {
        capture_deadline_remaining(deadline, "artifact read")?;
        let available = (bounded_length - bytes.len()).min(buffer.len());
        let count = file
            .read(&mut buffer[..available])
            .map_err(|error| format!("read artifact {}: {error}", metadata.path.display()))?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() != expected {
        return Err(format!(
            "artifact read {} bytes, expected {expected}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

fn verify_capture_png(
    bytes: &[u8],
    expected_dimensions: Option<(u32, u32)>,
    deadline: Instant,
) -> Result<(u32, u32), String> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("capture artifact is not a PNG".into());
    }
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("decode capture PNG header: {error}"))?;
    let width = reader.info().width;
    let height = reader.info().height;
    validate_capture_dimensions(width, height).map_err(|error| error.to_string())?;
    if let Some((expected_width, expected_height)) = expected_dimensions {
        if (width, height) != (expected_width, expected_height) {
            return Err(format!(
                "capture PNG dimensions {width}x{height} do not match reported {expected_width}x{expected_height}"
            ));
        }
    }
    loop {
        capture_deadline_remaining(deadline, "PNG verification")?;
        match reader
            .next_row()
            .map_err(|error| format!("decode capture PNG: {error}"))?
        {
            Some(_) => {}
            None => break,
        }
    }
    Ok((width, height))
}

#[derive(Debug)]
struct MaterializedCapture {
    metadata: artifact::ArtifactMetadata,
    output_path: Option<PathBuf>,
    size: usize,
    sha256: String,
    width: u32,
    height: u32,
    consumed: bool,
}

async fn materialize_capture_artifact(
    port: u16,
    id: &str,
    expected_size: Option<u64>,
    expected_dimensions: Option<(u32, u32)>,
    destination: Option<&std::path::Path>,
    deadline: Instant,
    context: &str,
) -> Result<MaterializedCapture, Box<dyn std::error::Error>> {
    use sha2::{Digest as _, Sha256};

    validate_artifact_id(id).map_err(|error| format!("{context}: {error}"))?;
    let work_deadline = capture_work_deadline(deadline);
    let primary: Result<MaterializedCapture, Box<dyn std::error::Error>> = async {
        let metadata = lookup_artifact_transport_until(port, id, work_deadline).await?;
        if let Some(expected_size) = expected_size {
            if metadata.size != expected_size {
                return Err(format!(
                    "{context}: daemon artifact size {} does not match reported {expected_size}",
                    metadata.size
                )
                .into());
            }
        }
        let bytes = read_bounded_capture_file(&metadata, work_deadline)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if !sha256.eq_ignore_ascii_case(&metadata.sha256) {
            return Err(format!(
                "{context}: SHA-256 mismatch (computed {sha256}, daemon {})",
                metadata.sha256
            )
            .into());
        }
        let (width, height) = verify_capture_png(&bytes, expected_dimensions, work_deadline)
            .map_err(|error| format!("{context}: {error}"))?;
        let output_path = if let Some(destination) = destination {
            capture_deadline_remaining(work_deadline, "capture output")?;
            if let Some(parent) = destination.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!("{context}: create {}: {error}", parent.display())
                    })?;
                }
            }
            std::fs::write(destination, &bytes)
                .map_err(|error| format!("{context}: write {}: {error}", destination.display()))?;
            Some(std::fs::canonicalize(destination).unwrap_or_else(|_| destination.to_path_buf()))
        } else {
            None
        };
        Ok(MaterializedCapture {
            metadata,
            output_path,
            size: bytes.len(),
            sha256,
            width,
            height,
            consumed: false,
        })
    }
    .await;

    let consume_result = consume_artifact_transport_until(port, id, deadline).await;
    match primary {
        Ok(mut materialized) => {
            materialized.consumed = consume_result.is_ok();
            if let Err(error) = consume_result {
                eprintln!("{context}: warning: could not remove transport artifact: {error}");
            }
            Ok(materialized)
        }
        Err(error) => {
            if let Err(cleanup) = consume_result {
                Err(format!("{error}; artifact cleanup also failed: {cleanup}").into())
            } else {
                Err(error)
            }
        }
    }
}

async fn cleanup_artifact_lease_until(port: u16, id: &str, token: &str, deadline: Instant) {
    if consume_artifact_transport_until(port, id, deadline)
        .await
        .is_ok()
    {
        return;
    }
    if capture_deadline_remaining(deadline, "artifact abort").is_ok() {
        let _ = http_post_json_until(
            port,
            &format!("/artifacts/{id}/abort"),
            &serde_json::json!({ "token": token }),
            deadline,
        )
        .await;
    }
}

fn capture_error_allows_macos_window_fallback(args: &CaptureScreenArgs, error: &str) -> bool {
    if !cfg!(target_os = "macos")
        || args.ui != CaptureUiMode::All
        || args.focus.is_some()
        || args.view.is_some()
        || args.padding.is_some()
    {
        return false;
    }
    let normalized = error.to_ascii_lowercase();
    normalized
        .contains("studio screenshot provider is unsupported after explicit capture authorization")
        && normalized.contains("feature not supported yet")
}

async fn run_macos_window_capture_fallback(
    args: &CaptureScreenArgs,
    region: Option<CaptureRegion>,
    output_size: Option<[u32; 2]>,
    deadline: Instant,
    studio_error: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use sha2::{Digest as _, Sha256};

    eprintln!(
        "capture: Studio screenshot provider unavailable ({studio_error}); using the macOS Roblox Studio window fallback for --ui all"
    );
    let project_hint = args
        .project
        .as_deref()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str);
    let result = native_capture::capture_studio_window(native_capture::NativeCaptureRequest {
        project_hint,
        region: region.map(|region| native_capture::CaptureRegion {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        }),
        output_size,
        pixelated: args.resample == CaptureResampleMode::Pixelated,
        output: &args.output,
        deadline,
        limits: native_capture::CaptureLimits {
            max_dimension: CAPTURE_MAX_DIMENSION,
            max_pixels: CAPTURE_MAX_PIXELS,
            max_bytes: CAPTURE_MAX_ARTIFACT_BYTES,
        },
    })
    .await
    .map_err(|native_error| {
        format!(
            "capture: Studio provider failed ({studio_error}); macOS window fallback failed: {native_error}"
        )
    })?;

    // Run the same structural/decode verification used for Studio transport
    // artifacts before reporting a native capture as successful.
    let bytes = std::fs::read(&result.output_path).map_err(|error| {
        format!(
            "capture: verify native output {}: {error}",
            result.output_path.display()
        )
    })?;
    if bytes.len() != result.size
        || bytes.is_empty()
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CAPTURE_MAX_ARTIFACT_BYTES
    {
        let _ = std::fs::remove_file(&result.output_path);
        return Err("capture: native output changed size before verification".into());
    }
    let (width, height) = match verify_capture_png(
        &bytes,
        Some((result.width, result.height)),
        capture_work_deadline(deadline),
    ) {
        Ok(dimensions) => dimensions,
        Err(error) => {
            let _ = std::fs::remove_file(&result.output_path);
            return Err(format!("capture: verify native output: {error}").into());
        }
    };
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if sha256 != result.sha256 {
        let _ = std::fs::remove_file(&result.output_path);
        return Err("capture: native output SHA-256 changed before verification".into());
    }
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "artifact": {
                    "path": result.output_path,
                    "transport": {
                        "kind": "direct-local",
                        "consumed": true,
                    },
                    "provider": "macos-window",
                    "fallbackFrom": "StudioCaptureService",
                    "mime": "image/png",
                    "size": result.size,
                    "sha256": sha256,
                    "width": width,
                    "height": height,
                    "position": {
                        "x": result.position[0],
                        "y": result.position[1],
                    },
                    "window": result.window,
                }
            }))?
        );
    } else {
        println!(
            "wrote {} ({}x{}, {} bytes, sha256 {}; macOS Roblox Studio window fallback)",
            result.output_path.display(),
            width,
            height,
            result.size,
            sha256
        );
    }
    Ok(())
}

async fn run_capture_screen(args: CaptureScreenArgs) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = capture_deadline(args.timeout, "capture")?;

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
    let mut request = serde_json::Map::new();
    request.insert(
        "ui".into(),
        serde_json::Value::String(args.ui.as_plugin_str().to_string()),
    );
    request.insert(
        "resample".into(),
        serde_json::Value::String(args.resample.as_plugin_str().to_string()),
    );
    request.insert("timeoutSeconds".into(), serde_json::json!(args.timeout));
    if let Some(region) = region {
        request.insert(
            "position".into(),
            serde_json::json!({ "x": region.x, "y": region.y }),
        );
        request.insert(
            "captureSize".into(),
            serde_json::json!({ "x": region.width, "y": region.height }),
        );
    }
    if let Some([width, height]) = output_size {
        request.insert(
            "outputSize".into(),
            serde_json::json!({ "x": width, "y": height }),
        );
    }
    if let Some(focus) = &args.focus {
        request.insert("focus".into(), serde_json::Value::String(focus.clone()));
    }
    if let Some(view) = args.view {
        request.insert(
            "view".into(),
            serde_json::Value::String(view.as_plugin_str().to_string()),
        );
    }
    if let Some(padding) = args.padding {
        request.insert("padding".into(), serde_json::json!(padding));
    }

    let work_deadline = capture_work_deadline(deadline);
    let prepare_resp = capture_remote_request_until(
        args.port,
        "capture_prepare",
        serde_json::Value::Object(request),
        work_deadline,
        "capture prepare",
    )
    .await?;
    let prepared_value = match response_value_or_err(&prepare_resp, "capture prepare") {
        Ok(value) => value,
        Err(error) => {
            let error = error.to_string();
            if capture_error_allows_macos_window_fallback(&args, &error) {
                return run_macos_window_capture_fallback(
                    &args,
                    region,
                    output_size,
                    deadline,
                    &error,
                )
                .await;
            }
            return Err(error.into());
        }
    };
    let session_hint = prepared_value
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let prepared: CapturePrepared = match serde_json::from_value(prepared_value) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(session_id) = session_hint {
                let _ = capture_remote_request_until(
                    args.port,
                    "capture_close",
                    serde_json::json!({ "sessionId": session_id }),
                    deadline,
                    "capture close",
                )
                .await;
            }
            return Err(format!("capture: plugin returned invalid metadata: {error}").into());
        }
    };
    let session_id = prepared.session_id.clone();
    let mut lease_credentials: Option<(String, String)> = None;
    let flow: Result<MaterializedCapture, Box<dyn std::error::Error>> = async {
        validate_capture_dimensions(prepared.width, prepared.height)?;
        let prepared_size = u64::try_from(prepared.byte_length)
            .map_err(|_| "capture: reported artifact size does not fit u64")?;
        if prepared_size == 0 || prepared_size > CAPTURE_MAX_ARTIFACT_BYTES {
            return Err(format!(
                "capture: plugin reported an invalid artifact size of {} bytes",
                prepared.byte_length
            )
            .into());
        }
        let lease_response = http_post_json_until(
            args.port,
            "/artifacts/lease",
            &serde_json::json!({
                "filename": "studio-capture.png",
                "mime": "image/png",
                "expectedSize": prepared_size,
            }),
            work_deadline,
        )
        .await
        .map_err(|error| format!("capture: create artifact lease: {error}"))?;
        if lease_response
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(format!("capture: artifact lease rejected: {lease_response}").into());
        }
        let lease = lease_response
            .get("lease")
            .cloned()
            .ok_or("capture: artifact lease response omitted lease")?;
        let lease_id = plugin_artifact_id(&lease, "capture lease")?.to_string();
        let lease_token = lease
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or("capture: artifact lease omitted token")?
            .to_string();
        lease_credentials = Some((lease_id.clone(), lease_token));
        let export_timeout = capture_deadline_remaining(work_deadline, "capture export")?;
        let export_response = capture_remote_request_until(
            args.port,
            "capture_export",
            serde_json::json!({
                "sessionId": session_id,
                "lease": lease,
                "timeoutSeconds": export_timeout.as_secs_f64(),
            }),
            work_deadline,
            "capture export",
        )
        .await?;
        let plugin_artifact = response_value_or_err(&export_response, "capture export")?;
        let returned_id = plugin_artifact_id(&plugin_artifact, "capture export")?;
        if returned_id != lease_id {
            return Err(format!(
                "capture: plugin finalized artifact {returned_id}, expected lease {lease_id}"
            )
            .into());
        }
        materialize_capture_artifact(
            args.port,
            &lease_id,
            Some(prepared_size),
            Some((prepared.width, prepared.height)),
            Some(&args.output),
            deadline,
            "capture",
        )
        .await
    }
    .await;

    if capture_deadline_remaining(deadline, "capture close").is_ok() {
        let _ = capture_remote_request_until(
            args.port,
            "capture_close",
            serde_json::json!({ "sessionId": session_id }),
            deadline,
            "capture close",
        )
        .await;
    }
    let mut materialized = match flow {
        Ok(materialized) => materialized,
        Err(error) => {
            if let Some((id, token)) = &lease_credentials {
                cleanup_artifact_lease_until(args.port, id, token, deadline).await;
            }
            return Err(error);
        }
    };
    if !materialized.consumed {
        if consume_artifact_transport_until(args.port, &materialized.metadata.id, deadline)
            .await
            .is_ok()
        {
            materialized.consumed = true;
        } else if let Some((id, token)) = &lease_credentials {
            cleanup_artifact_lease_until(args.port, id, token, deadline).await;
        }
    }
    let absolute = materialized
        .output_path
        .clone()
        .ok_or("capture: output path was not materialized")?;
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "artifact": {
                    "path": absolute,
                    "provider": "studio",
                    "transport": {
                        "metadata": materialized.metadata,
                        "consumed": materialized.consumed,
                    },
                    "mime": "image/png",
                    "size": materialized.size,
                    "sha256": materialized.sha256,
                    "width": materialized.width,
                    "height": materialized.height,
                    "position": prepared.position,
                }
            }))?
        );
    } else {
        println!(
            "wrote {} ({}x{}, {} bytes, sha256 {})",
            absolute.display(),
            materialized.width,
            materialized.height,
            materialized.size,
            materialized.sha256
        );
    }
    Ok(())
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
    if args.timeout <= 0.0 {
        return Err("transmit: --timeout must be greater than zero".into());
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
    // Plugin may be offline — treat failures as "no plugin connected" rather
    // than aborting the subcommand.
    let value = match fetch_plugin_version(args.port).await {
        Ok(v) => v,
        Err(e) => {
            if args.raw {
                println!(
                    "{}",
                    serde_json::json!({ "daemon": daemon, "plugin": null, "error": e })
                );
            } else {
                println!("daemon: rosync {daemon}");
                println!("plugin: (not connected — {e})");
            }
            return Ok(());
        }
    };
    if args.raw {
        println!(
            "{}",
            serde_json::json!({ "daemon": daemon, "plugin": value })
        );
        return Ok(());
    }
    println!("daemon: rosync {daemon}");
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
        if let Some(subcommands) = command.get("subcommands") {
            row["subcommands"] = subcommands.clone();
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
        "commands" | "context" | "plan" | "snapshot" | "diff" | "changes" | "status" | "doctor"
        | "refresh" | "init" | "daemon" => vec!["project"],
        "plugin" => vec!["bundled-plugin", "roblox-plugin-directory"],
        "auth" => vec!["credential-input-for-set"],
        _ => vec!["daemon", "studio-plugin"],
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

/// Bridges the filesystem-watcher's typed operation/resync stream into the
/// shared `broadcast::Sender<String>` that `/events` streams. Each Op is first run
/// through `ConflictEngine::on_fs_change` so that echoes of our own writes
/// (baseline matches) are dropped and conflicts are surfaced as their own
/// event type rather than a propagation op.
fn spawn_watch_bridge(
    watcher: Watch,
    root: PathBuf,
    events: broadcast::Sender<String>,
    conflicts: Arc<ConflictEngine>,
    push_quiet: Arc<Mutex<HashMap<PathBuf, Instant>>>,
) -> Result<(), String> {
    let mut rx = watcher.subscribe();
    let hydration_validation = fs_safety::SyncedPathValidationCache::new(&root)
        .map_err(|error| format!("initialize watcher hydration safety cache: {error}"))?;
    // Move the Watch into the task so the debouncer stays alive for the lifetime
    // of the daemon.
    tokio::spawn(async move {
        let _watcher = watcher;
        // Seed the parent-candidate set off the readiness path. `rx` is
        // subscribed above, so watcher events raised while the seed walk runs
        // buffer in the channel and are processed afterwards against the
        // fully seeded set (an overflow surfaces as the usual lag/resync).
        let seed_root = root.clone();
        let initial_parent_dirs = match tokio::task::spawn_blocking(move || {
            collect_existing_parent_candidates(&seed_root)
        })
        .await
        .map_err(|error| format!("seed watcher parent candidates: {error}"))
        .and_then(|seed| seed)
        {
            Ok(initial_parent_dirs) => initial_parent_dirs,
            Err(error) => {
                let _ = events.send(watcher_hydration_shutdown(&format!(
                    "validate watched filesystem: {error}"
                )));
                return;
            }
        };
        // Empty folders are intentionally absent from Studio. Seed every
        // existing disk directory and remember newly created ones so the first
        // script added beneath an unmaterialized folder can create its parent
        // chain before its own `set` arrives. Without this ordering, `mkdir
        // Workspace/tools`, followed later (even after a daemon restart) by
        // `Workspace/tools/Test.luau`, sends a child op whose parent does not
        // exist in Studio and the plugin has no safe way to infer it.
        let mut pending_parent_dirs = initial_parent_dirs;
        let mut hydration_validation = hydration_validation;
        loop {
            match rx.recv().await {
                Ok(watch::WatchEvent::Op(mut op)) => {
                    // Any observed change drops the cached content hash for
                    // the touched paths (correctness never depends on this —
                    // hash lookups re-check the file generation — it just
                    // keeps the cache small and current).
                    http::invalidate_cached_content_hash(&op.path);
                    if let Some(from) = &op.from {
                        http::invalidate_cached_content_hash(from);
                    }
                    if is_synced_service_root_op(&op, &root) {
                        continue;
                    }
                    if is_push_quiet(&push_quiet, &op.path, &root) {
                        continue;
                    }
                    // For renames, also suppress if the source side was a recent
                    // /push write — otherwise daemon-initiated renames echo back.
                    if let Some(from) = &op.from {
                        if is_push_quiet(&push_quiet, from, &root) {
                            continue;
                        }
                    }
                    if let Err(error) = hydrate_watcher_op(&mut op, &mut hydration_validation) {
                        reset_watch_bridge_after_barrier(
                            &_watcher,
                            &mut rx,
                            &root,
                            &mut pending_parent_dirs,
                        );
                        let _ = events.send(watcher_hydration_shutdown(&error));
                        continue;
                    }
                    if op.kind == OpKind::Rename && op.is_dir == Some(true) {
                        if let Some(from) = &op.from {
                            rebase_pending_parent_candidates(
                                &mut pending_parent_dirs,
                                from,
                                &op.path,
                            );
                        }
                    }
                    if op.kind == OpKind::Add && op.content.is_none() && op.is_dir == Some(true) {
                        pending_parent_dirs.insert(op.path.clone());
                    }
                    for parent in
                        take_pending_parent_materializations(&op, &root, &mut pending_parent_dirs)
                    {
                        emit_op(
                            &events,
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
                        // are independent streams. Hold destructive ops briefly
                        // so an already-in-flight Studio source push can prove
                        // whether Studio still matches the agreed baseline.
                        tokio::time::sleep(Duration::from_millis(FS_DESTRUCTIVE_PREFLIGHT_MS))
                            .await;
                    }
                    if let Some(blocked) = handle_op(op, &events, &conflicts) {
                        let _ = http::write_log_entry(axum::Json(serde_json::json!({
                            "source": "filesystem-sync-conflict",
                            "op": blocked.kind,
                            "path": blocked.path,
                            "from": blocked.from,
                            "outcome": "blocked-conflict",
                        })));
                    }
                }
                Ok(watch::WatchEvent::Resync { reason }) => {
                    reset_watch_bridge_after_barrier(
                        &_watcher,
                        &mut rx,
                        &root,
                        &mut pending_parent_dirs,
                    );
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
                    let _ = events.send(watcher_lag_shutdown(skipped));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Ok(())
}

fn reset_watch_bridge_after_barrier(
    watcher: &Watch,
    receiver: &mut broadcast::Receiver<watch::WatchEvent>,
    root: &std::path::Path,
    pending_parent_dirs: &mut HashSet<PathBuf>,
) {
    watcher.discard_retained_tail(receiver);
    refresh_parent_candidates_after_barrier(root, pending_parent_dirs);
}

fn refresh_parent_candidates_after_barrier(
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
const FS_DESTRUCTIVE_PREFLIGHT_MS: u64 = 500;

fn deleted_sync_path_is_dir(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    fs_map::classify_script_file(name).is_none() && !fs_map::is_init_file(name)
}

fn ensure_watcher_file_generation_unchanged(
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

fn read_stable_watcher_file(
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

fn hydrate_watcher_op(
    op: &mut Op,
    validation: &mut fs_safety::SyncedPathValidationCache,
) -> Result<(), String> {
    if matches!(op.kind, OpKind::Add | OpKind::Update | OpKind::Rename) && op.is_dir == Some(false)
    {
        op.content = Some(read_stable_watcher_file(&op.path, validation)?);
    }
    Ok(())
}

fn begin_fs_destructive_preflight(
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

fn watcher_lag_shutdown(skipped: u64) -> String {
    serde_json::json!({
        "type": "shutdown",
        "reason": "filesystem watcher lagged; reconnect to rebuild exact sync state",
        "code": "WATCHER_LAGGED",
        "retryable": true,
        "skipped": skipped,
    })
    .to_string()
}

fn watcher_resync_shutdown(code: &str, reason: &str) -> String {
    serde_json::json!({
        "type": "shutdown",
        "reason": reason,
        "code": code,
        "retryable": true,
    })
    .to_string()
}

fn watcher_hydration_shutdown(reason: &str) -> String {
    watcher_resync_shutdown("WATCHER_HYDRATION_FAILED", reason)
}

fn collect_existing_parent_candidates(root: &std::path::Path) -> Result<HashSet<PathBuf>, String> {
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

fn rebase_pending_parent_candidates(
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

fn take_pending_parent_materializations(
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

fn is_synced_service_root_op(op: &Op, root: &std::path::Path) -> bool {
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

fn is_push_quiet(
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

    let service_root = project_root.join(service);
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if guard.get(current).is_some_and(|deadline| *deadline > now) {
            return true;
        }
        if current == service_root {
            break;
        }
        candidate = current.parent();
    }
    false
}

/// Watch `<project>/ro-sync.json` itself. On change, re-parse and if gameId,
/// groupId, or placeIds differ from AppState's current snapshot, update state
/// and broadcast a `{"type":"config-changed",...}` event.
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
    let prev_group_id = state.group_id.read().unwrap().clone();
    let prev_place_ids = state.place_ids.read().unwrap().clone();
    let prev_name = state.project_name.read().unwrap().clone();
    let prev_wally_enabled = *state.wally_enabled.read().unwrap();
    let prev_wally_folder = state.wally_folder.read().unwrap().clone();

    // Always mirror the saved overwrite default: deleting the field from
    // ro-sync.json is how a user clears a remembered decision, and that
    // must take effect without any other field changing.
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
struct BlockedFsDestructive {
    kind: &'static str,
    path: PathBuf,
    from: Option<PathBuf>,
}

fn handle_op(
    op: Op,
    events: &broadcast::Sender<String>,
    conflicts: &ConflictEngine,
) -> Option<BlockedFsDestructive> {
    match op.kind {
        OpKind::Add | OpKind::Update => {
            let bytes = match &op.content {
                Some(b) => b.clone(),
                None => {
                    // Directory or unreadable file — forward as-is.
                    emit_op(events, &op);
                    return None;
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
                emit_op(events, &op);
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
                    emit_op(events, &op);
                    if let Some(from) = &op.from {
                        conflicts.commit_fs_rename(from, &op.path);
                    }
                    None
                }
            }
        }
    }
}

fn emit_op(events: &broadcast::Sender<String>, op: &Op) {
    // Journal every op with a sequence number so a resuming plugin can replay
    // the frames it missed instead of paying a full re-compare.
    let Ok(value) = serde_json::to_value(op) else {
        return;
    };
    if let Some(payload) = crate::ws::journal_op_event(&value) {
        let _ = events.send(payload);
    }
}

fn emit_conflict(events: &broadcast::Sender<String>, path: &std::path::Path) {
    let payload = serde_json::json!({ "type": "conflict", "path": path });
    if let Ok(s) = serde_json::to_string(&payload) {
        let _ = events.send(s);
    }
}

fn emit_sync_error(events: &broadcast::Sender<String>, path: &std::path::Path, error: &str) {
    let payload = serde_json::json!({
        "type": "sync-error",
        "path": path,
        "error": error,
    });
    if let Ok(serialized) = serde_json::to_string(&payload) {
        let _ = events.send(serialized);
    }
}

fn fs_mtime(path: &std::path::Path) -> u64 {
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

// ---------------------------------------------------------------------------
// Tier 3 — class introspection, enum listing, attribute-scoped find.
// ---------------------------------------------------------------------------

#[derive(ClapArgs, Debug)]
pub struct ClassInfoArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct EnumArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
    #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
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
mod tier2_tests {
    use super::*;
    use clap::CommandFactory;

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
    fn service_root_quiet_entry_suppresses_descendants_only_until_deadline() {
        let root = PathBuf::from("/project");
        let quiet = Arc::new(Mutex::new(HashMap::new()));
        quiet.lock().unwrap().insert(
            root.join("Workspace"),
            Instant::now() + Duration::from_secs(1),
        );
        assert!(is_push_quiet(
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
        let blocked = handle_op(op, &events, &conflicts).expect("delete must be blocked");

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
        let blocked = handle_op(op, &events, &conflicts).expect("rename must be blocked");

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
        let error =
            match acquire_daemon_port_allocation_lock(&path, Duration::from_millis(75)).await {
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
    fn cross_manager_daemon_is_external_before_capability_validation() {
        let mut status = DaemonLifecycleStatus {
            ok: true,
            running: true,
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
    async fn daemon_start_returns_cross_manager_boot_without_testing_its_secret_or_mutating_config()
    {
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

        assert!(Cli::try_parse_from(
            ["rosync", "serve", "--project", ".", "--parent-stdin-lease",]
        )
        .is_err());
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

        let cli =
            Cli::try_parse_from(["rosync", "lint", "--owned-only", "--path", "A.luau"]).unwrap();
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
        let strings = |values: &[&str]| -> Vec<String> {
            values.iter().map(|value| value.to_string()).collect()
        };

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
            collect_lint_compile_sources(root, std::slice::from_ref(&definitions), false, &[])
                .unwrap();
        assert_eq!(sources, vec![normalize_existing_path(&definitions)]);

        let sources =
            collect_lint_compile_sources(root, std::slice::from_ref(&vendor), false, &[]).unwrap();
        assert_eq!(sources, vec![normalize_existing_path(&vendor)]);

        let sources =
            collect_lint_compile_sources(root, &[main], false, &["**/Main.luau".to_string()])
                .unwrap();
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

        let error = collect_lint_compile_sources(
            project.path(),
            &[project.path().to_path_buf()],
            false,
            &[],
        )
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
                output: "./ReplicatedStorage/RegisterLimit.luau(3,7): CompileError: exceeded limit 200\n"
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
        let filtered =
            filter_lint_output_to_targets(&root, std::slice::from_ref(&owned), plain_output);
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
        let bundle: serde_json::Value = serde_json::from_str(COMMANDS_BUNDLE_JSON).unwrap();
        let mut documented = command_names_from_bundle(&bundle);
        documented.sort();
        let mut clap_commands: Vec<String> = Cli::command()
            .get_subcommands()
            .filter(|command| !command.is_hide_set())
            .map(|command| command.get_name().to_string())
            .collect();
        clap_commands.sort();
        assert_eq!(documented, clap_commands);
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

        let cli =
            Cli::try_parse_from(["rosync", "resolve", "--path", "a.luau", "--studio"]).unwrap();
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
            collect_upload_jobs(&[dir.path().to_path_buf()], true, None, None, &mut failures)
                .unwrap();
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

        let mp3 =
            resolve_upload_media(std::path::Path::new("sound.mp3"), None, None, true).unwrap();
        assert_eq!(mp3.asset_type, UploadAssetType::Audio);
        assert_eq!(mp3.content_type, "audio/mpeg");

        let model =
            resolve_upload_media(std::path::Path::new("thing.glb"), None, None, true).unwrap();
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
    async fn capture_photo_camera_cframe_rejects_programmatic_invalid_combinations_before_connect()
    {
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
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
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
        let error = read_bounded_capture_file(&metadata, Instant::now() + Duration::from_secs(1))
            .unwrap_err();
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
        let error =
            http_get_json_until(port, "/stall", Instant::now() + Duration::from_millis(100))
                .await
                .unwrap_err();
        assert!(!error.is_empty(), "timeout should return a diagnostic");
        assert!(started.elapsed() < Duration::from_secs(1));
        server.abort();
    }
}
