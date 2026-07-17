use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

pub const RUNTIME_RECORD_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeRecord {
    pub version: u32,
    pub project: String,
    #[serde(rename = "canonicalProject")]
    pub canonical_project: String,
    pub pid: u32,
    pub port: u16,
    #[serde(rename = "bootId")]
    pub boot_id: String,
    #[serde(rename = "controlToken")]
    pub control_token: String,
    #[serde(rename = "managedBy")]
    pub managed_by: String,
    #[serde(rename = "logPath")]
    pub log_path: String,
    #[serde(rename = "startedAt")]
    pub started_at: u64,
}

#[derive(Clone, Debug)]
pub struct RuntimePaths {
    pub record: PathBuf,
    pub start_lock: PathBuf,
    pub log: PathBuf,
}

pub fn canonical_project(project: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(project)?;
    if !canonical.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("project is not a directory: {}", project.display()),
        ));
    }
    Ok(canonical)
}

pub fn state_dir(explicit: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(explicit) = explicit {
        return absolute_path(explicit);
    }
    if let Some(value) = std::env::var_os("ROSYNC_STATE_DIR") {
        if !value.is_empty() {
            return absolute_path(Path::new(&value));
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
            if !value.is_empty() {
                return Ok(PathBuf::from(value).join("rosync"));
            }
        }
        if let Some(home) = dirs::home_dir() {
            return Ok(home.join(".local").join("state").join("rosync"));
        }
    }

    #[cfg(not(target_os = "linux"))]
    if let Some(base) = dirs::data_local_dir() {
        return Ok(base.join("RoSync"));
    }

    dirs::home_dir()
        .map(|home| home.join(".rosync"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))
}

pub fn legacy_widget_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".terminal64").join("widgets").join("ro-sync"))
}

pub fn credentials_path(state_dir: &Path) -> PathBuf {
    state_dir.join("secrets.json")
}

pub fn read_credential(path: &Path, key: &str) -> io::Result<Option<String>> {
    let store = read_credential_store(path)?;
    Ok(store.get(key).cloned())
}

pub fn write_credential(path: &Path, key: &str, value: &str) -> io::Result<()> {
    if key.is_empty() || value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential key and value must not be empty",
        ));
    }
    let mut store = read_credential_store(path)?;
    store.insert(key.to_string(), value.to_string());
    let bytes = serde_json::to_vec_pretty(&store)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid credential store path")
    })?;
    create_private_dir(parent)?;
    write_private_atomic(path, &bytes)
}

pub fn remove_credential(path: &Path, key: &str) -> io::Result<bool> {
    let mut store = read_credential_store(path)?;
    if store.remove(key).is_none() {
        return Ok(false);
    }
    if store.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        }
    }
    let bytes = serde_json::to_vec_pretty(&store)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_private_atomic(path, &bytes)?;
    Ok(true)
}

fn read_credential_store(path: &Path) -> io::Result<BTreeMap<String, String>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    const MAX_CREDENTIAL_STORE_BYTES: u64 = 4 * 1024 * 1024;
    if metadata.len() > MAX_CREDENTIAL_STORE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential store exceeds the 4 MiB limit",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "credential store {} is accessible by other users; require mode 0600",
                    path.display()
                ),
            ));
        }
    }
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn runtime_paths(state_dir: PathBuf, canonical_project: &Path) -> RuntimePaths {
    let key = project_key(canonical_project);
    let daemon_dir = state_dir.join("daemons");
    RuntimePaths {
        record: daemon_dir.join(format!("{key}.json")),
        start_lock: daemon_dir.join(format!("{key}.start.lock")),
        log: daemon_dir.join(format!("{key}.log")),
    }
}

pub fn project_key(canonical_project: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(canonical_project.to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())
}

pub fn read_record(path: &Path) -> io::Result<Option<RuntimeRecord>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record = serde_json::from_str::<RuntimeRecord>(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if record.version != RUNTIME_RECORD_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported daemon runtime record version {}; expected {}",
                record.version, RUNTIME_RECORD_VERSION
            ),
        ));
    }
    Ok(Some(record))
}

pub fn write_record(path: &Path, record: &RuntimeRecord) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid runtime record path")
    })?;
    create_private_dir(parent)?;
    let text = serde_json::to_vec_pretty(record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_private_atomic(path, &text)
}

