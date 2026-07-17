//! Safe, plugin-initiated project creation below a desktop-authorized root.
//!
//! The Studio plugin supplies metadata only. It never supplies an absolute or
//! relative filesystem path: the daemon derives a single safe directory name,
//! creates exactly one direct child of the configured root, and refuses to
//! overwrite an unrelated project.

use serde::Serialize;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{project_config, snapshot};

const MAX_DISPLAY_NAME_BYTES: usize = 256;
const MAX_DIRECTORY_NAME_BYTES: usize = 72;
const MAX_ID_DIGITS: usize = 20;
const MAX_PROJECT_ROOT_ENTRIES: usize = 4096;

#[derive(Debug, Clone)]
pub(crate) struct ProjectInitRequest {
    pub game_name: String,
    pub place_name: String,
    pub game_id: String,
    pub place_id: String,
    pub creator_type: Option<String>,
    pub creator_id: Option<String>,
    pub group_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectInitMetadata {
    pub game_name: String,
    pub place_name: String,
    pub game_id: String,
    /// Roblox calls this value `GameId`; Open Cloud calls the same identifier a
    /// universe ID. Include both names at the protocol boundary for clarity.
    pub universe_id: String,
    pub place_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectInitOutcome {
    pub project: PathBuf,
    pub directory_name: String,
    pub name: String,
    pub created: bool,
    pub metadata: ProjectInitMetadata,
    pub changed: ProjectInitChanged,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectInitChanged {
    pub config: bool,
    pub ro_sync_md: bool,
    pub claude_md: bool,
    pub codex_context: bool,
    pub tooling: bool,
}

#[derive(Debug)]
pub(crate) struct ProjectInitError {
    code: &'static str,
    message: String,
    suggested_directory_name: Option<String>,
}

impl ProjectInitError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            suggested_directory_name: None,
        }
    }

