//! Ro-Sync sourcemap generation for luau-lsp.
//!
//! The output intentionally follows the simple Rojo-style JSON shape consumed
//! by luau-lsp: `{ name, className, filePaths?, children? }`.

use crate::fs_map::{parse_init_file, path_to_instance_meta};
use crate::snapshot::SYNCED_SERVICES;
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub fn generate(project: &Path) -> io::Result<Value> {
    let mut children = Vec::new();
    for service in SYNCED_SERVICES {
        let service_dir = project.join(service);
        if !service_dir.is_dir() {
            continue;
        }
        children.push(json!({
            "name": service,
            "className": service,
            "filePaths": [rel_path(project, &service_dir)],
            "children": walk_children(project, &service_dir)?,
        }));
    }

    Ok(json!({
        "name": "DataModel",
        "className": "DataModel",
        "children": children,
    }))
}

fn walk_children(project: &Path, dir: &Path) -> io::Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if parse_init_file(name).is_some() {
            continue;
        }
        if let Some(node) = build_node(project, &path)? {
            out.push(node);
        }
    }

    Ok(out)
}

fn build_node(project: &Path, path: &Path) -> io::Result<Option<Value>> {
    let Some(inst) = path_to_instance_meta(path)? else {
        return Ok(None);
    };

    let mut obj = serde_json::Map::new();
    obj.insert("name".into(), Value::String(inst.name));
    obj.insert("className".into(), Value::String(inst.class));

    if let Some(source_path) = source_path_for(path, inst.is_script_with_children) {
        obj.insert(
            "filePaths".into(),
            Value::Array(vec![Value::String(rel_path(project, &source_path))]),
        );
    } else if path.is_dir() {
        obj.insert(
            "filePaths".into(),
            Value::Array(vec![Value::String(rel_path(project, path))]),
        );
    }

    if inst.is_dir {
        obj.insert(
            "children".into(),
            Value::Array(walk_children(project, path)?),
        );
    }

    Ok(Some(Value::Object(obj)))
}

fn source_path_for(path: &Path, is_script_with_children: bool) -> Option<PathBuf> {
    if !is_script_with_children {
        return path.is_file().then(|| path.to_path_buf());
    }

    let entries = fs::read_dir(path).ok()?;
    for entry in entries.flatten() {
        let child = entry.path();
        let Some(name) = child.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if parse_init_file(name).is_some() {
            return Some(child);
        }
    }
    None
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "rosync-sourcemap-{tag}-{}-{}",
                std::process::id(),
                unix_nanos()
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
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

    fn unix_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    #[test]
    fn generates_script_file_paths() {
        let d = TempDir::new("script");
        let rs = d.path().join("ReplicatedStorage");
        fs::create_dir_all(&rs).unwrap();
        fs::write(rs.join("Config.luau"), "return {}").unwrap();

        let map = generate(d.path()).unwrap();
        let config = &map["children"][0]["children"][0];
        assert_eq!(config["name"], "Config");
        assert_eq!(config["className"], "ModuleScript");
        assert_eq!(config["filePaths"][0], "ReplicatedStorage/Config.luau");
    }

    #[test]
    fn script_with_children_uses_init_file_path() {
        let d = TempDir::new("init");
        let net = d.path().join("ReplicatedStorage").join("Net");
        fs::create_dir_all(&net).unwrap();
        fs::write(net.join("init (Net).luau"), "return {}").unwrap();
        fs::write(net.join("Client.client.luau"), "print('client')").unwrap();

        let map = generate(d.path()).unwrap();
        let net_node = &map["children"][0]["children"][0];
        assert_eq!(net_node["className"], "ModuleScript");
        assert_eq!(
            net_node["filePaths"][0],
            "ReplicatedStorage/Net/init (Net).luau"
        );
        assert_eq!(net_node["children"][0]["className"], "LocalScript");
    }
}
