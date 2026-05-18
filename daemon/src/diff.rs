use crate::fs_map::{
    classify_script_file, instance_to_path, normalize_line_endings, InstanceDescriptor,
};
use crate::snapshot::SYNCED_SERVICES;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

const SCRIPT_CLASSES: &[&str] = &["Script", "LocalScript", "ModuleScript"];
const SYNC_CLASSES: &[&str] = &["Folder", "Script", "LocalScript", "ModuleScript"];
const SUPPRESS_CLASSES: &[&str] = &["Camera", "Terrain", "PlayerScripts", "PackageLink"];

#[derive(Clone, Copy)]
enum TreeFlavor {
    Snapshot,
    Studio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    Folder,
    Script,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffNode {
    pub path: String,
    pub class: String,
    pub kind: DiffKind,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffItem {
    pub path: String,
    pub class: String,
    pub kind: DiffKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedItem {
    pub path: String,
    pub kind: DiffKind,
    #[serde(rename = "localClass")]
    pub local_class: String,
    #[serde(rename = "studioClass")]
    pub studio_class: String,
    #[serde(rename = "classChanged")]
    pub class_changed: bool,
    #[serde(rename = "sourceChanged")]
    pub source_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffSummary {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffReport {
    pub ok: bool,
    pub summary: DiffSummary,
    /// Present locally but missing from Studio's syncable representation.
    pub added: Vec<DiffItem>,
    /// Present in Studio's syncable representation but missing locally.
    pub removed: Vec<DiffItem>,
    pub changed: Vec<ChangedItem>,
}

impl DiffReport {
    pub fn is_clean(&self) -> bool {
        self.summary.added == 0 && self.summary.removed == 0 && self.summary.changed == 0
    }
}

pub fn collect_local_nodes(services: &[Value]) -> BTreeMap<String, DiffNode> {
    let mut out = BTreeMap::new();
    for service in services {
        let Some(name) = service.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(children) = service.get("children").and_then(|v| v.as_array()) {
            collect_snapshot_children(children, name, &mut out);
        }
    }
    out
}

pub fn collect_studio_tree_nodes(root: &Value) -> BTreeMap<String, DiffNode> {
    let mut out = BTreeMap::new();
    let is_data_model_root = root
        .get("class")
        .and_then(|v| v.as_str())
        .is_some_and(|class| class == "DataModel");
    if is_data_model_root {
        if let Some(children) = root.get("children").and_then(|v| v.as_array()) {
            for service in children {
                if !is_synced_service_node(service) {
                    continue;
                }
                let Some(name) = service.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                collect_studio_children(service, name, &mut out);
            }
        }
    } else {
        collect_studio_node(root, "", &mut out);
    }
    out
}

pub fn studio_script_paths(nodes: &BTreeMap<String, DiffNode>) -> Vec<String> {
    nodes
        .values()
        .filter(|node| node.kind == DiffKind::Script)
        .map(|node| node.path.clone())
        .collect()
}

pub fn set_node_source(nodes: &mut BTreeMap<String, DiffNode>, path: &str, source: String) {
    if let Some(node) = nodes.get_mut(path) {
        node.source = Some(source);
    }
}

pub(crate) fn snapshot_sibling_sort_key(node: &Value, class: &str) -> String {
    sibling_sort_key(node, class, TreeFlavor::Snapshot)
}

pub fn has_truncated_tree(node: &Value) -> bool {
    if node.get("truncated").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    node.get("children")
        .and_then(|v| v.as_array())
        .map(|children| children.iter().any(has_truncated_tree))
        .unwrap_or(false)
}

pub fn compare(
    local: &BTreeMap<String, DiffNode>,
    studio: &BTreeMap<String, DiffNode>,
) -> DiffReport {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    for (path, local_node) in local {
        let Some(studio_node) = studio.get(path) else {
            added.push(item_from_node(local_node));
            continue;
        };
        let class_changed = local_node.class != studio_node.class;
        let source_changed = local_node.kind == DiffKind::Script
            && studio_node.kind == DiffKind::Script
            && !sources_equal(local_node.source.as_deref(), studio_node.source.as_deref());
        if class_changed || source_changed {
            changed.push(ChangedItem {
                path: path.clone(),
                kind: local_node.kind,
                local_class: local_node.class.clone(),
                studio_class: studio_node.class.clone(),
                class_changed,
                source_changed,
            });
        }
    }

    for (path, studio_node) in studio {
        if !local.contains_key(path) {
            removed.push(item_from_node(studio_node));
        }
    }

    DiffReport {
        ok: true,
        summary: DiffSummary {
            added: added.len(),
            removed: removed.len(),
            changed: changed.len(),
        },
        added,
        removed,
        changed,
    }
}

fn collect_snapshot_children(
    children: &[Value],
    parent: &str,
    out: &mut BTreeMap<String, DiffNode>,
) {
    let mut taken = Vec::new();
    let mut relevant = Vec::new();
    for child in children {
        let Some(name) = child.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(class) = child.get("class").and_then(|v| v.as_str()) else {
            continue;
        };
        if child
            .get("avoidSync")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if !is_sync_class(class) {
            continue;
        }

        relevant.push((
            sibling_sort_key(child, class, TreeFlavor::Snapshot),
            child,
            name,
            class,
        ));
    }
    relevant.sort_by(|a, b| a.0.cmp(&b.0));

    for (_sort_key, child, name, class) in relevant {
        let has_children = child
            .get("children")
            .and_then(|v| v.as_array())
            .is_some_and(|children| !children.is_empty());
        let fragment = instance_to_path(
            &InstanceDescriptor {
                class,
                name,
                has_children,
            },
            &taken,
        );
        taken.push(fragment.fragment.clone());
        let path = join_path(parent, &diff_segment_for_fragment(&fragment.fragment));
        collect_snapshot_node(child, &path, out);
    }
}

fn collect_snapshot_node(node: &Value, path: &str, out: &mut BTreeMap<String, DiffNode>) {
    let Some(name) = node.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let Some(class) = node.get("class").and_then(|v| v.as_str()) else {
        return;
    };
    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return;
    }
    if is_sync_class(class) {
        out.insert(
            path.to_string(),
            DiffNode {
                path: path.to_string(),
                class: class.to_string(),
                kind: kind_for_class(class),
                source: source_from_node(node),
            },
        );
    }
    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        collect_snapshot_children(children, path, out);
    }
    let _ = name;
}

fn collect_studio_children(
    parent_node: &Value,
    parent_path: &str,
    out: &mut BTreeMap<String, DiffNode>,
) -> bool {
    let mut taken = Vec::new();
    let mut has_syncable_child = false;
    let Some(children) = parent_node.get("children").and_then(|v| v.as_array()) else {
        return false;
    };

    let mut relevant = Vec::new();
    for child in children {
        if !studio_node_is_diff_relevant(child) {
            continue;
        }
        let Some(name) = child.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(class) = child.get("class").and_then(|v| v.as_str()) else {
            continue;
        };
        let mapped_class = mapped_studio_class(child, class);
        relevant.push((
            sibling_sort_key(child, mapped_class, TreeFlavor::Studio),
            child,
            name,
            mapped_class.to_string(),
        ));
    }
    relevant.sort_by(|a, b| a.0.cmp(&b.0));

    for (_sort_key, child, name, mapped_class) in relevant {
        let has_children = child
            .get("children")
            .and_then(|v| v.as_array())
            .is_some_and(|children| !children.is_empty());
        let fragment = instance_to_path(
            &InstanceDescriptor {
                class: &mapped_class,
                name,
                has_children,
            },
            &taken,
        );
        taken.push(fragment.fragment.clone());
        let path = join_path(parent_path, &diff_segment_for_fragment(&fragment.fragment));
        if collect_studio_node(child, &path, out) {
            has_syncable_child = true;
        }
    }

    has_syncable_child
}

fn sibling_sort_key(node: &Value, mapped_class: &str, flavor: TreeFlavor) -> String {
    let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let lower_name = name.to_ascii_lowercase();
    let signature = sync_relevant_signature(node, mapped_class, flavor);
    format!("{lower_name}\u{0}{name}\u{0}{mapped_class}\u{0}{signature}")
}

fn sync_relevant_signature(node: &Value, mapped_class: &str, flavor: TreeFlavor) -> String {
    let mut parts = Vec::new();
    parts.push(mapped_class.to_string());

    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        let mut child_keys = Vec::new();
        for child in children {
            if child
                .get("avoidSync")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(class) = child.get("class").and_then(|v| v.as_str()) else {
                continue;
            };
            let mapped_child_class = match flavor {
                TreeFlavor::Snapshot => {
                    if !is_sync_class(class) {
                        continue;
                    }
                    class
                }
                TreeFlavor::Studio => {
                    if !studio_node_is_diff_relevant(child) {
                        continue;
                    }
                    mapped_studio_class(child, class)
                }
            };
            child_keys.push(sibling_sort_key(child, mapped_child_class, flavor));
        }
        child_keys.sort();
        parts.extend(child_keys);
    }

    parts.join("\u{0}")
}

fn collect_studio_node(node: &Value, path: &str, out: &mut BTreeMap<String, DiffNode>) -> bool {
    let Some(name) = node.get("name").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(class) = node.get("class").and_then(|v| v.as_str()) else {
        return false;
    };
    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    let has_syncable_child = collect_studio_children(node, path, out);

    let is_script = SCRIPT_CLASSES.contains(&class);
    let is_sync_relevant_folder = class == "Folder" && has_syncable_child;
    let is_passthrough_container = !is_sync_class(class) && has_syncable_child;
    if is_script || is_sync_relevant_folder || is_passthrough_container {
        let mapped_class = if is_script || is_sync_relevant_folder {
            class
        } else {
            "Folder"
        };
        out.insert(
            path.to_string(),
            DiffNode {
                path: path.to_string(),
                class: mapped_class.to_string(),
                kind: kind_for_class(mapped_class),
                source: None,
            },
        );
    }
    let _ = name;
    is_script || is_sync_relevant_folder || is_passthrough_container
}

fn studio_node_is_diff_relevant(node: &Value) -> bool {
    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    let Some(class) = node.get("class").and_then(|v| v.as_str()) else {
        return false;
    };
    if SUPPRESS_CLASSES.contains(&class) {
        return false;
    }
    if SCRIPT_CLASSES.contains(&class) {
        return true;
    }
    let has_syncable_child = node
        .get("children")
        .and_then(|v| v.as_array())
        .map(|children| children.iter().any(studio_node_is_diff_relevant))
        .unwrap_or(false);
    (class == "Folder" || !is_sync_class(class)) && has_syncable_child
}

fn mapped_studio_class<'a>(node: &'a Value, class: &'a str) -> &'a str {
    if SCRIPT_CLASSES.contains(&class) || class == "Folder" {
        return class;
    }
    if node
        .get("children")
        .and_then(|v| v.as_array())
        .is_some_and(|children| children.iter().any(studio_node_is_diff_relevant))
    {
        "Folder"
    } else {
        class
    }
}

fn diff_segment_for_fragment(fragment: &str) -> String {
    if let Some((_class, stem)) = classify_script_file(fragment) {
        stem
    } else {
        fragment.to_string()
    }
}

fn item_from_node(node: &DiffNode) -> DiffItem {
    DiffItem {
        path: node.path.clone(),
        class: node.class.clone(),
        kind: node.kind,
    }
}

fn source_from_node(node: &Value) -> Option<String> {
    node.get("properties")
        .and_then(|v| v.get("Source"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn sources_equal(local: Option<&str>, studio: Option<&str>) -> bool {
    let local = normalize_line_endings(local.unwrap_or("").as_bytes());
    let studio = normalize_line_endings(studio.unwrap_or("").as_bytes());
    local.as_ref() == studio.as_ref()
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn is_sync_class(class: &str) -> bool {
    SYNC_CLASSES.contains(&class)
}

fn is_synced_service_node(node: &Value) -> bool {
    node.get("name")
        .and_then(|v| v.as_str())
        .map(|name| SYNCED_SERVICES.contains(&name))
        .unwrap_or(false)
}

fn kind_for_class(class: &str) -> DiffKind {
    if SCRIPT_CLASSES.contains(&class) {
        DiffKind::Script
    } else {
        DiffKind::Folder
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compare_reports_added_removed_and_changed() {
        let local_services = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [
                { "class": "ModuleScript", "name": "Config", "properties": { "Source": "return 1\r\n" }, "children": [] },
                { "class": "Folder", "name": "LocalOnly", "properties": {}, "children": [] }
            ]
        })];
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "ReplicatedStorage", "name": "ReplicatedStorage", "children": [
                    { "class": "ModuleScript", "name": "Config", "children": [] },
                    { "class": "Folder", "name": "StudioOnly", "children": [] }
                ] }
            ]
        });
        let local = collect_local_nodes(&local_services);
        let mut studio = collect_studio_tree_nodes(&studio_tree);
        set_node_source(&mut studio, "ReplicatedStorage/Config", "return 2\n".into());

        let report = compare(&local, &studio);
        assert_eq!(report.summary.added, 1);
        assert_eq!(report.added[0].path, "ReplicatedStorage/LocalOnly");
        assert_eq!(report.summary.removed, 0);
        assert_eq!(report.summary.changed, 1);
        assert_eq!(report.changed[0].path, "ReplicatedStorage/Config");
        assert!(report.changed[0].source_changed);
    }

