//! Offline inspection and recovery for filesystem projection conflicts which
//! otherwise prevent the daemon from starting.
//!
//! This module deliberately has no daemon or Studio dependency. Every scan is
//! rooted at an exact allowlisted synced service, never follows a link/reparse
//! point, and produces bounded source previews. Resolution always begins with
//! a fresh scan and addresses an opaque content-derived conflict id so a UI
//! cannot apply a decision to files that changed after inspection.

use crate::fs_map;
use crate::fs_safety::{
    self, metadata_no_follow, read_file_no_follow_bounded, SafeEntryKind, MAX_SERVICE_TREE_DEPTH,
    MAX_SYNCED_SCRIPT_BYTES, SYNCED_SERVICES,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::ffi::CString;
use std::fs;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::io::Write as _;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::{Read as _, Seek as _};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::fd::{AsRawFd as _, FromRawFd as _};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CONFLICT_ID_VERSION: &str = "rosync-projection-conflict-v1";
const MAX_REPAIR_SCAN_NODES: usize = 1_000_000;
const MAX_VISIBLE_CONFLICTS: usize = 128;
const MAX_MARKERS_PER_CONFLICT: usize = 32;
const MAX_PREVIEW_BYTES_PER_FILE: usize = 4 * 1024;
const MAX_TOTAL_PREVIEW_BYTES: usize = 512 * 1024;
// The desktop host accepts at most 2 MiB from the sidecar. Keep the compact
// conflict payload under 1 MiB so pretty JSON, the result envelope, and future
// additive fields retain comfortable headroom.
const MAX_STRUCTURED_CONFLICT_BYTES: usize = 1024 * 1024;
const MAX_TRANSACTION_MANIFEST_BYTES: usize = 512 * 1024;
const MAX_RECOVERY_TRANSACTIONS: usize = 4096;
const MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 128;
const BACKUP_ROOT: &str = ".rosync-backups";
const TRANSACTION_VERSION: u32 = 1;
const PREPARED_RECOVERY_PROTOCOL: &str = "A prepared receipt is non-terminal and must be treated as recovery-required. First verify kept.sha256 at kept.path without following links; a missing or mismatched kept file requires manual recovery. For every move, verify the recorded SHA-256 at originalPath and destinationPath without following links: source-only means pending, destination-only means moved, and both-or-neither requires manual recovery. Never overwrite either path.";

const MULTIPLE_INIT_MARKERS: &str = "multiple-init-markers";
const LEGACY_RESERVED_INIT_LEAF: &str = "legacy-reserved-init-leaf";

#[derive(Debug, Clone)]
pub struct ProjectionRepairError {
    code: &'static str,
    message: String,
}

impl ProjectionRepairError {
    pub(crate) fn classify(message: String) -> Self {
        let code = if message.contains("PROJECTION_RECOVERY_REQUIRED") {
            "PROJECTION_RECOVERY_REQUIRED"
        } else if message.contains("UNSUPPORTED_SECURE_PROJECTION_RESOLVE") {
            "UNSUPPORTED_SECURE_PROJECTION_RESOLVE"
        } else if message.contains("stale")
            || message.contains("changed during resolution")
            || message.contains("inspect again")
        {
            "STALE_PROJECTION_CONFLICT"
        } else if message.contains("another projection resolution")
            || message.contains("already in progress")
        {
            "PROJECTION_RESOLVE_BUSY"
        } else if message.contains("maximum")
            || message.contains("limit")
            || message.contains("too much data")
        {
            "PROJECTION_SCAN_LIMIT"
        } else if message.contains("symbolic link")
            || message.contains("linked/reparse")
            || message.contains("outside")
            || message.contains("escaped")
            || message.contains("collision")
        {
            "UNSAFE_PROJECTION_PATH"
        } else {
            "PROJECTION_REPAIR_FAILED"
        };
        Self {
            code,
            message: bounded_error(&message),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    #[cfg(test)]
    fn contains(&self, pattern: &str) -> bool {
        self.message.contains(pattern)
    }
}

impl std::fmt::Display for ProjectionRepairError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProjectionRepairError {}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionFile {
    pub name: String,
    pub path: String,
    pub style: String,
    pub class_name: String,
    pub size: u64,
    pub sha256: String,
    pub preview: String,
    pub preview_truncated: bool,
    pub utf8: bool,
    #[serde(skip)]
    pub generation: fs_safety::FileGeneration,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionConflict {
    pub id: String,
    pub kind: String,
    pub directory: String,
    pub files: Vec<ProjectionFile>,
    pub identical: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<String>,
    #[serde(skip)]
    pub directory_generation: fs_safety::FileGeneration,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionScan {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub project: String,
    pub conflicts: Vec<ProjectionConflict>,
    /// Total unresolved conflicts in the complete bounded filesystem scan.
    pub remaining: usize,
    pub total_conflicts: usize,
    pub counts_known: bool,
    /// True when `conflicts` contains only the first bounded page.
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ProjectionResolution>,
    #[serde(skip_serializing_if = "is_zero")]
    pub recovery_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionResolution {
    pub id: String,
    pub kind: String,
    pub kept_file: String,
    pub backup_paths: Vec<String>,
    pub receipt_path: String,
    pub receipt_available: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recovery_actions: Vec<String>,
    pub recovery_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionResolveResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub project: String,
    pub resolution: ProjectionResolution,
    pub conflicts: Vec<ProjectionConflict>,
    pub remaining: usize,
    pub total_conflicts: usize,
    pub counts_known: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionFile {
    path: String,
    name: String,
    size: u64,
    sha256: String,
    generation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionMove {
    operation: String,
    original_path: String,
    destination_path: String,
    size: u64,
    sha256: String,
    status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionManifest {
    version: u32,
    state: String,
    conflict_id: String,
    kind: String,
    project: String,
    directory: String,
    prepared_at_ms: u128,
    kept: TransactionFile,
    moves: Vec<TransactionMove>,
    recovery_protocol: String,
    recovery_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconciles_receipt_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconciled_at_ms: Option<u128>,
}

#[derive(Debug, Clone)]
struct PendingRecovery {
    id: String,
    transaction_relative: PathBuf,
    kind: String,
    receipt_path: String,
    receipt_available: bool,
    error: String,
    generation: fs_safety::FileGeneration,
    resume_manifest: Option<TransactionManifest>,
    receipt_file: Option<String>,
    receipt_sha256: Option<String>,
    quarantine_allowed: bool,
}

#[derive(Debug)]
enum ManifestRead {
    Missing,
    Valid {
        manifest: Box<TransactionManifest>,
        sha256: String,
    },
    Invalid(String),
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct DurableReceipt {
    path: String,
    file_name: String,
    conflict_id: String,
    generation: fs_safety::FileGeneration,
    bytes: Vec<u8>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct CommitOutcome {
    receipt_path: String,
    receipt_available: bool,
    recovery_required: bool,
    recovery_error: Option<String>,
}

#[derive(Debug)]
struct ScanCollector {
    conflicts: Vec<ProjectionConflict>,
    total_conflicts: usize,
    nodes: usize,
    preview_budget: usize,
    structured_bytes: usize,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct SecureDirectory {
    relative: PathBuf,
    handle: fs::File,
    generation: fs_safety::FileGeneration,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct SecureProjectMutation {
    project: PathBuf,
    root: SecureDirectory,
}

#[cfg(test)]
#[derive(Debug)]
pub struct ProjectOperationLock {
    _file: fs::File,
}

#[cfg(not(test))]
pub type ProjectOperationLock = crate::lifecycle::StartLock;

#[cfg(test)]
static AFTER_PREPARED_BARRIER: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Barrier>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static AFTER_MANIFEST_PUBLISH_BARRIER: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Barrier>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static BEFORE_POSTSCAN_BARRIER: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Barrier>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static AFTER_RECEIPT_FIRST_OPEN_BARRIER: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Barrier>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static FAIL_AFTER_MANIFEST_RENAME: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static FAIL_MANIFEST_QUARANTINE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn test_pause_after_prepared(conflict_id: &str) {
    let barrier = AFTER_PREPARED_BARRIER
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap()
        .get(conflict_id)
        .map(std::sync::Arc::clone);
    if let Some(barrier) = barrier {
        barrier.wait();
        barrier.wait();
    }
}

#[cfg(test)]
fn test_pause_after_manifest_publish(conflict_id: &str, file_name: &str) {
    let key = format!("{conflict_id}:{file_name}");
    let barrier = AFTER_MANIFEST_PUBLISH_BARRIER
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap()
        .remove(&key);
    if let Some(barrier) = barrier {
        barrier.wait();
        barrier.wait();
    }
}

#[cfg(test)]
fn test_pause_before_postscan(conflict_id: &str) {
    let barrier = BEFORE_POSTSCAN_BARRIER
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap()
        .get(conflict_id)
        .map(std::sync::Arc::clone);
    if let Some(barrier) = barrier {
        barrier.wait();
        barrier.wait();
    }
}

#[cfg(test)]
fn test_pause_after_receipt_first_open(conflict_id: &str, file_name: &str) {
    let key = format!("{conflict_id}:{file_name}");
    let barrier = AFTER_RECEIPT_FIRST_OPEN_BARRIER
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap()
        .remove(&key);
    if let Some(barrier) = barrier {
        barrier.wait();
        barrier.wait();
    }
}

#[cfg(test)]
fn test_fail_after_manifest_rename(conflict_id: &str, file_name: &str) -> Result<(), String> {
    let key = format!("{conflict_id}:{file_name}");
    if FAIL_AFTER_MANIFEST_RENAME
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap()
        .remove(&key)
    {
        Err("injected post-rename manifest publication failure".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn test_fail_manifest_quarantine(conflict_id: &str, file_name: &str) -> Result<(), String> {
    let key = format!("{conflict_id}:{file_name}");
    if FAIL_MANIFEST_QUARANTINE
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap()
        .contains(&key)
    {
        Err("injected manifest quarantine failure".to_string())
    } else {
        Ok(())
    }
}

impl ScanCollector {
    fn new() -> Self {
        Self {
            conflicts: Vec::new(),
            total_conflicts: 0,
            nodes: 0,
            preview_budget: MAX_TOTAL_PREVIEW_BYTES,
            structured_bytes: 0,
        }
    }

    fn record<F>(&mut self, build: F) -> Result<(), String>
    where
        F: FnOnce(&mut usize) -> Result<ProjectionConflict, String>,
    {
        self.total_conflicts = self
            .total_conflicts
            .checked_add(1)
            .ok_or_else(|| "projection conflict count overflow".to_string())?;
        if self.conflicts.len() < MAX_VISIBLE_CONFLICTS {
            let conflict = build(&mut self.preview_budget)?;
            let encoded_bytes = serde_json::to_vec(&conflict)
                .map_err(|error| format!("encode projection conflict: {error}"))?
                .len();
            if self
                .structured_bytes
                .checked_add(encoded_bytes)
                .is_some_and(|total| total <= MAX_STRUCTURED_CONFLICT_BYTES)
            {
                self.structured_bytes += encoded_bytes;
                self.conflicts.push(conflict);
            }
        }
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl SecureProjectMutation {
    fn open(project: &Path) -> Result<Self, String> {
        let expected = fs_safety::directory_generation_no_follow(project)
            .map_err(|error| format!("capture canonical project identity: {error}"))?;
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let handle = options
            .open(project)
            .map_err(|error| format!("open canonical project directory securely: {error}"))?;
        let opened = opened_generation(&handle)?;
        let after = fs_safety::directory_generation_no_follow(project)
            .map_err(|error| format!("recheck canonical project identity: {error}"))?;
        if opened != expected || after != expected {
            return Err(
                "canonical project directory changed while opening its secure mutation handle"
                    .to_string(),
            );
        }
        Ok(Self {
            project: project.to_path_buf(),
            root: SecureDirectory {
                relative: PathBuf::new(),
                handle,
                generation: opened,
            },
        })
    }

    fn open_directory(&self, relative: &Path) -> Result<SecureDirectory, String> {
        let mut current = self
            .root
            .handle
            .try_clone()
            .map_err(|error| format!("clone secure project root handle: {error}"))?;
        let components = normal_relative_components(relative)?;
        if components.len() > MAX_SERVICE_TREE_DEPTH {
            return Err(format!(
                "secure mutation path exceeds maximum depth {MAX_SERVICE_TREE_DEPTH}: {}",
                relative.display()
            ));
        }
        for component in components {
            current = open_directory_at(&current, component)?;
        }
        let generation = opened_generation(&current)?;
        Ok(SecureDirectory {
            relative: relative.to_path_buf(),
            handle: current,
            generation,
        })
    }

    fn verify_directory(
        &self,
        relative: &Path,
        expected: &fs_safety::FileGeneration,
    ) -> Result<SecureDirectory, String> {
        let directory = self.open_directory(relative)?;
        if &directory.generation != expected {
            return Err(format!(
                "projection conflict directory changed during resolution: {}; inspect again",
                relative.display()
            ));
        }
        Ok(directory)
    }

    fn verify_namespace_binding(
        &self,
        relative: &Path,
        held: &SecureDirectory,
    ) -> Result<(), String> {
        self.verify_project_root_binding()?;
        let rebound = self.open_directory(relative).map_err(|error| {
            format!(
                "projection namespace binding changed at {}: {error}",
                relative.display()
            )
        })?;
        if rebound.generation.identity != held.generation.identity {
            return Err(format!(
                "projection namespace now resolves to a different directory at {}; recovery required",
                relative.display()
            ));
        }
        Ok(())
    }

    fn verify_project_root_binding(&self) -> Result<(), String> {
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let rebound = options.open(&self.project).map_err(|error| {
            format!(
                "canonical project root namespace binding changed at {}: {error}",
                self.project.display()
            )
        })?;
        let generation = opened_generation(&rebound)?;
        if generation.identity != self.root.generation.identity {
            return Err(format!(
                "canonical project root now resolves to a different directory at {}; recovery required",
                self.project.display()
            ));
        }
        Ok(())
    }

    fn create_transaction_directory(&self, conflict_id: &str) -> Result<SecureDirectory, String> {
        self.verify_project_root_binding()?;
        let backup_name = std::ffi::OsStr::new(BACKUP_ROOT);
        require_exact_internal_entry_spelling(&self.root.handle, BACKUP_ROOT, true)?;
        match mkdir_at(&self.root.handle, backup_name, 0o700) {
            Ok(()) => {
                self.root
                    .handle
                    .sync_all()
                    .map_err(|error| format!("sync project after backup-root creation: {error}"))?;
                self.verify_project_root_binding()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                require_exact_internal_entry_spelling(&self.root.handle, BACKUP_ROOT, false)?;
            }
            Err(error) => {
                return Err(format!(
                    "create secure projection backup root under {}: {error}",
                    self.project.display()
                ))
            }
        }
        require_exact_internal_entry_spelling(&self.root.handle, BACKUP_ROOT, false)?;
        let backup_handle = open_directory_at(&self.root.handle, backup_name)?;
        fchmod_directory(&backup_handle, 0o700)?;
        let backup_directory = SecureDirectory {
            relative: PathBuf::from(BACKUP_ROOT),
            generation: opened_generation(&backup_handle)?,
            handle: backup_handle,
        };
        self.verify_namespace_binding(&backup_directory.relative, &backup_directory)?;

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let id_fragment = conflict_id
            .strip_prefix("pc_")
            .unwrap_or(conflict_id)
            .chars()
            .take(12)
            .collect::<String>();
        for attempt in 0..128u16 {
            let leaf = format!(
                "projection-conflict-{timestamp}-{}-{id_fragment}-{attempt}",
                std::process::id()
            );
            match mkdir_at(&backup_directory.handle, std::ffi::OsStr::new(&leaf), 0o700) {
                Ok(()) => {
                    let handle =
                        open_directory_at(&backup_directory.handle, std::ffi::OsStr::new(&leaf))?;
                    fchmod_directory(&handle, 0o700)?;
                    backup_directory.handle.sync_all().map_err(|error| {
                        format!("sync projection backup root after transaction create: {error}")
                    })?;
                    let generation = opened_generation(&handle)?;
                    let directory = SecureDirectory {
                        relative: backup_directory.relative.join(leaf),
                        handle,
                        generation,
                    };
                    self.verify_namespace_binding(&backup_directory.relative, &backup_directory)?;
                    self.verify_namespace_binding(&directory.relative, &directory)?;
                    return Ok(directory);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "create secure projection transaction directory: {error}"
                    ))
                }
            }
        }
        Err("could not allocate a unique projection backup directory".to_string())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn normal_relative_components(path: &Path) -> Result<Vec<&std::ffi::OsStr>, String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(fragment) => components.push(fragment),
            _ => {
                return Err(format!(
                    "secure mutation path must be a clean project-relative path: {}",
                    path.display()
                ))
            }
        }
    }
    Ok(components)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn leaf_c_string(leaf: &std::ffi::OsStr) -> std::io::Result<CString> {
    if leaf.as_bytes().is_empty()
        || Path::new(leaf).components().count() != 1
        || leaf.as_bytes().contains(&b'/')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "secure filesystem operation requires one non-empty leaf name",
        ));
    }
    CString::new(leaf.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in leaf name"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_directory_at(parent: &fs::File, leaf: &std::ffi::OsStr) -> Result<fs::File, String> {
    let leaf = leaf_c_string(leaf).map_err(|error| format!("validate directory leaf: {error}"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open secure directory leaf {:?}: {}",
            leaf,
            std::io::Error::last_os_error()
        ));
    }
    let handle = unsafe { fs::File::from_raw_fd(fd) };
    let metadata = handle
        .metadata()
        .map_err(|error| format!("inspect secure directory handle: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("secure directory handle is not a physical directory".to_string());
    }
    Ok(handle)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_regular_file_at(parent: &fs::File, leaf: &std::ffi::OsStr) -> Result<fs::File, String> {
    let leaf = leaf_c_string(leaf).map_err(|error| format!("validate file leaf: {error}"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open secure regular-file leaf {:?}: {}",
            leaf,
            std::io::Error::last_os_error()
        ));
    }
    let handle = unsafe { fs::File::from_raw_fd(fd) };
    let metadata = handle
        .metadata()
        .map_err(|error| format!("inspect secure regular-file handle: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("secure file handle is not a physical regular file".to_string());
    }
    Ok(handle)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn create_new_file_at(
    parent: &fs::File,
    leaf: &std::ffi::OsStr,
    mode: libc::mode_t,
) -> Result<fs::File, String> {
    let leaf = leaf_c_string(leaf).map_err(|error| format!("validate new-file leaf: {error}"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(format!(
            "create secure file leaf {:?}: {}",
            leaf,
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn mkdir_at(parent: &fs::File, leaf: &std::ffi::OsStr, mode: libc::mode_t) -> std::io::Result<()> {
    let leaf = leaf_c_string(leaf)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), leaf.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn fchmod_directory(directory: &fs::File, mode: libc::mode_t) -> Result<(), String> {
    if unsafe { libc::fchmod(directory.as_raw_fd(), mode) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "secure projection backup directory permissions: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn opened_generation(file: &fs::File) -> Result<fs_safety::FileGeneration, String> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect secure filesystem handle: {error}"))?;
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(fs_safety::FileGeneration {
        len: metadata.len(),
        modified_ns,
        identity: fs_safety::FileIdentity {
            device: Some(metadata.dev()),
            file: Some(metadata.ino()),
        },
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn entry_exists_at(parent: &fs::File, leaf: &std::ffi::OsStr) -> Result<bool, String> {
    let leaf = leaf_c_string(leaf).map_err(|error| format!("validate lookup leaf: {error}"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(format!("inspect secure destination leaf: {error}"))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn require_exact_internal_entry_spelling(
    parent: &fs::File,
    expected: &str,
    allow_missing: bool,
) -> Result<bool, String> {
    let duplicate = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if duplicate < 0 {
        return Err(format!(
            "duplicate secure directory handle for spelling check: {}",
            std::io::Error::last_os_error()
        ));
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(duplicate);
        }
        return Err(format!(
            "open secure directory stream for spelling check: {error}"
        ));
    }

    let expected_bytes = expected.as_bytes();
    let mut exact = false;
    let mut alias = None;
    set_errno(0);
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == expected_bytes {
            exact = true;
        } else if name.eq_ignore_ascii_case(expected_bytes) {
            alias = Some(String::from_utf8_lossy(name).into_owned());
        }
    }
    let read_errno = get_errno();
    unsafe {
        libc::closedir(stream);
    }
    if read_errno != 0 {
        return Err(format!(
            "enumerate secure directory for exact spelling: {}",
            std::io::Error::from_raw_os_error(read_errno)
        ));
    }
    if let Some(alias) = alias {
        return Err(format!(
            "internal projection directory {alias:?} aliases required exact spelling {expected:?}"
        ));
    }
    if !exact && !allow_missing {
        return Err(format!(
            "required internal projection directory {expected:?} is missing after creation"
        ));
    }
    Ok(exact)
}

#[cfg(target_os = "macos")]
fn set_errno(value: std::os::raw::c_int) {
    unsafe {
        *libc::__error() = value;
    }
}

#[cfg(target_os = "macos")]
fn get_errno() -> std::os::raw::c_int {
    unsafe { *libc::__error() }
}

#[cfg(target_os = "linux")]
fn set_errno(value: std::os::raw::c_int) {
    unsafe {
        *libc::__errno_location() = value;
    }
}

#[cfg(target_os = "linux")]
fn get_errno() -> std::os::raw::c_int {
    unsafe { *libc::__errno_location() }
}

fn discover_pending_recoveries(project: &Path) -> Result<Vec<PendingRecovery>, String> {
    let root_index = fs_safety::PortableDirectoryIndex::read_raw(project)
        .map_err(|error| format!("scan project root for projection recovery: {error}"))?;
    let physical = root_index.folded_matches(BACKUP_ROOT);
    let linked = root_index.folded_link_matches(BACKUP_ROOT);
    if !linked.is_empty() {
        return Err(format!(
            "projection recovery root is a linked/reparse entry: {}",
            linked[0].path.display()
        ));
    }
    if physical.len() > 1
        || physical
            .first()
            .is_some_and(|entry| entry.fragment != BACKUP_ROOT)
    {
        return Err(format!(
            "projection recovery root has a portable filename collision or wrong casing; required exact name is {BACKUP_ROOT:?}"
        ));
    }
    let Some(backup_entry) = physical.first() else {
        return Ok(Vec::new());
    };
    if backup_entry.kind != SafeEntryKind::Directory {
        return Err(format!(
            "projection recovery root is not a physical directory: {}",
            backup_entry.path.display()
        ));
    }
    let backup_root = &backup_entry.path;
    let before = fs_safety::directory_generation_no_follow(backup_root)
        .map_err(|error| format!("capture projection recovery root identity: {error}"))?;
    let mut transactions = Vec::new();
    for result in fs::read_dir(backup_root)
        .map_err(|error| format!("read projection recovery root: {error}"))?
    {
        let entry = result.map_err(|error| format!("read projection recovery entry: {error}"))?;
        let name = entry.file_name().into_string().map_err(|_| {
            format!(
                "non-UTF-8 projection recovery entry is not portable: {}",
                entry.path().display()
            )
        })?;
        if !name.starts_with("projection-conflict-") {
            continue;
        }
        if transactions.len() >= MAX_RECOVERY_TRANSACTIONS {
            return Err(format!(
                "projection recovery scan exceeds maximum transaction count {MAX_RECOVERY_TRANSACTIONS}"
            ));
        }
        transactions.push((name, entry.path()));
    }
    transactions.sort_by(|left, right| left.0.cmp(&right.0));

    let mut pending = Vec::new();
    for (name, path) in transactions {
        let relative = PathBuf::from(BACKUP_ROOT).join(&name);
        match metadata_no_follow(&path) {
            Ok(Some(metadata)) if metadata.is_dir() => metadata,
            Ok(Some(_)) => {
                pending.push(malformed_recovery(
                    project,
                    &relative,
                    format!(
                        "projection transaction is not a physical directory: {}",
                        path.display()
                    ),
                ));
                continue;
            }
            Ok(None) => continue,
            Err(error) => {
                pending.push(malformed_recovery(
                    project,
                    &relative,
                    format!(
                        "projection transaction is linked/reparse or unreadable: {}: {error}",
                        path.display()
                    ),
                ));
                continue;
            }
        };
        let generation = fs_safety::directory_generation_no_follow(&path)
            .map_err(|error| format!("capture projection transaction identity: {error}"))?;
        if let Some(recovery) =
            inspect_recovery_transaction(project, &path, &relative, &generation)?
        {
            pending.push(recovery);
        }
    }
    let after = fs_safety::directory_generation_no_follow(backup_root)
        .map_err(|error| format!("recheck projection recovery root identity: {error}"))?;
    if before != after {
        return Err(
            "projection recovery root changed while scanning; rerun offline repair".to_string(),
        );
    }
    Ok(pending)
}

fn inspect_recovery_transaction(
    project: &Path,
    transaction: &Path,
    relative: &Path,
    generation: &fs_safety::FileGeneration,
) -> Result<Option<PendingRecovery>, String> {
    let before = fs_safety::directory_generation_no_follow(transaction)
        .map_err(|error| format!("capture projection transaction generation: {error}"))?;
    if &before != generation {
        return Err("projection transaction changed before receipt scan".to_string());
    }
    let mut relevant_names = std::collections::HashMap::<String, String>::new();
    let mut entry_count = 0usize;
    for result in fs::read_dir(transaction).map_err(|error| {
        format!(
            "read projection transaction {}: {error}",
            transaction.display()
        )
    })? {
        let entry =
            result.map_err(|error| format!("read projection transaction entry: {error}"))?;
        entry_count += 1;
        if entry_count > MAX_RECOVERY_DIRECTORY_ENTRIES {
            return Ok(Some(malformed_directory_recovery(
                project,
                relative,
                format!(
                    "projection transaction exceeds maximum entry count {MAX_RECOVERY_DIRECTORY_ENTRIES}: {}",
                    transaction.display()
                ),
            )));
        }
        let name = entry.file_name().into_string().map_err(|_| {
            format!(
                "non-UTF-8 projection transaction entry is not portable: {}",
                entry.path().display()
            )
        })?;
        for expected in ["prepared.json", "committed.json", "reconciled.json"] {
            if name.eq_ignore_ascii_case(expected) {
                if name != expected || relevant_names.contains_key(expected) {
                    return Ok(Some(malformed_directory_recovery(
                        project,
                        relative,
                        format!(
                            "projection transaction receipt has wrong casing or a portable collision: {name:?}"
                        ),
                    )));
                }
                relevant_names.insert(expected.to_string(), name.clone());
            }
        }
    }

    let prepared = read_transaction_manifest(project, transaction, "prepared.json", "prepared");
    let committed = read_transaction_manifest(project, transaction, "committed.json", "committed");
    let reconciled =
        read_transaction_manifest(project, transaction, "reconciled.json", "reconciled");
    let after = fs_safety::directory_generation_no_follow(transaction)
        .map_err(|error| format!("recheck projection transaction generation: {error}"))?;
    if before != after {
        return Err(format!(
            "projection transaction changed while scanning receipts: {}",
            transaction.display()
        ));
    }

    if let ManifestRead::Valid {
        manifest: reconciled,
        ..
    } = &reconciled
    {
        if !reconciled.recovery_required
            && reconciled.state == "reconciled"
            && reconciled.moves.iter().all(|item| item.status == "moved")
            && (matches!(
                &prepared,
                ManifestRead::Valid {
                    manifest: prepared,
                    sha256,
                } if manifests_correlate(prepared, reconciled)
                    && reconciled.reconciles_receipt_sha256.as_deref()
                        == Some(sha256.as_str())
            ) || matches!(
                &committed,
                ManifestRead::Valid {
                    manifest: committed,
                    sha256,
                } if committed.recovery_required
                    && manifests_correlate(committed, reconciled)
                    && reconciled.reconciles_receipt_sha256.as_deref()
                        == Some(sha256.as_str())
            ))
        {
            return Ok(None);
        }
    }
    if let ManifestRead::Invalid(error) = &reconciled {
        return Ok(Some(recovery_from_reads(
            project,
            relative,
            generation,
            &prepared,
            &committed,
            format!("reconciled projection receipt is invalid: {error}"),
        )));
    }

    match &committed {
        ManifestRead::Valid {
            manifest,
            sha256: _,
        } if manifest.recovery_required => Ok(Some(recovery_from_reads(
            project,
            relative,
            generation,
            &prepared,
            &committed,
            manifest
                .error
                .clone()
                .unwrap_or_else(|| "committed receipt requires recovery".to_string()),
        ))),
        ManifestRead::Valid {
            manifest: terminal_manifest,
            ..
        } => match &prepared {
            ManifestRead::Valid {
                manifest: prepared, ..
            } if manifests_correlate(prepared, terminal_manifest)
                && !terminal_manifest.recovery_required
                && terminal_manifest
                    .moves
                    .iter()
                    .all(|item| item.status == "moved") =>
            {
                Ok(None)
            }
            _ => Ok(Some(recovery_from_reads(
                project,
                relative,
                generation,
                &prepared,
                &committed,
                "clean committed receipt is missing a matching valid prepared receipt".to_string(),
            ))),
        },
        ManifestRead::Invalid(error) => Ok(Some(recovery_from_reads(
            project,
            relative,
            generation,
            &prepared,
            &committed,
            format!("committed projection receipt is invalid: {error}"),
        ))),
        ManifestRead::Missing => match &prepared {
            ManifestRead::Valid { manifest, .. } => Ok(Some(recovery_from_reads(
                project,
                relative,
                generation,
                &prepared,
                &committed,
                format!(
                    "prepared projection transaction {} has no proven clean committed receipt",
                    manifest.conflict_id
                ),
            ))),
            ManifestRead::Invalid(error) => Ok(Some(recovery_from_reads(
                project,
                relative,
                generation,
                &prepared,
                &committed,
                format!("prepared projection receipt is invalid: {error}"),
            ))),
            ManifestRead::Missing => Ok(Some(malformed_directory_recovery(
                project,
                relative,
                "projection transaction has no prepared or committed receipt".to_string(),
            ))),
        },
    }
}

fn read_transaction_manifest(
    project: &Path,
    transaction: &Path,
    file_name: &str,
    expected_state: &str,
) -> ManifestRead {
    let path = transaction.join(file_name);
    let bytes = match read_file_no_follow_bounded(&path, MAX_TRANSACTION_MANIFEST_BYTES as u64) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return ManifestRead::Invalid(format!(
                "receipt exceeds maximum size {MAX_TRANSACTION_MANIFEST_BYTES}: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ManifestRead::Missing;
        }
        Err(error) => {
            return ManifestRead::Invalid(format!("read {}: {error}", path.display()));
        }
    };
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let manifest = match serde_json::from_slice::<TransactionManifest>(&bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            return ManifestRead::Invalid(format!("parse {}: {error}", path.display()));
        }
    };
    if let Err(error) = validate_transaction_manifest(project, &manifest, expected_state) {
        return ManifestRead::Invalid(error);
    }
    ManifestRead::Valid {
        manifest: Box::new(manifest),
        sha256,
    }
}

fn validate_transaction_manifest(
    project: &Path,
    manifest: &TransactionManifest,
    expected_state: &str,
) -> Result<(), String> {
    if manifest.version != TRANSACTION_VERSION || manifest.state != expected_state {
        return Err(format!(
            "transaction receipt has unsupported version/state: {}/{}",
            manifest.version, manifest.state
        ));
    }
    if manifest.project != project.display().to_string() {
        return Err("transaction receipt belongs to a different canonical project".to_string());
    }
    if manifest.conflict_id.is_empty()
        || manifest.conflict_id.len() > 128
        || !manifest.conflict_id.is_ascii()
    {
        return Err("transaction receipt conflict id is invalid".to_string());
    }
    if !matches!(
        manifest.kind.as_str(),
        MULTIPLE_INIT_MARKERS | LEGACY_RESERVED_INIT_LEAF
    ) {
        return Err("transaction receipt conflict kind is invalid".to_string());
    }
    validate_manifest_relative_path(&manifest.directory, true)?;
    if manifest.kept.size > MAX_SYNCED_SCRIPT_BYTES
        || !valid_sha256(&manifest.kept.sha256)
        || manifest.kept.name.len() > 1024
        || manifest.kept.generation.len() > 1024
    {
        return Err("transaction receipt kept-file proof is invalid or oversized".to_string());
    }
    validate_manifest_relative_path(&manifest.kept.path, false)?;
    if manifest.moves.is_empty() || manifest.moves.len() > MAX_MARKERS_PER_CONFLICT {
        return Err("transaction receipt move count is invalid".to_string());
    }
    for item in &manifest.moves {
        if !matches!(item.operation.as_str(), "archive" | "rename")
            || !matches!(item.status.as_str(), "pending" | "moved")
            || item.size > MAX_SYNCED_SCRIPT_BYTES
            || !valid_sha256(&item.sha256)
        {
            return Err("transaction receipt move proof is invalid".to_string());
        }
        validate_manifest_relative_path(&item.original_path, false)?;
        validate_manifest_relative_path(&item.destination_path, false)?;
    }
    if expected_state == "prepared" && !manifest.recovery_required {
        return Err("prepared receipt must be recovery-required".to_string());
    }
    if expected_state == "reconciled"
        && (manifest
            .reconciles_receipt_sha256
            .as_deref()
            .is_none_or(|value| !valid_sha256(value))
            || manifest.reconciled_at_ms.is_none())
    {
        return Err("reconciled receipt is missing its source-receipt proof".to_string());
    }
    if matches!(expected_state, "committed" | "reconciled")
        && !manifest.recovery_required
        && (manifest.error.is_some() || manifest.moves.iter().any(|item| item.status != "moved"))
    {
        return Err("clean terminal receipt has incomplete moves or an error".to_string());
    }
    Ok(())
}

fn validate_manifest_relative_path(path: &str, directory: bool) -> Result<(), String> {
    let path = Path::new(path);
    let components = path.components().collect::<Vec<_>>();
    if components.is_empty()
        || components.len() > MAX_SERVICE_TREE_DEPTH + 2
        || components
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("transaction receipt contains an unsafe relative path".to_string());
    }
    let first = components[0]
        .as_os_str()
        .to_str()
        .ok_or_else(|| "transaction receipt path is not UTF-8".to_string())?;
    if (directory || first != BACKUP_ROOT) && !SYNCED_SERVICES.contains(&first) {
        return Err("transaction receipt path is outside synced services".to_string());
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn manifests_correlate(prepared: &TransactionManifest, terminal: &TransactionManifest) -> bool {
    if prepared.version != terminal.version
        || prepared.conflict_id != terminal.conflict_id
        || prepared.kind != terminal.kind
        || prepared.project != terminal.project
        || prepared.directory != terminal.directory
        || prepared.prepared_at_ms != terminal.prepared_at_ms
        || prepared.moves.len() != terminal.moves.len()
        || prepared.kept.size != terminal.kept.size
        || prepared.kept.sha256 != terminal.kept.sha256
        || prepared.kept.generation != terminal.kept.generation
    {
        return false;
    }
    prepared
        .moves
        .iter()
        .zip(&terminal.moves)
        .all(|(left, right)| {
            left.operation == right.operation
                && left.original_path == right.original_path
                && left.destination_path == right.destination_path
                && left.size == right.size
                && left.sha256 == right.sha256
        })
}

fn recovery_from_reads(
    project: &Path,
    relative: &Path,
    generation: &fs_safety::FileGeneration,
    prepared: &ManifestRead,
    committed: &ManifestRead,
    error: String,
) -> PendingRecovery {
    let (receipt_file, receipt_manifest, receipt_hash) = match committed {
        ManifestRead::Valid { manifest, sha256 } if manifest.recovery_required => {
            ("committed.json", Some(manifest), sha256.as_str())
        }
        _ => match prepared {
            ManifestRead::Valid { manifest, sha256 } => {
                ("prepared.json", Some(manifest), sha256.as_str())
            }
            _ => ("", None, ""),
        },
    };
    let receipt_path = if receipt_file.is_empty() {
        String::new()
    } else {
        relative_path_string(relative.join(receipt_file)).unwrap_or_default()
    };
    let kind = receipt_manifest
        .map(|manifest| manifest.kind.clone())
        .unwrap_or_else(|| "projection-recovery".to_string());
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, &project.display().to_string());
    hash_field(&mut hasher, &relative.to_string_lossy());
    hash_generation(&mut hasher, generation);
    hash_field(&mut hasher, receipt_hash);
    hash_field(&mut hasher, &error);
    let resume_manifest = match prepared {
        ManifestRead::Valid {
            manifest: prepared_manifest,
            ..
        } => match committed {
            ManifestRead::Valid {
                manifest: committed_manifest,
                ..
            } if !manifests_correlate(prepared_manifest, committed_manifest) => None,
            _ => Some((**prepared_manifest).clone()),
        },
        _ => None,
    };
    PendingRecovery {
        id: format!("pr_{:x}", hasher.finalize()),
        transaction_relative: relative.to_path_buf(),
        kind,
        receipt_path,
        receipt_available: receipt_manifest.is_some(),
        error: bounded_error(&error),
        generation: generation.clone(),
        resume_manifest,
        receipt_file: (!receipt_file.is_empty()).then(|| receipt_file.to_string()),
        receipt_sha256: (!receipt_hash.is_empty()).then(|| receipt_hash.to_string()),
        quarantine_allowed: true,
    }
}

fn malformed_recovery(project: &Path, relative: &Path, error: String) -> PendingRecovery {
    let generation = fs_safety::FileGeneration {
        len: 0,
        modified_ns: None,
        identity: fs_safety::FileIdentity {
            device: None,
            file: None,
        },
    };
    let mut recovery = recovery_from_reads(
        project,
        relative,
        &generation,
        &ManifestRead::Missing,
        &ManifestRead::Missing,
        error,
    );
    recovery.quarantine_allowed = false;
    recovery
}

fn malformed_directory_recovery(project: &Path, relative: &Path, error: String) -> PendingRecovery {
    let mut recovery = malformed_recovery(project, relative, error);
    recovery.quarantine_allowed = true;
    recovery
}

pub fn inspect(project: &Path) -> Result<ProjectionScan, ProjectionRepairError> {
    inspect_untyped(project).map_err(ProjectionRepairError::classify)
}

pub fn ensure_no_pending_recovery(project: &Path) -> Result<(), ProjectionRepairError> {
    let project = fs_safety::stable_canonical_directory(project).map_err(|error| {
        ProjectionRepairError::classify(format!(
            "canonicalize project {}: {error}",
            project.display()
        ))
    })?;
    let recoveries =
        discover_pending_recoveries(&project).map_err(ProjectionRepairError::classify)?;
    if let Some(first) = recoveries.first() {
        let receipt = if first.receipt_available {
            format!(" Receipt: {}.", first.receipt_path)
        } else {
            " No receipt path is provably available.".to_string()
        };
        return Err(ProjectionRepairError::classify(format!(
            "PROJECTION_RECOVERY_REQUIRED: {} pending offline projection recovery transaction(s) block daemon startup. Run `rosync repair projection --project {:?} --raw`, then resolve recovery {} before starting Ro Sync.{} {}",
            recoveries.len(),
            project,
            first.id,
            receipt,
            first.error
        )));
    }
    Ok(())
}

fn inspect_untyped(project: &Path) -> Result<ProjectionScan, String> {
    let project = fs_safety::stable_canonical_directory(project)
        .map_err(|error| format!("canonicalize project {}: {error}", project.display()))?;
    let recoveries = discover_pending_recoveries(&project)?;
    if let Some(first) = recoveries.first() {
        let error = bounded_error(&format!(
            "{} pending offline projection recovery transaction(s) must be reconciled before Ro Sync can start; {}",
            recoveries.len(),
            first.error
        ));
        return Ok(ProjectionScan {
            ok: false,
            code: Some("PROJECTION_RECOVERY_REQUIRED".to_string()),
            error: Some(error.clone()),
            project: project.display().to_string(),
            conflicts: Vec::new(),
            remaining: 0,
            total_conflicts: 0,
            counts_known: false,
            truncated: true,
            resolution: Some(ProjectionResolution {
                id: first.id.clone(),
                kind: first.kind.clone(),
                kept_file: String::new(),
                backup_paths: Vec::new(),
                receipt_path: first.receipt_path.clone(),
                receipt_available: first.receipt_available,
                recovery_actions: {
                    let mut actions = Vec::new();
                    if first.resume_manifest.is_some() {
                        actions.push("resume".to_string());
                    }
                    if first.quarantine_allowed {
                        actions.push("quarantine".to_string());
                    }
                    actions
                },
                recovery_required: true,
                recovery_error: Some(error),
                source_path: None,
                canonical_path: None,
            }),
            recovery_count: recoveries.len(),
        });
    }
    inspect_conflicts_canonical(&project)
}

fn inspect_conflicts_canonical(project: &Path) -> Result<ProjectionScan, String> {
    let mut collector = ScanCollector::new();

    for service in SYNCED_SERVICES {
        let service_path = fs_safety::validate_service_path(project, service, true)
            .map_err(|error| format!("validate service {service}: {error}"))?;
        let Some(metadata) = metadata_no_follow(&service_path)
            .map_err(|error| format!("inspect service {}: {error}", service_path.display()))?
        else {
            continue;
        };
        if !metadata.is_dir() {
            return Err(format!(
                "synced service root is not a directory: {}",
                service_path.display()
            ));
        }

        let mut stack = vec![(service_path, PathBuf::from(service), 0usize)];
        while let Some((directory, relative_directory, depth)) = stack.pop() {
            if depth > MAX_SERVICE_TREE_DEPTH {
                return Err(format!(
                    "projection repair scan exceeds maximum depth {MAX_SERVICE_TREE_DEPTH} at {}",
                    directory.display()
                ));
            }
            let index = fs_safety::PortableDirectoryIndex::read_for_projection_repair(&directory)
                .map_err(|error| format!("scan {}: {error}", directory.display()))?;
            let directory_generation = fs_safety::directory_generation_no_follow(&directory)
                .map_err(|error| {
                    format!(
                        "capture projection directory generation {}: {error}",
                        directory.display()
                    )
                })?;
            collector.nodes = collector
                .nodes
                .checked_add(index.entries().len())
                .ok_or_else(|| "projection repair node count overflow".to_string())?;
            if collector.nodes > MAX_REPAIR_SCAN_NODES {
                return Err(format!(
                    "projection repair scan exceeds maximum node count {MAX_REPAIR_SCAN_NODES}"
                ));
            }

            let marker_entries = index
                .entries()
                .iter()
                .filter(|entry| {
                    entry.kind == SafeEntryKind::File
                        && fs_map::init_path_describes_parent(&entry.path)
                })
                .collect::<Vec<_>>();
            if marker_entries.len() > MAX_MARKERS_PER_CONFLICT {
                return Err(format!(
                    "projection repair found {} init markers in {}; maximum supported per conflict is {}",
                    marker_entries.len(),
                    directory.display(),
                    MAX_MARKERS_PER_CONFLICT
                ));
            }
            if marker_entries.len() > 1 {
                let relative_directory_string = relative_path_string(&relative_directory)?;
                collector.record(|preview_budget| {
                    build_multiple_marker_conflict(
                        project,
                        &relative_directory_string,
                        &marker_entries,
                        &directory_generation,
                        preview_budget,
                    )
                })?;
            }

            for entry in index.entries() {
                if entry.kind != SafeEntryKind::File
                    || fs_map::parse_reserved_init_filename(&entry.fragment).is_none()
                    || fs_map::init_path_describes_parent(&entry.path)
                {
                    continue;
                }
                let relative_directory_string = relative_path_string(&relative_directory)?;
                collector.record(|preview_budget| {
                    build_legacy_leaf_conflict(
                        project,
                        &relative_directory_string,
                        entry,
                        &directory_generation,
                        preview_budget,
                    )
                })?;
            }

            for entry in index.entries().iter().rev() {
                if entry.kind == SafeEntryKind::Directory {
                    stack.push((
                        entry.path.clone(),
                        relative_directory.join(&entry.fragment),
                        depth + 1,
                    ));
                }
            }
        }
    }

    let project_string = project.display().to_string();
    Ok(ProjectionScan {
        ok: true,
        code: None,
        error: None,
        project: project_string,
        truncated: collector.total_conflicts > collector.conflicts.len(),
        remaining: collector.total_conflicts,
        total_conflicts: collector.total_conflicts,
        counts_known: true,
        conflicts: collector.conflicts,
        resolution: None,
        recovery_count: 0,
    })
}

pub fn resolve(
    project: &Path,
    conflict_id: &str,
    keep: Option<&str>,
) -> Result<ProjectionResolveResult, ProjectionRepairError> {
    resolve_untyped(project, conflict_id, keep).map_err(ProjectionRepairError::classify)
}

fn resolve_untyped(
    project: &Path,
    conflict_id: &str,
    keep: Option<&str>,
) -> Result<ProjectionResolveResult, String> {
    if conflict_id.is_empty() || conflict_id.len() > 128 || !conflict_id.is_ascii() {
        return Err("projection conflict id is invalid; inspect again".to_string());
    }
    let canonical_project = fs_safety::stable_canonical_directory(project)
        .map_err(|error| format!("canonicalize project {}: {error}", project.display()))?;
    #[cfg(not(test))]
    let _daemon_start_lock = {
        let state_dir = crate::lifecycle::state_dir(None)
            .map_err(|error| format!("locate Ro Sync state directory: {error}"))?;
        let paths = crate::lifecycle::runtime_paths(state_dir, &canonical_project);
        crate::lifecycle::StartLock::acquire_named(
            &paths.start_lock,
            "daemon start or projection resolution",
        )
        .map_err(|error| format!("serialize projection resolution with daemon start: {error}"))?
    };
    let _project_operation_lock =
        acquire_project_operation_lock(&canonical_project, "projection resolution")?;

    let recoveries = discover_pending_recoveries(&canonical_project)?;
    if let Some(recovery) = recoveries
        .iter()
        .find(|recovery| recovery.id == conflict_id)
        .cloned()
    {
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (recovery, keep);
            return Err(
                "UNSUPPORTED_SECURE_PROJECTION_RESOLVE: recovery reconciliation requires handle-relative, atomic no-replace filesystem operations"
                    .to_string(),
            );
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let mutation = SecureProjectMutation::open(&canonical_project)?;
            let resolution =
                match reconcile_pending_recovery(&canonical_project, &mutation, &recovery, keep) {
                    Ok(resolution) => resolution,
                    Err(error) => {
                        let error = bounded_error(&error);
                        return Ok(ProjectionResolveResult {
                            ok: false,
                            code: Some("PROJECTION_RECOVERY_REQUIRED".to_string()),
                            error: Some(error.clone()),
                            project: canonical_project.display().to_string(),
                            resolution: pending_recovery_resolution(&recovery, error),
                            conflicts: Vec::new(),
                            remaining: 0,
                            total_conflicts: 0,
                            counts_known: false,
                            truncated: true,
                        });
                    }
                };
            let after = inspect_untyped(&canonical_project)?;
            let still_blocked = !after.ok;
            let resolution = if still_blocked {
                after.resolution.clone().unwrap_or(resolution)
            } else {
                resolution
            };
            return Ok(ProjectionResolveResult {
                ok: !resolution.recovery_required && !still_blocked,
                code: if resolution.recovery_required {
                    Some("PROJECTION_RECOVERY_REQUIRED".to_string())
                } else {
                    after.code.clone()
                },
                error: after
                    .error
                    .clone()
                    .or_else(|| resolution.recovery_error.clone()),
                project: canonical_project.display().to_string(),
                resolution,
                conflicts: after.conflicts,
                remaining: after.remaining,
                total_conflicts: after.total_conflicts,
                counts_known: after.counts_known,
                truncated: after.truncated,
            });
        }
    }
    if let Some(first) = recoveries.first() {
        return Err(format!(
            "PROJECTION_RECOVERY_REQUIRED: pending recovery {} must be reconciled before resolving another projection conflict",
            first.id
        ));
    }

    let before = inspect_conflicts_canonical(&canonical_project)?;
    let conflict = before
        .conflicts
        .iter()
        .find(|conflict| conflict.id == conflict_id)
        .cloned()
        .ok_or_else(|| {
            "projection conflict id is stale or unknown; inspect the project again".to_string()
        })?;
    let project = PathBuf::from(&before.project);

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (conflict, keep);
        return Err(
            "UNSUPPORTED_SECURE_PROJECTION_RESOLVE: offline projection inspection is available, but this platform does not yet provide Ro Sync's required handle-relative, atomic no-replace resolver"
                .to_string(),
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mutation = SecureProjectMutation::open(&project)?;
        let mut resolution = match conflict.kind.as_str() {
            MULTIPLE_INIT_MARKERS => {
                resolve_multiple_markers(&project, &mutation, &conflict, keep)?
            }
            LEGACY_RESERVED_INIT_LEAF => resolve_legacy_leaf(&project, &mutation, &conflict, keep)?,
            other => {
                return Err(format!(
                    "unsupported projection conflict kind {other:?}; inspect again"
                ))
            }
        };

        #[cfg(test)]
        test_pause_before_postscan(&conflict.id);
        if let Err(error) = mutation.verify_project_root_binding() {
            mark_root_binding_failure(&mut resolution, &error);
            return Ok(ProjectionResolveResult {
                ok: false,
                code: Some("PROJECTION_RECOVERY_REQUIRED".to_string()),
                error: resolution.recovery_error.clone(),
                project: project.display().to_string(),
                resolution,
                conflicts: Vec::new(),
                remaining: before.remaining,
                total_conflicts: before.total_conflicts,
                counts_known: false,
                truncated: true,
            });
        }
        let after = match inspect_untyped(&project) {
            Ok(after) => after,
            Err(error) => {
                return Ok(ProjectionResolveResult {
                    ok: false,
                    code: Some("PROJECTION_POSTSCAN_FAILED".to_string()),
                    error: Some(error),
                    project: project.display().to_string(),
                    resolution,
                    conflicts: Vec::new(),
                    remaining: before.remaining,
                    total_conflicts: before.total_conflicts,
                    counts_known: false,
                    truncated: true,
                })
            }
        };
        if let Err(error) = mutation.verify_project_root_binding() {
            mark_root_binding_failure(&mut resolution, &error);
            return Ok(ProjectionResolveResult {
                ok: false,
                code: Some("PROJECTION_RECOVERY_REQUIRED".to_string()),
                error: resolution.recovery_error.clone(),
                project: project.display().to_string(),
                resolution,
                conflicts: Vec::new(),
                remaining: before.remaining,
                total_conflicts: before.total_conflicts,
                counts_known: false,
                truncated: true,
            });
        }
        let recovery_required = resolution.recovery_required;
        let recovery_error = resolution.recovery_error.clone();
        if !after.ok {
            let next_resolution = after.resolution.clone().unwrap_or(resolution);
            return Ok(ProjectionResolveResult {
                ok: false,
                code: after
                    .code
                    .clone()
                    .or_else(|| Some("PROJECTION_RECOVERY_REQUIRED".to_string())),
                error: after
                    .error
                    .clone()
                    .or_else(|| next_resolution.recovery_error.clone()),
                project: after.project,
                resolution: next_resolution,
                conflicts: after.conflicts,
                remaining: after.remaining,
                total_conflicts: after.total_conflicts,
                counts_known: after.counts_known,
                truncated: after.truncated,
            });
        }
        Ok(ProjectionResolveResult {
            ok: !recovery_required,
            code: recovery_required.then(|| "PROJECTION_RECOVERY_REQUIRED".to_string()),
            error: recovery_error,
            project: after.project,
            resolution,
            conflicts: after.conflicts,
            remaining: after.remaining,
            total_conflicts: after.total_conflicts,
            counts_known: after.counts_known,
            truncated: after.truncated,
        })
    }
}

fn pending_recovery_resolution(
    recovery: &PendingRecovery,
    recovery_error: String,
) -> ProjectionResolution {
    let mut actions = Vec::new();
    if recovery.resume_manifest.is_some() {
        actions.push("resume".to_string());
    }
    if recovery.quarantine_allowed {
        actions.push("quarantine".to_string());
    }
    ProjectionResolution {
        id: recovery.id.clone(),
        kind: recovery.kind.clone(),
        kept_file: String::new(),
        backup_paths: Vec::new(),
        receipt_path: recovery.receipt_path.clone(),
        receipt_available: recovery.receipt_available,
        recovery_actions: actions,
        recovery_required: true,
        recovery_error: Some(recovery_error),
        source_path: None,
        canonical_path: None,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn mark_root_binding_failure(resolution: &mut ProjectionResolution, error: &str) {
    resolution.recovery_required = true;
    resolution.recovery_error = Some(bounded_error(&format!(
        "{}canonical project root binding changed during offline projection resolution: {error}",
        resolution
            .recovery_error
            .as_deref()
            .map(|value| format!("{value}; "))
            .unwrap_or_default()
    )));
    resolution.backup_paths.clear();
    resolution.receipt_path.clear();
    resolution.receipt_available = false;
}

fn build_multiple_marker_conflict(
    project: &Path,
    relative_directory: &str,
    entries: &[&fs_safety::PortableDirectoryEntry],
    directory_generation: &fs_safety::FileGeneration,
    preview_budget: &mut usize,
) -> Result<ProjectionConflict, String> {
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        files.push(describe_file(project, entry, preview_budget)?);
    }
    let identical = files
        .first()
        .is_some_and(|first| files.iter().all(|file| file.sha256 == first.sha256));
    let id = conflict_id(
        project,
        MULTIPLE_INIT_MARKERS,
        relative_directory,
        directory_generation,
        &files,
        None,
    );
    Ok(ProjectionConflict {
        id,
        kind: MULTIPLE_INIT_MARKERS.to_string(),
        directory: relative_directory.to_string(),
        files,
        identical,
        source_path: None,
        canonical_path: None,
        directory_generation: directory_generation.clone(),
    })
}

fn build_legacy_leaf_conflict(
    project: &Path,
    relative_directory: &str,
    entry: &fs_safety::PortableDirectoryEntry,
    directory_generation: &fs_safety::FileGeneration,
    preview_budget: &mut usize,
) -> Result<ProjectionConflict, String> {
    let file = describe_file(project, entry, preview_budget)?;
    let canonical_path = fs_map::legacy_reserved_init_leaf_migration(&entry.path)
        .map_err(|error| {
            format!(
                "compute canonical migration for {}: {error}",
                entry.path.display()
            )
        })?
        .ok_or_else(|| {
            format!(
                "legacy reserved init leaf no longer needs migration: {}",
                entry.path.display()
            )
        })?;
    let canonical_relative = canonical_path
        .strip_prefix(project)
        .map_err(|_| {
            format!(
                "canonical migration escaped project root: {}",
                canonical_path.display()
            )
        })
        .and_then(relative_path_string)?;
    let source_path = file.path.clone();
    let files = vec![file];
    let id = conflict_id(
        project,
        LEGACY_RESERVED_INIT_LEAF,
        relative_directory,
        directory_generation,
        &files,
        Some(&canonical_relative),
    );
    Ok(ProjectionConflict {
        id,
        kind: LEGACY_RESERVED_INIT_LEAF.to_string(),
        directory: relative_directory.to_string(),
        files,
        identical: true,
        source_path: Some(source_path),
        canonical_path: Some(canonical_relative),
        directory_generation: directory_generation.clone(),
    })
}

fn describe_file(
    project: &Path,
    entry: &fs_safety::PortableDirectoryEntry,
    preview_budget: &mut usize,
) -> Result<ProjectionFile, String> {
    let before_generation = fs_safety::file_generation_no_follow(&entry.path)?;
    let bytes = read_file_no_follow_bounded(&entry.path, MAX_SYNCED_SCRIPT_BYTES)
        .map_err(|error| format!("read {}: {error}", entry.path.display()))?
        .ok_or_else(|| {
            format!(
                "projection source exceeds maximum size {} bytes: {}",
                MAX_SYNCED_SCRIPT_BYTES,
                entry.path.display()
            )
        })?;
    let generation = fs_safety::file_generation_no_follow(&entry.path)?;
    if generation != before_generation {
        return Err(format!(
            "projection source changed while describing it: {}",
            entry.path.display()
        ));
    }
    let relative = entry
        .path
        .strip_prefix(project)
        .map_err(|_| {
            format!(
                "projection source escaped canonical project root: {}",
                entry.path.display()
            )
        })
        .and_then(relative_path_string)?;
    let parsed = fs_map::parse_reserved_init_filename(&entry.fragment).ok_or_else(|| {
        format!(
            "projection conflict source is not in the reserved init namespace: {}",
            entry.path.display()
        )
    })?;
    let style = if parsed.inner_name.is_some() {
        "named"
    } else {
        "plain"
    };
    let utf8 = std::str::from_utf8(&bytes).is_ok();
    let preview_limit = (*preview_budget).min(MAX_PREVIEW_BYTES_PER_FILE);
    let (preview, preview_truncated) = utf8_preview(&bytes, preview_limit);
    *preview_budget = preview_budget.saturating_sub(preview.len());

    Ok(ProjectionFile {
        name: entry.fragment.clone(),
        path: relative,
        style: style.to_string(),
        class_name: parsed.class.class_name().to_string(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        preview,
        preview_truncated,
        utf8,
        generation,
    })
}

fn utf8_preview(bytes: &[u8], max_output_bytes: usize) -> (String, bool) {
    if bytes.is_empty() {
        return (String::new(), false);
    }
    if max_output_bytes == 0 {
        return (String::new(), true);
    }
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= max_output_bytes {
        return (text.into_owned(), false);
    }
    let mut end = max_output_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn conflict_id(
    project: &Path,
    kind: &str,
    directory: &str,
    directory_generation: &fs_safety::FileGeneration,
    files: &[ProjectionFile],
    canonical_path: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, CONFLICT_ID_VERSION);
    hash_field(&mut hasher, &project.display().to_string());
    hash_field(&mut hasher, kind);
    hash_field(&mut hasher, directory);
    hash_generation(&mut hasher, directory_generation);
    for file in files {
        hash_field(&mut hasher, &file.name);
        hash_field(&mut hasher, &file.path);
        hash_field(&mut hasher, &file.class_name);
        hash_field(&mut hasher, &file.size.to_string());
        hash_field(&mut hasher, &file.sha256);
        hash_generation(&mut hasher, &file.generation);
    }
    if let Some(path) = canonical_path {
        hash_field(&mut hasher, path);
    }
    format!("pc_{:x}", hasher.finalize())
}

fn hash_generation(hasher: &mut Sha256, generation: &fs_safety::FileGeneration) {
    hash_field(hasher, &generation.len.to_string());
    hash_field(
        hasher,
        &generation
            .modified_ns
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    hash_field(
        hasher,
        &generation
            .identity
            .device
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    hash_field(
        hasher,
        &generation
            .identity
            .file
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_multiple_markers(
    project: &Path,
    mutation: &SecureProjectMutation,
    conflict: &ProjectionConflict,
    keep: Option<&str>,
) -> Result<ProjectionResolution, String> {
    let keep = keep.ok_or_else(|| {
        "resolving multiple init markers requires --keep <exact filename>".to_string()
    })?;
    if Path::new(keep).components().count() != 1 {
        return Err("--keep must be one exact filename, not a path".to_string());
    }
    let kept = conflict
        .files
        .iter()
        .find(|file| file.name == keep)
        .ok_or_else(|| format!("--keep {keep:?} is not one of this conflict's exact filenames"))?;
    let losers = conflict
        .files
        .iter()
        .filter(|file| file.name != kept.name)
        .collect::<Vec<_>>();
    if losers.is_empty() {
        return Err("multiple-marker conflict no longer has a losing file".to_string());
    }

    let conflict_relative = path_from_slashes(&conflict.directory);
    let conflict_directory =
        mutation.verify_directory(&conflict_relative, &conflict.directory_generation)?;
    verify_conflict_file_path(conflict, kept)?;
    verify_file_snapshot_at(&conflict_directory, kept)?;
    for loser in &losers {
        verify_conflict_file_path(conflict, loser)?;
        verify_file_snapshot_at(&conflict_directory, loser)?;
    }

    let transaction_directory = mutation.create_transaction_directory(&conflict.id)?;
    let mut manifest_moves = Vec::with_capacity(losers.len());
    for loser in &losers {
        if entry_exists_at(
            &transaction_directory.handle,
            std::ffi::OsStr::new(&loser.name),
        )? {
            return Err(format!(
                "archive target unexpectedly exists in secure transaction directory: {}",
                loser.name
            ));
        }
        let destination_relative =
            relative_path_string(transaction_directory.relative.join(&loser.name))?;
        manifest_moves.push(TransactionMove {
            operation: "archive".to_string(),
            original_path: loser.path.clone(),
            destination_path: destination_relative,
            size: loser.size,
            sha256: loser.sha256.clone(),
            status: "pending".to_string(),
        });
    }
    let mut manifest = new_manifest(project, conflict, kept, manifest_moves);
    let prepared_receipt = write_manifest_durable(
        project,
        mutation,
        &transaction_directory,
        "prepared.json",
        &manifest,
    )?;
    #[cfg(test)]
    test_pause_after_prepared(&conflict.id);

    let mut mutation_error = (|| {
        mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
        mutation
            .verify_namespace_binding(&transaction_directory.relative, &transaction_directory)?;
        verify_durable_receipt(mutation, &transaction_directory, &prepared_receipt)?;
        verify_opened_directory_generation(
            &conflict_directory,
            &conflict.directory_generation,
            "projection conflict directory changed after preparing recovery receipt",
        )
    })()
    .err();
    if mutation_error.is_none() {
        for (index, expected) in losers.iter().enumerate() {
            let step = (|| {
                mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
                mutation.verify_namespace_binding(
                    &transaction_directory.relative,
                    &transaction_directory,
                )?;
                verify_durable_receipt(mutation, &transaction_directory, &prepared_receipt)?;
                verify_file_snapshot_at(&conflict_directory, kept)?;
                verify_file_snapshot_at(&conflict_directory, expected)?;
                let leaf = std::ffi::OsStr::new(&expected.name);
                if entry_exists_at(&transaction_directory.handle, leaf)? {
                    return Err(format!(
                        "archive destination appeared before no-replace move: {}",
                        expected.name
                    ));
                }
                rename_no_replace_at(
                    &conflict_directory.handle,
                    leaf,
                    &transaction_directory.handle,
                    leaf,
                )
                .map_err(|error| {
                    format!(
                        "archive projection source {} with secure no-replace rename: {error}",
                        expected.path
                    )
                })?;
                manifest.moves[index].status = "moved".to_string();
                sync_moved_file_at(&conflict_directory, &transaction_directory, leaf, expected)?;
                mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
                mutation.verify_namespace_binding(
                    &transaction_directory.relative,
                    &transaction_directory,
                )?;
                Ok(())
            })();
            if let Err(error) = step {
                mutation_error = Some(error);
                break;
            }
        }
    }
    if mutation_error.is_none() {
        mutation_error = (|| {
            mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
            mutation.verify_namespace_binding(
                &transaction_directory.relative,
                &transaction_directory,
            )?;
            verify_file_snapshot_at(&conflict_directory, kept)
        })()
        .err();
    }
    let move_statuses = manifest
        .moves
        .iter()
        .map(|item| item.status.clone())
        .collect::<Vec<_>>();
    let outcome = commit_transaction(
        project,
        mutation,
        &transaction_directory,
        &prepared_receipt,
        &mut manifest,
        mutation_error,
        || {
            verify_multiple_final_state(
                mutation,
                &conflict_relative,
                &conflict_directory,
                &transaction_directory,
                kept,
                &losers,
                &move_statuses,
            )
        },
    );
    let backup_paths = manifest
        .moves
        .iter()
        .filter(|item| item.status == "moved")
        .map(|item| item.destination_path.clone())
        .collect();

    Ok(ProjectionResolution {
        id: conflict.id.clone(),
        kind: conflict.kind.clone(),
        kept_file: kept.name.clone(),
        backup_paths,
        receipt_path: outcome.receipt_path,
        receipt_available: outcome.receipt_available,
        recovery_actions: Vec::new(),
        recovery_required: outcome.recovery_required,
        recovery_error: outcome.recovery_error,
        source_path: None,
        canonical_path: None,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_legacy_leaf(
    project: &Path,
    mutation: &SecureProjectMutation,
    conflict: &ProjectionConflict,
    keep: Option<&str>,
) -> Result<ProjectionResolution, String> {
    let file = conflict
        .files
        .first()
        .ok_or_else(|| "legacy filename conflict has no source file".to_string())?;
    if let Some(keep) = keep {
        if keep != file.name {
            return Err(format!(
                "--keep for a legacy filename migration must exactly match {:?}",
                file.name
            ));
        }
    }
    let source_path = conflict
        .source_path
        .as_deref()
        .ok_or_else(|| "legacy filename conflict is missing sourcePath".to_string())?;
    let canonical_path = conflict
        .canonical_path
        .as_deref()
        .ok_or_else(|| "legacy filename conflict is missing canonicalPath".to_string())?;
    verify_conflict_file_path(conflict, file)?;
    let conflict_relative = path_from_slashes(&conflict.directory);
    let source_relative = path_from_slashes(source_path);
    let destination_relative = path_from_slashes(canonical_path);
    if source_relative.parent() != Some(conflict_relative.as_path())
        || destination_relative.parent() != Some(conflict_relative.as_path())
    {
        return Err(
            "legacy migration source and destination must share the exact conflict directory"
                .to_string(),
        );
    }
    let source_leaf = source_relative
        .file_name()
        .ok_or_else(|| "legacy migration source has no leaf name".to_string())?;
    let destination_leaf = destination_relative
        .file_name()
        .ok_or_else(|| "legacy migration destination has no leaf name".to_string())?;
    let conflict_directory =
        mutation.verify_directory(&conflict_relative, &conflict.directory_generation)?;
    verify_file_snapshot_at(&conflict_directory, file)?;
    if entry_exists_at(&conflict_directory.handle, destination_leaf)? {
        return Err(format!(
            "canonical migration target already exists: {canonical_path}"
        ));
    }

    let transaction_directory = mutation.create_transaction_directory(&conflict.id)?;
    let mut manifest = new_manifest(
        project,
        conflict,
        file,
        vec![TransactionMove {
            operation: "rename".to_string(),
            original_path: source_path.to_string(),
            destination_path: canonical_path.to_string(),
            size: file.size,
            sha256: file.sha256.clone(),
            status: "pending".to_string(),
        }],
    );
    let prepared_receipt = write_manifest_durable(
        project,
        mutation,
        &transaction_directory,
        "prepared.json",
        &manifest,
    )?;
    #[cfg(test)]
    test_pause_after_prepared(&conflict.id);

    let mutation_result = (|| {
        mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
        mutation
            .verify_namespace_binding(&transaction_directory.relative, &transaction_directory)?;
        verify_durable_receipt(mutation, &transaction_directory, &prepared_receipt)?;
        verify_opened_directory_generation(
            &conflict_directory,
            &conflict.directory_generation,
            "projection conflict directory changed after preparing recovery receipt",
        )?;
        verify_file_snapshot_at(&conflict_directory, file)?;
        if entry_exists_at(&conflict_directory.handle, destination_leaf)? {
            return Err(format!(
                "canonical migration target appeared before no-replace rename: {canonical_path}"
            ));
        }
        rename_no_replace_at(
            &conflict_directory.handle,
            source_leaf,
            &conflict_directory.handle,
            destination_leaf,
        )
        .map_err(|error| {
            format!(
                "rename legacy projection source {source_path} to {canonical_path} with secure no-replace rename: {error}"
            )
        })?;
        manifest.moves[0].status = "moved".to_string();
        sync_moved_file_at(
            &conflict_directory,
            &conflict_directory,
            destination_leaf,
            file,
        )?;
        mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
        mutation
            .verify_namespace_binding(&transaction_directory.relative, &transaction_directory)?;
        Ok(())
    })();
    if manifest.moves[0].status == "moved" {
        manifest.kept.path = canonical_path.to_string();
        manifest.kept.name = destination_leaf
            .to_str()
            .ok_or_else(|| "canonical migration filename is not UTF-8".to_string())?
            .to_string();
    }
    let move_status = manifest.moves[0].status.clone();
    let outcome = commit_transaction(
        project,
        mutation,
        &transaction_directory,
        &prepared_receipt,
        &mut manifest,
        mutation_result.err(),
        || {
            verify_legacy_final_state(
                mutation,
                &conflict_relative,
                &conflict_directory,
                &transaction_directory,
                (source_leaf, destination_leaf),
                file,
                &move_status,
            )
        },
    );

    let kept_file = destination_leaf
        .to_str()
        .ok_or_else(|| "canonical migration filename is not UTF-8".to_string())?
        .to_string();
    Ok(ProjectionResolution {
        id: conflict.id.clone(),
        kind: conflict.kind.clone(),
        kept_file,
        backup_paths: Vec::new(),
        receipt_path: outcome.receipt_path,
        receipt_available: outcome.receipt_available,
        recovery_actions: Vec::new(),
        recovery_required: outcome.recovery_required,
        recovery_error: outcome.recovery_error,
        source_path: Some(source_path.to_string()),
        canonical_path: Some(canonical_path.to_string()),
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reconcile_pending_recovery(
    project: &Path,
    mutation: &SecureProjectMutation,
    recovery: &PendingRecovery,
    action: Option<&str>,
) -> Result<ProjectionResolution, String> {
    let action = action.unwrap_or("resume");
    if action == "quarantine" {
        if !recovery.quarantine_allowed {
            return Err(
                "this recovery record cannot be securely quarantined because its exact transaction directory identity was not proven"
                    .to_string(),
            );
        }
        quarantine_recovery_transaction(mutation, recovery)?;
        return Ok(ProjectionResolution {
            id: recovery.id.clone(),
            kind: "projection-recovery-quarantined".to_string(),
            kept_file: String::new(),
            backup_paths: Vec::new(),
            receipt_path: String::new(),
            receipt_available: false,
            recovery_actions: Vec::new(),
            recovery_required: false,
            recovery_error: None,
            source_path: None,
            canonical_path: None,
        });
    }
    if action != "resume" {
        return Err(format!(
            "recovery action must be exactly \"resume\" or \"quarantine\", not {action:?}"
        ));
    }
    let mut manifest = recovery.resume_manifest.clone().ok_or_else(|| {
        "this recovery has no valid replayable prepared receipt; rerun with --keep quarantine after reviewing the recorded transaction"
            .to_string()
    })?;
    validate_replay_semantics(project, recovery, &manifest)?;
    let receipt_file = recovery
        .receipt_file
        .as_deref()
        .ok_or_else(|| "recovery receipt path is unavailable".to_string())?;
    let receipt_sha256 = recovery
        .receipt_sha256
        .as_deref()
        .ok_or_else(|| "recovery receipt hash is unavailable".to_string())?;
    let transaction_directory =
        mutation.verify_directory(&recovery.transaction_relative, &recovery.generation)?;
    let source_receipt = capture_hashed_receipt(
        mutation,
        &transaction_directory,
        receipt_file,
        receipt_sha256,
        &manifest.conflict_id,
    )?;

    let conflict_relative = path_from_slashes(&manifest.directory);
    let conflict_directory = mutation.open_directory(&conflict_relative)?;
    mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
    mutation.verify_namespace_binding(&recovery.transaction_relative, &transaction_directory)?;

    if manifest.kind == MULTIPLE_INIT_MARKERS {
        let kept_relative = path_from_slashes(&manifest.kept.path);
        let kept_leaf = kept_relative
            .file_name()
            .ok_or_else(|| "prepared receipt kept path has no leaf".to_string())?;
        verify_hashed_leaf_at(
            &conflict_directory,
            kept_leaf,
            MAX_SYNCED_SCRIPT_BYTES,
            &manifest.kept.sha256,
            "kept projection source",
        )?;
    }

    for item in &mut manifest.moves {
        verify_hashed_leaf_at(
            &transaction_directory,
            std::ffi::OsStr::new(receipt_file),
            MAX_TRANSACTION_MANIFEST_BYTES as u64,
            receipt_sha256,
            "recovery receipt",
        )?;
        mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
        mutation
            .verify_namespace_binding(&recovery.transaction_relative, &transaction_directory)?;

        let original = path_from_slashes(&item.original_path);
        let destination = path_from_slashes(&item.destination_path);
        let original_leaf = original
            .file_name()
            .ok_or_else(|| "recovery move source has no leaf".to_string())?;
        let destination_leaf = destination
            .file_name()
            .ok_or_else(|| "recovery move destination has no leaf".to_string())?;
        let destination_parent = destination
            .parent()
            .ok_or_else(|| "recovery move destination has no parent".to_string())?;
        let destination_directory = if destination_parent == conflict_relative {
            mutation.open_directory(&conflict_relative)?
        } else {
            mutation.open_directory(destination_parent)?
        };
        mutation.verify_namespace_binding(destination_parent, &destination_directory)?;

        let source_exists = entry_exists_at(&conflict_directory.handle, original_leaf)?;
        let destination_exists = entry_exists_at(&destination_directory.handle, destination_leaf)?;
        match (source_exists, destination_exists) {
            (true, false) => {
                verify_hashed_leaf_at(
                    &conflict_directory,
                    original_leaf,
                    MAX_SYNCED_SCRIPT_BYTES,
                    &item.sha256,
                    "pending recovery move source",
                )?;
                verify_hashed_leaf_at(
                    &transaction_directory,
                    std::ffi::OsStr::new(receipt_file),
                    MAX_TRANSACTION_MANIFEST_BYTES as u64,
                    receipt_sha256,
                    "recovery receipt",
                )?;
                mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
                mutation.verify_namespace_binding(
                    destination_parent,
                    &destination_directory,
                )?;
                rename_no_replace_at(
                    &conflict_directory.handle,
                    original_leaf,
                    &destination_directory.handle,
                    destination_leaf,
                )
                .map_err(|error| {
                    format!(
                        "resume recovery move {} -> {} with no-replace rename: {error}",
                        item.original_path, item.destination_path
                    )
                })?;
                conflict_directory
                    .handle
                    .sync_all()
                    .map_err(|error| format!("sync resumed recovery source directory: {error}"))?;
                destination_directory
                    .handle
                    .sync_all()
                    .map_err(|error| {
                        format!("sync resumed recovery destination directory: {error}")
                    })?;
                mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
                mutation.verify_namespace_binding(
                    destination_parent,
                    &destination_directory,
                )?;
                if entry_exists_at(&conflict_directory.handle, original_leaf)? {
                    return Err(format!(
                        "recovery move source reappeared after resume: {}",
                        item.original_path
                    ));
                }
                verify_hashed_leaf_at(
                    &destination_directory,
                    destination_leaf,
                    MAX_SYNCED_SCRIPT_BYTES,
                    &item.sha256,
                    "resumed recovery move destination",
                )?;
            }
            (false, true) => {
                verify_hashed_leaf_at(
                    &destination_directory,
                    destination_leaf,
                    MAX_SYNCED_SCRIPT_BYTES,
                    &item.sha256,
                    "already moved recovery destination",
                )?;
            }
            (true, true) => {
                return Err(format!(
                    "recovery move is ambiguous because both source and destination exist: {} and {}; use the quarantine action only after manual review",
                    item.original_path, item.destination_path
                ))
            }
            (false, false) => {
                return Err(format!(
                    "recovery move is ambiguous because both source and destination are missing: {} and {}; use the quarantine action only after manual review",
                    item.original_path, item.destination_path
                ))
            }
        }
        item.status = "moved".to_string();
    }

    if manifest.kind == LEGACY_RESERVED_INIT_LEAF {
        let destination = path_from_slashes(&manifest.moves[0].destination_path);
        manifest.kept.path = manifest.moves[0].destination_path.clone();
        manifest.kept.name = destination
            .file_name()
            .and_then(|leaf| leaf.to_str())
            .ok_or_else(|| "legacy recovery destination leaf is not UTF-8".to_string())?
            .to_string();
    }
    manifest.state = "reconciled".to_string();
    manifest.recovery_required = false;
    manifest.error = None;
    manifest.reconciles_receipt_sha256 = Some(receipt_sha256.to_string());
    manifest.reconciled_at_ms = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    );
    verify_reconciled_final_state(mutation, &manifest, &recovery.transaction_relative)?;
    let durable = match write_manifest_durable(
        project,
        mutation,
        &transaction_directory,
        "reconciled.json",
        &manifest,
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            let rescue_error = bounded_error(&format!(
                "failed to publish a proven reconciled receipt: {error}"
            ));
            let outcome = persist_recovery_receipt(
                project,
                mutation,
                &source_receipt,
                &mut manifest,
                rescue_error.clone(),
            );
            return Err(format!(
                "{rescue_error}; recovery-required rescue receipt {}",
                if outcome.receipt_available {
                    format!("was persisted at {}", outcome.receipt_path)
                } else {
                    format!(
                        "could not be proven: {}",
                        outcome
                            .recovery_error
                            .unwrap_or_else(|| "unknown rescue failure".to_string())
                    )
                }
            ));
        }
    };
    if let Err(error) =
        verify_reconciled_final_state(mutation, &manifest, &recovery.transaction_relative)
            .and_then(|()| verify_durable_receipt(mutation, &transaction_directory, &durable))
    {
        let quarantine = quarantine_manifest_leaf(mutation, &transaction_directory, &durable);
        return Err(match quarantine {
            Ok(()) => format!(
                "reconciled transaction failed its terminal proof and was quarantined: {error}"
            ),
            Err(quarantine_error) => {
                let rescue_error = bounded_error(&format!(
                    "reconciled transaction failed its terminal proof: {error}; failed to quarantine the untrusted clean receipt: {quarantine_error}"
                ));
                let outcome = persist_recovery_receipt(
                    project,
                    mutation,
                    &source_receipt,
                    &mut manifest,
                    rescue_error.clone(),
                );
                format!(
                    "{rescue_error}; recovery-required rescue receipt {}",
                    if outcome.receipt_available {
                        format!("was persisted at {}", outcome.receipt_path)
                    } else {
                        format!(
                            "could not be proven: {}",
                            outcome
                                .recovery_error
                                .unwrap_or_else(|| "unknown rescue failure".to_string())
                        )
                    }
                )
            }
        });
    }

    Ok(ProjectionResolution {
        id: recovery.id.clone(),
        kind: manifest.kind.clone(),
        kept_file: manifest.kept.name.clone(),
        backup_paths: manifest
            .moves
            .iter()
            .filter(|item| item.operation == "archive")
            .map(|item| item.destination_path.clone())
            .collect(),
        receipt_path: durable.path,
        receipt_available: true,
        recovery_actions: Vec::new(),
        recovery_required: false,
        recovery_error: None,
        source_path: (manifest.kind == LEGACY_RESERVED_INIT_LEAF)
            .then(|| manifest.moves[0].original_path.clone()),
        canonical_path: (manifest.kind == LEGACY_RESERVED_INIT_LEAF)
            .then(|| manifest.moves[0].destination_path.clone()),
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_replay_semantics(
    project: &Path,
    recovery: &PendingRecovery,
    manifest: &TransactionManifest,
) -> Result<(), String> {
    let conflict_relative = path_from_slashes(&manifest.directory);
    match manifest.kind.as_str() {
        MULTIPLE_INIT_MARKERS => {
            if manifest
                .moves
                .iter()
                .any(|item| item.operation != "archive")
            {
                return Err("multiple-marker recovery contains a non-archive move".to_string());
            }
            let kept = path_from_slashes(&manifest.kept.path);
            let kept_leaf = kept
                .file_name()
                .and_then(|leaf| leaf.to_str())
                .ok_or_else(|| "multiple-marker recovery kept filename is not UTF-8".to_string())?;
            if kept.parent() != Some(conflict_relative.as_path())
                || kept_leaf != manifest.kept.name
                || !fs_map::init_path_describes_parent(&project.join(&kept))
            {
                return Err(
                    "multiple-marker recovery kept path is not an exact parent init marker"
                        .to_string(),
                );
            }
            let kept_key = kept_leaf.to_ascii_lowercase();
            let mut original_leaves = std::collections::HashSet::new();
            let mut destination_leaves = std::collections::HashSet::new();
            for item in &manifest.moves {
                let original = path_from_slashes(&item.original_path);
                let destination = path_from_slashes(&item.destination_path);
                let original_leaf = original
                    .file_name()
                    .and_then(|leaf| leaf.to_str())
                    .ok_or_else(|| {
                        "multiple-marker recovery source filename is not UTF-8".to_string()
                    })?;
                let destination_leaf = destination
                    .file_name()
                    .and_then(|leaf| leaf.to_str())
                    .ok_or_else(|| {
                        "multiple-marker recovery destination filename is not UTF-8".to_string()
                    })?;
                let original_key = original_leaf.to_ascii_lowercase();
                let destination_key = destination_leaf.to_ascii_lowercase();
                if original.parent() != Some(conflict_relative.as_path())
                    || destination.parent() != Some(recovery.transaction_relative.as_path())
                    || original_leaf != destination_leaf
                    || original_key == kept_key
                    || !fs_map::init_path_describes_parent(&project.join(&original))
                    || !original_leaves.insert(original_key)
                    || !destination_leaves.insert(destination_key)
                {
                    return Err(
                        "multiple-marker recovery archive route is outside its exact conflict/transaction leaves"
                            .to_string(),
                    );
                }
            }
        }
        LEGACY_RESERVED_INIT_LEAF => {
            if manifest.moves.len() != 1 || manifest.moves[0].operation != "rename" {
                return Err("legacy recovery must contain exactly one rename".to_string());
            }
            let original = path_from_slashes(&manifest.moves[0].original_path);
            let destination = path_from_slashes(&manifest.moves[0].destination_path);
            if original.parent() != Some(conflict_relative.as_path())
                || destination.parent() != Some(conflict_relative.as_path())
                || manifest.kept.path != manifest.moves[0].original_path
                || original.file_name().and_then(|leaf| leaf.to_str())
                    != Some(manifest.kept.name.as_str())
                || manifest.kept.size != manifest.moves[0].size
                || manifest.kept.sha256 != manifest.moves[0].sha256
            {
                return Err(
                    "legacy recovery rename or kept-file proof is not its exact conflict source"
                        .to_string(),
                );
            }
            let authoritative =
                authoritative_legacy_recovery_destination(project, &original, &destination)?;
            if authoritative != project.join(&destination) {
                return Err(format!(
                    "legacy recovery destination is not authoritative; expected {}",
                    authoritative.display()
                ));
            }
        }
        _ => return Err("unsupported recovery kind".to_string()),
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn authoritative_legacy_recovery_destination(
    project: &Path,
    original: &Path,
    recorded_destination: &Path,
) -> Result<PathBuf, String> {
    let original_absolute = project.join(original);
    if metadata_no_follow(&original_absolute)
        .map_err(|error| format!("inspect legacy recovery source: {error}"))?
        .is_some()
    {
        return fs_map::legacy_reserved_init_leaf_migration(&original_absolute)
            .map_err(|error| format!("recompute authoritative legacy migration: {error}"))?
            .ok_or_else(|| {
                "legacy recovery source no longer requires the recorded migration".to_string()
            });
    }

    let original_leaf = original
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .ok_or_else(|| "legacy recovery source leaf is not UTF-8".to_string())?;
    let parsed = fs_map::parse_reserved_init_filename(original_leaf)
        .ok_or_else(|| "legacy recovery source is outside reserved init grammar".to_string())?;
    if fs_map::init_path_describes_parent(&original_absolute) {
        return Err("legacy recovery source describes its parent and is not a leaf".to_string());
    }
    let parent = original
        .parent()
        .ok_or_else(|| "legacy recovery source has no parent".to_string())?;
    let index =
        fs_safety::PortableDirectoryIndex::read_for_projection_repair(&project.join(parent))
            .map_err(|error| format!("scan legacy recovery siblings: {error}"))?;
    let recorded_leaf = recorded_destination.file_name();
    let taken = index
        .entries()
        .iter()
        .filter(|entry| {
            Some(entry.path.as_path()) != Some(original_absolute.as_path())
                && entry.path.file_name() != recorded_leaf
        })
        .map(|entry| entry.fragment.clone())
        .collect::<Vec<_>>();
    let canonical = fs_map::instance_to_path(
        &fs_map::InstanceDescriptor {
            class: parsed.class.class_name(),
            name: &parsed.leaf_name,
            has_children: false,
        },
        &taken,
    );
    Ok(project.join(parent).join(canonical.fragment))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_hashed_leaf_at(
    directory: &SecureDirectory,
    leaf: &std::ffi::OsStr,
    max_bytes: u64,
    expected_sha256: &str,
    context: &str,
) -> Result<(), String> {
    let mut file = open_regular_file_at(&directory.handle, leaf)?;
    let before = opened_generation(&file)?;
    if before.len > max_bytes {
        return Err(format!("{context} exceeds maximum size {max_bytes}"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(before.len).unwrap_or(0));
    std::io::Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {context}: {error}"))?;
    let after = opened_generation(&file)?;
    if before != after
        || bytes.len() as u64 > max_bytes
        || format!("{:x}", Sha256::digest(&bytes)) != expected_sha256
    {
        return Err(format!(
            "{context} content or identity does not match its receipt proof"
        ));
    }
    let mut rebound = open_regular_file_at(&directory.handle, leaf)?;
    if opened_generation(&rebound)?.identity != before.identity {
        return Err(format!("{context} leaf no longer names the verified file"));
    }
    let mut rebound_bytes = Vec::with_capacity(bytes.len());
    std::io::Read::by_ref(&mut rebound)
        .take(max_bytes + 1)
        .read_to_end(&mut rebound_bytes)
        .map_err(|error| format!("re-read {context}: {error}"))?;
    if opened_generation(&rebound)? != after || rebound_bytes != bytes {
        return Err(format!("{context} changed during exact leaf rebinding"));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn capture_hashed_receipt(
    mutation: &SecureProjectMutation,
    directory: &SecureDirectory,
    file_name: &str,
    expected_sha256: &str,
    conflict_id: &str,
) -> Result<DurableReceipt, String> {
    let leaf = std::ffi::OsStr::new(file_name);
    let mut file = open_regular_file_at(&directory.handle, leaf)?;
    let generation = opened_generation(&file)?;
    if generation.len > MAX_TRANSACTION_MANIFEST_BYTES as u64 {
        return Err("recovery receipt exceeds its maximum size".to_string());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(generation.len).unwrap_or(0));
    std::io::Read::by_ref(&mut file)
        .take((MAX_TRANSACTION_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read recovery receipt: {error}"))?;
    if opened_generation(&file)? != generation
        || bytes.len() > MAX_TRANSACTION_MANIFEST_BYTES
        || format!("{:x}", Sha256::digest(&bytes)) != expected_sha256
    {
        return Err("recovery receipt content or identity does not match its proof".to_string());
    }
    let receipt = DurableReceipt {
        path: relative_path_string(directory.relative.join(file_name))?,
        file_name: file_name.to_string(),
        conflict_id: conflict_id.to_string(),
        generation,
        bytes,
    };
    verify_durable_receipt(mutation, directory, &receipt)?;
    Ok(receipt)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_reconciled_final_state(
    mutation: &SecureProjectMutation,
    manifest: &TransactionManifest,
    transaction_relative: &Path,
) -> Result<(), String> {
    let conflict_relative = path_from_slashes(&manifest.directory);
    let conflict_directory = mutation.open_directory(&conflict_relative)?;
    mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
    for item in &manifest.moves {
        let original = path_from_slashes(&item.original_path);
        let destination = path_from_slashes(&item.destination_path);
        let original_leaf = original.file_name().unwrap();
        if entry_exists_at(&conflict_directory.handle, original_leaf)? {
            return Err(format!(
                "reconciled move source still exists: {}",
                item.original_path
            ));
        }
        let destination_parent = destination.parent().unwrap();
        let directory = mutation.open_directory(destination_parent)?;
        mutation.verify_namespace_binding(destination_parent, &directory)?;
        verify_hashed_leaf_at(
            &directory,
            destination.file_name().unwrap(),
            MAX_SYNCED_SCRIPT_BYTES,
            &item.sha256,
            "reconciled move destination",
        )?;
        mutation.verify_namespace_binding(destination_parent, &directory)?;
    }
    mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
    let transaction = mutation.open_directory(transaction_relative)?;
    mutation.verify_namespace_binding(transaction_relative, &transaction)?;
    if manifest.kind == MULTIPLE_INIT_MARKERS {
        let kept = path_from_slashes(&manifest.kept.path);
        verify_hashed_leaf_at(
            &conflict_directory,
            kept.file_name().unwrap(),
            MAX_SYNCED_SCRIPT_BYTES,
            &manifest.kept.sha256,
            "reconciled kept source",
        )?;
        mutation.verify_namespace_binding(&conflict_relative, &conflict_directory)?;
    }
    mutation.verify_namespace_binding(transaction_relative, &transaction)?;
    mutation.verify_project_root_binding()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn quarantine_recovery_transaction(
    mutation: &SecureProjectMutation,
    recovery: &PendingRecovery,
) -> Result<(), String> {
    mutation.verify_directory(&recovery.transaction_relative, &recovery.generation)?;
    let parent_relative = recovery
        .transaction_relative
        .parent()
        .ok_or_else(|| "recovery transaction has no parent".to_string())?;
    if parent_relative != Path::new(BACKUP_ROOT) {
        return Err("recovery transaction is outside the backup root".to_string());
    }
    let source_leaf = recovery
        .transaction_relative
        .file_name()
        .ok_or_else(|| "recovery transaction has no leaf".to_string())?;
    let parent = mutation.open_directory(parent_relative)?;
    mutation.verify_namespace_binding(parent_relative, &parent)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for attempt in 0..128u16 {
        let destination = format!(
            "projection-quarantined-{timestamp}-{}-{attempt}",
            std::process::id()
        );
        match rename_no_replace_at(
            &parent.handle,
            source_leaf,
            &parent.handle,
            std::ffi::OsStr::new(&destination),
        ) {
            Ok(()) => {
                parent
                    .handle
                    .sync_all()
                    .map_err(|error| format!("sync recovery quarantine: {error}"))?;
                mutation.verify_namespace_binding(parent_relative, &parent)?;
                if entry_exists_at(&parent.handle, source_leaf)? {
                    return Err("recovery transaction reappeared after quarantine".to_string());
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("quarantine recovery transaction: {error}")),
        }
    }
    Err("could not allocate a recovery quarantine directory name".to_string())
}

fn new_manifest(
    project: &Path,
    conflict: &ProjectionConflict,
    kept: &ProjectionFile,
    moves: Vec<TransactionMove>,
) -> TransactionManifest {
    TransactionManifest {
        version: TRANSACTION_VERSION,
        state: "prepared".to_string(),
        conflict_id: conflict.id.clone(),
        kind: conflict.kind.clone(),
        project: project.display().to_string(),
        directory: conflict.directory.clone(),
        prepared_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        kept: TransactionFile {
            path: kept.path.clone(),
            name: kept.name.clone(),
            size: kept.size,
            sha256: kept.sha256.clone(),
            generation: generation_string(&kept.generation),
        },
        moves,
        recovery_protocol: PREPARED_RECOVERY_PROTOCOL.to_string(),
        recovery_required: true,
        error: Some(
            "transaction is prepared but not durably committed; follow recoveryProtocol"
                .to_string(),
        ),
        reconciles_receipt_sha256: None,
        reconciled_at_ms: None,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn commit_transaction(
    project: &Path,
    mutation: &SecureProjectMutation,
    transaction_directory: &SecureDirectory,
    prepared_receipt: &DurableReceipt,
    manifest: &mut TransactionManifest,
    mutation_error: Option<String>,
    final_state_proof: impl Fn() -> Result<(), String>,
) -> CommitOutcome {
    let mutation_error = mutation_error.or_else(|| final_state_proof().err());
    manifest.state = "committed".to_string();
    manifest.recovery_required = mutation_error.is_some();
    manifest.error = mutation_error.map(|error| bounded_error(&error));
    let published = write_manifest_durable(
        project,
        mutation,
        transaction_directory,
        "committed.json",
        manifest,
    );
    let receipt = match published {
        Ok(receipt) => receipt,
        Err(error) => {
            return persist_recovery_receipt(
                project,
                mutation,
                prepared_receipt,
                manifest,
                bounded_error(&format!(
                    "{}failed to persist committed receipt: {error}",
                    manifest
                        .error
                        .as_deref()
                        .map(|value| format!("{value}; "))
                        .unwrap_or_default(),
                )),
            )
        }
    };

    if manifest.recovery_required {
        return CommitOutcome {
            receipt_path: receipt.path,
            receipt_available: true,
            recovery_required: true,
            recovery_error: manifest.error.clone(),
        };
    }

    let terminal_proof = final_state_proof()
        .and_then(|()| verify_durable_receipt(mutation, transaction_directory, &receipt));
    match terminal_proof {
        Ok(()) => CommitOutcome {
            receipt_path: receipt.path,
            receipt_available: true,
            recovery_required: false,
            recovery_error: None,
        },
        Err(error) => {
            let quarantine_error =
                quarantine_manifest_leaf(mutation, transaction_directory, &receipt);
            let error = match quarantine_error {
                Ok(()) => bounded_error(&format!(
                    "terminal transaction proof failed after receipt publication: {error}"
                )),
                Err(quarantine_error) => bounded_error(&format!(
                    "terminal transaction proof failed after receipt publication: {error}; failed to quarantine untrusted terminal receipt: {quarantine_error}"
                )),
            };
            persist_recovery_receipt(project, mutation, prepared_receipt, manifest, error)
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn persist_recovery_receipt(
    project: &Path,
    mutation: &SecureProjectMutation,
    prepared_receipt: &DurableReceipt,
    manifest: &mut TransactionManifest,
    error: String,
) -> CommitOutcome {
    manifest.state = "committed".to_string();
    manifest.recovery_required = true;
    manifest.error = Some(error.clone());
    let rescue = mutation
        .create_transaction_directory(&manifest.conflict_id)
        .and_then(|directory| {
            write_manifest_durable(project, mutation, &directory, "committed.json", manifest)
        });
    match rescue {
        Ok(receipt) => CommitOutcome {
            receipt_path: receipt.path,
            receipt_available: true,
            recovery_required: true,
            recovery_error: Some(error),
        },
        Err(rescue_error) => {
            let prepared_available = mutation
                .open_directory(
                    Path::new(&prepared_receipt.path)
                        .parent()
                        .unwrap_or_else(|| Path::new("")),
                )
                .and_then(|directory| {
                    verify_durable_receipt(mutation, &directory, prepared_receipt)
                })
                .is_ok();
            CommitOutcome {
                receipt_path: if prepared_available {
                    prepared_receipt.path.clone()
                } else {
                    String::new()
                },
                receipt_available: prepared_available,
                recovery_required: true,
                recovery_error: Some(bounded_error(&format!(
                    "{error}; failed to persist rescue receipt: {rescue_error}; {}",
                    if prepared_available {
                        "the conservative prepared receipt remains available"
                    } else {
                        "no receipt remains provably reachable at the canonical project path"
                    }
                ))),
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_manifest_durable(
    _project: &Path,
    mutation: &SecureProjectMutation,
    transaction_directory: &SecureDirectory,
    file_name: &str,
    manifest: &TransactionManifest,
) -> Result<DurableReceipt, String> {
    mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)?;
    let mut bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("encode transaction manifest: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_TRANSACTION_MANIFEST_BYTES {
        return Err(format!(
            "transaction manifest exceeds maximum size {MAX_TRANSACTION_MANIFEST_BYTES} bytes"
        ));
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut temporary = None;
    for attempt in 0..128u16 {
        let name = format!(
            ".{file_name}.{}.{timestamp}.{attempt}.pending",
            std::process::id()
        );
        match create_new_file_at(
            &transaction_directory.handle,
            std::ffi::OsStr::new(&name),
            0o600,
        ) {
            Ok(file) => {
                temporary = Some((name, file));
                break;
            }
            Err(error) if error.contains("File exists") => continue,
            Err(error) => return Err(error),
        }
    }
    let (temporary_name, mut file) =
        temporary.ok_or_else(|| "could not allocate a unique manifest staging file".to_string())?;
    let temporary_leaf = std::ffi::OsStr::new(&temporary_name);
    let write_result = file.write_all(&bytes).and_then(|_| file.sync_all());
    if let Err(error) = write_result {
        drop(file);
        let _ = unlink_file_at(&transaction_directory.handle, temporary_leaf);
        let _ = transaction_directory.handle.sync_all();
        return Err(format!(
            "persist secure transaction manifest {file_name}: {error}"
        ));
    }
    let generation = opened_generation(&file)?;
    verify_opened_exact_bytes(&mut file, &generation, &bytes, file_name)?;
    drop(file);
    verify_exact_file_leaf_at(
        transaction_directory,
        temporary_leaf,
        &generation,
        &bytes,
        file_name,
        None,
    )?;
    transaction_directory
        .handle
        .sync_all()
        .map_err(|error| format!("sync secure transaction directory: {error}"))?;
    mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)?;
    verify_exact_file_leaf_at(
        transaction_directory,
        temporary_leaf,
        &generation,
        &bytes,
        file_name,
        None,
    )?;

    let receipt_path = relative_path_string(transaction_directory.relative.join(file_name))?;
    let final_leaf = std::ffi::OsStr::new(file_name);
    rename_no_replace_at(
        &transaction_directory.handle,
        temporary_leaf,
        &transaction_directory.handle,
        final_leaf,
    )
    .map_err(|error| format!("publish secure transaction manifest {file_name}: {error}"))?;
    let receipt = DurableReceipt {
        path: receipt_path,
        file_name: file_name.to_string(),
        conflict_id: manifest.conflict_id.clone(),
        generation,
        bytes,
    };
    let publication_proof = {
        #[cfg(test)]
        {
            test_fail_after_manifest_rename(&manifest.conflict_id, file_name)
        }
        #[cfg(not(test))]
        {
            Ok(())
        }
    }
    .and_then(|()| {
        transaction_directory
            .handle
            .sync_all()
            .map_err(|error| format!("sync transaction manifest publication: {error}"))
    })
    .and_then(|()| {
        mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)
    })
    .and_then(|()| {
        #[cfg(test)]
        test_pause_after_manifest_publish(&manifest.conflict_id, file_name);
        verify_durable_receipt(mutation, transaction_directory, &receipt)
    });
    if let Err(error) = publication_proof {
        let quarantine = quarantine_manifest_leaf(mutation, transaction_directory, &receipt);
        return Err(match quarantine {
            Ok(()) => format!(
                "published transaction manifest {file_name} failed exact leaf proof and was quarantined: {error}"
            ),
            Err(quarantine_error) => format!(
                "published transaction manifest {file_name} failed exact leaf proof: {error}; failed to quarantine it: {quarantine_error}"
            ),
        });
    }
    Ok(receipt)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_durable_receipt(
    mutation: &SecureProjectMutation,
    transaction_directory: &SecureDirectory,
    receipt: &DurableReceipt,
) -> Result<(), String> {
    let expected_path =
        relative_path_string(transaction_directory.relative.join(&receipt.file_name))?;
    if receipt.path != expected_path {
        return Err("transaction receipt path no longer matches its held directory".to_string());
    }
    mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)?;
    verify_exact_file_leaf_at(
        transaction_directory,
        std::ffi::OsStr::new(&receipt.file_name),
        &receipt.generation,
        &receipt.bytes,
        &receipt.file_name,
        Some((&receipt.conflict_id, &receipt.file_name)),
    )?;
    mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_exact_file_leaf_at(
    directory: &SecureDirectory,
    leaf: &std::ffi::OsStr,
    expected_generation: &fs_safety::FileGeneration,
    expected_bytes: &[u8],
    context: &str,
    test_receipt_hook: Option<(&str, &str)>,
) -> Result<(), String> {
    let mut file = open_regular_file_at(&directory.handle, leaf)?;
    verify_opened_exact_bytes(&mut file, expected_generation, expected_bytes, context)?;
    #[cfg(test)]
    if let Some((conflict_id, file_name)) = test_receipt_hook {
        test_pause_after_receipt_first_open(conflict_id, file_name);
    }
    #[cfg(not(test))]
    let _ = test_receipt_hook;
    let mut rebound_file = open_regular_file_at(&directory.handle, leaf)?;
    let rebound = opened_generation(&rebound_file)?;
    if &rebound != expected_generation {
        return Err(format!(
            "secure file leaf changed while proving exact binding for {context}"
        ));
    }
    verify_opened_exact_bytes(
        &mut rebound_file,
        expected_generation,
        expected_bytes,
        context,
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_opened_exact_bytes(
    file: &mut fs::File,
    expected_generation: &fs_safety::FileGeneration,
    expected_bytes: &[u8],
    context: &str,
) -> Result<(), String> {
    if expected_bytes.len() > MAX_TRANSACTION_MANIFEST_BYTES {
        return Err(format!("exact-byte proof for {context} exceeds its bound"));
    }
    let before = opened_generation(file)?;
    if &before != expected_generation {
        return Err(format!(
            "secure file identity or generation changed before reading {context}"
        ));
    }
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| format!("seek secure transaction file {context}: {error}"))?;
    let mut actual = Vec::with_capacity(expected_bytes.len());
    file.take((MAX_TRANSACTION_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut actual)
        .map_err(|error| format!("read secure transaction file {context}: {error}"))?;
    let after = opened_generation(file)?;
    if &after != expected_generation || actual != expected_bytes {
        return Err(format!(
            "secure transaction file content or identity changed for {context}"
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn quarantine_manifest_leaf(
    mutation: &SecureProjectMutation,
    transaction_directory: &SecureDirectory,
    receipt: &DurableReceipt,
) -> Result<(), String> {
    #[cfg(test)]
    test_fail_manifest_quarantine(&receipt.conflict_id, &receipt.file_name)?;
    let source_leaf = std::ffi::OsStr::new(&receipt.file_name);
    if !entry_exists_at(&transaction_directory.handle, source_leaf)? {
        return Ok(());
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..128u16 {
        let destination = format!(".{}.untrusted.{timestamp}.{attempt}", receipt.file_name);
        match rename_no_replace_at(
            &transaction_directory.handle,
            source_leaf,
            &transaction_directory.handle,
            std::ffi::OsStr::new(&destination),
        ) {
            Ok(()) => {
                transaction_directory
                    .handle
                    .sync_all()
                    .map_err(|error| format!("sync quarantined transaction receipt: {error}"))?;
                mutation.verify_namespace_binding(
                    &transaction_directory.relative,
                    transaction_directory,
                )?;
                if entry_exists_at(&transaction_directory.handle, source_leaf)? {
                    return Err(
                        "untrusted terminal receipt reappeared after quarantine".to_string()
                    );
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("quarantine untrusted transaction receipt: {error}")),
        }
    }
    Err("could not allocate a quarantine name for an untrusted transaction receipt".to_string())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_conflict_file_path(
    conflict: &ProjectionConflict,
    expected: &ProjectionFile,
) -> Result<(), String> {
    let relative = path_from_slashes(&expected.path);
    let conflict_directory = path_from_slashes(&conflict.directory);
    if relative.parent() != Some(conflict_directory.as_path())
        || relative.file_name() != Some(std::ffi::OsStr::new(&expected.name))
    {
        return Err(format!(
            "projection source is not one exact leaf of its conflict directory: {}",
            expected.path
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_opened_directory_generation(
    directory: &SecureDirectory,
    expected: &fs_safety::FileGeneration,
    context: &str,
) -> Result<(), String> {
    let actual = opened_generation(&directory.handle)?;
    if &actual != expected {
        return Err(format!("{context}: {}", directory.relative.display()));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_file_snapshot_at(
    directory: &SecureDirectory,
    expected: &ProjectionFile,
) -> Result<(), String> {
    verify_file_snapshot_named_at(directory, std::ffi::OsStr::new(&expected.name), expected)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_file_snapshot_named_at(
    directory: &SecureDirectory,
    leaf: &std::ffi::OsStr,
    expected: &ProjectionFile,
) -> Result<(), String> {
    let mut file = open_regular_file_at(&directory.handle, leaf)?;
    verify_opened_file_contents(&mut file, expected)?;
    let opened = opened_generation(&file)?;
    let mut rebound = open_regular_file_at(&directory.handle, leaf)?;
    let rebound_generation = opened_generation(&rebound)?;
    if rebound_generation.identity != opened.identity || rebound_generation != expected.generation {
        return Err(format!(
            "projection leaf no longer names the verified file during resolution: {}; recovery required",
            directory.relative.join(leaf).display()
        ));
    }
    verify_opened_file_contents(&mut rebound, expected)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_opened_file_contents(
    file: &mut fs::File,
    expected: &ProjectionFile,
) -> Result<(), String> {
    let before_generation = opened_generation(file)?;
    if before_generation != expected.generation {
        return Err(format!(
            "projection source generation changed during resolution: {}; inspect again",
            expected.path
        ));
    }
    if before_generation.len > MAX_SYNCED_SCRIPT_BYTES {
        return Err(format!(
            "projection source exceeds maximum size {} bytes: {}",
            MAX_SYNCED_SCRIPT_BYTES, expected.path
        ));
    }
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| format!("seek projection source {}: {error}", expected.path))?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(before_generation.len)
            .unwrap_or(usize::MAX)
            .min(usize::try_from(MAX_SYNCED_SCRIPT_BYTES).unwrap_or(usize::MAX)),
    );
    {
        let mut bounded = file.take(MAX_SYNCED_SCRIPT_BYTES + 1);
        bounded
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read projection source {}: {error}", expected.path))?;
    }
    if bytes.len() as u64 > MAX_SYNCED_SCRIPT_BYTES {
        return Err(format!(
            "projection source exceeds maximum size {} bytes: {}",
            MAX_SYNCED_SCRIPT_BYTES, expected.path
        ));
    }
    let after_generation = opened_generation(file)?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if after_generation != expected.generation
        || bytes.len() as u64 != expected.size
        || sha256 != expected.sha256
    {
        return Err(format!(
            "projection source changed during resolution: {}; inspect again",
            expected.path
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sync_moved_file_at(
    source_directory: &SecureDirectory,
    destination_directory: &SecureDirectory,
    destination_leaf: &std::ffi::OsStr,
    expected: &ProjectionFile,
) -> Result<(), String> {
    let mut file = open_regular_file_at(&destination_directory.handle, destination_leaf)?;
    let before = opened_generation(&file)?;
    if before != expected.generation {
        return Err(format!(
            "moved projection file identity or generation changed unexpectedly: {}",
            destination_directory
                .relative
                .join(destination_leaf)
                .display()
        ));
    }
    verify_opened_file_contents(&mut file, expected)?;
    file.sync_all().map_err(|error| {
        format!(
            "sync moved projection file {}: {error}",
            destination_directory
                .relative
                .join(destination_leaf)
                .display()
        )
    })?;
    let after = opened_generation(&file)?;
    if after != expected.generation {
        return Err(format!(
            "moved projection file changed while syncing: {}",
            destination_directory
                .relative
                .join(destination_leaf)
                .display()
        ));
    }
    verify_opened_file_contents(&mut file, expected)?;
    verify_file_snapshot_named_at(destination_directory, destination_leaf, expected)?;
    source_directory
        .handle
        .sync_all()
        .map_err(|error| format!("sync source directory after secure rename: {error}"))?;
    if source_directory.handle.as_raw_fd() != destination_directory.handle.as_raw_fd() {
        destination_directory
            .handle
            .sync_all()
            .map_err(|error| format!("sync destination directory after secure rename: {error}"))?;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_multiple_final_state(
    mutation: &SecureProjectMutation,
    conflict_relative: &Path,
    conflict_directory: &SecureDirectory,
    transaction_directory: &SecureDirectory,
    kept: &ProjectionFile,
    losers: &[&ProjectionFile],
    move_statuses: &[String],
) -> Result<(), String> {
    if losers.len() != move_statuses.len() {
        return Err("transaction move proof no longer matches the conflict files".to_string());
    }
    mutation.verify_namespace_binding(conflict_relative, conflict_directory)?;
    mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)?;
    verify_file_snapshot_at(conflict_directory, kept)?;
    for (expected, status) in losers.iter().zip(move_statuses) {
        let leaf = std::ffi::OsStr::new(&expected.name);
        match status.as_str() {
            "moved" => {
                if entry_exists_at(&conflict_directory.handle, leaf)? {
                    return Err(format!(
                        "archived projection source reappeared after move: {}",
                        expected.path
                    ));
                }
                verify_file_snapshot_named_at(transaction_directory, leaf, expected)?;
            }
            "pending" => {
                verify_file_snapshot_at(conflict_directory, expected)?;
                if entry_exists_at(&transaction_directory.handle, leaf)? {
                    return Err(format!(
                        "pending archive destination unexpectedly exists: {}",
                        transaction_directory.relative.join(leaf).display()
                    ));
                }
            }
            other => {
                return Err(format!(
                    "unsupported transaction move status {other:?}; recovery required"
                ))
            }
        }
    }
    mutation.verify_namespace_binding(conflict_relative, conflict_directory)?;
    mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_legacy_final_state(
    mutation: &SecureProjectMutation,
    conflict_relative: &Path,
    conflict_directory: &SecureDirectory,
    transaction_directory: &SecureDirectory,
    leaves: (&std::ffi::OsStr, &std::ffi::OsStr),
    expected: &ProjectionFile,
    move_status: &str,
) -> Result<(), String> {
    let (source_leaf, destination_leaf) = leaves;
    mutation.verify_namespace_binding(conflict_relative, conflict_directory)?;
    mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)?;
    match move_status {
        "moved" => {
            if entry_exists_at(&conflict_directory.handle, source_leaf)? {
                return Err(format!(
                    "legacy projection source reappeared after rename: {}",
                    conflict_directory.relative.join(source_leaf).display()
                ));
            }
            verify_file_snapshot_named_at(conflict_directory, destination_leaf, expected)?;
        }
        "pending" => {
            verify_file_snapshot_named_at(conflict_directory, source_leaf, expected)?;
            if entry_exists_at(&conflict_directory.handle, destination_leaf)? {
                return Err(format!(
                    "pending canonical projection destination unexpectedly exists: {}",
                    conflict_directory.relative.join(destination_leaf).display()
                ));
            }
        }
        other => {
            return Err(format!(
                "unsupported transaction move status {other:?}; recovery required"
            ))
        }
    }
    mutation.verify_namespace_binding(conflict_relative, conflict_directory)?;
    mutation.verify_namespace_binding(&transaction_directory.relative, transaction_directory)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn unlink_file_at(parent: &fs::File, leaf: &std::ffi::OsStr) -> Result<(), String> {
    let leaf = leaf_c_string(leaf).map_err(|error| format!("validate unlink leaf: {error}"))?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "remove incomplete secure transaction file: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(test)]
pub fn acquire_project_operation_lock(
    project: &Path,
    _activity: &str,
) -> Result<ProjectOperationLock, String> {
    let backup_root = project.join(BACKUP_ROOT);
    let backup_root = fs_safety::ensure_descendant_directory_chain(project, &backup_root)
        .map_err(|error| format!("create projection backup root for lock: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&backup_root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("secure projection backup root: {error}"))?;
    }
    let lock_path = backup_root.join(".projection-resolve.lock");
    let guard = fs_safety::guard_descendant_parent_chain(project, &lock_path, true)
        .map_err(|error| format!("guard project projection resolve lock: {error}"))?;
    guard
        .verify()
        .map_err(|error| format!("verify projection resolve lock guard: {error}"))?;
    if let Some(metadata) = metadata_no_follow(&lock_path)
        .map_err(|error| format!("inspect projection resolve lock: {error}"))?
    {
        if !metadata.is_file() {
            return Err(format!(
                "projection resolve lock is not a regular file: {}",
                lock_path.display()
            ));
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let mut file = options
        .open(&lock_path)
        .map_err(|error| format!("open project projection resolve lock: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect opened projection resolve lock: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("opened projection resolve lock is not a regular file".to_string());
    }
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => {
            "another projection resolution is already in progress for this project".to_string()
        }
        fs::TryLockError::Error(error) => {
            format!("lock project projection resolve file: {error}")
        }
    })?;
    file.set_len(0)
        .and_then(|_| writeln!(file, "pid={}", std::process::id()))
        .and_then(|_| {
            writeln!(
                file,
                "acquiredAt={}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            )
        })
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("persist projection resolve lock diagnostics: {error}"))?;
    guard
        .verify()
        .map_err(|error| format!("verify projection resolve lock guard after acquire: {error}"))?;
    Ok(ProjectOperationLock { _file: file })
}

#[cfg(not(test))]
pub fn acquire_project_operation_lock(
    project: &Path,
    activity: &str,
) -> Result<ProjectOperationLock, String> {
    let state_dir = crate::lifecycle::state_dir(None)
        .map_err(|error| format!("locate Ro Sync state directory: {error}"))?;
    let lock_path = state_dir.join("projection-repair").join(format!(
        "{}.operation.lock",
        crate::lifecycle::project_key(project)
    ));
    crate::lifecycle::StartLock::acquire_named(&lock_path, activity)
        .map_err(|error| format!("acquire project filesystem operation lock: {error}"))
}

#[cfg(target_os = "macos")]
fn rename_no_replace_at(
    source_directory: &fs::File,
    source_leaf: &std::ffi::OsStr,
    destination_directory: &fs::File,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    const RENAME_EXCL: u32 = 0x0000_0004;
    unsafe extern "C" {
        fn renameatx_np(
            from_dir_fd: std::os::raw::c_int,
            from: *const std::os::raw::c_char,
            to_dir_fd: std::os::raw::c_int,
            to: *const std::os::raw::c_char,
            flags: u32,
        ) -> std::os::raw::c_int;
    }
    let source = leaf_c_string(source_leaf)?;
    let destination = leaf_c_string(destination_leaf)?;
    if unsafe {
        renameatx_np(
            source_directory.as_raw_fd(),
            source.as_ptr(),
            destination_directory.as_raw_fd(),
            destination.as_ptr(),
            RENAME_EXCL,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_no_replace_at(
    source_directory: &fs::File,
    source_leaf: &std::ffi::OsStr,
    destination_directory: &fs::File,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    let source = leaf_c_string(source_leaf)?;
    let destination = leaf_c_string(destination_leaf)?;
    if unsafe {
        libc::renameat2(
            source_directory.as_raw_fd(),
            source.as_ptr(),
            destination_directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn generation_string(generation: &fs_safety::FileGeneration) -> String {
    format!(
        "len={};mtime={};device={};file={}",
        generation.len,
        generation
            .modified_ns
            .map(|value| value.to_string())
            .unwrap_or_default(),
        generation
            .identity
            .device
            .map(|value| value.to_string())
            .unwrap_or_default(),
        generation
            .identity
            .file
            .map(|value| value.to_string())
            .unwrap_or_default()
    )
}

fn bounded_error(error: &str) -> String {
    error.chars().take(8192).collect()
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn relative_path_string(path: impl AsRef<Path>) -> Result<String, String> {
    let mut fragments = Vec::new();
    for component in path.as_ref().components() {
        match component {
            std::path::Component::Normal(fragment) => {
                let fragment = fragment.to_str().ok_or_else(|| {
                    format!(
                        "projection path is not valid UTF-8: {}",
                        path.as_ref().display()
                    )
                })?;
                fragments.push(fragment);
            }
            _ => {
                return Err(format!(
                    "projection relative path has a rooted, dot, or parent component: {}",
                    path.as_ref().display()
                ))
            }
        }
    }
    Ok(fragments.join("/"))
}

fn path_from_slashes(path: &str) -> PathBuf {
    path.split('/').collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn project() -> TempDir {
        let project = TempDir::new().unwrap();
        fs::create_dir(project.path().join("ReplicatedStorage")).unwrap();
        project
    }

    fn package(project: &TempDir, name: &str) -> PathBuf {
        let package = project.path().join("ReplicatedStorage").join(name);
        fs::create_dir(&package).unwrap();
        package
    }

    fn transaction_with(project: &Path, file_name: &str) -> PathBuf {
        fs::read_dir(project.join(BACKUP_ROOT))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.is_dir() && path.join(file_name).is_file())
            .unwrap_or_else(|| panic!("no transaction contains {file_name}"))
    }

    fn committed_receipts(project: &Path) -> Vec<PathBuf> {
        let backup_root = project.join(BACKUP_ROOT);
        if !backup_root.is_dir() {
            return Vec::new();
        }
        fs::read_dir(backup_root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("committed.json"))
            .filter(|path| path.is_file())
            .collect()
    }

    fn assert_no_clean_terminal_receipt(project: &Path) {
        for receipt in committed_receipts(project) {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
            assert_eq!(
                value["recoveryRequired"],
                true,
                "unexpected clean terminal receipt at {}",
                receipt.display()
            );
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn prepare_marker_recovery(
        project: &TempDir,
        package_name: &str,
        sources: &[(&str, &str)],
        keep: &str,
    ) -> (
        ProjectionConflict,
        SecureProjectMutation,
        SecureDirectory,
        TransactionManifest,
        DurableReceipt,
    ) {
        let package = package(project, package_name);
        for (name, source) in sources {
            fs::write(package.join(name), source).unwrap();
        }
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let canonical = fs_safety::stable_canonical_directory(project.path()).unwrap();
        let mutation = SecureProjectMutation::open(&canonical).unwrap();
        let transaction = mutation.create_transaction_directory(&conflict.id).unwrap();
        let kept = conflict
            .files
            .iter()
            .find(|file| file.name == keep)
            .unwrap();
        let moves = conflict
            .files
            .iter()
            .filter(|file| file.name != keep)
            .map(|file| TransactionMove {
                operation: "archive".to_string(),
                original_path: file.path.clone(),
                destination_path: relative_path_string(transaction.relative.join(&file.name))
                    .unwrap(),
                size: file.size,
                sha256: file.sha256.clone(),
                status: "pending".to_string(),
            })
            .collect();
        let manifest = new_manifest(&canonical, &conflict, kept, moves);
        let prepared = write_manifest_durable(
            &canonical,
            &mutation,
            &transaction,
            "prepared.json",
            &manifest,
        )
        .unwrap();
        (conflict, mutation, transaction, manifest, prepared)
    }

    #[test]
    fn inspect_reports_differing_and_identical_marker_sources() {
        let project = project();
        let differing = package(&project, "Differing");
        fs::write(differing.join("init (Differing).luau"), "return 'named'").unwrap();
        fs::write(differing.join("init.luau"), "return 'plain'").unwrap();
        let identical = package(&project, "Identical");
        fs::write(identical.join("init (Identical).luau"), "return 1").unwrap();
        fs::write(identical.join("init.luau"), "return 1").unwrap();

        let scan = inspect(project.path()).unwrap();
        let repeated = inspect(project.path()).unwrap();
        assert_eq!(
            scan.conflicts
                .iter()
                .map(|conflict| &conflict.id)
                .collect::<Vec<_>>(),
            repeated
                .conflicts
                .iter()
                .map(|conflict| &conflict.id)
                .collect::<Vec<_>>()
        );
        assert_eq!(scan.remaining, 2);
        assert!(scan.counts_known);
        assert!(!scan.truncated);
        let encoded = serde_json::to_value(&scan).unwrap();
        assert_eq!(encoded["countsKnown"], true);
        let differing = scan
            .conflicts
            .iter()
            .find(|conflict| conflict.directory.ends_with("/Differing"))
            .unwrap();
        assert_eq!(differing.kind, MULTIPLE_INIT_MARKERS);
        assert!(!differing.identical);
        assert_eq!(differing.files.len(), 2);
        assert_eq!(differing.files[0].style, "named");
        assert_eq!(differing.files[1].style, "plain");
        assert!(differing
            .files
            .iter()
            .all(|file| file.preview.starts_with("return")));

        let identical = scan
            .conflicts
            .iter()
            .find(|conflict| conflict.directory.ends_with("/Identical"))
            .unwrap();
        assert!(identical.identical);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn resolve_marker_conflict_keeps_choice_and_preserves_loser_bytes() {
        let project = project();
        let package = package(&project, "Pkg");
        let named = b"return 'named'\n";
        let plain = b"return 'plain'\n";
        fs::write(package.join("init (Pkg).luau"), named).unwrap();
        fs::write(package.join("init.luau"), plain).unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);

        let result = resolve(project.path(), &conflict.id, Some("init (Pkg).luau")).unwrap();
        assert_eq!(result.remaining, 0);
        assert_eq!(result.resolution.kept_file, "init (Pkg).luau");
        assert_eq!(fs::read(package.join("init (Pkg).luau")).unwrap(), named);
        assert!(!package.join("init.luau").exists());
        assert_eq!(result.resolution.backup_paths.len(), 1);
        assert_eq!(
            fs::read(
                project
                    .path()
                    .join(path_from_slashes(&result.resolution.backup_paths[0]))
            )
            .unwrap(),
            plain
        );
        let receipt = project
            .path()
            .join(path_from_slashes(&result.resolution.receipt_path));
        assert_eq!(receipt.file_name().unwrap(), "committed.json");
        assert!(receipt.with_file_name("prepared.json").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(project.path().join(BACKUP_ROOT))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(receipt.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        let prepared: serde_json::Value =
            serde_json::from_slice(&fs::read(receipt.with_file_name("prepared.json")).unwrap())
                .unwrap();
        assert_eq!(prepared["state"], "prepared");
        assert_eq!(prepared["recoveryRequired"], true);
        assert!(prepared["recoveryProtocol"]
            .as_str()
            .unwrap()
            .contains("source-only means pending"));
        assert_eq!(manifest["state"], "committed");
        assert_eq!(manifest["recoveryRequired"], false);
        assert_eq!(
            manifest["kept"]["sha256"],
            format!("{:x}", Sha256::digest(named))
        );
        assert_eq!(manifest["moves"][0]["status"], "moved");
        assert_eq!(
            manifest["moves"][0]["sha256"],
            format!("{:x}", Sha256::digest(plain))
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn resolve_legacy_leaf_renames_to_canonical_escaped_spelling() {
        let project = project();
        let misc = package(&project, "Misc");
        let source = misc.join("init (Notifications).luau");
        fs::write(&source, "return 'notification'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        assert_eq!(conflict.kind, LEGACY_RESERVED_INIT_LEAF);
        assert_eq!(
            conflict.canonical_path.as_deref(),
            Some("ReplicatedStorage/Misc/%69nit (Notifications).luau")
        );

        let result = resolve(project.path(), &conflict.id, None).unwrap();
        assert_eq!(result.remaining, 0);
        assert!(result.counts_known);
        assert!(result.resolution.receipt_available);
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(misc.join("%69nit (Notifications).luau")).unwrap(),
            "return 'notification'"
        );
        let committed: serde_json::Value = serde_json::from_slice(
            &fs::read(
                project
                    .path()
                    .join(path_from_slashes(&result.resolution.receipt_path)),
            )
            .unwrap(),
        )
        .unwrap();
        let prepared: serde_json::Value = serde_json::from_slice(
            &fs::read(
                project
                    .path()
                    .join(path_from_slashes(&result.resolution.receipt_path))
                    .with_file_name("prepared.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            prepared["kept"]["path"],
            "ReplicatedStorage/Misc/init (Notifications).luau"
        );
        assert_eq!(
            committed["kept"]["path"],
            "ReplicatedStorage/Misc/%69nit (Notifications).luau"
        );
        assert_eq!(committed["kept"]["name"], "%69nit (Notifications).luau");
    }

    #[test]
    fn changed_source_rejects_stale_conflict_id() {
        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 1").unwrap();
        fs::write(package.join("init.luau"), "return 2").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        fs::write(package.join("init.luau"), "return 3").unwrap();

        let error = resolve(project.path(), &conflict.id, Some("init.luau")).unwrap_err();
        assert!(error.contains("stale or unknown"), "{error}");
        assert!(package.join("init (Pkg).luau").exists());
        assert!(package.join("init.luau").exists());
    }

    #[test]
    fn changed_directory_generation_rejects_stale_conflict_id() {
        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 1").unwrap();
        fs::write(package.join("init.luau"), "return 2").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        fs::write(package.join("Extra.luau"), "return 3").unwrap();

        let error = resolve(project.path(), &conflict.id, Some("init.luau")).unwrap_err();
        assert!(error.contains("stale or unknown"), "{error}");
        assert!(package.join("init (Pkg).luau").exists());
        assert!(package.join("init.luau").exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn keep_rejects_paths_and_unknown_names() {
        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 1").unwrap();
        fs::write(package.join("init.luau"), "return 2").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);

        let error = resolve(project.path(), &conflict.id, Some("../init.luau")).unwrap_err();
        assert!(error.contains("exact filename"), "{error}");
        let error = resolve(project.path(), &conflict.id, Some("other.luau")).unwrap_err();
        assert!(error.contains("not one of"), "{error}");
    }

    #[test]
    fn previews_are_utf8_and_bounded() {
        let project = project();
        let package = package(&project, "Pkg");
        let mut invalid = vec![b'a'; MAX_PREVIEW_BYTES_PER_FILE * 2];
        invalid.push(0xff);
        fs::write(package.join("init (Pkg).luau"), &invalid).unwrap();
        fs::write(package.join("init.luau"), "return 2").unwrap();

        let scan = inspect(project.path()).unwrap();
        let file = &scan.conflicts[0].files[0];
        assert!(file.preview.len() <= MAX_PREVIEW_BYTES_PER_FILE);
        assert!(file.preview_truncated);
        assert!(!file.utf8);
    }

    #[test]
    fn structured_report_is_bounded_and_marks_hidden_conflicts() {
        let project = project();
        for index in 0..(MAX_VISIBLE_CONFLICTS + 12) {
            let package = package(&project, &format!("Pkg{index:03}"));
            fs::write(
                package.join(format!("init (Pkg{index:03}).luau")),
                "a".repeat(MAX_PREVIEW_BYTES_PER_FILE * 2),
            )
            .unwrap();
            fs::write(package.join("init.luau"), format!("return {index}")).unwrap();
        }

        let scan = inspect(project.path()).unwrap();
        assert_eq!(scan.total_conflicts, MAX_VISIBLE_CONFLICTS + 12);
        assert!(scan.truncated);
        assert!(scan.conflicts.len() <= MAX_VISIBLE_CONFLICTS);
        let encoded = serde_json::to_vec_pretty(&scan).unwrap();
        assert!(encoded.len() < 2 * 1024 * 1024, "{}", encoded.len());
    }

    #[test]
    fn projection_scan_rejects_portable_case_collisions() {
        let project = project();
        let service = project.path().join("ReplicatedStorage");
        fs::write(service.join("Foo.luau"), "return 1").unwrap();
        fs::write(service.join("foo.luau"), "return 2").unwrap();

        if fs::read_dir(&service).unwrap().count() == 2 {
            let error = inspect(project.path()).unwrap_err();
            assert!(error.contains("portable filename collision"), "{error}");
        } else {
            assert_eq!(
                fs_safety::ascii_fold("Foo.luau"),
                fs_safety::ascii_fold("foo.luau")
            );
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn legacy_migration_uses_authoritative_allocator_when_base_target_exists() {
        let project = project();
        let misc = package(&project, "Misc");
        fs::write(misc.join("init (Notifications).luau"), "return 'legacy'").unwrap();
        fs::write(
            misc.join("%69nit (Notifications).luau"),
            "return 'existing'",
        )
        .unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);

        let result = resolve(project.path(), &conflict.id, None).unwrap();
        assert!(result.ok);
        assert!(!misc.join("init (Notifications).luau").exists());
        assert_eq!(
            fs::read_to_string(misc.join("%69nit (Notifications).luau")).unwrap(),
            "return 'existing'"
        );
        assert_eq!(
            fs::read_to_string(misc.join("%69nit (Notifications) [1].luau")).unwrap(),
            "return 'legacy'"
        );
    }

    #[test]
    fn legacy_migration_uses_authoritative_class_suffix() {
        let project = project();
        let misc = package(&project, "Misc");
        fs::write(misc.join("Init (ClientLeaf).client.lua"), "return 'client'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        assert_eq!(
            conflict.canonical_path.as_deref(),
            Some("ReplicatedStorage/Misc/%49nit (ClientLeaf).client.luau")
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn no_replace_move_never_overwrites_an_existing_destination() {
        let temporary = TempDir::new().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::write(&source, "source bytes").unwrap();
        fs::write(&destination, "destination bytes").unwrap();

        let mutation = SecureProjectMutation::open(temporary.path()).unwrap();
        assert!(rename_no_replace_at(
            &mutation.root.handle,
            std::ffi::OsStr::new("source"),
            &mutation.root.handle,
            std::ffi::OsStr::new("destination"),
        )
        .is_err());
        assert_eq!(fs::read_to_string(&source).unwrap(), "source bytes");
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "destination bytes"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn committed_recovery_receipt_survives_a_post_prepare_failure() {
        let project = project();
        let canonical_project = fs_safety::stable_canonical_directory(project.path()).unwrap();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 1").unwrap();
        fs::write(package.join("init.luau"), "return 2").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let kept = &conflict.files[0];
        let mutation = SecureProjectMutation::open(&canonical_project).unwrap();
        let transaction = mutation.create_transaction_directory(&conflict.id).unwrap();
        let mut manifest = new_manifest(
            &canonical_project,
            &conflict,
            kept,
            vec![TransactionMove {
                operation: "archive".to_string(),
                original_path: conflict.files[1].path.clone(),
                destination_path: ".rosync-backups/example/init.luau".to_string(),
                size: conflict.files[1].size,
                sha256: conflict.files[1].sha256.clone(),
                status: "pending".to_string(),
            }],
        );
        let prepared = write_manifest_durable(
            &canonical_project,
            &mutation,
            &transaction,
            "prepared.json",
            &manifest,
        )
        .unwrap();
        let outcome = commit_transaction(
            &canonical_project,
            &mutation,
            &transaction,
            &prepared,
            &mut manifest,
            Some("injected proof failure".to_string()),
            || Ok(()),
        );

        assert!(outcome.recovery_required);
        assert_eq!(
            outcome.recovery_error.as_deref(),
            Some("injected proof failure")
        );
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(canonical_project.join(path_from_slashes(&outcome.receipt_path))).unwrap(),
        )
        .unwrap();
        assert_eq!(value["state"], "committed");
        assert_eq!(value["recoveryRequired"], true);
        assert_eq!(value["error"], "injected proof failure");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn resolver_returns_recovery_receipt_when_file_changes_after_prepare() {
        use std::sync::{Arc, Barrier};
        let project = project();
        let package = package(&project, "Pkg");
        let kept_path = package.join("init (Pkg).luau");
        fs::write(&kept_path, "return 1").unwrap();
        fs::write(package.join("init.luau"), "return 2").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        AFTER_PREPARED_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(conflict.id.clone(), Arc::clone(&barrier));
        let project_path = project.path().to_path_buf();
        let id = conflict.id.clone();
        let handle =
            std::thread::spawn(move || resolve(&project_path, &id, Some("init (Pkg).luau")));
        barrier.wait();
        fs::write(&kept_path, "return 'changed after prepare'").unwrap();
        barrier.wait();
        let result = handle.join().unwrap().unwrap();
        AFTER_PREPARED_BARRIER
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&conflict.id);

        assert!(!result.ok);
        assert_eq!(result.code.as_deref(), Some("PROJECTION_RECOVERY_REQUIRED"));
        assert!(result.resolution.recovery_required);
        assert!(result.resolution.backup_paths.is_empty());
        assert!(package.join("init (Pkg).luau").is_file());
        assert!(package.join("init.luau").is_file());
        let receipt = project
            .path()
            .join(path_from_slashes(&result.resolution.receipt_path));
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(receipt).unwrap()).unwrap();
        assert_eq!(manifest["state"], "committed");
        assert_eq!(manifest["recoveryRequired"], true);
        assert!(manifest["error"]
            .as_str()
            .unwrap()
            .contains("changed during resolution"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ancestor_swap_cannot_redirect_secure_rename_outside_project() {
        use std::os::unix::fs::symlink;
        use std::sync::{Arc, Barrier};

        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 'inside named'").unwrap();
        fs::write(package.join("init.luau"), "return 'inside plain'").unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(
            outside.path().join("init (Pkg).luau"),
            "return 'outside named'",
        )
        .unwrap();
        fs::write(outside.path().join("init.luau"), "return 'outside plain'").unwrap();

        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        AFTER_PREPARED_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(conflict.id.clone(), Arc::clone(&barrier));
        let project_path = project.path().to_path_buf();
        let id = conflict.id.clone();
        let handle =
            std::thread::spawn(move || resolve(&project_path, &id, Some("init (Pkg).luau")));

        barrier.wait();
        let moved_package = project
            .path()
            .join("ReplicatedStorage")
            .join("Pkg-original");
        fs::rename(&package, &moved_package).unwrap();
        symlink(outside.path(), &package).unwrap();
        barrier.wait();

        let result = handle.join().unwrap().unwrap();
        AFTER_PREPARED_BARRIER
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&conflict.id);
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert_eq!(
            fs::read_to_string(outside.path().join("init (Pkg).luau")).unwrap(),
            "return 'outside named'"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("init.luau")).unwrap(),
            "return 'outside plain'"
        );
        assert!(moved_package.join("init (Pkg).luau").is_file());
        assert!(moved_package.join("init.luau").is_file());
        assert!(result.resolution.backup_paths.is_empty());
        assert!(project
            .path()
            .join(path_from_slashes(&result.resolution.receipt_path))
            .is_file());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn relocated_transaction_directory_cannot_produce_a_false_clean_receipt() {
        use std::sync::{Arc, Barrier};

        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 'named'").unwrap();
        fs::write(package.join("init.luau"), "return 'plain'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        AFTER_PREPARED_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(conflict.id.clone(), Arc::clone(&barrier));
        let project_path = project.path().to_path_buf();
        let id = conflict.id.clone();
        let handle =
            std::thread::spawn(move || resolve(&project_path, &id, Some("init (Pkg).luau")));

        barrier.wait();
        let backup_root = project.path().join(BACKUP_ROOT);
        let prepared_transaction = fs::read_dir(&backup_root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.is_dir() && path.join("prepared.json").is_file())
            .unwrap();
        let outside = TempDir::new().unwrap();
        let relocated = outside.path().join("relocated-transaction");
        fs::rename(&prepared_transaction, &relocated).unwrap();
        barrier.wait();

        let result = handle.join().unwrap().unwrap();
        AFTER_PREPARED_BARRIER
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&conflict.id);
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert!(result.resolution.backup_paths.is_empty());
        assert!(package.join("init (Pkg).luau").is_file());
        assert!(package.join("init.luau").is_file());
        let rescue_receipt = project
            .path()
            .join(path_from_slashes(&result.resolution.receipt_path));
        assert!(rescue_receipt.is_file(), "{result:#?}");
        assert_ne!(rescue_receipt.parent().unwrap(), prepared_transaction);
        assert!(relocated.join("prepared.json").is_file());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn project_root_replacement_after_prepare_blocks_source_moves_and_unproven_paths() {
        use std::sync::{Arc, Barrier};

        let container = TempDir::new().unwrap();
        let project_path = container.path().join("project");
        fs::create_dir(&project_path).unwrap();
        let service = project_path.join("ReplicatedStorage");
        fs::create_dir(&service).unwrap();
        let package = service.join("Pkg");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("init (Pkg).luau"), "return 'named'").unwrap();
        fs::write(package.join("init.luau"), "return 'plain'").unwrap();
        let conflict = inspect(&project_path).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        AFTER_PREPARED_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(conflict.id.clone(), Arc::clone(&barrier));
        let id = conflict.id.clone();
        let resolver_project = project_path.clone();
        let handle =
            std::thread::spawn(move || resolve(&resolver_project, &id, Some("init (Pkg).luau")));

        barrier.wait();
        let relocated = container.path().join("relocated-project");
        fs::rename(&project_path, &relocated).unwrap();
        fs::create_dir(&project_path).unwrap();
        fs::create_dir(project_path.join("ReplicatedStorage")).unwrap();
        barrier.wait();

        let result = handle.join().unwrap().unwrap();
        AFTER_PREPARED_BARRIER
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&conflict.id);
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert!(!result.resolution.receipt_available, "{result:#?}");
        assert!(result.resolution.receipt_path.is_empty(), "{result:#?}");
        assert!(result.resolution.backup_paths.is_empty(), "{result:#?}");
        assert!(!result.counts_known, "{result:#?}");
        assert!(result.truncated, "{result:#?}");
        assert!(relocated
            .join("ReplicatedStorage/Pkg/init (Pkg).luau")
            .is_file());
        assert!(relocated.join("ReplicatedStorage/Pkg/init.luau").is_file());
        assert!(fs::read_dir(project_path.join("ReplicatedStorage"))
            .unwrap()
            .next()
            .is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn project_root_replacement_before_postscan_never_returns_false_clean() {
        use std::sync::{Arc, Barrier};

        let container = TempDir::new().unwrap();
        let project_path = container.path().join("project");
        fs::create_dir(&project_path).unwrap();
        let service = project_path.join("ReplicatedStorage");
        fs::create_dir(&service).unwrap();
        let package = service.join("Pkg");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("init (Pkg).luau"), "return 'named'").unwrap();
        fs::write(package.join("init.luau"), "return 'plain'").unwrap();
        let conflict = inspect(&project_path).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        BEFORE_POSTSCAN_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(conflict.id.clone(), Arc::clone(&barrier));
        let id = conflict.id.clone();
        let resolver_project = project_path.clone();
        let handle =
            std::thread::spawn(move || resolve(&resolver_project, &id, Some("init (Pkg).luau")));

        barrier.wait();
        let relocated = container.path().join("relocated-project");
        fs::rename(&project_path, &relocated).unwrap();
        fs::create_dir(&project_path).unwrap();
        fs::create_dir(project_path.join("ReplicatedStorage")).unwrap();
        barrier.wait();

        let result = handle.join().unwrap().unwrap();
        BEFORE_POSTSCAN_BARRIER
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&conflict.id);
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert!(!result.resolution.receipt_available, "{result:#?}");
        assert!(result.resolution.receipt_path.is_empty(), "{result:#?}");
        assert!(result.resolution.backup_paths.is_empty(), "{result:#?}");
        assert!(!result.counts_known, "{result:#?}");
        assert!(result.truncated, "{result:#?}");
        assert_eq!(
            [
                relocated
                    .join("ReplicatedStorage/Pkg/init (Pkg).luau")
                    .is_file(),
                relocated.join("ReplicatedStorage/Pkg/init.luau").is_file(),
            ]
            .into_iter()
            .filter(|present| *present)
            .count(),
            1
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn prepared_receipt_leaf_rebind_is_detected_before_source_mutation() {
        use std::sync::{Arc, Barrier};

        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 'named'").unwrap();
        fs::write(package.join("init.luau"), "return 'plain'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        AFTER_RECEIPT_FIRST_OPEN_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(
                format!("{}:prepared.json", conflict.id),
                Arc::clone(&barrier),
            );
        let project_path = project.path().to_path_buf();
        let id = conflict.id.clone();
        let handle =
            std::thread::spawn(move || resolve(&project_path, &id, Some("init (Pkg).luau")));

        barrier.wait();
        let transaction = transaction_with(project.path(), "prepared.json");
        let prepared = transaction.join("prepared.json");
        let bytes = fs::read(&prepared).unwrap();
        fs::rename(&prepared, transaction.join("attacker-saved-prepared.json")).unwrap();
        fs::write(&prepared, bytes).unwrap();
        barrier.wait();

        let error = handle.join().unwrap().unwrap_err();
        assert!(
            error.contains("exact leaf proof") || error.contains("identity"),
            "{error}"
        );
        assert!(package.join("init (Pkg).luau").is_file());
        assert!(package.join("init.luau").is_file());
        assert!(committed_receipts(project.path()).is_empty());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn committed_receipt_leaf_rebind_cannot_leave_a_clean_terminal_marker() {
        use std::sync::{Arc, Barrier};

        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 'named'").unwrap();
        fs::write(package.join("init.luau"), "return 'plain'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        AFTER_RECEIPT_FIRST_OPEN_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(
                format!("{}:committed.json", conflict.id),
                Arc::clone(&barrier),
            );
        let project_path = project.path().to_path_buf();
        let id = conflict.id.clone();
        let handle =
            std::thread::spawn(move || resolve(&project_path, &id, Some("init (Pkg).luau")));

        barrier.wait();
        let transaction = transaction_with(project.path(), "committed.json");
        let committed = transaction.join("committed.json");
        let bytes = fs::read(&committed).unwrap();
        fs::rename(&committed, transaction.join("attacker-saved-clean.json")).unwrap();
        fs::write(&committed, bytes).unwrap();
        barrier.wait();

        let result = handle.join().unwrap().unwrap();
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert!(result.resolution.receipt_available, "{result:#?}");
        assert_no_clean_terminal_receipt(project.path());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn committed_post_rename_failure_quarantines_the_clean_terminal_marker() {
        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 'named'").unwrap();
        fs::write(package.join("init.luau"), "return 'plain'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        FAIL_AFTER_MANIFEST_RENAME
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
            .lock()
            .unwrap()
            .insert(format!("{}:committed.json", conflict.id));

        let result = resolve(project.path(), &conflict.id, Some("init (Pkg).luau")).unwrap();
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert!(result.resolution.receipt_available, "{result:#?}");
        assert_no_clean_terminal_receipt(project.path());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn kept_leaf_rebind_after_commit_publication_requires_recovery() {
        use std::sync::{Arc, Barrier};

        let project = project();
        let package = package(&project, "Pkg");
        let kept = package.join("init (Pkg).luau");
        fs::write(&kept, "return 'named'").unwrap();
        fs::write(package.join("init.luau"), "return 'plain'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        AFTER_MANIFEST_PUBLISH_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(
                format!("{}:committed.json", conflict.id),
                Arc::clone(&barrier),
            );
        let project_path = project.path().to_path_buf();
        let id = conflict.id.clone();
        let handle =
            std::thread::spawn(move || resolve(&project_path, &id, Some("init (Pkg).luau")));

        barrier.wait();
        fs::rename(&kept, package.join("attacker-saved-kept.luau")).unwrap();
        fs::write(&kept, "return 'named'").unwrap();
        barrier.wait();

        let result = handle.join().unwrap().unwrap();
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert_no_clean_terminal_receipt(project.path());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn moved_leaf_rebind_after_commit_publication_requires_recovery() {
        use std::sync::{Arc, Barrier};

        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 'named'").unwrap();
        fs::write(package.join("init.luau"), "return 'plain'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(2));
        AFTER_MANIFEST_PUBLISH_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(
                format!("{}:committed.json", conflict.id),
                Arc::clone(&barrier),
            );
        let project_path = project.path().to_path_buf();
        let id = conflict.id.clone();
        let handle =
            std::thread::spawn(move || resolve(&project_path, &id, Some("init (Pkg).luau")));

        barrier.wait();
        let transaction = transaction_with(project.path(), "committed.json");
        let moved = transaction.join("init.luau");
        fs::rename(&moved, transaction.join("attacker-saved-moved.luau")).unwrap();
        fs::write(&moved, "return 'plain'").unwrap();
        barrier.wait();

        let result = handle.join().unwrap().unwrap();
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert_no_clean_terminal_receipt(project.path());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn prepared_before_moves_is_discovered_blocks_daemon_and_resumes_safely() {
        let project = project();
        let (_conflict, _mutation, _transaction, _manifest, _prepared) = prepare_marker_recovery(
            &project,
            "Pkg",
            &[
                ("init (Pkg).luau", "return 'kept'"),
                ("init.luau", "return 'archive'"),
            ],
            "init (Pkg).luau",
        );

        let scan = inspect(project.path()).unwrap();
        assert!(!scan.ok, "{scan:#?}");
        assert_eq!(scan.code.as_deref(), Some("PROJECTION_RECOVERY_REQUIRED"));
        let pending = scan.resolution.as_ref().unwrap();
        assert!(pending.recovery_required);
        assert!(pending.receipt_available);
        assert_eq!(
            pending.recovery_actions,
            vec!["resume".to_string(), "quarantine".to_string()]
        );
        let guard = ensure_no_pending_recovery(project.path()).unwrap_err();
        assert_eq!(guard.code(), "PROJECTION_RECOVERY_REQUIRED");

        let result = resolve(project.path(), &pending.id, Some("resume")).unwrap();
        assert!(result.ok, "{result:#?}");
        assert!(result.resolution.receipt_available);
        assert_eq!(
            Path::new(&result.resolution.receipt_path)
                .file_name()
                .unwrap(),
            "reconciled.json"
        );
        let clean = inspect(project.path()).unwrap();
        assert!(clean.ok, "{clean:#?}");
        ensure_no_pending_recovery(project.path()).unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn partial_marker_moves_are_resumed_from_exact_receipt_state() {
        let project = project();
        let (_conflict, mutation, transaction, manifest, _prepared) = prepare_marker_recovery(
            &project,
            "Pkg",
            &[
                ("init (Pkg).luau", "return 'kept'"),
                ("init (Pkg).server.luau", "return 'first'"),
                ("init.luau", "return 'second'"),
            ],
            "init (Pkg).luau",
        );
        let first = &manifest.moves[0];
        let source = path_from_slashes(&first.original_path);
        let source_directory = mutation.open_directory(source.parent().unwrap()).unwrap();
        let leaf = source.file_name().unwrap();
        rename_no_replace_at(&source_directory.handle, leaf, &transaction.handle, leaf).unwrap();
        source_directory.handle.sync_all().unwrap();
        transaction.handle.sync_all().unwrap();

        let scan = inspect(project.path()).unwrap();
        assert!(!scan.ok);
        let pending = scan.resolution.unwrap();
        let result = resolve(project.path(), &pending.id, Some("resume")).unwrap();
        assert!(result.ok, "{result:#?}");
        assert_eq!(result.resolution.backup_paths.len(), 2);
        assert!(inspect(project.path()).unwrap().ok);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn legacy_post_rename_crash_reconciles_without_allocator_drift() {
        let project = project();
        let misc = package(&project, "Misc");
        let source = misc.join("init (Notifications).luau");
        fs::write(&source, "return 'notification'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let canonical = fs_safety::stable_canonical_directory(project.path()).unwrap();
        let mutation = SecureProjectMutation::open(&canonical).unwrap();
        let transaction = mutation.create_transaction_directory(&conflict.id).unwrap();
        let file = &conflict.files[0];
        let mut manifest = new_manifest(
            &canonical,
            &conflict,
            file,
            vec![TransactionMove {
                operation: "rename".to_string(),
                original_path: conflict.source_path.clone().unwrap(),
                destination_path: conflict.canonical_path.clone().unwrap(),
                size: file.size,
                sha256: file.sha256.clone(),
                status: "pending".to_string(),
            }],
        );
        write_manifest_durable(
            &canonical,
            &mutation,
            &transaction,
            "prepared.json",
            &manifest,
        )
        .unwrap();
        let conflict_directory = mutation
            .open_directory(&path_from_slashes(&conflict.directory))
            .unwrap();
        let original = path_from_slashes(&manifest.moves[0].original_path);
        let destination = path_from_slashes(&manifest.moves[0].destination_path);
        rename_no_replace_at(
            &conflict_directory.handle,
            original.file_name().unwrap(),
            &conflict_directory.handle,
            destination.file_name().unwrap(),
        )
        .unwrap();
        conflict_directory.handle.sync_all().unwrap();
        manifest.moves[0].status = "moved".to_string();

        let pending = inspect(project.path()).unwrap().resolution.unwrap();
        let result = resolve(project.path(), &pending.id, Some("resume")).unwrap();
        assert!(result.ok, "{result:#?}");
        assert_eq!(
            result.resolution.canonical_path.as_deref(),
            conflict.canonical_path.as_deref()
        );
        assert!(inspect(project.path()).unwrap().ok);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn recovery_committed_receipt_is_discovered_and_can_be_quarantined() {
        let project = project();
        let (_conflict, mutation, transaction, mut manifest, _prepared) = prepare_marker_recovery(
            &project,
            "Pkg",
            &[
                ("init (Pkg).luau", "return 'kept'"),
                ("init.luau", "return 'archive'"),
            ],
            "init (Pkg).luau",
        );
        manifest.state = "committed".to_string();
        manifest.recovery_required = true;
        manifest.error = Some("injected lost recovery result".to_string());
        write_manifest_durable(
            project.path(),
            &mutation,
            &transaction,
            "committed.json",
            &manifest,
        )
        .unwrap();

        let scan = inspect(project.path()).unwrap();
        let pending = scan.resolution.unwrap();
        assert!(pending.receipt_path.ends_with("committed.json"));
        assert!(pending.recovery_actions.contains(&"quarantine".to_string()));
        let result = resolve(project.path(), &pending.id, Some("quarantine")).unwrap();
        assert!(!result.resolution.recovery_required, "{result:#?}");
        assert!(inspect(project.path()).unwrap().ok);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn clean_committed_pair_does_not_block_recovery_discovery() {
        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 'kept'").unwrap();
        fs::write(package.join("init.luau"), "return 'archive'").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let result = resolve(project.path(), &conflict.id, Some("init (Pkg).luau")).unwrap();
        assert!(result.ok, "{result:#?}");
        let scan = inspect(project.path()).unwrap();
        assert!(scan.ok, "{scan:#?}");
        assert!(scan.resolution.is_none());
        ensure_no_pending_recovery(project.path()).unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn malformed_recovery_exposes_only_secure_quarantine_and_unblocks_after_action() {
        let project = project();
        let backup = project.path().join(BACKUP_ROOT);
        fs::create_dir(&backup).unwrap();
        let transaction = backup.join("projection-conflict-malformed");
        fs::create_dir(&transaction).unwrap();
        fs::write(transaction.join("prepared.json"), b"{not json").unwrap();

        let scan = inspect(project.path()).unwrap();
        let pending = scan.resolution.unwrap();
        assert_eq!(pending.recovery_actions, vec!["quarantine".to_string()]);
        assert!(!pending.receipt_available);
        let result = resolve(project.path(), &pending.id, Some("quarantine")).unwrap();
        assert!(result.ok, "{result:#?}");
        assert!(inspect(project.path()).unwrap().ok);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn multiple_pending_recoveries_chain_the_next_actionable_resolution() {
        let project = project();
        let backup = project.path().join(BACKUP_ROOT);
        fs::create_dir(&backup).unwrap();
        for name in [
            "projection-conflict-malformed-a",
            "projection-conflict-malformed-b",
        ] {
            let transaction = backup.join(name);
            fs::create_dir(&transaction).unwrap();
            fs::write(transaction.join("prepared.json"), b"bad").unwrap();
        }
        let first = inspect(project.path()).unwrap().resolution.unwrap();
        let result = resolve(project.path(), &first.id, Some("quarantine")).unwrap();
        assert!(!result.ok, "{result:#?}");
        assert!(result.resolution.recovery_required, "{result:#?}");
        assert_ne!(result.resolution.id, first.id);
        assert_eq!(
            result.resolution.recovery_actions,
            vec!["quarantine".to_string()]
        );
        let second_id = result.resolution.id.clone();
        let result = resolve(project.path(), &second_id, Some("quarantine")).unwrap();
        assert!(result.ok, "{result:#?}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn mismatched_valid_recovery_receipts_never_expose_resume() {
        let project = project();
        let (_conflict, mutation, transaction, mut manifest, _prepared) = prepare_marker_recovery(
            &project,
            "Pkg",
            &[
                ("init (Pkg).luau", "return 'kept'"),
                ("init.luau", "return 'archive'"),
            ],
            "init (Pkg).luau",
        );
        manifest.state = "committed".to_string();
        manifest.recovery_required = true;
        manifest.prepared_at_ms += 1;
        manifest.error = Some("mismatched but syntactically valid".to_string());
        write_manifest_durable(
            project.path(),
            &mutation,
            &transaction,
            "committed.json",
            &manifest,
        )
        .unwrap();
        let pending = inspect(project.path()).unwrap().resolution.unwrap();
        assert_eq!(pending.recovery_actions, vec!["quarantine".to_string()]);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn forged_recovery_cannot_archive_the_kept_marker() {
        let project = project();
        let (_conflict, mutation, transaction, mut manifest, _prepared) = prepare_marker_recovery(
            &project,
            "Pkg",
            &[
                ("init (Pkg).luau", "return 'kept'"),
                ("init.luau", "return 'archive'"),
            ],
            "init (Pkg).luau",
        );
        fs::remove_file(
            project
                .path()
                .join(transaction.relative.join("prepared.json")),
        )
        .unwrap();
        manifest.moves[0].original_path = manifest.kept.path.clone();
        manifest.moves[0].destination_path =
            relative_path_string(transaction.relative.join(&manifest.kept.name)).unwrap();
        manifest.moves[0].size = manifest.kept.size;
        manifest.moves[0].sha256 = manifest.kept.sha256.clone();
        write_manifest_durable(
            project.path(),
            &mutation,
            &transaction,
            "prepared.json",
            &manifest,
        )
        .unwrap();

        let pending = inspect(project.path()).unwrap().resolution.unwrap();
        let result = resolve(project.path(), &pending.id, Some("resume")).unwrap();
        assert!(!result.ok, "{result:#?}");
        assert!(project
            .path()
            .join("ReplicatedStorage/Pkg/init (Pkg).luau")
            .is_file());
        assert!(project
            .path()
            .join("ReplicatedStorage/Pkg/init.luau")
            .is_file());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn forged_duplicate_recovery_routes_are_rejected_before_any_move() {
        let project = project();
        let (_conflict, mutation, transaction, mut manifest, _prepared) = prepare_marker_recovery(
            &project,
            "Pkg",
            &[
                ("init (Pkg).luau", "return 'kept'"),
                ("init (Pkg).server.luau", "return 'first'"),
                ("init.luau", "return 'second'"),
            ],
            "init (Pkg).luau",
        );
        fs::remove_file(
            project
                .path()
                .join(transaction.relative.join("prepared.json")),
        )
        .unwrap();
        manifest.moves[1] = manifest.moves[0].clone();
        write_manifest_durable(
            project.path(),
            &mutation,
            &transaction,
            "prepared.json",
            &manifest,
        )
        .unwrap();

        let pending = inspect(project.path()).unwrap().resolution.unwrap();
        let result = resolve(project.path(), &pending.id, Some("resume")).unwrap();
        assert!(!result.ok, "{result:#?}");
        for leaf in ["init (Pkg).luau", "init (Pkg).server.luau", "init.luau"] {
            assert!(
                project
                    .path()
                    .join("ReplicatedStorage/Pkg")
                    .join(leaf)
                    .is_file(),
                "{leaf} moved before semantic validation"
            );
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn failed_reconciled_quarantine_persists_a_recovery_rescue_receipt() {
        use std::sync::{Arc, Barrier};

        let project = project();
        let (conflict, _mutation, _transaction, _manifest, _prepared) = prepare_marker_recovery(
            &project,
            "Pkg",
            &[
                ("init (Pkg).luau", "return 'kept'"),
                ("init.luau", "return 'archive'"),
            ],
            "init (Pkg).luau",
        );
        let pending = inspect(project.path()).unwrap().resolution.unwrap();
        let barrier = Arc::new(Barrier::new(2));
        AFTER_MANIFEST_PUBLISH_BARRIER
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap()
            .insert(
                format!("{}:reconciled.json", conflict.id),
                Arc::clone(&barrier),
            );
        let quarantine_key = format!("{}:reconciled.json", conflict.id);
        FAIL_MANIFEST_QUARANTINE
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
            .lock()
            .unwrap()
            .insert(quarantine_key.clone());
        let project_path = project.path().to_path_buf();
        let recovery_id = pending.id.clone();
        let handle =
            std::thread::spawn(move || resolve(&project_path, &recovery_id, Some("resume")));

        barrier.wait();
        let kept = project.path().join("ReplicatedStorage/Pkg/init (Pkg).luau");
        fs::rename(
            &kept,
            project
                .path()
                .join("ReplicatedStorage/Pkg/attacker-saved-kept.luau"),
        )
        .unwrap();
        fs::write(&kept, "return 'tampered'").unwrap();
        barrier.wait();

        let result = handle.join().unwrap().unwrap();
        FAIL_MANIFEST_QUARANTINE
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&quarantine_key);
        assert!(!result.ok, "{result:#?}");
        assert!(committed_receipts(project.path()).iter().any(|receipt| {
            serde_json::from_slice::<serde_json::Value>(&fs::read(receipt).unwrap()).unwrap()
                ["recoveryRequired"]
                == true
        }));
        let scan = inspect(project.path()).unwrap();
        assert!(!scan.ok, "{scan:#?}");
        assert_eq!(scan.code.as_deref(), Some("PROJECTION_RECOVERY_REQUIRED"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn simultaneous_opposite_resolutions_cannot_remove_both_markers() {
        use std::sync::{Arc, Barrier};
        let project = project();
        let package = package(&project, "Pkg");
        fs::write(package.join("init (Pkg).luau"), "return 1").unwrap();
        fs::write(package.join("init.luau"), "return 2").unwrap();
        let conflict = inspect(project.path()).unwrap().conflicts.remove(0);
        let barrier = Arc::new(Barrier::new(3));
        let project_path = project.path().to_path_buf();
        let mut handles = Vec::new();
        for keep in ["init (Pkg).luau", "init.luau"] {
            let barrier = Arc::clone(&barrier);
            let project_path = project_path.clone();
            let id = conflict.id.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                resolve(&project_path, &id, Some(keep))
            }));
        }
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        let live_markers = [
            package.join("init (Pkg).luau").is_file(),
            package.join("init.luau").is_file(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        assert_eq!(live_markers, 1, "{results:#?}");
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    }

    #[test]
    fn deep_projection_scan_is_rejected_at_cap() {
        let project = project();
        let mut current = project.path().join("ReplicatedStorage");
        for _ in 0..=MAX_SERVICE_TREE_DEPTH {
            current = current.join("d");
            fs::create_dir(&current).unwrap();
        }
        let error = inspect(project.path()).unwrap_err();
        assert!(error.contains("maximum depth"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn projection_scan_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let project = project();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("init.luau"), "return 1").unwrap();
        symlink(
            outside.path().join("init.luau"),
            project.path().join("ReplicatedStorage").join("linked.luau"),
        )
        .unwrap();

        let error = inspect(project.path()).unwrap_err();
        assert!(
            error.contains("refusing linked/reparse synced filesystem entry"),
            "{error}"
        );
    }

    #[test]
    fn source_larger_than_cap_is_rejected_without_unbounded_read() {
        let project = project();
        let package = package(&project, "Pkg");
        let path = package.join("init (Pkg).luau");
        let mut file = fs::File::create(&path).unwrap();
        file.set_len(MAX_SYNCED_SCRIPT_BYTES + 1).unwrap();
        file.flush().unwrap();
        fs::write(package.join("init.luau"), "return 2").unwrap();

        let error = inspect(project.path()).unwrap_err();
        assert!(error.contains("exceeds maximum size"), "{error}");
    }
}
