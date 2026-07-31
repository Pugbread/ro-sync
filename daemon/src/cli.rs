use super::DEFAULT_DAEMON_PORT;
use crate::{img_upload, path_resolver, playtest_run, studio_clipboard};
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

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
    pub(crate) fn as_plugin_str(self) -> &'static str {
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
    pub(crate) fn as_plugin_str(self) -> &'static str {
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
    pub(crate) fn as_plugin_str(self) -> &'static str {
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
    pub(crate) fn as_wire_str(self) -> &'static str {
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
    pub(crate) fn as_wire_str(self) -> &'static str {
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
    pub(crate) fn as_plugin_str(self) -> &'static str {
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
    pub(crate) fn as_plugin_str(self) -> &'static str {
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
    pub(crate) fn as_upload_mode(self) -> img_upload::AuthMode {
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
    pub(crate) fn as_cloud_str(self) -> &'static str {
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
pub(crate) enum MonetizationKind {
    Gamepass,
    Product,
}

impl MonetizationKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Gamepass => "gamepass",
            Self::Product => "product",
        }
    }

    pub(crate) fn id_field(self) -> &'static str {
        match self {
            Self::Gamepass => "gamePassId",
            Self::Product => "productId",
        }
    }

    pub(crate) fn create_image_field(self) -> &'static str {
        "imageFile"
    }

    pub(crate) fn update_image_field(self) -> &'static str {
        match self {
            Self::Gamepass => "file",
            Self::Product => "imageFile",
        }
    }

    pub(crate) fn base_url(self, universe_id: &str) -> String {
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
    pub(crate) fn as_plugin_str(self) -> &'static str {
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
    pub(crate) fn as_str(self) -> &'static str {
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
    pub(crate) fn as_str(self) -> &'static str {
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