    #[test]
    fn studio_tree_ignores_folder_without_script_descendants() {
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "ReplicatedStorage", "name": "ReplicatedStorage", "children": [
                    { "class": "Folder", "name": "Assets", "children": [
                        { "class": "Folder", "name": "Models", "children": [] }
                    ] }
                ] }
            ]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);
        assert!(!studio.contains_key("ReplicatedStorage/Assets"));
        assert!(!studio.contains_key("ReplicatedStorage/Assets/Models"));
    }

    #[test]
    fn studio_tree_keeps_folder_ancestors_of_scripts() {
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "ReplicatedStorage", "name": "ReplicatedStorage", "children": [
                    { "class": "Folder", "name": "Shared", "children": [
                        { "class": "ModuleScript", "name": "Config", "children": [] }
                    ] }
                ] }
            ]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);
        assert_eq!(studio["ReplicatedStorage/Shared"].class, "Folder");
        assert_eq!(
            studio["ReplicatedStorage/Shared/Config"].class,
            "ModuleScript"
        );
    }

    #[test]
    fn studio_tree_suppresses_camera_subtrees_even_with_scripts() {
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "ServerStorage", "name": "ServerStorage", "children": [
                    { "class": "Camera", "name": "Photobooth", "children": [
                        { "class": "ModuleScript", "name": "Bindings", "children": [] }
                    ] }
                ] }
            ]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);
        assert!(!studio.contains_key("ServerStorage/Photobooth"));
        assert!(!studio.contains_key("ServerStorage/Photobooth/Bindings"));
    }

    #[test]
    fn case_only_siblings_disambiguate_independent_of_snapshot_order() {
        let local_services = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [{
                "class": "Folder",
                "name": "Packages",
                "properties": {},
                "children": [
                    { "class": "ModuleScript", "name": "Net", "properties": { "Source": "return 'Net'\n" }, "children": [] },
                    { "class": "ModuleScript", "name": "net", "properties": { "Source": "return 'net'\n" }, "children": [] }
                ]
            }]
        })];
        let studio_services = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [{
                "class": "Folder",
                "name": "Packages",
                "properties": {},
                "children": [
                    { "class": "ModuleScript", "name": "net", "properties": { "Source": "return 'net'\n" }, "children": [] },
                    { "class": "ModuleScript", "name": "Net", "properties": { "Source": "return 'Net'\n" }, "children": [] }
                ]
            }]
        })];

        let local = collect_local_nodes(&local_services);
        let studio = collect_local_nodes(&studio_services);

        assert!(local.contains_key("ReplicatedStorage/Packages/Net"));
        assert!(local.contains_key("ReplicatedStorage/Packages/net [1]"));
        assert!(studio.contains_key("ReplicatedStorage/Packages/Net"));
        assert!(studio.contains_key("ReplicatedStorage/Packages/net [1]"));
        assert!(compare(&local, &studio).is_clean());
    }

    #[test]
    fn duplicate_snapshot_siblings_sort_by_sync_relevant_subtree() {
        let local_services = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [
                { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                    { "class": "LocalScript", "name": "Animate", "properties": { "Source": "animate" }, "children": [] }
                ] },
                { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                    { "class": "Folder", "name": "HumanoidRootPart", "properties": {}, "children": [
                        { "class": "LocalScript", "name": "DialogueDemo", "properties": { "Source": "dialogue" }, "children": [] }
                    ] }
                ] }
            ]
        })];
        let studio_services = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [
                { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                    { "class": "Folder", "name": "HumanoidRootPart", "properties": {}, "children": [
                        { "class": "LocalScript", "name": "DialogueDemo", "properties": { "Source": "dialogue" }, "children": [] }
                    ] }
                ] },
                { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                    { "class": "LocalScript", "name": "Animate", "properties": { "Source": "animate" }, "children": [] }
                ] }
            ]
        })];

        let local = collect_local_nodes(&local_services);
        let studio = collect_local_nodes(&studio_services);

        assert!(local.contains_key("Workspace/SellNPC/Animate"));
        assert!(local.contains_key("Workspace/SellNPC [1]/HumanoidRootPart/DialogueDemo"));
        assert!(studio.contains_key("Workspace/SellNPC/Animate"));
        assert!(studio.contains_key("Workspace/SellNPC [1]/HumanoidRootPart/DialogueDemo"));
        assert!(compare(&local, &studio).is_clean());
    }

    #[test]
    fn duplicate_studio_names_use_disk_disambiguation() {
        let local_services = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [
                { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                    { "class": "LocalScript", "name": "Animate", "properties": { "Source": "simple" }, "children": [] }
                ] },
                { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                    { "class": "LocalScript", "name": "Animate", "properties": { "Source": "r15" }, "children": [] }
                ] }
            ]
        })];
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "Workspace", "name": "Workspace", "children": [
                    { "class": "Model", "name": "SellNPC", "children": [
                        { "class": "LocalScript", "name": "Animate", "children": [] }
                    ] },
                    { "class": "Model", "name": "SellNPC", "children": [
                        { "class": "LocalScript", "name": "Animate", "children": [] }
                    ] }
                ] }
            ]
        });

        let local = collect_local_nodes(&local_services);
        let mut studio = collect_studio_tree_nodes(&studio_tree);
        set_node_source(&mut studio, "Workspace/SellNPC/Animate", "simple".into());
        set_node_source(&mut studio, "Workspace/SellNPC [1]/Animate", "r15".into());

        assert!(local.contains_key("Workspace/SellNPC/Animate"));
        assert!(local.contains_key("Workspace/SellNPC [1]/Animate"));
        assert!(studio.contains_key("Workspace/SellNPC/Animate"));
        assert!(studio.contains_key("Workspace/SellNPC [1]/Animate"));
        assert!(compare(&local, &studio).is_clean());
    }

    #[test]
    fn duplicate_studio_names_with_distinct_subtrees_ignore_studio_order() {
        let local_services = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [
                { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                    { "class": "LocalScript", "name": "Animate", "properties": { "Source": "animate" }, "children": [] }
                ] },
                { "class": "Folder", "name": "SellNPC", "properties": {}, "children": [
                    { "class": "Folder", "name": "HumanoidRootPart", "properties": {}, "children": [
                        { "class": "LocalScript", "name": "DialogueDemo", "properties": { "Source": "dialogue" }, "children": [] }
                    ] }
                ] }
            ]
        })];
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "Workspace", "name": "Workspace", "children": [
                    { "class": "Model", "name": "SellNPC", "children": [
                        { "class": "Part", "name": "HumanoidRootPart", "children": [
                            { "class": "LocalScript", "name": "DialogueDemo", "children": [] }
                        ] }
                    ] },
                    { "class": "Model", "name": "SellNPC", "children": [
                        { "class": "LocalScript", "name": "Animate", "children": [] }
                    ] }
                ] }
            ]
        });

        let local = collect_local_nodes(&local_services);
        let mut studio = collect_studio_tree_nodes(&studio_tree);
        set_node_source(&mut studio, "Workspace/SellNPC/Animate", "animate".into());
        set_node_source(
            &mut studio,
            "Workspace/SellNPC [1]/HumanoidRootPart/DialogueDemo",
            "dialogue".into(),
        );

        assert!(studio.contains_key("Workspace/SellNPC/Animate"));
        assert!(studio.contains_key("Workspace/SellNPC [1]/HumanoidRootPart/DialogueDemo"));
        assert!(compare(&local, &studio).is_clean());
    }

    #[test]
    fn crlf_and_lf_sources_compare_equal() {
        let mut local = BTreeMap::new();
        local.insert(
            "ServerScriptService/Main".into(),
            DiffNode {
                path: "ServerScriptService/Main".into(),
                class: "Script".into(),
                kind: DiffKind::Script,
                source: Some("print(1)\r\n".into()),
            },
        );
        let mut studio = BTreeMap::new();
        studio.insert(
            "ServerScriptService/Main".into(),
            DiffNode {
                path: "ServerScriptService/Main".into(),
                class: "Script".into(),
                kind: DiffKind::Script,
                source: Some("print(1)\n".into()),
            },
        );

        assert!(compare(&local, &studio).is_clean());
    }

    #[test]
    fn studio_non_sync_container_with_script_descendant_maps_to_folder() {
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "Workspace", "name": "Workspace", "children": [
                    { "class": "Part", "name": "ModelRoot", "children": [
                        { "class": "Script", "name": "Runner", "children": [] }
                    ] }
                ] }
            ]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);
        assert_eq!(studio["Workspace/ModelRoot"].class, "Folder");
        assert_eq!(studio["Workspace/ModelRoot"].kind, DiffKind::Folder);
        assert_eq!(studio["Workspace/ModelRoot/Runner"].class, "Script");
    }

    #[test]
    fn studio_tree_ignores_unsynced_top_level_services() {
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "CoreGui", "name": "CoreGui", "children": [
                    { "class": "Folder", "name": "PluginNoise", "children": [] }
                ] },
                { "class": "ReplicatedStorage", "name": "ReplicatedStorage", "children": [
                    { "class": "ModuleScript", "name": "Config", "children": [] }
                ] }
            ]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);
        assert!(!studio.contains_key("CoreGui/PluginNoise"));
        assert_eq!(studio["ReplicatedStorage/Config"].class, "ModuleScript");
    }

    #[test]
    fn studio_tree_ignores_avoid_sync_subtrees() {
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "Workspace", "name": "Workspace", "children": [
                    { "class": "Folder", "name": "Ignored", "avoidSync": true, "children": [
                        { "class": "Script", "name": "Runner", "children": [] }
                    ] },
                    { "class": "Folder", "name": "Included", "children": [
                        { "class": "Script", "name": "Runner", "children": [] }
                    ] }
                ] }
            ]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);
        assert!(!studio.contains_key("Workspace/Ignored"));
        assert!(!studio.contains_key("Workspace/Ignored/Runner"));
        assert_eq!(studio["Workspace/Included/Runner"].class, "Script");
    }

    #[test]
    fn detects_truncated_tree_anywhere() {
        let tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "Workspace", "name": "Workspace", "truncated": true, "children": [] }
            ]
        });
        assert!(has_truncated_tree(&tree));
    }
}
