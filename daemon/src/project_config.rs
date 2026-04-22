//! Project config file (`ro-sync.json`) — identifies the project and its
//! bound Roblox GameId / PlaceIds. Written on first daemon startup, read on
//! subsequent ones; fields are only overwritten by explicit CLI args.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

pub const CONFIG_FILE: &str = "ro-sync.json";
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    #[serde(rename = "gameId")]
    pub game_id: Option<String>,
    #[serde(rename = "placeIds", default)]
    pub place_ids: Vec<String>,
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    CONFIG_VERSION
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
            place_ids: Vec::new(),
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
        let cfg: ProjectConfig = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        return Ok(cfg);
    }
    let cfg = ProjectConfig::default_for(root);
    write(root, &cfg)?;
    Ok(cfg)
}

pub fn write(root: &Path, cfg: &ProjectConfig) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let text = serde_json::to_string_pretty(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(config_path(root), text)
}

/// Re-parse `<root>/ro-sync.json` from disk. Returns `Ok(None)` if the file
/// doesn't exist — callers should treat that as "no change" rather than an error.
pub fn read_from_disk(root: &Path) -> io::Result<Option<ProjectConfig>> {
    let p = config_path(root);
    if !p.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&p)?;
    let cfg: ProjectConfig = serde_json::from_str(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(cfg))
}

/// Apply CLI overrides to `cfg`. Returns true if any field changed.
pub fn apply_overrides(
    cfg: &mut ProjectConfig,
    game_id: Option<String>,
    place_ids: Option<Vec<String>>,
) -> bool {
    let mut changed = false;
    if let Some(gid) = game_id {
        if cfg.game_id.as_deref() != Some(gid.as_str()) {
            cfg.game_id = Some(gid);
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
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let p = std::env::temp_dir().join(format!(
                "rosync-pcfg-{}-{}-{:x}",
                tag,
                std::process::id(),
                nanos
            ));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
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
        let text = r#"{"name":"MyProj","gameId":"1234567890","placeIds":["111","222"],"version":1}"#;
        fs::write(d.path().join(CONFIG_FILE), text).unwrap();
        let cfg = load_or_create(d.path()).unwrap();
        assert_eq!(cfg.name, "MyProj");
        assert_eq!(cfg.game_id.as_deref(), Some("1234567890"));
        assert_eq!(cfg.place_ids, vec!["111".to_string(), "222".to_string()]);
    }

    #[test]
    fn apply_overrides_updates_fields() {
        let mut cfg = ProjectConfig::default_for(Path::new("/tmp/x"));
        assert!(apply_overrides(&mut cfg, Some("42".into()), Some(vec!["9".into()])));
        assert_eq!(cfg.game_id.as_deref(), Some("42"));
        assert_eq!(cfg.place_ids, vec!["9".to_string()]);
        // second call with same values -> no change
        assert!(!apply_overrides(
            &mut cfg,
            Some("42".into()),
            Some(vec!["9".into()])
        ));
    }
}
