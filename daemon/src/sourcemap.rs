//! Ro-Sync sourcemap generation for luau-lsp.
//!
//! The output intentionally follows the simple Rojo-style JSON shape consumed
//! by luau-lsp: `{ name, className, filePaths?, children? }`.

use crate::fs_map::{is_init_file, path_to_instance_meta};
use crate::snapshot::SYNCED_SERVICES;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
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

/// Enrich a disk-backed luau-lsp sourcemap with the full live Studio tree.
///
/// Disk nodes remain canonical so their `filePaths` are never lost. A disk
/// `Folder` may be a projection of a Studio-owned container such as a `Model`
/// or `Tool`; when the live node matches, its actual class replaces the
/// projected `Folder` class. Live-only nodes are converted from the plugin's
/// `class` shape to luau-lsp's `className` shape and appended in live order.
///
/// The remote `tree` command returns a root object, while the plugin's cached
/// full-tree push is an array of service nodes. Accepting both shapes keeps the
/// merge independent of how callers obtained the live tree.
pub fn merge_live_tree(sourcemap: &mut Value, live_tree: &Value) {
    if let Some(live_children) = live_tree.as_array() {
        merge_children(sourcemap, live_children);
    } else if live_tree.is_object() {
        merge_node(sourcemap, live_tree);
    }
}

fn merge_node(disk_node: &mut Value, live_node: &Value) {
    let Some(live_class) = live_node.get("class").and_then(Value::as_str) else {
        return;
    };
    let Some(disk_object) = disk_node.as_object_mut() else {
        return;
    };

    if disk_object.get("className").and_then(Value::as_str) == Some("Folder") {
        disk_object.insert("className".into(), Value::String(live_class.to_string()));
    }

    let Some(live_children) = live_node.get("children").and_then(Value::as_array) else {
        return;
    };
    merge_children(disk_node, live_children);
}

fn merge_children(disk_parent: &mut Value, live_children: &[Value]) {
    let Some(disk_object) = disk_parent.as_object_mut() else {
        return;
    };
    let disk_children = disk_object
        .entry("children")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(disk_children) = disk_children.as_array_mut() else {
        return;
    };

    // Match exact name+class pairs first. This prevents an earlier projected
    // Folder from taking a same-named script that has its own exact disk node.
    let mut exact_live: HashMap<(String, String), VecDeque<usize>> = HashMap::new();
    let mut named_live: HashMap<String, VecDeque<usize>> = HashMap::new();
    for (index, node) in live_children.iter().enumerate() {
        let Some(name) = node.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(class) = node.get("class").and_then(Value::as_str) else {
            continue;
        };
        exact_live
            .entry((name.to_string(), class.to_string()))
            .or_default()
            .push_back(index);
        named_live
            .entry(name.to_string())
            .or_default()
            .push_back(index);
    }

    let original_disk_len = disk_children.len();
    let mut live_matched = vec![false; live_children.len()];
    let mut matches = vec![None; original_disk_len];

    for (disk_index, node) in disk_children.iter().take(original_disk_len).enumerate() {
        let Some(name) = node.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(class) = node.get("className").and_then(Value::as_str) else {
            continue;
        };
        let key = (name.to_string(), class.to_string());
        if let Some(queue) = exact_live.get_mut(&key) {
            if let Some(live_index) = take_unmatched(queue, &live_matched) {
                live_matched[live_index] = true;
                matches[disk_index] = Some(live_index);
            }
        }
    }

    // Pair any remaining same-named siblings in occurrence order. A vector of
    // indices (rather than name -> node) preserves duplicate instances.
    for (disk_index, node) in disk_children.iter().take(original_disk_len).enumerate() {
        if matches[disk_index].is_some() {
            continue;
        }
        let Some(name) = node.get("name").and_then(Value::as_str) else {
            continue;
        };
        if let Some(queue) = named_live.get_mut(name) {
            if let Some(live_index) = take_unmatched(queue, &live_matched) {
                live_matched[live_index] = true;
                matches[disk_index] = Some(live_index);
            }
        }
    }

    for (disk_index, live_index) in matches.into_iter().enumerate() {
        if let Some(live_index) = live_index {
            merge_node(&mut disk_children[disk_index], &live_children[live_index]);
        }
    }

    for (live_index, live_node) in live_children.iter().enumerate() {
        if !live_matched[live_index] {
            if let Some(node) = live_to_sourcemap(live_node) {
                disk_children.push(node);
            }
        }
    }
}

fn take_unmatched(queue: &mut VecDeque<usize>, matched: &[bool]) -> Option<usize> {
    while let Some(index) = queue.pop_front() {
        if !matched[index] {
            return Some(index);
        }
    }
    None
}

fn live_to_sourcemap(live_node: &Value) -> Option<Value> {
    let name = live_node.get("name")?.as_str()?;
    let class = live_node.get("class")?.as_str()?;
    let mut object = serde_json::Map::new();
    object.insert("name".into(), Value::String(name.to_string()));
    object.insert("className".into(), Value::String(class.to_string()));

    if let Some(live_children) = live_node.get("children").and_then(Value::as_array) {
        let children = live_children
            .iter()
            .filter_map(live_to_sourcemap)
            .collect::<Vec<_>>();
        if !children.is_empty() {
            object.insert("children".into(), Value::Array(children));
        }
    }

    Some(Value::Object(object))
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

    let Some(relative_path) = safe_rojo_relative_path(path) else {
        return Ok(None);
    };

    Ok(Some(dir.join(relative_path)))
}

