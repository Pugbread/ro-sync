//! `rosync query` — read-only DataModel selector evaluator over `tree.json`.
//!
//! Selector grammar (`/`-separated):
//!   * literal segment — exact name match
//!   * `*`             — exactly one segment, any name
//!   * `**`            — zero or more segments
//!
//! The root of the path space is the synthetic `DataModel`. The first selector
//! segment matches against top-level service names (`Workspace`,
//! `ReplicatedStorage`, …).

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub path: Vec<String>,
    pub class: String,
    pub name: String,
    pub children_count: usize,
}

/// Match `selector` against `tree`. `tree` may be either an array of service
/// nodes (the shape the Studio plugin writes to `tree.json`) or an object with
/// a `children` field (a `DataModel`-rooted node).
pub fn query(tree: &Value, selector: &str) -> Vec<Match> {
    let pattern: Vec<&str> = if selector.is_empty() {
        Vec::new()
    } else {
        selector.split('/').collect()
    };

    let mut out = Vec::new();
    for svc in service_nodes(tree) {
        walk(svc, &mut Vec::new(), &pattern, &mut out);
    }
    out
}

fn service_nodes(tree: &Value) -> Vec<&Value> {
    match tree {
        Value::Array(arr) => arr.iter().collect(),
        Value::Object(_) => {
            if let Some(arr) = tree.get("children").and_then(|c| c.as_array()) {
                arr.iter().collect()
            } else {
                vec![tree]
            }
        }
        _ => Vec::new(),
    }
}

fn walk(node: &Value, path: &mut Vec<String>, pattern: &[&str], out: &mut Vec<Match>) {
    let Some(name) = node.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let class = node
        .get("class")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    path.push(name.to_string());

    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    if selector_matches(&path_refs, pattern) {
        let children_count = node
            .get("children")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        out.push(Match {
            path: path.clone(),
            class,
            name: name.to_string(),
            children_count,
        });
    }

    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for child in children {
            walk(child, path, pattern, out);
        }
    }
    path.pop();
}

/// Test whether a concrete `path` (sequence of instance names) matches
/// `pattern`. Public so the CLI and tests share one definition.
pub fn selector_matches(path: &[&str], pattern: &[&str]) -> bool {
    match (path.first(), pattern.first()) {
        (None, None) => true,
        (None, Some(_)) => pattern.iter().all(|p| *p == "**"),
        (Some(_), None) => false,
        (Some(p_seg), Some(pat_seg)) => {
            if *pat_seg == "**" {
                // `**` matches zero or more segments.
                // Try zero (advance pattern only) then one (consume a segment).
                if selector_matches(path, &pattern[1..]) {
                    return true;
                }
                selector_matches(&path[1..], pattern)
            } else if *pat_seg == "*" {
                selector_matches(&path[1..], &pattern[1..])
            } else {
                p_seg == pat_seg && selector_matches(&path[1..], &pattern[1..])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn literal_matches_exact_path() {
        let p = ["Workspace", "Camera"];
        assert!(selector_matches(&p, &["Workspace", "Camera"]));
        assert!(!selector_matches(&p, &["Workspace", "Other"]));
        assert!(!selector_matches(&p, &["Workspace"]));
        assert!(!selector_matches(&p, &["Workspace", "Camera", "Lens"]));
    }

    #[test]
    fn star_matches_one_segment() {
        let p = ["Workspace", "Stuff"];
        assert!(selector_matches(&p, &["Workspace", "*"]));
        assert!(selector_matches(&p, &["*", "*"]));
        assert!(!selector_matches(&p, &["*"]));
        assert!(!selector_matches(&p, &["*", "*", "*"]));
    }

    #[test]
    fn double_star_matches_zero_or_more() {
        // Zero segments.
        assert!(selector_matches(&["Workspace"], &["Workspace", "**"]));
        // One.
        assert!(selector_matches(
            &["Workspace", "Camera"],
            &["Workspace", "**"]
        ));
        // Many.
        assert!(selector_matches(
            &["Workspace", "A", "B", "C"],
            &["Workspace", "**"]
        ));
        // `**` at root: matches everything non-empty (and empty too).
        assert!(selector_matches(&["X", "Y"], &["**"]));
        assert!(selector_matches(&[], &["**"]));
    }

    #[test]
    fn double_star_in_the_middle() {
        let pat = ["Workspace", "**", "Camera"];
        assert!(selector_matches(&["Workspace", "Camera"], &pat));
        assert!(selector_matches(&["Workspace", "A", "Camera"], &pat));
        assert!(selector_matches(&["Workspace", "A", "B", "Camera"], &pat));
        assert!(!selector_matches(&["Workspace", "A", "B"], &pat));
        assert!(!selector_matches(&["Other", "Camera"], &pat));
    }

    #[test]
    fn mixed_star_and_literal() {
        let pat = ["**", "RemoteEvent"];
        assert!(selector_matches(&["RemoteEvent"], &pat));
        assert!(selector_matches(&["A", "RemoteEvent"], &pat));
        assert!(selector_matches(&["A", "B", "RemoteEvent"], &pat));
        assert!(!selector_matches(&["RemoteEvent", "Trailing"], &pat));
    }

    #[test]
    fn query_finds_nested_match() {
        let tree = json!([
            {
                "class": "Workspace",
                "name": "Workspace",
                "children": [
                    { "class": "Folder", "name": "Mid", "children": [
                        { "class": "Camera", "name": "Camera", "children": [] }
                    ]}
                ]
            }
        ]);
        let hits = query(&tree, "Workspace/**/Camera");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, vec!["Workspace", "Mid", "Camera"]);
        assert_eq!(hits[0].class, "Camera");
        assert_eq!(hits[0].children_count, 0);
    }

    #[test]
    fn query_collects_all_with_double_star() {
        let tree = json!([
            { "class": "Workspace", "name": "Workspace", "children": [
                { "class": "RemoteEvent", "name": "RemoteEvent", "children": [] }
            ]},
            { "class": "ReplicatedStorage", "name": "ReplicatedStorage", "children": [
                { "class": "Folder", "name": "Net", "children": [
                    { "class": "RemoteEvent", "name": "RemoteEvent", "children": [] }
                ]}
            ]}
        ]);
        let hits = query(&tree, "**/RemoteEvent");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn query_handles_object_root() {
        let tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [
                { "class": "Workspace", "name": "Workspace", "children": [
                    { "class": "Folder", "name": "F", "children": [] }
                ]}
            ]
        });
        let hits = query(&tree, "Workspace/F");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "F");
    }
}
