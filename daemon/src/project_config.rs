//! Project config file (`ro-sync.json`) — identifies the project and its
//! bound Roblox GameId / PlaceIds. Written on first daemon startup, read on
//! subsequent ones; fields are only overwritten by explicit CLI args.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;

pub const CONFIG_FILE: &str = "ro-sync.json";
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "gameId", default)]
    pub game_id: Option<String>,
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
    #[serde(default = "default_version")]
    pub version: u32,
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
            game_id: None,
            group_id: None,
            place_ids: Vec::new(),
            wally_enabled: false,
            wally_folder: None,
            wally_file: None,
            version: CONFIG_VERSION,
        }
    }
}

fn config_path(root: &Path) -> std::path::PathBuf {
    root.join(CONFIG_FILE)
}

/// Read the project config if present; otherwise write a default and return it.
pub fn load_or_create(root: &Path) -> io::Result<ProjectConfig> {
    let p = config_path(root);
    if p.exists() {
        let text = fs::read_to_string(&p)?;
        return parse(root, &text);
    }
    let cfg = ProjectConfig::default_for(root);
    write(root, &cfg)?;
    Ok(cfg)
}

pub fn write(root: &Path, cfg: &ProjectConfig) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let text = serde_json::to_string_pretty(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_text_replace(&config_path(root), &(text + "\n"))
}

/// Re-parse `<root>/ro-sync.json` from disk. Returns `Ok(None)` if the file
/// doesn't exist — callers should treat that as "no change" rather than an error.
pub fn read_from_disk(root: &Path) -> io::Result<Option<ProjectConfig>> {
    let p = config_path(root);
    if !p.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&p)?;
    Ok(Some(parse(root, &text)?))
}

fn parse(root: &Path, text: &str) -> io::Result<ProjectConfig> {
    let mut cfg: ProjectConfig =
        serde_json::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if cfg.name.trim().is_empty() {
        cfg.name = ProjectConfig::default_for(root).name;
    }
    Ok(cfg)
}

fn write_text_replace(path: &Path, text: &str) -> io::Result<()> {
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
    let tmp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));

    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);

        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path)?;
        }

        fs::rename(&tmp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Apply CLI overrides to `cfg`. Returns true if any field changed.
pub fn apply_overrides(
    cfg: &mut ProjectConfig,
    game_id: Option<String>,
    group_id: Option<String>,
    place_ids: Option<Vec<String>>,
) -> bool {
    let mut changed = false;
    if let Some(gid) = game_id {
        if cfg.game_id.as_deref() != Some(gid.as_str()) {
            cfg.game_id = Some(gid);
            changed = true;
        }
    }
    if let Some(gid) = group_id {
        if cfg.group_id.as_deref() != Some(gid.as_str()) {
            cfg.group_id = Some(gid);
            changed = true;
        }
    }
    if let Some(pids) = place_ids {
        if cfg.place_ids != pids {
            cfg.place_ids = pids;
            changed = true;
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
}
