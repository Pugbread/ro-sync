//! Cross-platform filesystem safety primitives for the synced projection.
//!
//! The sync boundary must never follow a link. Unix symbolic links and every
//! Windows reparse point (junctions included) are rejected before callers read,
//! hash, enumerate, or mutate the corresponding path.

use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub const WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const MAX_SERVICE_TREE_DEPTH: usize = 256;
pub const MAX_SERVICE_TREE_NODES: usize = 1_000_000;
/// Maximum source payload accepted from one filesystem watcher notification.
///
/// This is deliberately enforced from handle metadata before allocating the
/// read buffer, then enforced again while reading in case the file grows.
pub const MAX_SYNCED_SCRIPT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ROJO_PROJECT_BYTES: u64 = 4 * 1024 * 1024;

pub const SYNCED_SERVICES: &[&str] = &[
    "ReplicatedStorage",
    "ServerScriptService",
    "StarterPlayer",
    "StarterGui",
    "Workspace",
    "ReplicatedFirst",
    "ServerStorage",
    "Lighting",
];

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn is_reparse_point(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        attributes_have_reparse_point(metadata.file_attributes())
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub const fn attributes_have_reparse_point(attributes: u32) -> bool {
    attributes & WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Inspect a path without following its final component.
///
/// `Ok(None)` means precisely "not found". A link, junction, or other reparse
/// point is an error even when it is dangling.
pub fn metadata_no_follow(path: &Path) -> io::Result<Option<Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Err(invalid_data(format!(
            "refusing to follow symbolic link in synced filesystem: {}",
            path.display()
        )));
    }
    if is_reparse_point(&metadata) {
        return Err(invalid_data(format!(
            "refusing to follow Windows reparse point in synced filesystem: {}",
            path.display()
        )));
    }
    Ok(Some(metadata))
}

pub fn require_metadata_no_follow(path: &Path) -> io::Result<Metadata> {
    metadata_no_follow(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("synced filesystem path does not exist: {}", path.display()),
        )
    })
}

/// Open a regular file while refusing a link/reparse point at the final path
/// component, including when the object changes between metadata inspection
/// and `open`.
///
/// Existing parent components are validated by the path-boundary helpers
/// before callers reach this function. `O_NOFOLLOW` closes the final-component
/// race on Unix; `FILE_FLAG_OPEN_REPARSE_POINT` plus handle metadata does the
/// equivalent on Windows.
pub fn open_regular_file_no_follow(path: &Path) -> io::Result<fs::File> {
    let before = require_metadata_no_follow(path)?;
    if !before.is_file() {
        return Err(invalid_data(format!(
            "expected a regular file in synced filesystem: {}",
            path.display()
        )));
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself so handle metadata can reject it,
        // rather than allowing CreateFileW to traverse to its target.
        options.custom_flags(0x0020_0000);
    }

    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.file_type().is_symlink() || is_reparse_point(&opened) {
        return Err(invalid_data(format!(
            "filesystem object became linked/reparse or non-file while opening: {}",
            path.display()
        )));
    }
    Ok(file)
}

