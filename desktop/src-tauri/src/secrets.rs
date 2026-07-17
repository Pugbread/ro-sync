use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::storage::{read_json_object, validate_key, write_json_object};

const SERVICE: &str = "com.pugbread.rosync.desktop";
const MAX_SECRET_BYTES: usize = 64 * 1024;

pub(crate) struct SecretStore {
    fallback_path: PathBuf,
    use_native: bool,
}

impl SecretStore {
    pub(crate) fn new(fallback_path: PathBuf) -> Self {
        Self {
            fallback_path,
            use_native: true,
        }
    }

    #[cfg(test)]
    fn file_only(fallback_path: PathBuf) -> Self {
        Self {
            fallback_path,
            use_native: false,
        }
    }

    pub(crate) fn get(&self, key: &str) -> Result<Option<String>, String> {
        validate_key(key)?;
        if self.use_native {
            if let NativeResult::Value(Some(value)) = native_get(key) {
                return Ok(Some(value));
            }
        }
        fallback_get(&self.fallback_path, key)
    }

    pub(crate) fn set(&self, key: &str, value: &str) -> Result<(), String> {
        validate_key(key)?;
        if value.len() > MAX_SECRET_BYTES {
            return Err("secret exceeds the size limit".into());
        }
        if self.use_native && matches!(native_set(key, value), NativeResult::Value(())) {
            return fallback_delete(&self.fallback_path, key);
        }
        fallback_set(&self.fallback_path, key, value)
    }

    pub(crate) fn delete(&self, key: &str) -> Result<(), String> {
        validate_key(key)?;
        if self.use_native {
            let _ = native_delete(key);
        }
        fallback_delete(&self.fallback_path, key)
    }
}

enum NativeResult<T> {
    Value(T),
    Unavailable,
}

#[cfg(feature = "native-keyring")]
fn native_get(key: &str) -> NativeResult<Option<String>> {
    let Ok(entry) = keyring::Entry::new(SERVICE, key) else {
        return NativeResult::Unavailable;
    };
    match entry.get_password() {
        Ok(value) => NativeResult::Value(Some(value)),
        Err(keyring::Error::NoEntry) => NativeResult::Value(None),
        Err(_) => NativeResult::Unavailable,
    }
}

#[cfg(not(feature = "native-keyring"))]
fn native_get(_key: &str) -> NativeResult<Option<String>> {
    NativeResult::Unavailable
}

#[cfg(feature = "native-keyring")]
fn native_set(key: &str, value: &str) -> NativeResult<()> {
    let Ok(entry) = keyring::Entry::new(SERVICE, key) else {
        return NativeResult::Unavailable;
    };
    match entry.set_password(value) {
        Ok(()) => NativeResult::Value(()),
        Err(_) => NativeResult::Unavailable,
    }
}

#[cfg(not(feature = "native-keyring"))]
fn native_set(_key: &str, _value: &str) -> NativeResult<()> {
    NativeResult::Unavailable
}

#[cfg(feature = "native-keyring")]
fn native_delete(key: &str) -> NativeResult<()> {
    let Ok(entry) = keyring::Entry::new(SERVICE, key) else {
        return NativeResult::Unavailable;
    };
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => NativeResult::Value(()),
        Err(_) => NativeResult::Unavailable,
    }
}

#[cfg(not(feature = "native-keyring"))]
fn native_delete(_key: &str) -> NativeResult<()> {
    NativeResult::Unavailable
}

fn fallback_get(path: &Path, key: &str) -> Result<Option<String>, String> {
    let object = read_json_object(path, 4 * 1024 * 1024)?;
    Ok(object
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}

fn fallback_set(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let mut object = read_json_object(path, 4 * 1024 * 1024)?;
    object.insert(key.to_owned(), Value::String(value.to_owned()));
    write_json_object(path, &object, 0o600)
}

fn fallback_delete(path: &Path, key: &str) -> Result<(), String> {
    let mut object = read_json_object(path, 4 * 1024 * 1024)?;
    if object.remove(key).is_some() {
        write_json_object(path, &object, 0o600)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_fallback_round_trips_without_native_keychain() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secrets.json");
        let store = SecretStore::file_only(path.clone());
        store.set("open-cloud", "super-secret").unwrap();
        assert_eq!(
            store.get("open-cloud").unwrap().as_deref(),
            Some("super-secret")
        );
        store.delete("open-cloud").unwrap();
        assert_eq!(store.get("open-cloud").unwrap(), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
