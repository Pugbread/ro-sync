use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

mod conflict;
mod diff;
mod fs_map;
mod http;
mod img_upload;
mod initial_sync;
mod path_resolver;
mod project_config;
mod query;
mod remote;
mod snapshot;
mod sourcemap;
mod watch;
mod ws;

use conflict::{ConflictEngine, FsDecision};
use initial_sync::PendingInitial;
use watch::{Op, OpKind, Watch};
use ws::{PendingRoutes, RequestEnvelope};

const COMMANDS_BUNDLE_JSON: &str = include_str!("../../docs/client-commands.generated.json");

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

    #[arg(long, default_value_t = 7878)]
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
    /// Run the HTTP/WebSocket sync daemon.
    Serve(ServeArgs),
    /// Print machine-readable command docs from the generated command registry.
    Commands(CommandsArgs),
    /// Print an LLM-oriented project context snapshot as JSON.
    Context(ContextArgs),
    /// Build a read-only JSON plan for a mutating command.
    Plan(PlanArgs),
    /// Match a selector against the plugin-emitted `tree.json` skeleton.
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
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    /// Include the full command registry. Omitted by default to keep context compact.
    #[arg(long = "full-commands")]
    pub full_commands: bool,
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ConflictsArgs {
    #[arg(long, default_value_t = 7878)]
    pub port: u16,
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct ResolveArgs {
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    /// Refresh `tree.json` from the live Studio tree.
    Tree(RepairTreeArgs),
    /// Regenerate luau-lsp sourcemap JSON.
    Sourcemap(RepairSourcemapArgs),
}

#[derive(ClapArgs, Debug)]
pub struct RepairTreeArgs {
    /// Project directory. Defaults to current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
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
pub struct SnapshotArgs {
    /// Project directory used for the default output location.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    /// Environment variable that holds the Roblox Open Cloud API key or OAuth token.
    #[arg(long = "api-key-env", default_value = "ROBLOX_API_KEY")]
    pub api_key_env: String,
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
    /// Environment variable that holds the Roblox Open Cloud API key or OAuth token.
    #[arg(long = "api-key-env", default_value = "ROBLOX_API_KEY")]
    pub api_key_env: String,
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
    /// Environment variable that holds the Roblox Open Cloud API key or OAuth token.
    #[arg(long = "api-key-env", default_value = "ROBLOX_API_KEY")]
    pub api_key_env: String,
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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

#[derive(ClapArgs, Debug)]
pub struct StatusArgs {
    /// Project directory. Defaults to the current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    #[arg(long, default_value_t = 7878)]
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
    /// Add a CollectionService tag.
    Add(TagMutArgs),
    /// Remove a CollectionService tag.
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
    #[arg(long, hide = true)]
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
    #[arg(long, hide = true)]
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
    #[arg(long, default_value_t = 7878)]
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

    #[arg(long, default_value_t = 7878)]
    pub port: u16,

    #[arg(long = "game-id")]
    pub game_id: Option<String>,

    #[arg(long = "group-id")]
    pub group_id: Option<String>,

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

#[derive(ClapArgs, Debug)]
pub struct PathArgs {
    /// Project directory containing `tree.json`.
    #[arg(long)]
    pub project: PathBuf,

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
pub struct LintArgs {
    /// Project directory. Defaults to the current working directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// File or directory to analyze. May be repeated. Relative paths are
    /// resolved from `--project` when provided, otherwise from the current
    /// working directory.
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    /// Additional luau-lsp diagnostic ignore glob. May be repeated.
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
    /// Path to a luau-lsp executable. If omitted, `ROSYNC_LUAU_LSP` is checked,
    /// then `luau-lsp` is resolved from PATH.
    #[arg(long = "luau-lsp")]
    pub luau_lsp: Option<PathBuf>,
    /// Do not generate/pass a Ro-Sync sourcemap to luau-lsp.
    #[arg(long = "no-sourcemap")]
    pub no_sourcemap: bool,
    /// Extra arguments passed to `luau-lsp analyze` after `--`.
    #[arg(last = true)]
    pub extra_args: Vec<String>,
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
    pub group_id: Arc<RwLock<Option<String>>>,
    pub place_ids: Arc<RwLock<Vec<String>>>,
    pub wally_enabled: Arc<RwLock<bool>>,
    pub wally_folder: Arc<RwLock<Option<String>>>,
    pub pending_initial: Arc<Mutex<Option<PendingInitial>>>,
    /// Deadline after a user chooses "keep Studio" during initial sync.
    /// Bootstrap pushes inside this window prune disk-only paths even if an
    /// older already-loaded plugin does not send the newer strict flags.
    pub strict_bootstrap_until: Arc<Mutex<Option<Instant>>>,
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
}

/// Duration of the per-path quiet window after a `/push` write.
pub const PUSH_QUIET_MS: u64 = 1500;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Commands(args)) => run_commands(args),
        Some(Command::Context(args)) => run_context(args),
        Some(Command::Plan(args)) => run_plan(args),
        Some(Command::Query(args)) => run_query(args),
        Some(Command::Path(args)) => run_path(args),
        Some(Command::Serve(args)) => run_serve(args).await,
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
        Some(Command::Classinfo(args)) => run_classinfo(args).await,
        Some(Command::Enums(args)) => run_enums(args).await,
        Some(Command::Enum(args)) => run_enum(args).await,
        Some(Command::FindAttr(args)) => run_find_attr(args).await,
        Some(Command::Lint(args)) => run_lint(args),
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
    if let Err(e) = snapshot::write_codex_context_if_missing_or_merge(&args.project) {
        eprintln!("rosync: failed to write Codex context: {e}");
    }
    if let Err(e) = snapshot::write_project_tooling_if_missing_or_merge(&args.project) {
        eprintln!("rosync: failed to write project tooling files: {e}");
    }

    // Project config: load or create, then apply CLI overrides (persist if anything changed).
    let mut cfg = match project_config::load_or_create(&args.project) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rosync: failed to load ro-sync.json: {e}");
            project_config::ProjectConfig::default_for(&args.project)
        }
    };
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
        group_id: Arc::new(RwLock::new(cfg.group_id.clone())),
        place_ids: Arc::new(RwLock::new(cfg.place_ids.clone())),
        wally_enabled: Arc::new(RwLock::new(cfg.wally_enabled)),
        wally_folder: Arc::new(RwLock::new(cfg.wally_folder.clone())),
        pending_initial: Arc::new(Mutex::new(None)),
        strict_bootstrap_until: Arc::new(Mutex::new(None)),
        push_quiet: push_quiet.clone(),
        request_tx,
        pending_routes: Arc::new(Mutex::new(HashMap::new())),
        active_plugin: Arc::new(Mutex::new(None)),
    };

    spawn_watch_bridge(
        watcher,
        canonical_project.clone(),
        tx.clone(),
        conflict_engine.clone(),
        push_quiet.clone(),
    );
    spawn_config_hot_reload(state.clone());

    let addr = format!("127.0.0.1:{}", args.port);
    eprintln!(
        "rosync listening on http://{} (project: {})",
        addr,
        args.project.display()
    );

    let app = http::router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(serve_shutdown_signal(tx.clone()))
        .await?;
    Ok(())
}