pub fn remove_record_if_boot(path: &Path, boot_id: &str) -> io::Result<bool> {
    let Some(record) = read_record(path)? else {
        return Ok(false);
    };
    if record.boot_id != boot_id {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn open_private_log(path: &Path) -> io::Result<fs::File> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid daemon log path"))?;
    create_private_dir(parent)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

#[derive(Debug)]
pub struct StartLock {
    // Keep the file handle alive for the full critical section. The operating
    // system releases the lock if this process exits, including after a crash.
    // The lock file itself is intentionally persistent: unlinking it on drop
    // would let another process create and lock a new inode while a waiter
    // still holds the old one.
    _file: fs::File,
}

impl StartLock {
    pub fn acquire(path: &Path) -> io::Result<Self> {
        Self::acquire_named(path, "daemon start")
    }

    pub fn acquire_named(path: &Path, activity: &str) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid daemon start-lock path",
            )
        })?;
        create_private_dir(parent)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "another {activity} is already in progress (lock {})",
                    path.display()
                ),
            ),
            fs::TryLockError::Error(error) => io::Error::new(
                error.kind(),
                format!("lock {activity} file {}: {error}", path.display()),
            ),
        })?;

        // Contents are diagnostic only; exclusivity comes from the held OS
        // lock. Rewriting them after acquisition also makes a stale file left
        // by an older/crashed manager harmless.
        file.set_len(0)?;
        writeln!(file, "pid={}", std::process::id())?;
        writeln!(
            file,
            "acquiredAt={}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        )?;
        file.sync_all()?;
        Ok(Self { _file: file })
    }
}

pub struct RuntimeRecordGuard {
    path: PathBuf,
    boot_id: String,
}

impl RuntimeRecordGuard {
    pub fn new(path: PathBuf, boot_id: String) -> Self {
        Self { path, boot_id }
    }
}

impl Drop for RuntimeRecordGuard {
    fn drop(&mut self) {
        let _ = remove_record_if_boot(&self.path, &self.boot_id);
    }
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_private_atomic_impl(path, bytes, true)
}

/// Atomically writes private binary data without the newline terminator used
/// by Ro Sync's human-readable JSON state records.
pub(crate) fn write_private_atomic_exact(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_private_atomic_impl(path, bytes, false)
}

fn write_private_atomic_impl(path: &Path, bytes: &[u8], terminate_line: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid state path"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid state filename"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".{file_name}.{}-{nonce}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        if terminate_line {
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        replace_private_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_private_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_private_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x00000001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x00000008;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, destination: *const u16, flags: u32) -> i32;
    }

    let existing = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_is_stable_and_path_specific() {
        let first = project_key(Path::new("/tmp/one"));
        let repeated = project_key(Path::new("/tmp/one"));
        let second = project_key(Path::new("/tmp/two"));
        assert_eq!(first, repeated);
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn runtime_record_round_trips_and_guard_respects_boot_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let record_path = temporary.path().join("daemon.json");
        let record = RuntimeRecord {
            version: RUNTIME_RECORD_VERSION,
            project: "/tmp/project".into(),
            canonical_project: "/tmp/project".into(),
            pid: 42,
            port: 49_123,
            boot_id: "boot-a".into(),
            control_token: "token".into(),
            managed_by: "test".into(),
            log_path: "/tmp/daemon.log".into(),
            started_at: 123,
        };
        write_record(&record_path, &record).unwrap();
        assert_eq!(read_record(&record_path).unwrap(), Some(record.clone()));
        assert!(!remove_record_if_boot(&record_path, "boot-b").unwrap());
        assert!(record_path.exists());
        assert!(remove_record_if_boot(&record_path, "boot-a").unwrap());
        assert!(!record_path.exists());
    }

    #[test]
    fn start_lock_is_exclusive_and_recoverable_after_release() {
        let temporary = tempfile::tempdir().unwrap();
        let lock_path = temporary.path().join("daemon.start.lock");
        let first = StartLock::acquire(&lock_path).unwrap();
        assert_eq!(
            StartLock::acquire(&lock_path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(first);
        StartLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
    }

    #[test]
    fn start_lock_recovers_from_a_stale_unlocked_file() {
        let temporary = tempfile::tempdir().unwrap();
        let lock_path = temporary.path().join("daemon.start.lock");
        fs::write(&lock_path, "pid=999999\nacquiredAt=1\n").unwrap();

        let lock = StartLock::acquire(&lock_path).unwrap();
        let contents = fs::read_to_string(&lock_path).unwrap();
        assert!(contents.contains(&format!("pid={}", std::process::id())));
        drop(lock);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_records_are_private_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let record_path = temporary.path().join("private").join("daemon.json");
        let record = RuntimeRecord {
            version: RUNTIME_RECORD_VERSION,
            project: "p".into(),
            canonical_project: "p".into(),
            pid: 1,
            port: 1,
            boot_id: "b".into(),
            control_token: "secret".into(),
            managed_by: "test".into(),
            log_path: "log".into(),
            started_at: 1,
        };
        write_record(&record_path, &record).unwrap();
        assert_eq!(
            fs::metadata(&record_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(record_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