pub fn read_file_no_follow(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = open_regular_file_no_follow(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Read a regular file without following links while bounding allocation.
///
/// `Ok(None)` means the file exceeded `max_bytes`, either in the metadata
/// observed before allocation or because it grew while being read. Callers
/// that require an exact snapshot should treat that as a retry/full-resync
/// condition rather than silently dropping the file.
pub fn read_file_no_follow_bounded(path: &Path, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
    let mut file = open_regular_file_no_follow(path)?;
    let before_metadata = file.metadata()?;
    let before = generation_from_metadata(path, &before_metadata)?;
    if before.len > max_bytes {
        return Ok(None);
    }

    let capacity = usize::try_from(before.len)
        .unwrap_or(usize::MAX)
        .min(usize::try_from(max_bytes).unwrap_or(usize::MAX));
    let mut bytes = Vec::with_capacity(capacity);
    {
        let mut limited = file.by_ref().take(max_bytes);
        limited.read_to_end(&mut bytes)?;
    }
    let mut overflow_probe = [0u8; 1];
    if file.read(&mut overflow_probe)? != 0 {
        return Ok(None);
    }

    let after_metadata = file.metadata()?;
    let after = generation_from_metadata(path, &after_metadata)?;
    if before != after {
        return Err(invalid_data(format!(
            "filesystem file changed while reading bounded content: {}",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

pub fn read_to_string_no_follow(path: &Path) -> io::Result<String> {
    let bytes = read_file_no_follow(path)?;
    String::from_utf8(bytes).map_err(|error| {
        invalid_data(format!(
            "synced text file is not valid UTF-8 ({}): {error}",
            path.display()
        ))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
pub struct PortableDirectoryEntry {
    pub fragment: String,
    pub path: PathBuf,
    pub kind: SafeEntryKind,
    pub metadata: Metadata,
}

#[derive(Debug, Clone)]
pub struct UnsafeLinkedEntry {
    pub fragment: String,
    pub path: PathBuf,
}

/// One deterministic scan of a physical directory.
///
/// Exact spelling remains authoritative. The folded index exists only to
/// reject aliases which would collapse on Windows or common macOS volumes.
#[derive(Debug, Clone)]
pub struct PortableDirectoryIndex {
    entries: Vec<PortableDirectoryEntry>,
    exact: HashMap<String, usize>,
    folded: HashMap<String, Vec<usize>>,
    linked_exact: HashMap<String, usize>,
    linked_folded: HashMap<String, Vec<usize>>,
    linked: Vec<UnsafeLinkedEntry>,
    init_source: Option<usize>,
    generation: FileGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DirectoryPolicy {
    ProjectRoot,
    SyncedProjection,
    RawLookup,
}

impl PortableDirectoryIndex {
    pub fn read(directory: &Path) -> io::Result<Self> {
        Self::read_with_policy(directory, DirectoryPolicy::SyncedProjection)
    }

    pub fn read_project_root(directory: &Path) -> io::Result<Self> {
        Self::read_with_policy(directory, DirectoryPolicy::ProjectRoot)
    }

    pub fn read_raw(directory: &Path) -> io::Result<Self> {
        Self::read_with_policy(directory, DirectoryPolicy::RawLookup)
    }

    fn read_with_policy(directory: &Path, policy: DirectoryPolicy) -> io::Result<Self> {
        let metadata = require_metadata_no_follow(directory)?;
        if !metadata.is_dir() {
            return Err(invalid_data(format!(
                "expected a physical directory, found another object: {}",
                directory.display()
            )));
        }
        let before_generation = generation_from_metadata(directory, &metadata)?;

        let mut entries = Vec::new();
        let mut linked = Vec::new();
        for result in fs::read_dir(directory)? {
            let entry = result?;
            let fragment = entry.file_name().into_string().map_err(|_| {
                invalid_data(format!(
                    "non-UTF-8 filesystem fragment is not portable: {}",
                    entry.path().display()
                ))
            })?;
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                if link_fragment_is_sync_relevant(&fragment, policy) {
                    return Err(invalid_data(format!(
                        "refusing linked/reparse synced filesystem entry: {}",
                        path.display()
                    )));
                }
                if policy == DirectoryPolicy::RawLookup {
                    linked.push(UnsafeLinkedEntry { fragment, path });
                }
                // An unrelated project-root link is never projected and is
                // safe to ignore. We deliberately do not follow it.
                continue;
            }
            let kind = if metadata.is_dir() {
                SafeEntryKind::Directory
            } else if metadata.is_file() {
                SafeEntryKind::File
            } else {
                if link_fragment_is_sync_relevant(&fragment, policy) {
                    return Err(invalid_data(format!(
                        "unsupported filesystem object in synced tree: {}",
                        path.display()
                    )));
                }
                // Project roots and raw path lookups may share a directory
                // with unrelated sockets/FIFOs/devices. They are outside the
                // projection and must neither block an exact service lookup
                // nor be opened.
                continue;
            };
            entries.push(PortableDirectoryEntry {
                fragment,
                path,
                kind,
                metadata,
            });
        }
        entries.sort_by(|left, right| left.fragment.cmp(&right.fragment));
        linked.sort_by(|left, right| left.fragment.cmp(&right.fragment));

        let mut exact = HashMap::with_capacity(entries.len());
        let mut folded: HashMap<String, Vec<usize>> = HashMap::with_capacity(entries.len());
        let mut linked_exact = HashMap::with_capacity(linked.len());
        let mut linked_folded: HashMap<String, Vec<usize>> = HashMap::with_capacity(linked.len());
        let mut init_source = None;
        for (index, entry) in entries.iter().enumerate() {
            exact.insert(entry.fragment.clone(), index);
            let folded_fragment = ascii_fold(&entry.fragment);
            let aliases = folded.entry(folded_fragment).or_default();
            if fragment_is_collision_relevant(entry, policy) {
                if let Some(previous) = aliases
                    .iter()
                    .copied()
                    .find(|previous| fragment_is_collision_relevant(&entries[*previous], policy))
                {
                    return Err(invalid_data(format!(
                        "portable filename collision in {}: {:?} and {:?} differ only by ASCII case",
                        directory.display(),
                        entries[previous].fragment,
                        entry.fragment
                    )));
                }
            }
            aliases.push(index);
            if policy == DirectoryPolicy::SyncedProjection
                && init_source_describes_directory(directory, &entry.fragment)
            {
                if entry.kind != SafeEntryKind::File {
                    return Err(invalid_data(format!(
                        "init source marker is not a regular file: {}",
                        entry.path.display()
                    )));
                }
                if let Some(previous) = init_source.replace(index) {
                    return Err(invalid_data(format!(
                        "multiple init source markers in {}: {:?} and {:?}; keep exactly one source marker (use plain init.* for Wally/Rojo packages, or init (<Name>).* for a Ro Sync script-with-children directory)",
                        directory.display(),
                        entries[previous].fragment,
                        entry.fragment
                    )));
                }
            }
        }
        for (index, entry) in linked.iter().enumerate() {
            linked_exact.insert(entry.fragment.clone(), index);
            linked_folded
                .entry(ascii_fold(&entry.fragment))
                .or_default()
                .push(index);
        }

        let after_metadata = require_metadata_no_follow(directory)?;
        if !after_metadata.is_dir() {
            return Err(invalid_data(format!(
                "directory changed into another object while scanning: {}",
                directory.display()
            )));
        }
        let after_generation = generation_from_metadata(directory, &after_metadata)?;
        ensure_same_directory_generation(&before_generation, &after_generation, directory)?;

        Ok(Self {
            entries,
            exact,
            folded,
            linked_exact,
            linked_folded,
            linked,
            init_source,
            generation: after_generation,
        })
    }

    pub fn entries(&self) -> &[PortableDirectoryEntry] {
        &self.entries
    }

    pub fn exact(&self, fragment: &str) -> Option<&PortableDirectoryEntry> {
        self.exact
            .get(fragment)
            .and_then(|index| self.entries.get(*index))
    }

    pub fn folded_matches(&self, fragment: &str) -> Vec<&PortableDirectoryEntry> {
        self.folded
            .get(&ascii_fold(fragment))
            .into_iter()
            .flatten()
            .filter_map(|index| self.entries.get(*index))
            .collect()
    }

    pub fn exact_link(&self, fragment: &str) -> Option<&UnsafeLinkedEntry> {
        self.linked_exact
            .get(fragment)
            .and_then(|index| self.linked.get(*index))
    }

    pub fn folded_link_matches(&self, fragment: &str) -> Vec<&UnsafeLinkedEntry> {
        self.linked_folded
            .get(&ascii_fold(fragment))
            .into_iter()
            .flatten()
            .filter_map(|index| self.linked.get(*index))
            .collect()
    }

    pub fn unique_init_source(&self) -> Option<&PortableDirectoryEntry> {
        self.init_source.and_then(|index| self.entries.get(index))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirectoryCacheKey {
    path: PathBuf,
    policy: DirectoryPolicy,
}

/// Batch-scoped exact-path validator for watcher and ingestion bursts.
///
/// A directory index is reusable only while a fresh no-follow metadata read
/// reports the same directory generation and physical identity captured after
/// the original scan. A changed, replaced, linked, or reparse directory evicts
/// the cached index before the path is resolved again. Stable wide parents are
/// therefore scanned once per batch while every event still pays an O(depth)
/// identity/generation fence before index reuse.
#[derive(Debug)]
pub struct SyncedPathValidationCache {
    requested_root: PathBuf,
    canonical_root: PathBuf,
    indices: HashMap<DirectoryCacheKey, PortableDirectoryIndex>,
    #[cfg(test)]
    completed_scans: usize,
}

impl SyncedPathValidationCache {
    pub fn new(root: &Path) -> io::Result<Self> {
        Ok(Self {
            requested_root: root.to_path_buf(),
            canonical_root: stable_canonical_directory(root)?,
            indices: HashMap::new(),
            #[cfg(test)]
            completed_scans: 0,
        })
    }

    /// Validate an exact path rooted at an allowlisted top-level service.
    ///
    /// Callers reading or mutating a leaf must call this immediately before
    /// and after that operation. The cache removes repeated directory scans;
    /// it does not remove the generation checks which fence each reuse.
    pub fn validate(&mut self, path: &Path, allow_missing_tail: bool) -> io::Result<PathBuf> {
        let relative = path
            .strip_prefix(&self.requested_root)
            .or_else(|_| path.strip_prefix(&self.canonical_root))
            .map_err(|_| {
                invalid_input(format!(
                    "path {} is outside canonical project root {}",
                    path.display(),
                    self.canonical_root.display()
                ))
            })?;
        let components = normal_components(relative)?;
        let Some(service) = components.first() else {
            return Err(invalid_input("project root is not a synced instance path"));
        };
        if !SYNCED_SERVICES.contains(&service.as_str()) {
            return Err(invalid_input(format!(
                "top-level fragment {service:?} is not an exact synced service; expected one of {}",
                SYNCED_SERVICES.join(", ")
            )));
        }

        let mut current = self.canonical_root.clone();
        let mut missing = false;
        for (depth, fragment) in components.into_iter().enumerate() {
            if missing {
                current.push(fragment);
                continue;
            }
            let policy = if depth == 0 {
                DirectoryPolicy::ProjectRoot
            } else {
                DirectoryPolicy::SyncedProjection
            };
            let index = self.index(&current, policy)?;
            if let Some(entry) = index.exact(&fragment) {
                current = entry.path.clone();
                continue;
            }
            let aliases = index.folded_matches(&fragment);
            if aliases.len() > 1 {
                return Err(invalid_data(format!(
                    "requested synced fragment {fragment:?} has multiple physical case aliases in {}",
                    current.display()
                )));
            }
            if let Some(alias) = aliases.first() {
                return Err(invalid_data(format!(
                    "requested synced fragment {fragment:?} does not exactly match physical fragment {:?} in {}",
                    alias.fragment,
                    current.display()
                )));
            }
            if !allow_missing_tail {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "synced filesystem component {fragment:?} does not exist in {}",
                        current.display()
                    ),
                ));
            }
            current.push(fragment);
            missing = true;
        }
        Ok(current)
    }

    fn index(
        &mut self,
        directory: &Path,
        policy: DirectoryPolicy,
    ) -> io::Result<&PortableDirectoryIndex> {
        let key = DirectoryCacheKey {
            path: directory.to_path_buf(),
            policy,
        };
        let current_generation = match directory_generation_no_follow(directory) {
            Ok(generation) => generation,
            Err(error) => {
                self.indices.remove(&key);
                return Err(error);
            }
        };
        let reusable = self
            .indices
            .get(&key)
            .is_some_and(|index| index.generation == current_generation);
        if !reusable {
            self.indices.remove(&key);
            let index = PortableDirectoryIndex::read_with_policy(directory, policy)?;
            self.indices.insert(key.clone(), index);
            #[cfg(test)]
            {
                self.completed_scans += 1;
            }
        }
        self.indices.get(&key).ok_or_else(|| {
            invalid_data(format!(
                "directory index disappeared from batch cache: {}",
                directory.display()
            ))
        })
    }

    #[cfg(test)]
    pub fn completed_scans(&self) -> usize {
        self.completed_scans
    }
}

fn fragment_is_collision_relevant(entry: &PortableDirectoryEntry, policy: DirectoryPolicy) -> bool {
    match policy {
        DirectoryPolicy::ProjectRoot => SYNCED_SERVICES
            .iter()
            .any(|service| service.eq_ignore_ascii_case(&entry.fragment)),
        DirectoryPolicy::SyncedProjection => {
            entry.kind == SafeEntryKind::Directory
                || looks_like_script_source(&entry.fragment)
                || entry.fragment == "default.project.json"
        }
        DirectoryPolicy::RawLookup => false,
    }
}

fn link_fragment_is_sync_relevant(fragment: &str, policy: DirectoryPolicy) -> bool {
    match policy {
        DirectoryPolicy::ProjectRoot => SYNCED_SERVICES
            .iter()
            .any(|service| service.eq_ignore_ascii_case(fragment)),
        // Without following a link we cannot distinguish an unrelated file
        // from a directory (and dotted directory names are valid instances).
        // Every linked/reparse entry below a synced service is therefore unsafe.
        DirectoryPolicy::SyncedProjection => true,
        DirectoryPolicy::RawLookup => false,
    }
}

fn looks_like_script_source(fragment: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        ".server.luau",
        ".client.luau",
        ".server.lua",
        ".client.lua",
        ".luau",
        ".lua",
    ];
    SUFFIXES.iter().any(|suffix| {
        fragment
            .strip_suffix(suffix)
            .is_some_and(|stem| !stem.is_empty())
    })
}

pub fn ascii_fold(fragment: &str) -> String {
    fragment.to_ascii_lowercase()
}

fn init_source_describes_directory(directory: &Path, fragment: &str) -> bool {
    let Some(parsed) = crate::fs_map::parse_reserved_init_filename(fragment) else {
        return false;
    };
    if parsed.outer_ordinal.is_some() {
        return false;
    }
    match parsed.inner_name {
        None => crate::fs_map::parse_plain_init_file(fragment).is_some(),
        Some(inner_name) => {
            crate::fs_map::named_init_describes_parent(&directory.join(fragment), &inner_name)
        }
    }
}

fn normal_components(path: &Path) -> io::Result<Vec<String>> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| invalid_input("synced paths must be valid UTF-8")),
            _ => Err(invalid_input(format!(
                "synced relative path contains a rooted, drive, dot, or parent component: {}",
                path.display()
            ))),
        })
        .collect()
}

fn ensure_same_identity(
    before: &FileGeneration,
    after: &FileGeneration,
    context: &str,
) -> io::Result<()> {
    if before.identity != after.identity {
        return Err(invalid_data(format!(
            "filesystem object identity changed while {context}"
        )));
    }
    Ok(())
}

fn ensure_same_directory_generation(
    before: &FileGeneration,
    after: &FileGeneration,
    directory: &Path,
) -> io::Result<()> {
    if before != after {
        return Err(invalid_data(format!(
            "directory identity or contents changed while scanning; retry: {}",
            directory.display()
        )));
    }
    Ok(())
}

/// Canonicalize a physical directory while proving the caller's original path
/// still names the same object before and after canonicalization.
///
/// This preserves benign ancestor aliases such as macOS `/var` while closing
/// the direct-root swap window where `canonicalize` could otherwise traverse a
/// newly substituted Unix symlink or Windows junction.
pub fn stable_canonical_directory(path: &Path) -> io::Result<PathBuf> {
    let before_metadata = require_metadata_no_follow(path)?;
    if !before_metadata.is_dir() {
        return Err(invalid_input(format!(
            "filesystem safety root is not a directory: {}",
            path.display()
        )));
    }
    let before = generation_from_metadata(path, &before_metadata)?;
    let canonical = fs::canonicalize(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("canonicalize safety root {}: {error}", path.display()),
        )
    })?;

    let after_metadata = require_metadata_no_follow(path)?;
    if !after_metadata.is_dir() {
        return Err(invalid_data(format!(
            "filesystem safety root changed into another object: {}",
            path.display()
        )));
    }
    let after = generation_from_metadata(path, &after_metadata)?;
    ensure_same_identity(&before, &after, "canonicalizing the original directory")?;

    let canonical_metadata = require_metadata_no_follow(&canonical)?;
    if !canonical_metadata.is_dir() {
        return Err(invalid_data(format!(
            "canonical filesystem safety root is not a directory: {}",
            canonical.display()
        )));
    }
    let canonical_generation = generation_from_metadata(&canonical, &canonical_metadata)?;
    ensure_same_identity(
        &after,
        &canonical_generation,
        "matching the original and canonical directories",
    )?;
    Ok(canonical)
}

/// Validate every existing component below `base` without following links.
///
/// The returned path uses the caller's exact requested spelling. A differently
/// cased physical alias is rejected instead of silently serving as the target.
pub fn validate_descendant_no_follow(
    base: &Path,
    relative: &Path,
    allow_missing_tail: bool,
) -> io::Result<PathBuf> {
    let components = normal_components(relative)?;
    let base = stable_canonical_directory(base)?;

    let mut current = base;
    let mut missing = false;
    for fragment in components {
        if missing {
            current.push(fragment);
            continue;
        }
        let index = PortableDirectoryIndex::read_raw(&current)?;
        if let Some(link) = index.exact_link(&fragment) {
            return Err(invalid_data(format!(
                "refusing linked/reparse path component: {}",
                link.path.display()
            )));
        }
        if let Some(entry) = index.exact(&fragment) {
            current = entry.path.clone();
            continue;
        }
        let aliases = index.folded_matches(&fragment);
        let linked_aliases = index.folded_link_matches(&fragment);
        if aliases.len() + linked_aliases.len() > 1 {
            return Err(invalid_data(format!(
                "requested fragment {fragment:?} has multiple case aliases in {}",
                current.display()
            )));
        }
        if let Some(link) = linked_aliases.first() {
            return Err(invalid_data(format!(
                "requested fragment {fragment:?} case-aliases linked/reparse component {:?} in {}",
                link.fragment,
                current.display()
            )));
        }
        if let Some(alias) = aliases.first() {
            return Err(invalid_data(format!(
                "requested fragment {fragment:?} does not exactly match physical fragment {:?} in {}",
                alias.fragment,
                current.display()
            )));
        }
        if !allow_missing_tail {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "synced filesystem component {fragment:?} does not exist in {}",
                    current.display()
                ),
            ));
        }
        current.push(fragment);
        missing = true;
    }
    Ok(current)
}