    fn with_suggestion(mut self, suggestion: String) -> Self {
        self.suggested_directory_name = Some(suggestion);
        self
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn suggested_directory_name(&self) -> Option<&str> {
        self.suggested_directory_name.as_deref()
    }
}

impl fmt::Display for ProjectInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProjectInitError {}

pub(crate) fn resolve_projects_root(path: &Path) -> Result<PathBuf, ProjectInitError> {
    if !path.is_absolute() {
        return Err(ProjectInitError::new(
            "INVALID_PROJECTS_ROOT",
            "projects root must be an absolute path",
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        ProjectInitError::new(
            "INVALID_PROJECTS_ROOT",
            format!("inspect projects root {}: {error}", path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ProjectInitError::new(
            "INVALID_PROJECTS_ROOT",
            format!(
                "projects root must be an existing, non-symlink directory: {}",
                path.display()
            ),
        ));
    }
    fs::canonicalize(path).map_err(|error| {
        ProjectInitError::new(
            "INVALID_PROJECTS_ROOT",
            format!("resolve projects root {}: {error}", path.display()),
        )
    })
}

pub(crate) fn initialize_project(
    projects_root: &Path,
    request: ProjectInitRequest,
) -> Result<ProjectInitOutcome, ProjectInitError> {
    let root = resolve_projects_root(projects_root)?;
    let metadata = validate_request(request)?;
    if let Some((existing, directory_name)) = find_existing_project(&root, &metadata.game_id)? {
        if let Some(outcome) =
            merge_existing_project(&root, &existing, &directory_name, metadata.clone())?
        {
            return Ok(outcome);
        }
    }
    let base = directory_slug(&metadata.game_name, &metadata.game_id);
    let with_game_id = append_suffix(&base, &metadata.game_id);
    let candidates = if base == with_game_id {
        vec![base.clone()]
    } else {
        vec![base.clone(), with_game_id.clone()]
    };

    for directory_name in &candidates {
        let candidate = root.join(directory_name);
        match fs::symlink_metadata(&candidate) {
            Ok(existing) => {
                if existing.file_type().is_symlink() || !existing.is_dir() {
                    continue;
                }
                if let Some(outcome) =
                    merge_existing_project(&root, &candidate, directory_name, metadata.clone())?
                {
                    return Ok(outcome);
                }
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ProjectInitError::new(
                    "PROJECT_INIT_FAILED",
                    format!("inspect project candidate {}: {error}", candidate.display()),
                ));
            }
        }

        match fs::create_dir(&candidate) {
            Ok(()) => {
                let initialized = initialize_created_directory(
                    &root,
                    &candidate,
                    directory_name,
                    metadata.clone(),
                );
                if initialized.is_err() {
                    // The directory was created by this request and has never
                    // been exposed as a successful project. Roll back only that
                    // exact direct child; never touch a pre-existing collision.
                    let _ = fs::remove_dir_all(&candidate);
                }
                return initialized;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A concurrent request won the create race. Treat an exact
                // universe match as idempotent, otherwise try the deterministic
                // game-ID suffix before reporting a collision.
                if let Some(outcome) =
                    merge_existing_project(&root, &candidate, directory_name, metadata.clone())?
                {
                    return Ok(outcome);
                }
            }
            Err(error) => {
                return Err(ProjectInitError::new(
                    "PROJECT_INIT_FAILED",
                    format!("create project directory {}: {error}", candidate.display()),
                ));
            }
        }
    }

    Err(ProjectInitError::new(
        "PROJECT_PATH_COLLISION",
        format!(
            "both '{}' and '{}' already exist and belong to another project",
            base, with_game_id
        ),
    )
    .with_suggestion(append_suffix(&with_game_id, &metadata.place_id)))
}

fn initialize_created_directory(
    root: &Path,
    candidate: &Path,
    directory_name: &str,
    metadata: ProjectInitMetadata,
) -> Result<ProjectInitOutcome, ProjectInitError> {
    let canonical = fs::canonicalize(candidate).map_err(|error| {
        ProjectInitError::new(
            "PROJECT_INIT_FAILED",
            format!("resolve new project {}: {error}", candidate.display()),
        )
    })?;
    if canonical.parent() != Some(root) {
        return Err(ProjectInitError::new(
            "PROJECT_PATH_ESCAPE",
            "new project did not resolve to a direct child of the configured projects root",
        ));
    }

    let mut config = project_config::ProjectConfig::default_for(&canonical);
    config.name = preferred_project_name(&metadata.game_name, &metadata.place_name);
    config.game_name = Some(metadata.game_name.clone());
    config.game_id = Some(metadata.game_id.clone());
    config.group_id = metadata.group_id.clone();
    config.place_ids = vec![metadata.place_id.clone()];
    config.place_name = Some(metadata.place_name.clone());
    config.creator_type = metadata.creator_type.clone();
    config.creator_id = metadata.creator_id.clone();
    project_config::write(&canonical, &config).map_err(|error| {
        ProjectInitError::new(
            "PROJECT_INIT_FAILED",
            format!(
                "write {}: {error}",
                canonical.join(project_config::CONFIG_FILE).display()
            ),
        )
    })?;

    let ro_sync_md = snapshot::write_ro_sync_md_if_missing(&canonical)
        .map_err(|error| init_file_error("ro-sync.md", error))?;
    let claude_md = snapshot::write_claude_md_if_missing_or_merge(&canonical)
        .map_err(|error| init_file_error("CLAUDE.md", error))?;
    let codex_context = snapshot::write_codex_context_if_missing_or_merge(&canonical)
        .map_err(|error| init_file_error("Codex context", error))?;
    let tooling = snapshot::write_project_tooling_if_missing_or_merge(&canonical)
        .map_err(|error| init_file_error("project tooling", error))?;

    Ok(ProjectInitOutcome {
        project: canonical,
        directory_name: directory_name.to_string(),
        name: config.name,
        created: true,
        metadata,
        changed: ProjectInitChanged {
            config: true,
            ro_sync_md,
            claude_md,
            codex_context,
            tooling,
        },
    })
}

fn init_file_error(label: &str, error: std::io::Error) -> ProjectInitError {
    ProjectInitError::new(
        "PROJECT_INIT_FAILED",
        format!("initialize {label}: {error}"),
    )
}

fn find_existing_project(
    root: &Path,
    game_id: &str,
) -> Result<Option<(PathBuf, String)>, ProjectInitError> {
    let entries = fs::read_dir(root).map_err(|error| {
        ProjectInitError::new(
            "PROJECT_INIT_FAILED",
            format!("inspect projects root {}: {error}", root.display()),
        )
    })?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_PROJECT_ROOT_ENTRIES {
            return Err(ProjectInitError::new(
                "PROJECT_INIT_FAILED",
                "projects root contains too many direct children",
            ));
        }
        let Ok(entry) = entry else {
            continue;
        };
        let candidate = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let Ok(Some(config)) = project_config::read_from_disk(&candidate) else {
            continue;
        };
        if config.game_id.as_deref() != Some(game_id) {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&candidate) else {
            continue;
        };
        if canonical.parent() != Some(root) {
            continue;
        }
        let directory_name = entry.file_name().to_string_lossy().into_owned();
        return Ok(Some((canonical, directory_name)));
    }
    Ok(None)
}

fn merge_existing_project(
    root: &Path,
    candidate: &Path,
    directory_name: &str,
    metadata: ProjectInitMetadata,
) -> Result<Option<ProjectInitOutcome>, ProjectInitError> {
    let Ok(canonical) = fs::canonicalize(candidate) else {
        return Ok(None);
    };
    if canonical.parent() != Some(root) {
        return Ok(None);
    }
    let Ok(Some(mut config)) = project_config::read_from_disk(&canonical) else {
        return Ok(None);
    };
    if config.game_id.as_deref() != Some(metadata.game_id.as_str()) {
        return Ok(None);
    }

    let mut changed = false;
    let preferred_name = preferred_project_name(&metadata.game_name, &metadata.place_name);
    if is_placeholder_project_name(&config.name) && config.name != preferred_name {
        config.name = preferred_name;
        changed = true;
    }
    if config.game_name.as_deref() != Some(metadata.game_name.as_str()) {
        config.game_name = Some(metadata.game_name.clone());
        changed = true;
    }
    if config.place_name.as_deref() != Some(metadata.place_name.as_str()) {
        config.place_name = Some(metadata.place_name.clone());
        changed = true;
    }
    if let Some(group_id) = metadata.group_id.as_ref() {
        if config.group_id.as_ref() != Some(group_id) {
            config.group_id = Some(group_id.clone());
            changed = true;
        }
    }
    if let Some(creator_type) = metadata.creator_type.as_ref() {
        if config.creator_type.as_ref() != Some(creator_type) {
            config.creator_type = Some(creator_type.clone());
            changed = true;
        }
    }
    if let Some(creator_id) = metadata.creator_id.as_ref() {
        if config.creator_id.as_ref() != Some(creator_id) {
            config.creator_id = Some(creator_id.clone());
            changed = true;
        }
    }
    if !config.place_ids.contains(&metadata.place_id) {
        config.place_ids.push(metadata.place_id.clone());
        config.place_ids.sort();
        config.place_ids.dedup();
        changed = true;
    }
    if changed {
        project_config::write(&canonical, &config).map_err(|error| {
            ProjectInitError::new(
                "PROJECT_INIT_FAILED",
                format!(
                    "merge metadata into {}: {error}",
                    canonical.join(project_config::CONFIG_FILE).display()
                ),
            )
        })?;
    }

    Ok(Some(ProjectInitOutcome {
        project: canonical,
        directory_name: directory_name.to_string(),
        name: config.name,
        created: false,
        metadata,
        changed: ProjectInitChanged {
            config: changed,
            ..ProjectInitChanged::default()
        },
    }))
}

fn validate_request(request: ProjectInitRequest) -> Result<ProjectInitMetadata, ProjectInitError> {
    let game_name = validate_display_name(&request.game_name, "gameName")?;
    let place_name = validate_display_name(&request.place_name, "placeName")?;
    let game_id = validate_id(&request.game_id, "gameId", true)?.expect("required gameId");
    let place_id = validate_id(&request.place_id, "placeId", true)?.expect("required placeId");
    let creator_id = validate_id(
        request.creator_id.as_deref().unwrap_or(""),
        "creatorId",
        false,
    )?;
    let explicit_group = validate_id(request.group_id.as_deref().unwrap_or(""), "groupId", false)?;
    let creator_type = match request.creator_type.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(value) if value.eq_ignore_ascii_case("group") => Some("Group".to_string()),
        Some(value) if value.eq_ignore_ascii_case("user") => Some("User".to_string()),
        Some(_) => {
            return Err(ProjectInitError::new(
                "INVALID_METADATA",
                "creatorType must be User or Group",
            ));
        }
    };
    if creator_type.is_some() != creator_id.is_some() {
        return Err(ProjectInitError::new(
            "INVALID_METADATA",
            "creatorType and creatorId must be supplied together",
        ));
    }
    let group_id = if creator_type.as_deref() == Some("Group") {
        match (&explicit_group, &creator_id) {
            (Some(group), Some(creator)) if group != creator => {
                return Err(ProjectInitError::new(
                    "INVALID_METADATA",
                    "groupId must match creatorId for a group-owned experience",
                ));
            }
            (Some(group), _) => Some(group.clone()),
            (None, Some(creator)) => Some(creator.clone()),
            _ => None,
        }
    } else {
        explicit_group
    };

    Ok(ProjectInitMetadata {
        game_name,
        place_name,
        game_id: game_id.clone(),
        universe_id: game_id,
        place_id,
        creator_type,
        creator_id,
        group_id,
    })
}

fn is_placeholder_project_name(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() || matches!(normalized.as_str(), "game" | "untitled experience") {
        return true;
    }
    normalized.strip_prefix("place").is_some_and(|suffix| {
        suffix.is_empty() || suffix.chars().all(|value| value.is_ascii_digit())
    })
}

fn preferred_project_name(game_name: &str, place_name: &str) -> String {
    for candidate in [game_name, place_name] {
        let candidate = candidate.trim();
        if !is_placeholder_project_name(candidate) {
            return candidate.to_string();
        }
    }
    game_name.trim().to_string()
}

fn validate_display_name(value: &str, field: &str) -> Result<String, ProjectInitError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_DISPLAY_NAME_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ProjectInitError::new(
            "INVALID_METADATA",
            format!(
                "{field} must be 1-{MAX_DISPLAY_NAME_BYTES} UTF-8 bytes without control characters"
            ),
        ));
    }
    Ok(value.to_string())
}

