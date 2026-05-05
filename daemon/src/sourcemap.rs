//! Ro-Sync sourcemap generation for luau-lsp.
//!
//! The output intentionally follows the simple Rojo-style JSON shape consumed
//! by luau-lsp: `{ name, className, filePaths?, children? }`.

use crate::fs_map::{is_init_file, path_to_instance_meta};
use crate::snapshot::SYNCED_SERVICES;
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const ROJO_PROJECT_FILE: &str = "default.project.json";

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
        if is_init_file(name) {
            continue;
        }
        if let Some(node) = build_node(project, &path)? {
            out.push(node);
        }
    }

    Ok(out)
}

fn build_node(project: &Path, path: &Path) -> io::Result<Option<Value>> {
    if path.is_dir() {
        if let Some(target) = default_project_path(path)? {
            if target.exists() {
                let name = path_to_instance_meta(path)?
                    .map(|inst| inst.name)
                    .or_else(|| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.to_string())
                    });
                return build_node_at(project, &target, name);
            }
        }
    }

    build_node_at(project, path, None)
}

fn build_node_at(
    project: &Path,
    path: &Path,
    name_override: Option<String>,
) -> io::Result<Option<Value>> {
    let Some(inst) = path_to_instance_meta(path)? else {
        return Ok(None);
    };

    let mut obj = serde_json::Map::new();
    obj.insert(
        "name".into(),
        Value::String(name_override.unwrap_or(inst.name)),
    );
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

fn default_project_path(dir: &Path) -> io::Result<Option<PathBuf>> {
    let project_file = dir.join(ROJO_PROJECT_FILE);
    if !project_file.is_file() {
        return Ok(None);
    }

    let text = fs::read_to_string(project_file)?;
    let value: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let Some(path) = value
        .get("tree")
        .and_then(|tree| tree.get("$path"))
        .and_then(|path| path.as_str())
    else {
        return Ok(None);
    };

    if path.is_empty() || Path::new(path).is_absolute() || path.split('/').any(|seg| seg == "..") {
        return Ok(None);
    }

    Ok(Some(dir.join(path)))
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
        if is_init_file(name) {
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

    #[test]
    fn wally_plain_init_folder_uses_init_file_path() {
        let d = TempDir::new("wally-init");
        let net = d
            .path()
            .join("ReplicatedStorage")
            .join("Packages")
            .join("_Index")
            .join("sleitnick_net@0.2.0")
            .join("net");
        fs::create_dir_all(&net).unwrap();
        fs::write(net.join("init.lua"), "return {}").unwrap();

        let map = generate(d.path()).unwrap();
        let net_node =
            &map["children"][0]["children"][0]["children"][0]["children"][0]["children"][0];
        assert_eq!(net_node["name"], "net");
        assert_eq!(net_node["className"], "ModuleScript");
        assert_eq!(
            net_node["filePaths"][0],
            "ReplicatedStorage/Packages/_Index/sleitnick_net@0.2.0/net/init.lua"
        );
    }

    #[test]
    fn wally_default_project_path_resolves_package_root() {
        let d = TempDir::new("wally-default-project");
        let promise = d
            .path()
            .join("ReplicatedStorage")
            .join("Packages")
            .join("_Index")
            .join("evaera_promise@4.0.0")
            .join("promise");
        let lib = promise.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            promise.join("default.project.json"),
            br#"{"name":"promise","tree":{"$path":"lib"}}"#,
        )
        .unwrap();
        fs::write(lib.join("init.lua"), "return {}").unwrap();
        fs::write(lib.join("Error.lua"), "return {}").unwrap();

        let map = generate(d.path()).unwrap();
        let promise_node =
            &map["children"][0]["children"][0]["children"][0]["children"][0]["children"][0];
        assert_eq!(promise_node["name"], "promise");
        assert_eq!(promise_node["className"], "ModuleScript");
        assert_eq!(
            promise_node["filePaths"][0],
            "ReplicatedStorage/Packages/_Index/evaera_promise@4.0.0/promise/lib/init.lua"
        );
        assert_eq!(promise_node["children"][0]["name"], "Error");
    }
}