fn safe_rojo_relative_path(path: &str) -> Option<PathBuf> {
    if path.is_empty() || Path::new(path).is_absolute() || looks_like_windows_rooted_path(path) {
        return None;
    }

    let mut out = PathBuf::new();
    for segment in path.split(['/', '\\']) {
        if segment.is_empty() || segment == ".." {
            return None;
        }
        if segment != "." {
            out.push(segment);
        }
    }
    Some(out)
}

fn looks_like_windows_rooted_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || path.starts_with('\\')
        || path.starts_with("//")
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

    struct TempDir(tempfile::TempDir);
    impl TempDir {
        fn new(tag: &str) -> Self {
            Self(
                tempfile::Builder::new()
                    .prefix(&format!("rosync-sourcemap-{tag}-"))
                    .tempdir()
                    .unwrap(),
            )
        }

        fn path(&self) -> &Path {
            self.0.path()
        }
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

    #[test]
    fn default_project_path_rejects_windows_parent_traversal() {
        let d = TempDir::new("wally-default-project-traversal");
        let package = d.path().join("ReplicatedStorage").join("Packages");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("default.project.json"),
            r#"{"tree":{"$path":"..\\Outside"}}"#,
        )
        .unwrap();

        assert!(default_project_path(&package).unwrap().is_none());
    }

    #[test]
    fn live_tree_replaces_projected_folder_class_and_preserves_file_paths() {
        let mut map = json!({
            "name": "DataModel",
            "className": "DataModel",
            "children": [{
                "name": "ReplicatedStorage",
                "className": "ReplicatedStorage",
                "filePaths": ["ReplicatedStorage"],
                "children": [{
                    "name": "Vehicles",
                    "className": "Folder",
                    "filePaths": ["ReplicatedStorage/Vehicles"],
                    "children": [{
                        "name": "Config",
                        "className": "ModuleScript",
                        "filePaths": ["ReplicatedStorage/Vehicles/Config.luau"]
                    }]
                }]
            }]
        });
        let live = json!({
            "name": "Race Stars",
            "class": "DataModel",
            "children": [{
                "name": "ReplicatedStorage",
                "class": "ReplicatedStorage",
                "children": [{
                    "name": "Vehicles",
                    "class": "Model",
                    "children": [
                        {"name": "Config", "class": "ModuleScript", "children": []},
                        {"name": "Primary", "class": "Part", "children": []}
                    ]
                }]
            }]
        });

        merge_live_tree(&mut map, &live);

        let service = &map["children"][0];
        let vehicles = &service["children"][0];
        assert_eq!(service["filePaths"][0], "ReplicatedStorage");
        assert_eq!(vehicles["className"], "Model");
        assert_eq!(vehicles["filePaths"][0], "ReplicatedStorage/Vehicles");
        assert_eq!(
            vehicles["children"][0]["filePaths"][0],
            "ReplicatedStorage/Vehicles/Config.luau"
        );
        assert_eq!(vehicles["children"][1]["className"], "Part");
    }

    #[test]
    fn live_service_array_adds_studio_only_nested_instances() {
        let mut map = json!({
            "name": "DataModel",
            "className": "DataModel",
            "children": []
        });
        let live_services = json!([{
            "name": "Workspace",
            "class": "Workspace",
            "children": [{
                "name": "Spawn",
                "class": "SpawnLocation",
                "children": [{"name": "Attachment", "class": "Attachment", "children": []}]
            }]
        }]);

        merge_live_tree(&mut map, &live_services);

        assert_eq!(map["children"][0]["className"], "Workspace");
        assert_eq!(
            map["children"][0]["children"][0]["className"],
            "SpawnLocation"
        );
        assert_eq!(
            map["children"][0]["children"][0]["children"][0]["className"],
            "Attachment"
        );
    }

    #[test]
    fn duplicate_names_match_one_to_one_without_collapsing_nodes() {
        let mut map = json!({
            "name": "DataModel",
            "className": "DataModel",
            "children": [{
                "name": "Workspace",
                "className": "Workspace",
                "children": [
                    {"name": "Thing", "className": "Folder", "filePaths": ["Workspace/Thing [1]"]},
                    {"name": "Thing", "className": "ModuleScript", "filePaths": ["Workspace/Thing [2].luau"]}
                ]
            }]
        });
        // Put the exact script match first to prove the projected Folder does
        // not consume it merely because the names are equal.
        let live = json!({
            "name": "Race Stars",
            "class": "DataModel",
            "children": [{
                "name": "Workspace",
                "class": "Workspace",
                "children": [
                    {"name": "Thing", "class": "ModuleScript", "children": []},
                    {"name": "Thing", "class": "Model", "children": []},
                    {"name": "Thing", "class": "Part", "children": []}
                ]
            }]
        });

        merge_live_tree(&mut map, &live);

        let things = map["children"][0]["children"].as_array().unwrap();
        assert_eq!(things.len(), 3);
        assert_eq!(things[0]["className"], "Model");
        assert_eq!(things[0]["filePaths"][0], "Workspace/Thing [1]");
        assert_eq!(things[1]["className"], "ModuleScript");
        assert_eq!(things[1]["filePaths"][0], "Workspace/Thing [2].luau");
        assert_eq!(things[2]["className"], "Part");
    }
}