/// Validate an exact path rooted at an allowlisted top-level service.
pub fn validate_synced_path(
    root: &Path,
    path: &Path,
    allow_missing_tail: bool,
) -> io::Result<PathBuf> {
    SyncedPathValidationCache::new(root)?.validate(path, allow_missing_tail)
}

pub fn validate_service_path(
    root: &Path,
    service: &str,
    allow_missing: bool,
) -> io::Result<PathBuf> {
    if !SYNCED_SERVICES.contains(&service) {
        return Err(invalid_input(format!(
            "unsupported synced service {service:?}"
        )));
    }
    validate_synced_path(root, &root.join(service), allow_missing)
}

/// Strict lexical parser for Rojo's `$path`.
pub fn parse_rojo_relative_path(raw: &str) -> io::Result<PathBuf> {
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.starts_with('\\')
        || raw.starts_with("//")
        || raw.starts_with("\\\\")
    {
        return Err(invalid_input(format!(
            "unsafe Rojo $path {raw:?}: rooted or empty"
        )));
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(invalid_input(format!(
            "unsafe Rojo $path {raw:?}: drive-relative paths are forbidden"
        )));
    }

    let mut result = PathBuf::new();
    for segment in raw.split(['/', '\\']) {
        if segment.is_empty() || segment == ".." {
            return Err(invalid_input(format!(
                "unsafe Rojo $path {raw:?}: empty and parent segments are forbidden"
            )));
        }
        if segment.contains(':') {
            return Err(invalid_input(format!(
                "unsafe Rojo $path {raw:?}: colon/alternate-data-stream syntax is forbidden"
            )));
        }
        if segment != "." {
            result.push(segment);
        }
    }
    if result.as_os_str().is_empty() {
        return Err(invalid_input(format!(
            "unsafe Rojo $path {raw:?}: path has no target"
        )));
    }
    Ok(result)
}

