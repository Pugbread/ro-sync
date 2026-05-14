use crate::fs_map::normalize_line_endings;
use crate::snapshot::SYNCED_SERVICES;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

const SCRIPT_CLASSES: &[&str] = &["Script", "LocalScript", "ModuleScript"];
const SYNC_CLASSES: &[&str] = &["Folder", "Script", "LocalScript", "ModuleScript"];

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
        collect_snapshot_node(service, "", true, &mut out);
    }
    out
}

pub fn collect_studio_tree_nodes(root: &Value) -> BTreeMap<String, DiffNode> {
    let mut out = BTreeMap::new();
    collect_studio_node(root, "", true, &mut out);
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

fn collect_snapshot_node(
    node: &Value,
    parent: &str,
    is_service: bool,
    out: &mut BTreeMap<String, DiffNode>,
) {
    let Some(name) = node.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let Some(class) = node.get("class").and_then(|v| v.as_str()) else {
        return;
    };
    let path = join_path(parent, name);
    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return;
    }
    if !is_service && is_sync_class(class) {
        out.insert(
            path.clone(),
            DiffNode {
                path: path.clone(),
                class: class.to_string(),
                kind: kind_for_class(class),
                source: source_from_node(node),
            },
        );
    }
    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for child in children {
            collect_snapshot_node(child, &path, false, out);
        }
    }
}

fn collect_studio_node(
    node: &Value,
    parent: &str,
    is_service: bool,
    out: &mut BTreeMap<String, DiffNode>,
) -> bool {
    let Some(name) = node.get("name").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(class) = node.get("class").and_then(|v| v.as_str()) else {
        return false;
    };
    let is_data_model_root = is_service && parent.is_empty() && class == "DataModel";
    let path = if is_data_model_root {
        String::new()
    } else {
        join_path(parent, name)
    };
    if node
        .get("avoidSync")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    let mut has_syncable_child = false;
    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for child in children {
            if is_data_model_root && !is_synced_service_node(child) {
                continue;
            }
            if collect_studio_node(child, &path, is_data_model_root, out) {
                has_syncable_child = true;
            }
        }
    }

    let allowed = is_sync_class(class);
    if !is_service && (allowed || has_syncable_child) {
        let mapped_class = if allowed { class } else { "Folder" };
        out.insert(
            path.clone(),
            DiffNode {
                path,
                class: mapped_class.to_string(),
                kind: kind_for_class(mapped_class),
                source: None,
            },
        );
    }
    allowed || has_syncable_child
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
        assert_eq!(report.summary.removed, 1);
        assert_eq!(report.removed[0].path, "ReplicatedStorage/StudioOnly");
        assert_eq!(report.summary.changed, 1);
        assert_eq!(report.changed[0].path, "ReplicatedStorage/Config");
        assert!(report.changed[0].source_changed);
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