fn validate_id(
    value: &str,
    field: &str,
    required: bool,
) -> Result<Option<String>, ProjectInitError> {
    let value = value.trim();
    if value.is_empty() && !required {
        return Ok(None);
    }
    if value.is_empty()
        || value.len() > MAX_ID_DIGITS
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(ProjectInitError::new(
            "INVALID_METADATA",
            format!("{field} must be a positive Roblox integer encoded as a decimal string"),
        ));
    }
    let normalized = value.trim_start_matches('0');
    Ok(Some(normalized.to_string()))
}

fn directory_slug(name: &str, game_id: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() {
            separator = true;
        }
        if slug.len() >= MAX_DIRECTORY_NAME_BYTES {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug = format!("game-{game_id}");
    }
    if is_windows_reserved_name(&slug) || slug == "." || slug == ".." {
        slug = format!("game-{slug}");
    }
    slug.truncate(MAX_DIRECTORY_NAME_BYTES);
    while slug.ends_with(['.', ' ', '-']) {
        slug.pop();
    }
    if slug.is_empty() {
        format!("game-{game_id}")
    } else {
        slug
    }
}

fn append_suffix(base: &str, suffix: &str) -> String {
    let reserved = suffix.len().saturating_add(1);
    let keep = MAX_DIRECTORY_NAME_BYTES.saturating_sub(reserved);
    let mut prefix = base[..base.len().min(keep)]
        .trim_end_matches('-')
        .to_string();
    if prefix.is_empty() {
        prefix = "game".to_string();
    }
    format!("{prefix}-{suffix}")
}

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(game_name: &str, game_id: &str) -> ProjectInitRequest {
        ProjectInitRequest {
            game_name: game_name.to_string(),
            place_name: "Main Place".to_string(),
            game_id: game_id.to_string(),
            place_id: "456".to_string(),
            creator_type: Some("Group".to_string()),
            creator_id: Some("789".to_string()),
            group_id: Some("789".to_string()),
        }
    }

    #[test]
    fn slug_never_contains_path_components_or_reserved_names() {
        assert_eq!(directory_slug("../../Race: Stars", "123"), "race-stars");
        assert_eq!(directory_slug("CON", "123"), "game-con");
        assert_eq!(directory_slug("日本語", "123"), "game-123");
        let long = directory_slug(&"a".repeat(500), "123");
        assert!(long.len() <= MAX_DIRECTORY_NAME_BYTES);
        assert!(!long.contains('/'));
        assert!(!long.contains('\\'));
    }

    #[test]
    fn creates_one_direct_child_with_complete_metadata_and_docs() {
        let root = tempfile::tempdir().unwrap();
        let outcome = initialize_project(root.path(), request("Race Stars", "123")).unwrap();
        assert!(outcome.created);
        assert_eq!(
            outcome.project.parent(),
            Some(fs::canonicalize(root.path()).unwrap().as_path())
        );
        assert_eq!(outcome.directory_name, "race-stars");
        assert_eq!(outcome.name, "Race Stars");
        let config = project_config::read_from_disk(&outcome.project)
            .unwrap()
            .unwrap();
        assert_eq!(config.name, "Race Stars");
        assert_eq!(config.game_name.as_deref(), Some("Race Stars"));
        assert_eq!(config.game_id.as_deref(), Some("123"));
        assert_eq!(config.group_id.as_deref(), Some("789"));
        assert_eq!(config.place_ids, ["456"]);
        assert_eq!(config.place_name.as_deref(), Some("Main Place"));
        assert_eq!(config.creator_type.as_deref(), Some("Group"));
        assert_eq!(config.creator_id.as_deref(), Some("789"));
        assert!(outcome.project.join("ro-sync.md").is_file());
        assert!(outcome.project.join("AGENTS.md").is_file());
        assert!(outcome.project.join(".codex/config.toml").is_file());
    }

    #[test]
    fn repeated_same_universe_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let first = initialize_project(root.path(), request("Race Stars", "123")).unwrap();
        let second = initialize_project(root.path(), request("Race Stars", "123")).unwrap();
        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.project, second.project);
        assert!(!second.changed.config);
    }

    #[test]
    fn existing_universe_merges_new_place_and_metadata_without_losing_unknown_settings() {
        let root = tempfile::tempdir().unwrap();
        let first = initialize_project(root.path(), request("Race Stars", "123")).unwrap();
        let mut config = project_config::read_from_disk(&first.project)
            .unwrap()
            .unwrap();
        config.extra.insert(
            "AutoReconnect".to_string(),
            serde_json::Value::String("on".to_string()),
        );
        project_config::write(&first.project, &config).unwrap();

        let mut second_request = request("Race Stars Reborn", "123");
        second_request.place_name = "Desert Place".to_string();
        second_request.place_id = "999".to_string();
        second_request.creator_id = Some("101".to_string());
        second_request.group_id = Some("101".to_string());
        let second = initialize_project(root.path(), second_request.clone()).unwrap();
        assert!(!second.created);
        assert!(second.changed.config);
        assert_eq!(
            second.project, first.project,
            "renames must not fork a universe"
        );

        let merged = project_config::read_from_disk(&first.project)
            .unwrap()
            .unwrap();
        assert_eq!(merged.name, "Race Stars");
        assert_eq!(merged.game_name.as_deref(), Some("Race Stars Reborn"));
        assert_eq!(merged.place_name.as_deref(), Some("Desert Place"));
        assert_eq!(merged.place_ids, ["456", "999"]);
        assert_eq!(merged.group_id.as_deref(), Some("101"));
        assert_eq!(merged.creator_id.as_deref(), Some("101"));
        assert_eq!(
            merged.extra.get("AutoReconnect"),
            Some(&serde_json::Value::String("on".to_string()))
        );

        let third = initialize_project(root.path(), second_request).unwrap();
        assert!(!third.created);
        assert!(!third.changed.config);
    }

    #[test]
    fn placeholder_project_names_upgrade_without_overwriting_custom_names() {
        let root = tempfile::tempdir().unwrap();
        let first = initialize_project(root.path(), request("Place1", "123")).unwrap();
        let placeholder = project_config::read_from_disk(&first.project)
            .unwrap()
            .unwrap();
        assert_eq!(placeholder.name, "Main Place");

        let mut custom = placeholder;
        custom.name = "My Local Race Project".into();
        project_config::write(&first.project, &custom).unwrap();
        let mut renamed = request("Race Stars Reborn", "123");
        renamed.place_name = "Desert Place".into();
        let outcome = initialize_project(root.path(), renamed).unwrap();
        assert_eq!(outcome.name, "My Local Race Project");
        let preserved = project_config::read_from_disk(&first.project)
            .unwrap()
            .unwrap();
        assert_eq!(preserved.name, "My Local Race Project");
        assert_eq!(preserved.game_name.as_deref(), Some("Race Stars Reborn"));
    }

    #[test]
    fn unrelated_collision_uses_deterministic_game_id_suffix_without_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let collision = root.path().join("race-stars");
        fs::create_dir(&collision).unwrap();
        fs::write(collision.join("keep.txt"), "user data").unwrap();

        let outcome = initialize_project(root.path(), request("Race Stars", "123")).unwrap();
        assert_eq!(outcome.directory_name, "race-stars-123");
        assert_eq!(
            fs::read_to_string(collision.join("keep.txt")).unwrap(),
            "user data"
        );
    }

    #[test]
    fn double_collision_is_refused_and_symlinks_are_never_followed() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("race-stars")).unwrap();
        fs::create_dir(root.path().join("race-stars-123")).unwrap();
        let error = initialize_project(root.path(), request("Race Stars", "123")).unwrap_err();
        assert_eq!(error.code(), "PROJECT_PATH_COLLISION");
        assert_eq!(error.suggested_directory_name(), Some("race-stars-123-456"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let second_root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), second_root.path().join("race-stars")).unwrap();
            let outcome =
                initialize_project(second_root.path(), request("Race Stars", "123")).unwrap();
            assert_eq!(outcome.directory_name, "race-stars-123");
            assert!(!outside.path().join(project_config::CONFIG_FILE).exists());
        }
    }

    #[test]
    fn rejects_invalid_ids_creator_mismatches_and_relative_roots() {
        let root = tempfile::tempdir().unwrap();
        let mut invalid = request("Race Stars", "../123");
        assert_eq!(
            initialize_project(root.path(), invalid.clone())
                .unwrap_err()
                .code(),
            "INVALID_METADATA"
        );
        invalid.game_id = "123".into();
        invalid.group_id = Some("999".into());
        assert_eq!(
            initialize_project(root.path(), invalid).unwrap_err().code(),
            "INVALID_METADATA"
        );
        assert_eq!(
            resolve_projects_root(Path::new("relative/projects"))
                .unwrap_err()
                .code(),
            "INVALID_PROJECTS_ROOT"
        );
    }

    #[test]
    fn init_token_generation_dependency_remains_available() {
        // Project initialization uses the same OS-backed CSPRNG dependency as
        // the daemon capability and artifact transports. Keep that dependency
        // exercised here so packaging cannot silently remove it.
        assert_eq!(crate::artifact::random_hex(16).unwrap().len(), 32);
    }
}