pub fn resolve_rojo_path_no_follow(
    package_dir: &Path,
    raw: &str,
    allow_missing_tail: bool,
) -> io::Result<PathBuf> {
    let relative = parse_rojo_relative_path(raw)?;
    let _validated = validate_descendant_no_follow(package_dir, &relative, allow_missing_tail)?;
    let target = package_dir.join(&relative);
    // If an ignored unrelated link had the exact requested spelling, surface
    // it now instead of treating it as a missing tail.
    let _ = metadata_no_follow(&target)?;
    Ok(target)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    pub device: Option<u64>,
    pub file: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGeneration {
    pub len: u64,
    pub modified_ns: Option<u128>,
    pub identity: FileIdentity,
}

/// Identity fence for every existing directory from a trusted base through
/// the target's parent. Callers verify it immediately before and after a
/// path-based read or mutation so an intermediate directory replaced by a
/// Unix symlink, Windows junction, or different physical directory cannot go
/// unnoticed.
#[derive(Debug, Clone)]
pub struct PathParentGuard {
    parents: Vec<(PathBuf, FileIdentity)>,
}

impl PathParentGuard {
    pub fn verify(&self) -> io::Result<()> {
        for (path, expected) in &self.parents {
            let metadata = require_metadata_no_follow(path)?;
            if !metadata.is_dir() {
                return Err(invalid_data(format!(
                    "guarded parent changed into another object: {}",
                    path.display()
                )));
            }
            let current = generation_from_metadata(path, &metadata)?;
            if &current.identity != expected {
                return Err(invalid_data(format!(
                    "guarded parent identity changed during filesystem operation: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

fn capture_parent_guard_from_validated(base: &Path, target: &Path) -> io::Result<PathParentGuard> {
    let canonical_base = stable_canonical_directory(base)?;
    let relative = target
        .strip_prefix(base)
        .or_else(|_| target.strip_prefix(&canonical_base))
        .map_err(|_| {
            invalid_input(format!(
                "guard target {} is outside base {}",
                target.display(),
                canonical_base.display()
            ))
        })?;
    let components = normal_components(relative)?;
    let parent_count = components.len().saturating_sub(1);
    let mut current = canonical_base;
    let current_metadata = require_metadata_no_follow(&current)?;
    let mut parents = vec![(
        current.clone(),
        generation_from_metadata(&current, &current_metadata)?.identity,
    )];

    for fragment in components.iter().take(parent_count) {
        let index = PortableDirectoryIndex::read_raw(&current)?;
        if let Some(link) = index.exact_link(fragment) {
            return Err(invalid_data(format!(
                "refusing linked/reparse guarded parent: {}",
                link.path.display()
            )));
        }
        let Some(entry) = index.exact(fragment) else {
            // A missing tail is safe to guard only through its nearest
            // existing parent. Callers creating directories recapture after
            // each component is installed.
            break;
        };
        if entry.kind != SafeEntryKind::Directory {
            return Err(invalid_data(format!(
                "guarded parent is not a directory: {}",
                entry.path.display()
            )));
        }
        current = entry.path.clone();
        parents.push((
            current.clone(),
            generation_from_metadata(&current, &entry.metadata)?.identity,
        ));
    }

    Ok(PathParentGuard { parents })
}

/// Capture parent identities for an exact descendant after validating the
/// complete existing chain without following links or reparse points.
pub fn guard_descendant_parent_chain(
    base: &Path,
    target: &Path,
    allow_missing_tail: bool,
) -> io::Result<PathParentGuard> {
    let canonical_base = stable_canonical_directory(base)?;
    let relative = target
        .strip_prefix(base)
        .or_else(|_| target.strip_prefix(&canonical_base))
        .map_err(|_| {
            invalid_input(format!(
                "guard target {} is outside base {}",
                target.display(),
                canonical_base.display()
            ))
        })?;
    let validated = validate_descendant_no_follow(&canonical_base, relative, allow_missing_tail)?;
    capture_parent_guard_from_validated(&canonical_base, &validated)
}

/// Capture identities through an existing descendant directory itself.
pub fn guard_descendant_directory_chain(
    base: &Path,
    directory: &Path,
) -> io::Result<PathParentGuard> {
    let canonical_base = stable_canonical_directory(base)?;
    let relative = directory
        .strip_prefix(base)
        .or_else(|_| directory.strip_prefix(&canonical_base))
        .map_err(|_| {
            invalid_input(format!(
                "guarded directory {} is outside base {}",
                directory.display(),
                canonical_base.display()
            ))
        })?;
    let validated = validate_descendant_no_follow(&canonical_base, relative, false)?;
    let metadata = require_metadata_no_follow(&validated)?;
    if !metadata.is_dir() {
        return Err(invalid_data(format!(
            "guarded descendant is not a directory: {}",
            validated.display()
        )));
    }
    let mut guard = capture_parent_guard_from_validated(&canonical_base, &validated)?;
    guard.parents.push((
        validated.clone(),
        generation_from_metadata(&validated, &metadata)?.identity,
    ));
    Ok(guard)
}

/// Synced-service counterpart to [`guard_descendant_parent_chain`].
pub fn guard_synced_parent_chain(
    root: &Path,
    target: &Path,
    allow_missing_tail: bool,
) -> io::Result<PathParentGuard> {
    let validated = validate_synced_path(root, target, allow_missing_tail)?;
    let canonical_root = stable_canonical_directory(root)?;
    capture_parent_guard_from_validated(&canonical_root, &validated)
}

/// Capture identities through an existing synced directory itself. This is
/// used to fence rename/create operations whose final target may legitimately
/// be missing or differ only by case from an existing source.
pub fn guard_synced_directory_chain(root: &Path, directory: &Path) -> io::Result<PathParentGuard> {
    let validated = validate_synced_path(root, directory, false)?;
    let metadata = require_metadata_no_follow(&validated)?;
    if !metadata.is_dir() {
        return Err(invalid_data(format!(
            "guarded synced directory is not a directory: {}",
            validated.display()
        )));
    }
    let canonical_root = stable_canonical_directory(root)?;
    let mut guard = capture_parent_guard_from_validated(&canonical_root, &validated)?;
    guard.parents.push((
        validated.clone(),
        generation_from_metadata(&validated, &metadata)?.identity,
    ));
    Ok(guard)
}

/// Create a descendant directory chain one physical component at a time.
///
/// Existing components are validated without following links/reparse points.
/// Every create is fenced by the identities of its existing parents, and a
/// concurrently-created component is accepted only when it is the exact
/// requested physical directory.
pub fn ensure_descendant_directory_chain(base: &Path, directory: &Path) -> io::Result<PathBuf> {
    let canonical_base = stable_canonical_directory(base)?;
    let relative = directory
        .strip_prefix(base)
        .or_else(|_| directory.strip_prefix(&canonical_base))
        .map_err(|_| {
            invalid_input(format!(
                "directory {} is outside base {}",
                directory.display(),
                canonical_base.display()
            ))
        })?;
    let components = normal_components(relative)?;
    if components.len() > MAX_SERVICE_TREE_DEPTH {
        return Err(invalid_input(format!(
            "directory chain exceeds the maximum depth of {MAX_SERVICE_TREE_DEPTH}: {}",
            directory.display()
        )));
    }

    let mut current = canonical_base.clone();
    for fragment in components {
        let current_guard = guard_descendant_directory_chain(&canonical_base, &current)?;
        current_guard.verify()?;
        let index = PortableDirectoryIndex::read_raw(&current)?;
        current_guard.verify()?;
        if let Some(link) = index.exact_link(&fragment) {
            return Err(invalid_data(format!(
                "refusing linked/reparse directory component: {}",
                link.path.display()
            )));
        }
        if let Some(entry) = index.exact(&fragment) {
            if entry.kind != SafeEntryKind::Directory {
                return Err(invalid_data(format!(
                    "directory component is another object: {}",
                    entry.path.display()
                )));
            }
            current = entry.path.clone();
            continue;
        }
        let aliases = index.folded_matches(&fragment);
        let linked_aliases = index.folded_link_matches(&fragment);
        if !aliases.is_empty() || !linked_aliases.is_empty() {
            return Err(invalid_data(format!(
                "directory fragment {fragment:?} does not exactly match a physical entry in {}",
                current.display()
            )));
        }

        let next = current.join(&fragment);
        let guard = guard_descendant_parent_chain(&canonical_base, &next, true)?;
        guard.verify()?;
        match fs::create_dir(&next) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        guard.verify()?;
        let metadata = require_metadata_no_follow(&next)?;
        if !metadata.is_dir() {
            return Err(invalid_data(format!(
                "created directory component is another object: {}",
                next.display()
            )));
        }
        current = next;
    }
    Ok(current)
}

#[cfg(windows)]
fn windows_file_identity_no_follow(path: &Path) -> io::Result<FileIdentity> {
    use std::ffi::c_void;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    type Handle = *mut c_void;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            file: Handle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    // Let std perform Windows' verbatim-path conversion so deep project paths
    // are not constrained by legacy MAX_PATH. The custom flags inspect a
    // directory or reparse point itself rather than traversing it.
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let handle = file.as_raw_handle() as Handle;
    let mut information = std::mem::MaybeUninit::<ByHandleFileInformation>::uninit();
    // SAFETY: the handle is live and the output points at enough writable memory.
    if unsafe { GetFileInformationByHandle(handle, information.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetFileInformationByHandle returned success and initialized it.
    let information = unsafe { information.assume_init() };
    if attributes_have_reparse_point(information.file_attributes) {
        return Err(invalid_data(format!(
            "filesystem object became a reparse point while inspecting identity: {}",
            path.display()
        )));
    }
    let file_index =
        (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
    Ok(FileIdentity {
        device: Some(u64::from(information.volume_serial_number)),
        file: Some(file_index),
    })
}

fn generation_from_metadata(path: &Path, metadata: &Metadata) -> io::Result<FileGeneration> {
    #[cfg(not(windows))]
    let _ = path;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            device: Some(metadata.dev()),
            file: Some(metadata.ino()),
        }
    };
    #[cfg(windows)]
    let identity = windows_file_identity_no_follow(path)?;
    #[cfg(not(any(unix, windows)))]
    let identity = FileIdentity {
        device: None,
        file: None,
    };
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(FileGeneration {
        len: metadata.len(),
        modified_ns,
        identity,
    })
}

pub fn file_generation_no_follow(path: &Path) -> Result<FileGeneration, String> {
    let metadata = require_metadata_no_follow(path)
        .map_err(|error| format!("inspect regular file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("expected regular file: {}", path.display()));
    }
    generation_from_metadata(path, &metadata)
        .map_err(|error| format!("read file identity {}: {error}", path.display()))
}

/// Capture the identity and metadata generation of an existing physical
/// directory without traversing a final Unix link or Windows reparse point.
pub fn directory_generation_no_follow(path: &Path) -> io::Result<FileGeneration> {
    let metadata = require_metadata_no_follow(path)?;
    if !metadata.is_dir() {
        return Err(invalid_data(format!(
            "expected a physical directory: {}",
            path.display()
        )));
    }
    generation_from_metadata(path, &metadata)
}

/// Compare two existing physical filesystem objects without traversing a
/// final symlink or Windows reparse point.
pub fn same_physical_object_no_follow(left: &Path, right: &Path) -> io::Result<bool> {
    let Some(left_metadata) = metadata_no_follow(left)? else {
        return Ok(false);
    };
    let Some(right_metadata) = metadata_no_follow(right)? else {
        return Ok(false);
    };
    let left_generation = generation_from_metadata(left, &left_metadata)?;
    let right_generation = generation_from_metadata(right, &right_metadata)?;
    Ok(left_generation.identity == right_generation.identity)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeTreeEntry {
    pub path: PathBuf,
    pub relative: PathBuf,
    pub kind: SafeEntryKind,
    pub generation: FileGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeGeneration {
    pub service: String,
    pub present: bool,
    pub root_generation: Option<FileGeneration>,
    pub entries: Vec<SafeTreeEntry>,
}

impl TreeGeneration {
    pub fn entries(&self) -> &[SafeTreeEntry] {
        &self.entries
    }
}

/// Capture a deterministic, iterative generation of one whole service.
///
/// This is intentionally metadata-only. Callers that require a content fence
/// can hash the regular files from `entries()` incrementally and compare this
/// generation again immediately before commit.
pub fn capture_tree_metadata(root: &Path, service: &str) -> Result<TreeGeneration, String> {
    let service_path = validate_service_path(root, service, true)
        .map_err(|error| format!("validate service {service}: {error}"))?;
    let Some(service_metadata) = metadata_no_follow(&service_path)
        .map_err(|error| format!("inspect service {}: {error}", service_path.display()))?
    else {
        return Ok(TreeGeneration {
            service: service.to_string(),
            present: false,
            root_generation: None,
            entries: Vec::new(),
        });
    };
    if !service_metadata.is_dir() {
        return Err(format!(
            "synced service root is not a directory: {}",
            service_path.display()
        ));
    }

    let mut entries = Vec::new();
    let mut stack = vec![(service_path.clone(), PathBuf::new(), 0usize)];
    while let Some((directory, relative, depth)) = stack.pop() {
        if depth > MAX_SERVICE_TREE_DEPTH {
            return Err(format!(
                "service {service} exceeds maximum depth {MAX_SERVICE_TREE_DEPTH} at {}",
                directory.display()
            ));
        }
        let index = PortableDirectoryIndex::read(&directory)
            .map_err(|error| format!("scan {}: {error}", directory.display()))?;

        if let Some(project) = index.exact("default.project.json") {
            validate_rojo_project_file(&directory, project)
                .map_err(|error| format!("validate {}: {error}", project.path.display()))?;
        }

        for entry in index.entries() {
            if entries.len() >= MAX_SERVICE_TREE_NODES {
                return Err(format!(
                    "service {service} exceeds maximum node count {MAX_SERVICE_TREE_NODES}"
                ));
            }
            let entry_relative = relative.join(&entry.fragment);
            entries.push(SafeTreeEntry {
                path: entry.path.clone(),
                relative: entry_relative.clone(),
                kind: entry.kind,
                generation: generation_from_metadata(&entry.path, &entry.metadata).map_err(
                    |error| format!("read file identity {}: {error}", entry.path.display()),
                )?,
            });
        }
        for entry in index.entries().iter().rev() {
            if entry.kind == SafeEntryKind::Directory {
                stack.push((
                    entry.path.clone(),
                    relative.join(&entry.fragment),
                    depth + 1,
                ));
            }
        }
    }
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(TreeGeneration {
        service: service.to_string(),
        present: true,
        root_generation: Some(
            generation_from_metadata(&service_path, &service_metadata)
                .map_err(|error| format!("read service identity {service}: {error}"))?,
        ),
        entries,
    })
}

#[allow(dead_code)] // protocol stream commit fences call this when enabled.
pub fn validate_service_tree_no_follow(root: &Path, service: &str) -> Result<(), String> {
    capture_tree_metadata(root, service).map(|_| ())
}

pub fn validate_rojo_project_directory(directory: &Path) -> io::Result<()> {
    let index = PortableDirectoryIndex::read(directory)?;
    validate_rojo_project_in_index(directory, &index)
}

/// Validate the Rojo marker from an index the caller already scanned.
///
/// Wide-tree counting and projection passes use this to avoid a second
/// `read_dir` for every directory while retaining the same strict `$path`
/// checks.
pub fn validate_rojo_project_in_index(
    directory: &Path,
    index: &PortableDirectoryIndex,
) -> io::Result<()> {
    if let Some(project) = index.exact("default.project.json") {
        validate_rojo_project_file(directory, project)?;
    }
    Ok(())
}

fn validate_rojo_project_file(
    package_dir: &Path,
    project: &PortableDirectoryEntry,
) -> io::Result<()> {
    if project.metadata.len() > MAX_ROJO_PROJECT_BYTES {
        return Err(invalid_data(format!(
            "Rojo project file is too large ({} bytes; max {MAX_ROJO_PROJECT_BYTES}): {}",
            project.metadata.len(),
            project.path.display()
        )));
    }
    let text = read_to_string_no_follow(&project.path)?;
    let value: Value =
        serde_json::from_str(&text).map_err(|error| invalid_data(error.to_string()))?;
    let Some(raw) = value
        .get("tree")
        .and_then(|tree| tree.get("$path"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let _ = resolve_rojo_path_no_follow(package_dir, raw, true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_index_rejects_ascii_case_aliases() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("Foo.luau"), "a").unwrap();
        fs::write(temp.path().join("foo.luau"), "b").unwrap();
        let physical_entries = fs::read_dir(temp.path()).unwrap().count();
        if physical_entries == 2 {
            let error = PortableDirectoryIndex::read(temp.path()).unwrap_err();
            assert!(error.to_string().contains("ASCII case"));
        } else {
            // The host itself folded the names. The pure portable key still
            // proves both spellings share one identity.
            assert_eq!(ascii_fold("Foo.luau"), ascii_fold("foo.luau"));
        }
    }

    #[test]
    fn portable_index_has_deterministic_order() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["z.luau", "A.luau", "m.luau"] {
            fs::write(temp.path().join(name), "").unwrap();
        }
        let index = PortableDirectoryIndex::read(temp.path()).unwrap();
        let names = index
            .entries()
            .iter()
            .map(|entry| entry.fragment.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["A.luau", "m.luau", "z.luau"]);
    }

    #[test]
    fn regular_file_reads_are_bounded_to_a_physical_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Main.luau");
        fs::write(&path, b"return 42").unwrap();
        assert_eq!(read_file_no_follow(&path).unwrap(), b"return 42");
        assert_eq!(
            read_file_no_follow_bounded(&path, 9).unwrap(),
            Some(b"return 42".to_vec())
        );
        assert_eq!(read_to_string_no_follow(&path).unwrap(), "return 42");
    }

    #[test]
    fn bounded_read_rejects_oversize_metadata_before_buffer_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Oversize.luau");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_SYNCED_SCRIPT_BYTES + 1).unwrap();

        assert_eq!(
            read_file_no_follow_bounded(&path, MAX_SYNCED_SCRIPT_BYTES).unwrap(),
            None
        );
    }

    #[test]
    fn stable_canonical_directory_preserves_identity_and_rejects_mismatch_seam() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = stable_canonical_directory(temp.path()).unwrap();
        assert_eq!(canonical, fs::canonicalize(temp.path()).unwrap());

        let before = FileGeneration {
            len: 0,
            modified_ns: None,
            identity: FileIdentity {
                device: Some(7),
                file: Some(11),
            },
        };
        let mut after = before.clone();
        after.identity.file = Some(12);
        let error =
            ensure_same_identity(&before, &after, "testing deterministic identity mismatch")
                .unwrap_err();
        assert!(error.to_string().contains("identity changed"));

        let mut changed_contents = before.clone();
        changed_contents.modified_ns = Some(1);
        let error =
            ensure_same_directory_generation(&before, &changed_contents, temp.path()).unwrap_err();
        assert!(error.to_string().contains("contents changed"));
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_read_refuses_a_final_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, b"do not read").unwrap();
        let linked = temp.path().join("Main.luau");
        symlink(&sentinel, &linked).unwrap();

        let error = read_file_no_follow(&linked).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert_eq!(fs::read(&sentinel).unwrap(), b"do not read");
    }

    #[test]
    fn duplicate_init_markers_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("Pkg");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("init.lua"), "").unwrap();
        fs::write(package.join("init (Pkg).luau"), "").unwrap();
        let error = PortableDirectoryIndex::read(&package).unwrap_err();
        assert!(error.to_string().contains("multiple init"));
    }

    #[test]
    fn init_marker_safety_grammar_is_case_insensitive_and_requires_named_content() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("Pkg");
        fs::create_dir(&package).unwrap();
        assert!(init_source_describes_directory(
            &package,
            "INIT (Pkg).server.luau"
        ));
        assert!(!init_source_describes_directory(
            &package,
            "Init.client.lua"
        ));
        assert!(!init_source_describes_directory(&package, "init ().luau"));
        assert!(!init_source_describes_directory(
            &package,
            "init (Pkg) [1].luau"
        ));

        fs::write(temp.path().join("init ().luau"), "").unwrap();
        fs::write(temp.path().join("init.lua"), "").unwrap();
        let index = PortableDirectoryIndex::read(temp.path()).unwrap();
        assert_eq!(
            index
                .unique_init_source()
                .map(|entry| entry.fragment.as_str()),
            Some("init.lua")
        );

        fs::write(package.join("INIT (Pkg).luau"), "").unwrap();
        fs::write(package.join("init.server.lua"), "").unwrap();
        let error = PortableDirectoryIndex::read(&package).unwrap_err();
        assert!(error.to_string().contains("multiple init"));
    }

    #[test]
    fn mismatched_named_init_remains_a_literal_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let misc = temp.path().join("Misc");
        fs::create_dir(&misc).unwrap();
        fs::write(misc.join("init.luau"), "return \"misc\"").unwrap();
        fs::write(
            misc.join("init (Notifications).luau"),
            "return \"notifications\"",
        )
        .unwrap();

        let index = PortableDirectoryIndex::read(&misc).unwrap();
        assert_eq!(
            index
                .unique_init_source()
                .map(|entry| entry.fragment.as_str()),
            Some("init.luau")
        );
        assert!(index.exact("init (Notifications).luau").is_some());
    }

    #[test]
    fn rojo_path_rejects_cross_platform_escape_forms() {
        for path in [
            "",
            "../outside",
            "a//b",
            "/root",
            "\\\\server\\share",
            "C:relative",
            "C:\\root",
            "nested/file.lua:stream",
        ] {
            assert!(parse_rojo_relative_path(path).is_err(), "{path:?}");
        }
        assert_eq!(
            parse_rojo_relative_path("src/init.luau").unwrap(),
            PathBuf::from("src/init.luau")
        );
    }

    #[test]
    fn absent_service_has_distinct_generation() {
        let temp = tempfile::tempdir().unwrap();
        let generation = capture_tree_metadata(temp.path(), "Workspace").unwrap();
        assert!(!generation.present);
        assert!(generation.entries.is_empty());
    }

    #[test]
    fn unrelated_project_root_aliases_and_init_files_do_not_block_service_lookup() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("README"), "").unwrap();
        fs::write(temp.path().join("readme"), "").unwrap();
        fs::write(temp.path().join("init.lua"), "").unwrap();
        fs::write(temp.path().join("init (Root).luau"), "").unwrap();
        fs::create_dir(temp.path().join("Workspace")).unwrap();

        let path = validate_service_path(temp.path(), "Workspace", false).unwrap();
        assert_eq!(
            path,
            fs::canonicalize(temp.path()).unwrap().join("Workspace")
        );
    }

    #[test]
    fn lowercase_service_alias_cannot_masquerade_as_workspace() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let error = validate_service_path(temp.path(), "Workspace", false).unwrap_err();
        assert!(error.to_string().contains("does not exactly match"));
    }

    #[cfg(unix)]
    #[test]
    fn linked_and_dangling_entries_are_rejected_without_following() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), "keep").unwrap();
        symlink(outside.path(), temp.path().join("linked")).unwrap();
        let error = PortableDirectoryIndex::read(temp.path()).unwrap_err();
        assert!(error.to_string().contains("linked/reparse"));
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );

        fs::remove_file(temp.path().join("linked")).unwrap();
        symlink(temp.path().join("missing"), temp.path().join("init.lua")).unwrap();
        let error = PortableDirectoryIndex::read(temp.path()).unwrap_err();
        assert!(error.to_string().contains("linked/reparse"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_chain_creation_refuses_a_linked_parent() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), "keep").unwrap();
        symlink(outside.path(), project.path().join("tools")).unwrap();

        let requested = project.path().join("tools/luau-lsp/roblox");
        let error = ensure_descendant_directory_chain(project.path(), &requested).unwrap_err();
        assert!(error.to_string().contains("linked/reparse"));
        assert!(!outside.path().join("luau-lsp").exists());
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dotted_directory_link_inside_service_is_rejected_and_external_tree_survives() {
        use std::os::unix::fs::symlink;
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join("Workspace")).unwrap();
        fs::write(outside.path().join("sentinel"), "keep").unwrap();
        symlink(
            outside.path(),
            project.path().join("Workspace").join("assets.v1"),
        )
        .unwrap();

        let error = capture_tree_metadata(project.path(), "Workspace").unwrap_err();
        assert!(error.contains("linked/reparse"));
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rojo_intermediate_and_wrong_case_links_never_enter_missing_tail_mode() {
        use std::os::unix::fs::symlink;
        let package_parent = tempfile::tempdir().unwrap();
        let package = package_parent.path().join("Package");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(&package).unwrap();
        fs::create_dir_all(outside.path().join("sub")).unwrap();
        fs::write(outside.path().join("sub").join("sentinel"), "keep").unwrap();
        symlink(outside.path(), package.join("link")).unwrap();

        let exact = resolve_rojo_path_no_follow(&package, "link/sub/file.luau", true).unwrap_err();
        assert!(exact.to_string().contains("linked/reparse"));
        let wrong_case =
            resolve_rojo_path_no_follow(&package, "LINK/sub/file.luau", true).unwrap_err();
        assert!(wrong_case.to_string().contains("linked/reparse"));
        assert_eq!(
            fs::read_to_string(outside.path().join("sub").join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_project_or_package_root_is_rejected_before_canonicalization() {
        use std::os::unix::fs::symlink;
        let holder = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(outside.path().join("Workspace")).unwrap();
        fs::write(outside.path().join("sentinel"), "keep").unwrap();
        let project_link = holder.path().join("project-link");
        symlink(outside.path(), &project_link).unwrap();

        let project_error = validate_service_path(&project_link, "Workspace", false).unwrap_err();
        assert!(project_error.to_string().contains("symbolic link"));
        let package_error =
            resolve_rojo_path_no_follow(&project_link, "Workspace/file.luau", true).unwrap_err();
        assert!(package_error.to_string().contains("symbolic link"));
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn reparse_constant_matches_windows_contract() {
        assert_eq!(WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT, 0x400);
        assert!(attributes_have_reparse_point(0x400));
        assert!(attributes_have_reparse_point(0x420));
        assert!(!attributes_have_reparse_point(0x20));
    }

    #[test]
    fn tree_generation_changes_when_file_metadata_changes() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join("Workspace")).unwrap();
        let source = project.path().join("Workspace").join("Main.luau");
        fs::write(&source, "a").unwrap();
        let before = capture_tree_metadata(project.path(), "Workspace").unwrap();
        fs::write(&source, "a much longer source").unwrap();
        let after = capture_tree_metadata(project.path(), "Workspace").unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn batch_validation_scans_one_stable_wide_parent_once() {
        let project = tempfile::tempdir().unwrap();
        let workspace = project.path().join("Workspace");
        fs::create_dir(&workspace).unwrap();
        let mut paths = Vec::new();
        for index in 0..1_024 {
            let path = workspace.join(format!("Item{index:04}.luau"));
            fs::write(&path, "").unwrap();
            paths.push(path);
        }

        let mut cache = SyncedPathValidationCache::new(project.path()).unwrap();
        let canonical_project = stable_canonical_directory(project.path()).unwrap();
        for path in &paths {
            let relative = path.strip_prefix(project.path()).unwrap();
            let expected = canonical_project.join(relative);
            assert_eq!(cache.validate(path, false).unwrap(), expected);
            assert_eq!(cache.validate(path, false).unwrap(), expected);
        }
        assert_eq!(
            cache.completed_scans(),
            2,
            "the project root and one stable wide service should each scan once"
        );

        fs::write(&paths[0], "return true").unwrap();
        cache.validate(&paths[0], false).unwrap();
        assert_eq!(
            cache.completed_scans(),
            2,
            "file-content writes do not invalidate an unchanged directory index"
        );
    }

    #[test]
    fn batch_validation_rebuilds_after_directory_identity_changes() {
        let project = tempfile::tempdir().unwrap();
        let workspace = project.path().join("Workspace");
        let retired = project.path().join("Workspace-retired");
        fs::create_dir(&workspace).unwrap();
        let old_source = workspace.join("Old.luau");
        fs::write(&old_source, "").unwrap();

        let mut cache = SyncedPathValidationCache::new(project.path()).unwrap();
        cache.validate(&old_source, false).unwrap();
        assert_eq!(cache.completed_scans(), 2);

        fs::rename(&workspace, &retired).unwrap();
        fs::create_dir(&workspace).unwrap();
        let new_source = workspace.join("New.luau");
        fs::write(&new_source, "").unwrap();
        cache.validate(&new_source, false).unwrap();
        assert!(
            cache.completed_scans() >= 3,
            "the replaced service directory must not reuse its retired index"
        );
    }

    #[cfg(unix)]
    #[test]
    fn batch_validation_never_reuses_an_index_through_a_new_link() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let workspace = project.path().join("Workspace");
        let retired = project.path().join("Workspace-retired");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(&workspace).unwrap();
        let safe_source = workspace.join("Safe.luau");
        fs::write(&safe_source, "").unwrap();
        fs::write(outside.path().join("Sentinel.luau"), "external").unwrap();

        let mut cache = SyncedPathValidationCache::new(project.path()).unwrap();
        cache.validate(&safe_source, false).unwrap();

        fs::rename(&workspace, &retired).unwrap();
        symlink(outside.path(), &workspace).unwrap();
        let through_link = workspace.join("Sentinel.luau");
        let error = cache.validate(&through_link, false).unwrap_err();
        assert!(
            error.to_string().contains("symbolic link")
                || error.to_string().contains("linked/reparse"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("Sentinel.luau")).unwrap(),
            "external"
        );
    }

    #[test]
    fn wide_service_index_handles_twenty_five_thousand_entries_deterministically() {
        let project = tempfile::tempdir().unwrap();
        let workspace = project.path().join("Workspace");
        fs::create_dir(&workspace).unwrap();
        for index in 0..25_000 {
            fs::write(workspace.join(format!("Item{index:05}.luau")), "").unwrap();
        }
        let generation = capture_tree_metadata(project.path(), "Workspace").unwrap();
        assert_eq!(generation.entries.len(), 25_000);
        assert_eq!(
            generation.entries.first().unwrap().relative,
            PathBuf::from("Item00000.luau")
        );
        assert_eq!(
            generation.entries.last().unwrap().relative,
            PathBuf::from("Item24999.luau")
        );
    }
}
