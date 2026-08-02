//! Project config file (`ro-sync.json`) — identifies the project and its
//! bound Roblox GameId / PlaceIds. Written on first daemon startup, read on
//! subsequent ones; fields are only overwritten by explicit CLI args.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "ro-sync.json";
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "gameName", default, skip_serializing_if = "Option::is_none")]
    pub game_name: Option<String>,
    #[serde(rename = "gameId", default)]
    pub game_id: Option<String>,
    #[serde(rename = "placeName", default, skip_serializing_if = "Option::is_none")]
    pub place_name: Option<String>,
    #[serde(
        rename = "creatorType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub creator_type: Option<String>,
    #[serde(rename = "creatorId", default, skip_serializing_if = "Option::is_none")]
    pub creator_id: Option<String>,
    #[serde(rename = "groupId", default)]
    pub group_id: Option<String>,
    #[serde(rename = "placeIds", default)]
    pub place_ids: Vec<String>,
    #[serde(rename = "wallyEnabled", default, skip_serializing_if = "is_false")]
    pub wally_enabled: bool,
    #[serde(
        rename = "wallyFolder",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub wally_folder: Option<String>,
    #[serde(rename = "wallyFile", default, skip_serializing_if = "Option::is_none")]
    pub wally_file: Option<String>,
    /// Last full overwrite decision ("studio" | "disk"). Advertised via
    /// `/hello` so the plugin can auto-answer the next initial compare
    /// instead of re-asking a question the user already answered for this
    /// project. Selective disk pulls are one-offs and never recorded.
    #[serde(
        rename = "initialChoiceDefault",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub initial_choice_default: Option<String>,
    #[serde(default = "default_version")]
    pub version: u32,
    /// Preserve desktop/user settings that this daemon version does not know
    /// about when project initialization merges newly discovered metadata.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ProjectConfig {
    pub fn default_for(root: &Path) -> Self {
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "project".to_string());
        ProjectConfig {
            name,
            game_name: None,
            game_id: None,
            place_name: None,
            creator_type: None,
            creator_id: None,
            group_id: None,
            place_ids: Vec::new(),
            wally_enabled: false,
            wally_folder: None,
            wally_file: None,
            initial_choice_default: None,
            version: CONFIG_VERSION,
            extra: BTreeMap::new(),
        }
    }
}

fn validated_config_path(root: &Path, allow_missing: bool) -> io::Result<PathBuf> {
    let canonical_root = crate::fs_safety::stable_canonical_directory(root)?;
    crate::fs_safety::validate_descendant_no_follow(
        &canonical_root,
        Path::new(CONFIG_FILE),
        allow_missing,
    )
}

fn read_config_text(root: &Path) -> io::Result<Option<String>> {
    let canonical_root = crate::fs_safety::stable_canonical_directory(root)?;
    let path = crate::fs_safety::validate_descendant_no_follow(
        &canonical_root,
        Path::new(CONFIG_FILE),
        true,
    )?;
    let guard = crate::fs_safety::guard_descendant_parent_chain(&canonical_root, &path, true)?;
    guard.verify()?;
    let Some(metadata) = crate::fs_safety::metadata_no_follow(&path)? else {
        guard.verify()?;
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("project config is not a regular file: {}", path.display()),
        ));
    }
    guard.verify()?;
    let text = crate::fs_safety::read_to_string_no_follow(&path)?;
    guard.verify()?;
    Ok(Some(text))
}

/// Read the project config if present; otherwise write a default and return it.
pub fn load_or_create(root: &Path) -> io::Result<ProjectConfig> {
    if let Some(text) = read_config_text(root)? {
        return parse(root, &text);
    }
    let cfg = ProjectConfig::default_for(root);
    write(root, &cfg)?;
    Ok(cfg)
}