async fn serve_shutdown_signal(events: broadcast::Sender<String>) {
    wait_for_shutdown_signal().await;
    let _ = events.send(
        serde_json::json!({
            "type": "shutdown",
            "reason": "daemon shutting down",
        })
        .to_string(),
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
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
    let tree: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", tree_path.display()))?;

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
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::Value::Array(arr))?
            );
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

fn run_path(args: PathArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resolved = path_resolver::resolve(&args.project, &args.target, args.from)?;
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
    let canonical_project = std::fs::canonicalize(&project).unwrap_or_else(|_| project.clone());
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
            "cheapFirst": ["query", "path", "meta", "services", "status --raw"],
            "targetedReads": ["get --prop", "props", "source --disk", "source"],
            "expensiveReads": ["changes", "diff --raw", "snapshot", "get without --prop", "logs --tail", "watch"],
            "mutationRule": "Use `rosync plan` before supported writes and explicit preflight/list commands before upload or monetization writes."
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
            "tree": context_tree_summary(&project),
            "files": context_project_files(&project),
            "conflicts": conflicts,
        },
        "commands": commands,
        "nextActions": [
            "Use `rosync commands <name>` for exact command usage JSON.",
            "Use `rosync plan <operation>` before mutating Studio or disk from an LLM workflow.",
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
        let api_key = resolve_img_api_key(&args.api_key_env)?;

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

    for env_name in &env_names {
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    let env_values = read_project_env_values(project);
    for env_name in &env_names {
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
        "monetization: missing Roblox Open Cloud API key. Set one of {}, add it to an env file, or save it in Ro Sync Settings > Secrets.",
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
        sources.push("widget-secret:robloxCloudApiKey".to_string());
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

fn run_lint(args: LintArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = match args.project {
        Some(p) => p,
        None => std::env::current_dir().map_err(|e| format!("lint: current directory: {e}"))?,
    };
    if !project.exists() {
        return Err(format!("lint: project path does not exist: {}", project.display()).into());
    }
    if !project.is_dir() {
        return Err(format!(
            "lint: project path is not a directory: {}",
            project.display()
        )
        .into());
    }

    if args.scope_only && args.paths.is_empty() {
        return Err("lint: --scope-only requires at least one --path".into());
    }

    let targets = if args.paths.is_empty() {
        vec![project.clone()]
    } else {
        args.paths
            .iter()
            .map(|path| lint_target_path(&project, path))
            .collect()
    };
    for target in &targets {
        if !target.exists() {
            return Err(format!("lint: path does not exist: {}", target.display()).into());
        }
    }

    let luau_lsp = resolve_luau_lsp(args.luau_lsp);
    let sourcemap = if args.no_sourcemap || extra_args_include_sourcemap(&args.extra_args) {
        None
    } else {
        Some(write_temp_sourcemap(&project)?)
    };
    let definitions = if extra_args_include_definitions(&args.extra_args) {
        None
    } else {
        find_bundled_luau_definitions()
    };
    let mut cmd = std::process::Command::new(&luau_lsp);
    cmd.arg("analyze");
    if let Some(path) = &sourcemap {
        cmd.arg(format!("--sourcemap={}", path.display()));
    }
    if let Some(path) = &definitions {
        cmd.arg(format!("--definitions={}", path.display()));
    }

    if !args.no_vendor_ignores && !extra_args_include_ignore(&args.extra_args) {
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

    let capture_output = args.scope_only || args.summary;
    let status = if capture_output {
        let output = match cmd.output() {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cleanup_sourcemap(&sourcemap);
                print_luau_lsp_missing(&luau_lsp);
                std::process::exit(127);
            }
            Err(e) => {
                cleanup_sourcemap(&sourcemap);
                return Err(
                    format!("lint: failed to run {}: {e}", luau_lsp.to_string_lossy()).into(),
                );
            }
        };
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        let rendered = if args.scope_only {
            filter_lint_output_to_targets(&project, &targets, &combined)
        } else {
            combined
        };
        print!("{rendered}");
        if args.summary {
            print_lint_summary(&rendered);
        }
        output.status
    } else {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        match cmd.status() {
            Ok(status) => status,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cleanup_sourcemap(&sourcemap);
                print_luau_lsp_missing(&luau_lsp);
                std::process::exit(127);
            }
            Err(e) => {
                cleanup_sourcemap(&sourcemap);
                return Err(
                    format!("lint: failed to run {}: {e}", luau_lsp.to_string_lossy()).into(),
                );
            }
        }
    };

    cleanup_sourcemap(&sourcemap);
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

const DEFAULT_LINT_VENDOR_IGNORES: &[&str] = &[
    "**/Packages/**",
    "**/_Index/**",
    "**/Madwork*/**",
    "**/PlayerModule/**",
    "**/node_modules/**",
    "**/.codex/**",
    "**/.vscode/**",
    "**/tools/**",
];

fn lint_target_path(project: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project.join(path)
    }
}

fn extra_args_include_ignore(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--ignore" || arg.starts_with("--ignore="))
}

#[derive(Debug)]
struct LintDiagnostic {
    path: PathBuf,
    file_label: String,
    category: String,
}

fn filter_lint_output_to_targets(
    project: &std::path::Path,
    targets: &[PathBuf],
    output: &str,
) -> String {
    let scopes: Vec<PathBuf> = targets
        .iter()
        .map(|target| normalize_existing_path(target))
        .collect();
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

fn normalize_existing_path(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn parse_lint_diagnostic(project: &std::path::Path, line: &str) -> Option<LintDiagnostic> {
    let (file_part, rest) = line.split_once('(')?;
    let (location, message) = rest.split_once("): ")?;
    if !location
        .split(',')
        .all(|part| part.chars().all(|ch| ch.is_ascii_digit()))
    {
        return None;
    }
    let (category, _) = message.split_once(':')?;
    let file_path = std::path::Path::new(file_part);
    let absolute = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        project.join(file_path)
    };
    Some(LintDiagnostic {
        path: normalize_existing_path(&absolute),
        file_label: file_part.to_string(),
        category: category.trim().to_string(),
    })
}

fn print_lint_summary(output: &str) {
    let mut by_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_file: BTreeMap<String, usize> = BTreeMap::new();
    for line in output.lines() {
        let Some(diag) = parse_lint_diagnostic(std::path::Path::new("."), line) else {
            continue;
        };
        *by_category.entry(diag.category).or_insert(0) += 1;
        *by_file.entry(diag.file_label).or_insert(0) += 1;
    }
    let total: usize = by_category.values().sum();
    if total == 0 {
        println!("\nSummary: 0 diagnostics");
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
}

fn plural_s(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn write_temp_sourcemap(project: &std::path::Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let map = sourcemap::generate(project)?;
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

fn extra_args_include_definitions(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "--definitions"
            || arg.starts_with("--definitions=")
            || arg == "--defs"
            || arg.starts_with("--defs=")
    })
}

fn cleanup_sourcemap(path: &Option<PathBuf>) {
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
    OsString::from("luau-lsp")
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

fn find_bundled_luau_definitions() -> Option<PathBuf> {
    let rel = PathBuf::from("tools")
        .join("luau-lsp")
        .join("roblox")
        .join("globalTypes.d.luau");
    find_in_tool_bases(&rel)
}

fn resolve_img_api_key(env_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(value) = std::env::var(env_name) {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Ok(value);
        }
    }

    if let Some(value) = find_widget_secret("robloxCloudApiKey") {
        return Ok(value);
    }

    Err(format!(
        "upload: missing Roblox Open Cloud credential. Set {env_name}, or save it in the Ro Sync widget Settings > Secrets."
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

    let mut files = Vec::new();
    for base in bases {
        let candidate = base.join("state.json");
        if !files.contains(&candidate) {
            files.push(candidate);
        }
    }
    files
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
        let candidate = base.join(&rel);
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
        eprintln!("");
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
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| {
        format!(
            "parse {}: {e} (expected a JSON array)",
            batch_path.display()
        )
    })?;
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
    let err = resp
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("request failed");
    Err(format!("{context}: {err}").into())
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
    for path in diff::studio_script_paths(&studio) {
        let resp = remote::request(
            args.port,
            "get",
            serde_json::json!({ "path": path, "prop": "Source" }),
        )
        .await?;
        let source_value = response_value_or_err(&resp, &format!("diff get {path}.Source"))?;
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
        if let Ok(resolved) =
            path_resolver::resolve(project, &args.target, path_resolver::PathInputKind::Auto)
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
            serde_json::Value::String(
                resp.get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("live search failed")
                    .to_string(),
            ),
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
        let resolved =
            path_resolver::resolve(&project, &args.path, path_resolver::PathInputKind::Auto)
                .map_err(|e| format!("source: {e}"))?;
        let source_path = disk_source_path(&resolved.fs_path)
            .ok_or_else(|| format!("source: no source file at {}", resolved.fs_path.display()))?;
        let source = std::fs::read_to_string(&source_path)
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
    let resolved = path_resolver::resolve(&project, &args.target, args.from)
        .map_err(|e| format!("meta: {e}"))?;
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
            serde_json::json!({
                "name": service,
                "disk": path.is_dir(),
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
    let project = project_or_cwd(args.project.as_deref(), "repair tree")?;
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
    let output = project.join(snapshot::TREE_JSON);
    std::fs::write(
        &output,
        format!("{}\n", serde_json::to_string_pretty(&tree)?),
    )
    .map_err(|e| format!("repair tree: write {}: {e}", output.display()))?;
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
    print_response(&resp, args.raw, |v| print_find(v));
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
        let err = ping_resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        if args.raw {
            println!(
                "{}",
                serde_json::to_string_pretty(&ping_resp).unwrap_or_default()
            );
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
                    serde_json::json!({ "daemon": daemon, "plugin": null, "error": e }).to_string()
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
            serde_json::json!({ "daemon": daemon, "plugin": value }).to_string()
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
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("plugin request failed");
        return Err(err.to_string());
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
        check_tree_json(&project),
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

    let project_ok = project.is_dir();
    checks.push(check_project_path(&project));
    checks.push(check_project_config(&project));
    checks.push(check_tree_json(&project));
    checks.push(check_sourcemap(&project));
    checks.push(check_daemon_hello(args.port));
    checks.push(check_luau_lsp());
    checks.push(check_luau_definitions());
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

    let claude_existed = project.join(snapshot::CLAUDE_MD).exists();
    let claude_changed = snapshot::write_claude_md_if_missing_or_merge(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::CLAUDE_MD,
        status: refresh_file_status(claude_existed, claude_changed),
        note: Some("custom content preserved; @AGENTS.md ensured"),
    });

    let codex_config_path = project
        .join(snapshot::CODEX_DIR)
        .join(snapshot::CODEX_CONFIG_TOML);
    let codex_config_existed = codex_config_path.exists();
    let codex_config_changed = snapshot::write_codex_config_if_missing_or_merge(&project)?;
    files.push(RefreshFileStatus {
        path: ".codex/config.toml",
        status: refresh_file_status(codex_config_existed, codex_config_changed),
        note: Some("project doc fallbacks merged"),
    });

    let agents_existed = project.join(snapshot::AGENTS_MD).exists();
    let agents_changed = snapshot::write_agents_md_if_missing_or_merge(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::AGENTS_MD,
        status: refresh_file_status(agents_existed, agents_changed),
        note: Some("only the Ro Sync marker block was regenerated"),
    });

    let stylua_existed = project.join(snapshot::STYLUA_TOML).exists();
    let stylua_changed = snapshot::write_stylua_toml_if_missing(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::STYLUA_TOML,
        status: refresh_file_status(stylua_existed, stylua_changed),
        note: Some("Luau formatter config ensured"),
    });

    let aftman_existed = project.join(snapshot::AFTMAN_TOML).exists();
    let aftman_changed = snapshot::write_aftman_stylua_if_missing_or_merge(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::AFTMAN_TOML,
        status: refresh_file_status(aftman_existed, aftman_changed),
        note: Some("StyLua tool pin ensured"),
    });

    let definitions_existed = project.join(snapshot::ROBLOX_DEFINITIONS_PATH).exists();
    let definitions_changed = snapshot::write_roblox_definitions_if_missing_or_update(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::ROBLOX_DEFINITIONS_PATH,
        status: refresh_file_status(definitions_existed, definitions_changed),
        note: Some("Roblox Luau definitions ensured"),
    });

    let luaurc_existed = project.join(snapshot::LUAURC).exists();
    let luaurc_changed = snapshot::write_luaurc_if_missing_or_cleanup(&project)?;
    files.push(RefreshFileStatus {
        path: snapshot::LUAURC,
        status: refresh_file_status(luaurc_existed, luaurc_changed),
        note: Some("luau-lsp Roblox definitions wired"),
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
        "tree.json" => "tree",
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
        "tree.json" | "sourcemap" => {
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
    if !project.exists() {
        return doctor_check(
            "project",
            DoctorStatus::Fail,
            format!("missing: {}", project.display()),
        );
    }
    if !project.is_dir() {
        return doctor_check(
            "project",
            DoctorStatus::Fail,
            format!("not a directory: {}", project.display()),
        );
    }
    match std::fs::canonicalize(project) {
        Ok(path) => doctor_check("project", DoctorStatus::Ok, path.display().to_string()),
        Err(e) => doctor_check(
            "project",
            DoctorStatus::Warn,
            format!("exists, but canonicalize failed: {e}"),
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

fn check_tree_json(project: &std::path::Path) -> DoctorCheck {
    let path = project.join(snapshot::TREE_JSON);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return doctor_check("tree.json", DoctorStatus::Warn, "missing");
        }
        Err(e) => return doctor_check("tree.json", DoctorStatus::Fail, format!("read: {e}")),
    };
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
        return doctor_check("tree.json", DoctorStatus::Fail, format!("parse: {e}"));
    }
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(format_duration_short)
        .unwrap_or_else(|| "unknown age".into());
    doctor_check("tree.json", DoctorStatus::Ok, format!("valid ({age} old)"))
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

fn check_luau_lsp() -> DoctorCheck {
    let luau_lsp = resolve_luau_lsp(None);
    match std::process::Command::new(&luau_lsp)
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            doctor_check(
                "luau-lsp",
                DoctorStatus::Ok,
                format!("{} ({version})", luau_lsp.to_string_lossy()),
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

fn check_luau_definitions() -> DoctorCheck {
    match find_bundled_luau_definitions() {
        Some(path) => doctor_check("roblox defs", DoctorStatus::Ok, path.display().to_string()),
        None => doctor_check("roblox defs", DoctorStatus::Warn, "not bundled"),
    }
}

fn check_writes_log_path() -> DoctorCheck {
    let Some(home) = dirs::home_dir() else {
        return doctor_check("writes.log", DoctorStatus::Warn, "home directory not found");
    };
    let dir = home.join(".terminal64").join("widgets").join("ro-sync");
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
    doctor_check(
        "writes.log",
        DoctorStatus::Warn,
        format!("directory missing: {}", dir.display()),
    )
}

fn fetch_daemon_hello(port: u16) -> Result<serde_json::Value, String> {
    http_get_json(port, "/hello")
}

fn http_get_json(port: u16, path: &str) -> Result<serde_json::Value, String> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = Duration::from_millis(750);
    let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("connect http://127.0.0.1:{port}{path}: {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;
    let mut resp = String::new();
    stream
        .read_to_string(&mut resp)
        .map_err(|e| format!("read response: {e}"))?;
    let mut parts = resp.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        let status = head.lines().next().unwrap_or("no HTTP status");
        return Err(status.to_string());
    }
    serde_json::from_str(body).map_err(|e| format!("parse response JSON: {e}"))
}

async fn http_post_json(
    port: u16,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = resp.status();
    let value = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("parse response JSON: {e}"))?;
    if !status.is_success() {
        return Err(format!("{status}: {value}"));
    }
    Ok(value)
}

fn format_duration_short(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
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
    let daemon_canonical =
        std::fs::canonicalize(daemon_path).unwrap_or_else(|_| daemon_path.to_path_buf());
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
        rows.push(serde_json::json!({
            "name": command_name,
            "category": command.get("category").and_then(|value| value.as_str()).unwrap_or(""),
            "summary": command.get("description").and_then(|value| value.as_str()).unwrap_or(""),
            "outputCost": command_output_cost(command_name),
            "safety": command_safety_class(command_name),
            "requires": command_requirements(command_name),
            "preferBefore": command_prefer_before(command_name),
            "usageLookup": format!("rosync commands {command_name}"),
        }));
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
        "context" | "plan" | "query" | "path" | "meta" | "services" | "where" | "open"
        | "classinfo" | "enums" | "enum" | "ping" | "version" => "low",
        "status" | "doctor" | "ls" | "tree" | "props" | "source" | "find" | "find-attr"
        | "logs" | "conflicts" | "resolve" | "lint" | "upload" | "monetization" | "set" | "new"
        | "rm" | "mv" | "attr" | "tag" | "select" | "save" | "waypoint" | "undo" | "redo"
        | "refresh" => "medium",
        "diff" | "changes" | "snapshot" | "get" | "eval" | "transmit" | "call" | "tail"
        | "watch" => "high-or-streaming",
        _ => "unknown",
    }
}

fn command_safety_class(name: &str) -> &'static str {
    match name {
        "set" | "new" | "rm" | "mv" | "attr" | "tag" | "save" | "waypoint" | "undo" | "redo" => {
            "mutates-studio"
        }
        "resolve" => "mutates-disk-or-studio",
        "eval" | "call" | "transmit" => "risky-live-execution",
        "select" | "open" => "mutates-studio-selection",
        "upload" | "monetization" => "open-cloud-mutating",
        "refresh" | "snapshot" => "writes-local-files",
        "tail" | "watch" => "streaming-read",
        _ => "read-only",
    }
}

fn command_requirements(name: &str) -> Vec<&'static str> {
    match name {
        "query" | "path" | "meta" | "services" | "source" | "lint" => vec!["project"],
        "upload" | "monetization" => vec!["project", "roblox-open-cloud-credential"],
        "commands" | "context" | "plan" | "snapshot" | "diff" | "changes" | "status" | "doctor"
        | "refresh" => vec!["project"],
        _ => vec!["daemon", "studio-plugin"],
    }
}

fn command_prefer_before(name: &str) -> Vec<&'static str> {
    match name {
        "get" | "props" => vec!["meta", "get --prop when possible"],
        "source" => vec![
            "meta",
            "source --disk before live source when checking local code",
        ],
        "diff" | "changes" => vec!["status --raw", "services --raw"],
        "snapshot" => vec!["tree --depth 3", "changes"],
        "set" | "new" | "rm" | "mv" => vec!["plan"],
        "resolve" => vec!["conflicts", "changes", "plan resolve"],
        "attr" | "tag" | "call" | "eval" | "transmit" | "select" | "save" => {
            vec!["status --raw", "waypoint for multi-step edits"]
        }
        "upload" => vec!["enumerate exact files", "use --manifest for bulk uploads"],
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
            serde_json::json!({
                "name": service,
                "diskPath": path.display().to_string(),
                "exists": path.is_dir(),
            })
        })
        .collect()
}

fn context_tree_summary(project: &std::path::Path) -> serde_json::Value {
    let path = project.join(snapshot::TREE_JSON);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return serde_json::json!({ "ok": false, "missing": true, "path": path.display().to_string() });
        }
        Err(e) => {
            return serde_json::json!({ "ok": false, "error": e.to_string(), "path": path.display().to_string() });
        }
    };
    let tree: serde_json::Value = match serde_json::from_str(&text) {
        Ok(tree) => tree,
        Err(e) => {
            return serde_json::json!({ "ok": false, "error": format!("parse: {e}"), "path": path.display().to_string() });
        }
    };
    let services = tree
        .get("children")
        .and_then(|value| value.as_array())
        .map(|children| {
            children
                .iter()
                .filter_map(|child| child.get("name").and_then(|value| value.as_str()))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "nodes": count_tree_nodes(&tree),
        "services": services,
    })
}

fn count_tree_nodes(node: &serde_json::Value) -> usize {
    1 + node
        .get("children")
        .and_then(|value| value.as_array())
        .map(|children| children.iter().map(count_tree_nodes).sum::<usize>())
        .unwrap_or(0)
}

fn context_project_files(project: &std::path::Path) -> serde_json::Value {
    serde_json::json!({
        "projectConfig": file_summary(&project.join(project_config::CONFIG_FILE)),
        "treeJson": file_summary(&project.join(snapshot::TREE_JSON)),
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

fn disk_source_path(path: &std::path::Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if !path.is_dir() {
        return None;
    }
    let entries = std::fs::read_dir(path).ok()?;
    let mut candidates = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.into_iter().find(|child| {
        child
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(crate::fs_map::parse_init_file)
            .is_some()
    })
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

/// Bridges the filesystem-watcher's `broadcast::Sender<Op>` into the shared
/// `broadcast::Sender<String>` that `/events` streams. Each Op is first run
/// through `ConflictEngine::on_fs_change` so that echoes of our own writes
/// (baseline matches) are dropped and conflicts are surfaced as their own
/// event type rather than a propagation op.
fn spawn_watch_bridge(
    watcher: Watch,
    root: PathBuf,
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
                    if is_synced_service_root_op(&op, &root) {
                        continue;
                    }
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
    snapshot::SYNCED_SERVICES
        .iter()
        .any(|service| *service == name)
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
    fn synced_service_root_directory_ops_are_filtered() {
        let root = PathBuf::from("ro-sync-test-project");
        let service_op = Op {
            kind: OpKind::Update,
            path: root.join("ReplicatedStorage"),
            from: None,
            content: None,
        };
        let script_op = Op {
            kind: OpKind::Update,
            path: root.join("ReplicatedStorage").join("Client.luau"),
            from: None,
            content: Some(b"return {}".to_vec()),
        };

        assert!(is_synced_service_root_op(&service_op, &root));
        assert!(!is_synced_service_root_op(&script_op, &root));
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

        let cli =
            Cli::try_parse_from(["rosync", "lint", "--owned-only", "--path", "A.luau"]).unwrap();
        let Some(Command::Lint(args)) = cli.command else {
            panic!("expected lint command");
        };
        assert!(args.scope_only);
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
        let filtered = filter_lint_output_to_targets(&root, &[owned], output);
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
    fn status_json_uses_stable_keys_and_flags() {
        assert_eq!(status_json_key("project"), "project_path");
        assert_eq!(status_json_key("ro-sync.json"), "project_config");
        assert_eq!(status_json_key("tree.json"), "tree");
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
        assert_eq!(args.api_key_env, "ROBLOX_OAUTH_TOKEN");
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
        assert_eq!(args.api_key_env, "ROBLOX_OAUTH_TOKEN");
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
}
