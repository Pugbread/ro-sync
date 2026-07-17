use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Map, Value};

use crate::resources::display_path;

const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_PROJECT_FILE_BYTES: u64 = 4 * 1024 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > 64 {
        return Err("key must contain between 1 and 64 characters".into());
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("key may contain only letters, numbers, dots, underscores, and hyphens".into());
    }
    Ok(())
}

pub(crate) fn state_get(path: &Path, key: &str) -> Result<Option<Value>, String> {
    validate_key(key)?;
    if key == "secrets" {
        return Err("secrets are available only through the secure secret commands".into());
    }
    Ok(read_json_object(path, MAX_STATE_BYTES)?.remove(key))
}

pub(crate) fn state_set(path: &Path, key: &str, value: Value) -> Result<(), String> {
    validate_key(key)?;
    if key == "secrets" {
        return Err("secrets are available only through the secure secret commands".into());
    }
    let mut object = read_json_object(path, MAX_STATE_BYTES)?;
    object.insert(key.to_owned(), value);
    write_json_object(path, &object, 0o600)
}

pub(crate) fn read_json_object(path: &Path, limit: u64) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", display_path(path)))?;
    if metadata.len() > limit {
        return Err(format!("{} exceeds the size limit", display_path(path)));
    }
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open {}: {error}", display_path(path)))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {}: {error}", display_path(path)))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} contains invalid JSON: {error}", display_path(path)))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("{} must contain a JSON object", display_path(path)))
}

pub(crate) fn write_json_object(
    path: &Path,
    object: &Map<String, Value>,
    mode: u32,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(object)
        .map_err(|error| format!("could not encode state: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("state exceeds the size limit".into());
    }
    atomic_write(path, &bytes, mode)
}

pub(crate) fn validate_project_file_path(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err("project file path must be absolute".into());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("project file path must not contain . or .. components".into());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "project file path must have a UTF-8 file name".to_string())?;
    if !matches!(name, "ro-sync.json" | "wally.toml") {
        return Err("only ro-sync.json and wally.toml project files are allowed".into());
    }
    Ok(path)
}

pub(crate) fn authorize_project_root(store: &Path, root: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("could not resolve {}: {error}", display_path(root)))?;
    if !root.is_dir() {
        return Err("authorized project root must be a folder".into());
    }
    let mut roots = read_authorized_roots(store)?;
    if !roots.contains(&root) {
        if roots.len() >= 256 {
            return Err("authorized project root limit reached".into());
        }
        roots.push(root.clone());
        roots.sort();
        let mut bytes = serde_json::to_vec_pretty(
            &roots
                .iter()
                .map(|path| display_path(path))
                .collect::<Vec<_>>(),
        )
        .map_err(|error| format!("could not encode authorized project roots: {error}"))?;
        bytes.push(b'\n');
        atomic_write(store, &bytes, 0o600)?;
    }
    Ok(root)
}

pub(crate) fn ensure_authorized_path(store: &Path, path: &Path) -> Result<(), String> {
    let candidate = canonicalize_with_missing_tail(path)?;
    if read_authorized_roots(store)?
        .iter()
        .any(|root| candidate == *root || candidate.starts_with(root))
    {
        return Ok(());
    }
    Err(format!(
        "{} is outside the project folders explicitly selected in Ro Sync",
        display_path(path)
    ))
}

fn read_authorized_roots(store: &Path) -> Result<Vec<PathBuf>, String> {
    if !store.exists() {
        return Ok(Vec::new());
    }
    let metadata = fs::metadata(store)
        .map_err(|error| format!("could not inspect {}: {error}", display_path(store)))?;
    if metadata.len() > MAX_STATE_BYTES {
        return Err("authorized project root store exceeds the size limit".into());
    }
    let roots: Vec<String> = serde_json::from_slice(
        &fs::read(store)
            .map_err(|error| format!("could not read {}: {error}", display_path(store)))?,
    )
    .map_err(|error| format!("authorized project root store is invalid: {error}"))?;
    Ok(roots.into_iter().map(PathBuf::from).collect())
}

fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf, String> {
    let mut cursor = path;
    let mut missing = Vec::new();
    while !cursor.exists() {
        let name = cursor
            .file_name()
            .ok_or_else(|| format!("could not resolve {}", display_path(path)))?;
        missing.push(name.to_os_string());
        cursor = cursor
            .parent()
            .ok_or_else(|| format!("could not resolve {}", display_path(path)))?;
    }
    let mut resolved = cursor
        .canonicalize()
        .map_err(|error| format!("could not resolve {}: {error}", display_path(cursor)))?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

pub(crate) fn read_utf8_file(path: &Path, limit: u64) -> Result<String, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", display_path(path)))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", display_path(path)));
    }
    if metadata.len() > limit {
        return Err(format!("{} exceeds the size limit", display_path(path)));
    }
    fs::read_to_string(path)
        .map_err(|error| format!("could not read {} as UTF-8: {error}", display_path(path)))
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", display_path(path)))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", display_path(parent)))?;

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.tmp-{}-{serial}", std::process::id()));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }

    let result = (|| -> Result<(), String> {
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("could not create {}: {error}", display_path(&temporary)))?;
        file.write_all(bytes)
            .map_err(|error| format!("could not write {}: {error}", display_path(&temporary)))?;
        file.sync_all()
            .map_err(|error| format!("could not flush {}: {error}", display_path(&temporary)))?;

        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path)
                .map_err(|error| format!("could not replace {}: {error}", display_path(path)))?;
        }

        fs::rename(&temporary, path).map_err(|error| {
            format!(
                "could not replace {} with temporary file: {error}",
                display_path(path)
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|error| format!("could not secure {}: {error}", display_path(path)))?;
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_narrowly_allowlisted() {
        assert!(validate_key("projects.v2").is_ok());
        assert!(validate_key("../../secrets").is_err());
        assert!(validate_key("").is_err());
    }

    #[test]
    fn project_files_are_allowlisted() {
        let root = if cfg!(windows) {
            "C:\\project"
        } else {
            "/project"
        };
        assert!(validate_project_file_path(&format!("{root}/ro-sync.json")).is_ok());
        assert!(validate_project_file_path(&format!("{root}/wally.toml")).is_ok());
        assert!(validate_project_file_path(&format!("{root}/notes.txt")).is_err());
        assert!(validate_project_file_path("relative/ro-sync.json").is_err());
    }

    #[test]
    fn state_round_trips_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        state_set(&path, "projects", serde_json::json!([{"id": 1}])).unwrap();
        assert_eq!(
            state_get(&path, "projects").unwrap(),
            Some(serde_json::json!([{"id": 1}]))
        );
        assert!(state_set(&path, "secrets", serde_json::json!({})).is_err());
    }

    #[test]
    fn authorization_restricts_paths_to_picked_roots() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let store = directory.path().join("roots.json");
        authorize_project_root(&store, &project).unwrap();
        assert!(ensure_authorized_path(&store, &project.join("ro-sync.json")).is_ok());
        assert!(
            ensure_authorized_path(&store, &directory.path().join("other/wally.toml")).is_err()
        );
    }
}
