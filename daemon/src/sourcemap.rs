//! Ro-Sync sourcemap generation for luau-lsp.
//!
//! The output intentionally follows the simple Rojo-style JSON shape consumed
//! by luau-lsp: `{ name, className, filePaths?, children? }`.

use crate::fs_map::{path_is_parent_init_source, path_to_instance_meta};
use crate::fs_safety::{
    file_generation_no_follow, metadata_no_follow, read_to_string_no_follow,
    resolve_rojo_path_no_follow, validate_rojo_project_directory, validate_service_path,
    PortableDirectoryIndex, SafeEntryKind,
};
use crate::snapshot::SYNCED_SERVICES;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

const ROJO_PROJECT_FILE: &str = "default.project.json";
const MAX_SOURCEMAP_DEPTH: usize = 256;

pub fn generate(project: &Path) -> io::Result<Value> {
    let mut children = Vec::new();
    for service in SYNCED_SERVICES {
        let _validated_service = validate_service_path(project, service, true)?;
        let service_dir = project.join(service);
        if metadata_no_follow(&service_dir)?.is_none() {
            continue;
        }
        validate_rojo_project_directory(&service_dir)?;
        children.push(json!({
            "name": service,
            "className": service,
            "filePaths": [rel_path(project, &service_dir)],
            "children": walk_children(project, &service_dir, 1)?,
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

fn walk_children(project: &Path, dir: &Path, depth: usize) -> io::Result<Vec<Value>> {
    if depth > MAX_SOURCEMAP_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "sourcemap tree exceeds maximum depth {MAX_SOURCEMAP_DEPTH} at {}",
                dir.display()
            ),
        ));
    }
    let mut out = Vec::new();
    let index = PortableDirectoryIndex::read(dir)?;
    let parent_source = index.unique_init_source().map(|entry| entry.path.as_path());
    for entry in index.entries() {
        if parent_source == Some(entry.path.as_path()) {
            continue;
        }
        if let Some(node) = build_node(project, &entry.path, depth)? {
            out.push(node);
        }
    }

    Ok(out)
}

fn build_node(project: &Path, path: &Path, depth: usize) -> io::Result<Option<Value>> {
    let Some(metadata) = metadata_no_follow(path)? else {
        return Ok(None);
    };
    if metadata.is_dir() {
        if let Some(target) = default_project_path(path)? {
            let target_is_own_init =
                target.parent() == Some(path) && path_is_parent_init_source(&target)?;
            if !target_is_own_init && metadata_no_follow(&target)?.is_some() {
                let name = path_to_instance_meta(path)?
                    .map(|inst| inst.name)
                    .or_else(|| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.to_string())
                    });
                return build_node_at(project, &target, name, depth);
            }
        }
    }

    build_node_at(project, path, None, depth)
}

fn build_node_at(
    project: &Path,
    path: &Path,
    name_override: Option<String>,
    depth: usize,
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

    if let Some(source_path) = source_path_for(path, inst.is_script_with_children)? {
        obj.insert(
            "filePaths".into(),
            Value::Array(vec![Value::String(rel_path(project, &source_path))]),
        );
    } else if metadata_no_follow(path)?.is_some_and(|metadata| metadata.is_dir()) {
        obj.insert(
            "filePaths".into(),
            Value::Array(vec![Value::String(rel_path(project, path))]),
        );
    }

    if inst.is_dir {
        obj.insert(
            "children".into(),
            Value::Array(walk_children(project, path, depth + 1)?),
        );
    }

    Ok(Some(Value::Object(obj)))
}

fn default_project_path(dir: &Path) -> io::Result<Option<PathBuf>> {
    let index = PortableDirectoryIndex::read(dir)?;
    let Some(project_file) = index.exact(ROJO_PROJECT_FILE) else {
        return Ok(None);
    };
    if project_file.kind != SafeEntryKind::File {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Rojo project marker is not a regular file: {}",
                project_file.path.display()
            ),
        ));
    }

    file_generation_no_follow(&project_file.path).map_err(io::Error::other)?;
    let text = read_to_string_no_follow(&project_file.path)?;
    let value: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let Some(path) = value
        .get("tree")
        .and_then(|tree| tree.get("$path"))
        .and_then(|path| path.as_str())
    else {
        return Ok(None);
    };

    Ok(Some(resolve_rojo_path_no_follow(dir, path, true)?))
}

fn source_path_for(path: &Path, is_script_with_children: bool) -> io::Result<Option<PathBuf>> {
    if !is_script_with_children {
        let Some(metadata) = metadata_no_follow(path)? else {
            return Ok(None);
        };
        if !metadata.is_file() {
            return Ok(None);
        }
        file_generation_no_follow(path).map_err(io::Error::other)?;
        return Ok(Some(path.to_path_buf()));
    }

    let index = PortableDirectoryIndex::read(path)?;
    let source = index.unique_init_source().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "script directory has no unique init source: {}",
                path.display()
            ),
        )
    })?;
    file_generation_no_follow(&source.path).map_err(io::Error::other)?;
    Ok(Some(source.path.clone()))
}

fn rel_path(root: &Path, path: &Path) -> String {
    let mut out = String::new();
    for component in path.strip_prefix(root).unwrap_or(path).components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
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
    fn script_with_children_keeps_mismatched_named_init_leaf() {
        let d = TempDir::new("mismatched-init-leaf");
        let misc = d.path().join("ReplicatedStorage").join("Misc");
        fs::create_dir_all(&misc).unwrap();
        fs::write(misc.join("init (Misc).luau"), "return 'parent'").unwrap();
        fs::write(
            misc.join("init (Notifications).luau"),
            "return 'literal child'",
        )
        .unwrap();

        let map = generate(d.path()).unwrap();
        let misc_node = &map["children"][0]["children"][0];
        assert_eq!(misc_node["name"], "Misc");
        assert_eq!(
            misc_node["filePaths"][0],
            "ReplicatedStorage/Misc/init (Misc).luau"
        );
        let children = misc_node["children"].as_array().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["name"], "init (Notifications)");
        assert_eq!(children[0]["className"], "ModuleScript");
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

        let error = default_project_path(&package).unwrap_err();
        assert!(error.to_string().contains("unsafe Rojo $path"));
    }

    #[cfg(unix)]
    #[test]
    fn linked_init_source_is_an_error_not_a_silently_missing_file_path() {
        use std::os::unix::fs::symlink;
        let d = TempDir::new("linked-init");
        let package = d.path().join("ReplicatedStorage").join("Package");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(&package).unwrap();
        fs::write(outside.path().join("sentinel"), "keep").unwrap();
        symlink(outside.path().join("sentinel"), package.join("init.lua")).unwrap();

        let error = generate(d.path()).unwrap_err();
        assert!(error.to_string().contains("linked/reparse"));
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
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