pub fn write(root: &Path, cfg: &ProjectConfig) -> io::Result<()> {
    let text = serde_json::to_string_pretty(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = validated_config_path(root, true)?;
    write_text_replace(root, &path, &(text + "\n"))
}

/// Re-parse `<root>/ro-sync.json` from disk. Returns `Ok(None)` if the file
/// doesn't exist — callers should treat that as "no change" rather than an error.
pub fn read_from_disk(root: &Path) -> io::Result<Option<ProjectConfig>> {
    read_config_text(root)?
        .map(|text| parse(root, &text))
        .transpose()
}

fn parse(root: &Path, text: &str) -> io::Result<ProjectConfig> {
    let mut cfg: ProjectConfig =
        serde_json::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if cfg.name.trim().is_empty() {
        cfg.name = ProjectConfig::default_for(root).name;
    }
    Ok(cfg)
}

fn write_text_replace(root: &Path, path: &Path, text: &str) -> io::Result<()> {
    let canonical_root = crate::fs_safety::stable_canonical_directory(root)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid config path: {}", path.display()),
            )
        })?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = parent.join(format!(".{file_name}.{}-{nonce}.tmp", std::process::id()));
    let _ = crate::fs_safety::validate_descendant_no_follow(
        &canonical_root,
        Path::new(tmp.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid config temp path")
        })?),
        true,
    )?;
    let guard = crate::fs_safety::guard_descendant_parent_chain(&canonical_root, path, true)?;

    let result = (|| {
        guard.verify()?;
        if let Some(metadata) = crate::fs_safety::metadata_no_follow(path)? {
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("project config is not a regular file: {}", path.display()),
                ));
            }
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);

        guard.verify()?;
        let _ = crate::fs_safety::metadata_no_follow(path)?;
        crate::lifecycle::replace_file_atomic(&tmp, path)?;
        guard.verify()?;
        let metadata = crate::fs_safety::require_metadata_no_follow(path)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "project config replacement is not a regular file: {}",
                    path.display()
                ),
            ));
        }
        Ok(())
    })();

    if result.is_err()
        && guard.verify().is_ok()
        && matches!(
            crate::fs_safety::metadata_no_follow(&tmp),
            Ok(Some(metadata)) if metadata.is_file()
        )
    {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Apply CLI overrides to `cfg`. Returns true if any field changed.
/// Apply caller-supplied identity to the config. `ro-sync.json` is the
/// source of truth: overrides only FILL EMPTY fields (adopting an
/// unidentified project), never replace a recorded identity. The desktop
/// app passes its own cached gameId/placeIds on every daemon start, and
/// letting that cache win rebound projects to the wrong place whenever it
/// went stale — the on-disk identity would flip under a running window.
pub fn apply_overrides(
    cfg: &mut ProjectConfig,
    game_id: Option<String>,
    group_id: Option<String>,
    place_ids: Option<Vec<String>>,
) -> bool {
    let mut changed = false;
    if let Some(gid) = game_id {
        match cfg.game_id.as_deref() {
            None => {
                cfg.game_id = Some(gid);
                changed = true;
            }
            Some(existing) if existing != gid.as_str() => {
                eprintln!(
                    "ignoring gameId override {gid}: ro-sync.json already records {existing}"
                );
            }
            Some(_) => {}
        }
    }
    if let Some(gid) = group_id {
        if cfg.group_id.is_none() {
            cfg.group_id = Some(gid);
            changed = true;
        }
    }
    if let Some(pids) = place_ids {
        if cfg.place_ids.is_empty() && !pids.is_empty() {
            cfg.place_ids = pids;
            changed = true;
        } else if cfg.place_ids != pids {
            eprintln!(
                "ignoring placeIds override {pids:?}: ro-sync.json already records {:?}",
                cfg.place_ids
            );
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(tempfile::TempDir);
    impl TempDir {
        fn new(tag: &str) -> Self {
            TempDir(
                tempfile::Builder::new()
                    .prefix(&format!("rosync-pcfg-{tag}-"))
                    .tempdir()
                    .unwrap(),
            )
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    #[test]
    fn creates_on_first_load() {
        let d = TempDir::new("create");
        let cfg = load_or_create(d.path()).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.game_id.is_none());
        assert!(cfg.place_ids.is_empty());
        assert!(d.path().join(CONFIG_FILE).exists());
    }

    #[test]
    fn reads_existing_without_overwriting() {
        let d = TempDir::new("read");
        let text = r#"{"name":"MyProj","gameId":"1234567890","groupId":"333","placeIds":["111","222"],"version":1}"#;
        fs::write(d.path().join(CONFIG_FILE), text).unwrap();
        let cfg = load_or_create(d.path()).unwrap();
        assert_eq!(cfg.name, "MyProj");
        assert_eq!(cfg.game_id.as_deref(), Some("1234567890"));
        assert_eq!(cfg.group_id.as_deref(), Some("333"));
        assert_eq!(cfg.place_ids, vec!["111".to_string(), "222".to_string()]);
        assert!(!cfg.wally_enabled);
        assert!(cfg.wally_folder.is_none());
        assert!(cfg.wally_file.is_none());
    }

    #[test]
    fn fills_defaults_for_minimal_existing_config() {
        let d = TempDir::new("minimal");
        fs::write(d.path().join(CONFIG_FILE), "{}\r\n").unwrap();

        let cfg = load_or_create(d.path()).unwrap();

        assert_eq!(
            cfg.name,
            d.path().file_name().and_then(|name| name.to_str()).unwrap()
        );
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.game_id.is_none());
        assert!(cfg.group_id.is_none());
        assert!(cfg.place_ids.is_empty());
    }

    #[test]
    fn replaces_existing_config_with_trailing_newline() {
        let d = TempDir::new("replace");
        fs::write(d.path().join(CONFIG_FILE), b"old bytes").unwrap();

        let mut cfg = ProjectConfig::default_for(d.path());
        cfg.game_id = Some("123".to_string());
        write(d.path(), &cfg).unwrap();

        let path = d.path().join(CONFIG_FILE);
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.ends_with('\n'));
        assert!(!body.contains("old bytes"));
        assert!(body.contains("\"gameId\": \"123\""));
        assert!(
            !d.path()
                .join(format!(".{}.{}.tmp", CONFIG_FILE, std::process::id()))
                .exists(),
            "temporary config file should be removed after replace"
        );
    }

    #[test]
    fn reads_and_writes_wally_settings() {
        let d = TempDir::new("wally");
        let text = r#"{"name":"MyProj","wallyEnabled":true,"wallyFolder":"ReplicatedStorage/Assets/Packages","wallyFile":"[dependencies]\nReact = \"jsdotlua/react@17.1.0\"\n","version":1}"#;
        fs::write(d.path().join(CONFIG_FILE), text).unwrap();

        let cfg = load_or_create(d.path()).unwrap();
        assert!(cfg.wally_enabled);
        assert_eq!(
            cfg.wally_folder.as_deref(),
            Some("ReplicatedStorage/Assets/Packages")
        );
        assert!(cfg
            .wally_file
            .as_deref()
            .unwrap_or("")
            .contains("jsdotlua/react"));

        write(d.path(), &cfg).unwrap();
        let round_trip = fs::read_to_string(d.path().join(CONFIG_FILE)).unwrap();
        assert!(round_trip.contains("\"wallyEnabled\": true"));
        assert!(round_trip.contains("\"wallyFolder\""));
        assert!(round_trip.contains("\"wallyFile\""));
    }

    #[test]
    fn apply_overrides_updates_fields() {
        let mut cfg = ProjectConfig::default_for(Path::new("x"));
        assert!(apply_overrides(
            &mut cfg,
            Some("42".into()),
            Some("7".into()),
            Some(vec!["9".into()])
        ));
        assert_eq!(cfg.game_id.as_deref(), Some("42"));
        assert_eq!(cfg.group_id.as_deref(), Some("7"));
        assert_eq!(cfg.place_ids, vec!["9".to_string()]);
        // second call with same values -> no change
        assert!(!apply_overrides(
            &mut cfg,
            Some("42".into()),
            Some("7".into()),
            Some(vec!["9".into()])
        ));
    }

    #[cfg(unix)]
    #[test]
    fn linked_config_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let project = TempDir::new("linked-config-project");
        let external = TempDir::new("linked-config-external");
        let sentinel = external.path().join("sentinel.json");
        fs::write(&sentinel, b"external sentinel\n").unwrap();
        symlink(&sentinel, project.path().join(CONFIG_FILE)).unwrap();

        let read_error = load_or_create(project.path()).unwrap_err();
        assert_eq!(read_error.kind(), io::ErrorKind::InvalidData);

        let config = ProjectConfig::default_for(project.path());
        let write_error = write(project.path(), &config).unwrap_err();
        assert_eq!(write_error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&sentinel).unwrap(), b"external sentinel\n");
    }
}
