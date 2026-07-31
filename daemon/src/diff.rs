use crate::conflict::{hash, Hash};
use crate::fs_map::{normalize_line_endings, InstanceDescriptor, PathFragmentAllocator};
use crate::snapshot::SYNCED_SERVICES;
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

const SCRIPT_CLASSES: &[&str] = &["Script", "LocalScript", "ModuleScript"];
const SUPPRESS_CLASSES: &[&str] = &["Camera", "Terrain", "PlayerScripts", "PackageLink"];

fn is_path_reservation_node(node: &Value) -> bool {
    node.get("avoidSync").and_then(Value::as_bool) == Some(true)
        || node.get("avoidSyncCarrier").and_then(Value::as_bool) == Some(true)
}

#[derive(Clone, Copy)]
enum TreeFlavor {
    Snapshot,
    Studio,
}

struct CachedTreeNode {
    diff_relevant: bool,
    mapped_class: Option<String>,
    sibling_sort_signature: Option<CachedSortSignature>,
    /// Content is deliberately only a final tiebreaker for structurally
    /// indistinguishable script siblings. This mirrors the Studio allocator:
    /// ordinary nodes keep the stable structural ordering, while duplicate
    /// leaves no longer depend on GetChildren()/directory enumeration order.
    source_sort_hash: Option<Hash>,
}

/// A compact rope for the legacy recursive sibling key.
///
/// Storing each fully expanded subtree key makes a depth-N chain consume
/// O(N²) bytes. Keeping only this node's names and its already-sorted child
/// IDs stores every tree edge once; `SortByteIter` replays the exact legacy
/// byte sequence when two siblings need comparison.
struct CachedSortSignature {
    lower_name: String,
    name: String,
    child_ids: Vec<usize>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CacheStats {
    nodes_prepared: usize,
    relevance_computations: usize,
    mapped_class_computations: usize,
    sort_signature_computations: usize,
    sort_signature_child_links: usize,
}

struct ComparisonCache {
    flavor: TreeFlavor,
    nodes: HashMap<usize, CachedTreeNode>,
    #[cfg(test)]
    stats: CacheStats,
}

#[derive(Clone, Copy)]
enum SortStringField {
    LowerName,
    Name,
    MappedClass,
}

enum SortFrame {
    Node(usize),
    Zero,
    Bytes {
        node_id: usize,
        field: SortStringField,
        index: usize,
    },
}

struct SortByteIter<'a> {
    cache: &'a ComparisonCache,
    frames: Vec<SortFrame>,
}

impl<'a> SortByteIter<'a> {
    fn new(cache: &'a ComparisonCache, node_id: usize) -> Self {
        Self {
            cache,
            frames: vec![SortFrame::Node(node_id)],
        }
    }
}

impl Iterator for SortByteIter<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.frames.pop()? {
                SortFrame::Bytes {
                    node_id,
                    field,
                    index,
                } => {
                    let bytes = self.cache.sort_field_bytes(node_id, field);
                    if let Some(byte) = bytes.get(index).copied() {
                        self.frames.push(SortFrame::Bytes {
                            node_id,
                            field,
                            index: index + 1,
                        });
                        return Some(byte);
                    }
                }
                SortFrame::Zero => return Some(0),
                SortFrame::Node(node_id) => {
                    let signature = self
                        .cache
                        .cached_by_id(node_id)
                        .sibling_sort_signature
                        .as_ref()
                        .expect("sort iterators only contain diff-relevant nodes");
                    for child_id in signature.child_ids.iter().rev().copied() {
                        self.frames.push(SortFrame::Node(child_id));
                        self.frames.push(SortFrame::Zero);
                    }
                    self.frames.push(SortFrame::Bytes {
                        node_id,
                        field: SortStringField::MappedClass,
                        index: 0,
                    });
                    self.frames.push(SortFrame::Zero);
                    self.frames.push(SortFrame::Bytes {
                        node_id,
                        field: SortStringField::MappedClass,
                        index: 0,
                    });
                    self.frames.push(SortFrame::Zero);
                    self.frames.push(SortFrame::Bytes {
                        node_id,
                        field: SortStringField::Name,
                        index: 0,
                    });
                    self.frames.push(SortFrame::Zero);
                    self.frames.push(SortFrame::Bytes {
                        node_id,
                        field: SortStringField::LowerName,
                        index: 0,
                    });
                }
            }
        }
    }
}

impl ComparisonCache {
    fn new(flavor: TreeFlavor) -> Self {
        Self {
            flavor,
            nodes: HashMap::new(),
            #[cfg(test)]
            stats: CacheStats::default(),
        }
    }

    fn prepare_children(&mut self, node: &Value) {
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            self.prepare_slice(children);
        }
    }

    fn prepare_slice(&mut self, children: &[Value]) {
        for child in children {
            self.prepare(child);
        }
    }

    fn prepare(&mut self, root: &Value) {
        let mut pending = vec![(root, false)];
        while let Some((node, children_prepared)) = pending.pop() {
            let node_id = Self::node_id(node);
            if self.nodes.contains_key(&node_id) {
                continue;
            }

            if children_prepared {
                self.finish_prepare(node);
                continue;
            }

            pending.push((node, true));
            if self.should_descend(node) {
                if let Some(children) = node.get("children").and_then(Value::as_array) {
                    pending.extend(children.iter().rev().map(|child| (child, false)));
                }
            }
        }
    }

    fn should_descend(&self, node: &Value) -> bool {
        let avoid_sync = node
            .get("avoidSync")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let class = node.get("class").and_then(Value::as_str);
        !avoid_sync
            && class.is_some_and(|class| match self.flavor {
                TreeFlavor::Snapshot => is_sync_class(class),
                TreeFlavor::Studio => !SUPPRESS_CLASSES.contains(&class),
            })
    }

    fn finish_prepare(&mut self, node: &Value) {
        let node_id = Self::node_id(node);

        #[cfg(test)]
        {
            self.stats.nodes_prepared += 1;
            self.stats.relevance_computations += 1;
        }

        let avoid_sync = node
            .get("avoidSync")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let class = node.get("class").and_then(Value::as_str);
        let should_descend = self.should_descend(node);

        let has_relevant_child = should_descend
            && node
                .get("children")
                .and_then(Value::as_array)
                .is_some_and(|children| children.iter().any(|child| self.is_diff_relevant(child)));

        let diff_relevant = !avoid_sync
            && class.is_some_and(|class| match self.flavor {
                TreeFlavor::Snapshot => {
                    is_sync_class(class)
                        && node.get("avoidSyncCarrier").and_then(Value::as_bool) != Some(true)
                }
                TreeFlavor::Studio => {
                    !SUPPRESS_CLASSES.contains(&class)
                        && (SCRIPT_CLASSES.contains(&class)
                            || (class == "Folder" || !is_sync_class(class)) && has_relevant_child)
                }
            });

        #[cfg(test)]
        if class.is_some() {
            self.stats.mapped_class_computations += 1;
        }

        let mapped_class = class.map(|class| match self.flavor {
            TreeFlavor::Snapshot => class.to_string(),
            TreeFlavor::Studio => {
                if SCRIPT_CLASSES.contains(&class) || class == "Folder" {
                    class.to_string()
                } else if has_relevant_child {
                    "Folder".to_string()
                } else {
                    class.to_string()
                }
            }
        });

        let sibling_sort_signature = if diff_relevant || is_path_reservation_node(node) {
            #[cfg(test)]
            {
                self.stats.sort_signature_computations += 1;
            }
            let signature = self.build_sibling_sort_signature(node);
            #[cfg(test)]
            {
                self.stats.sort_signature_child_links += signature.child_ids.len();
            }
            Some(signature)
        } else {
            None
        };
        let source_sort_hash = mapped_class
            .as_deref()
            .filter(|class| SCRIPT_CLASSES.contains(class))
            .map(|_| source_hash_from_node(node));

        self.nodes.insert(
            node_id,
            CachedTreeNode {
                diff_relevant,
                mapped_class,
                sibling_sort_signature,
                source_sort_hash,
            },
        );
    }

    fn build_sibling_sort_signature(&self, node: &Value) -> CachedSortSignature {
        let name = node.get("name").and_then(Value::as_str).unwrap_or("");
        let lower_name = name.to_ascii_lowercase();
        let mut child_ids = Vec::new();

        if self.should_descend(node) {
            if let Some(children) = node.get("children").and_then(Value::as_array) {
                for child in children {
                    let cached = self.cached(child);
                    if cached.diff_relevant || is_path_reservation_node(child) {
                        child_ids.push(Self::node_id(child));
                    }
                }
            }
        }

        child_ids.sort_by(|left, right| self.compare_node_ids(*left, *right));
        CachedSortSignature {
            lower_name,
            name: name.to_string(),
            child_ids,
        }
    }

    fn compare_nodes(&self, left: &Value, right: &Value) -> Ordering {
        self.compare_node_ids(Self::node_id(left), Self::node_id(right))
    }

    fn sorted_projection_indices(&self, children: &[Value]) -> Vec<usize> {
        let mut indices = children
            .iter()
            .enumerate()
            .filter(|(_, child)| {
                (self.is_diff_relevant(child) || is_path_reservation_node(child))
                    && child.get("name").and_then(Value::as_str).is_some()
                    && child.get("class").and_then(Value::as_str).is_some()
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        indices.sort_by(|left, right| {
            let left_reserved = is_path_reservation_node(&children[*left]);
            let right_reserved = is_path_reservation_node(&children[*right]);
            right_reserved
                .cmp(&left_reserved)
                .then_with(|| self.compare_nodes(&children[*left], &children[*right]))
        });
        indices
    }

    fn compare_node_ids(&self, left: usize, right: usize) -> Ordering {
        if left == right {
            return Ordering::Equal;
        }

        let mut left_bytes = SortByteIter::new(self, left);
        let mut right_bytes = SortByteIter::new(self, right);
        let structural_order = loop {
            match (left_bytes.next(), right_bytes.next()) {
                (Some(left), Some(right)) => match left.cmp(&right) {
                    Ordering::Equal => {}
                    ordering => break ordering,
                },
                (None, Some(_)) => break Ordering::Less,
                (Some(_), None) => break Ordering::Greater,
                (None, None) => break Ordering::Equal,
            }
        };
        if structural_order != Ordering::Equal {
            return structural_order;
        }

        match (
            self.cached_by_id(left).source_sort_hash.as_ref(),
            self.cached_by_id(right).source_sort_hash.as_ref(),
        ) {
            (Some(left), Some(right)) => left.cmp(right),
            _ => Ordering::Equal,
        }
    }

    fn is_diff_relevant(&self, node: &Value) -> bool {
        self.cached(node).diff_relevant
    }

    fn mapped_class<'a>(&'a self, node: &Value) -> Option<&'a str> {
        self.cached(node).mapped_class.as_deref()
    }

    fn cached(&self, node: &Value) -> &CachedTreeNode {
        self.nodes
            .get(&Self::node_id(node))
            .expect("comparison nodes are prepared before traversal")
    }

    fn cached_by_id(&self, node_id: usize) -> &CachedTreeNode {
        self.nodes
            .get(&node_id)
            .expect("sort signatures only reference prepared nodes")
    }

    fn sort_field_bytes(&self, node_id: usize, field: SortStringField) -> &[u8] {
        let cached = self.cached_by_id(node_id);
        match field {
            SortStringField::LowerName => cached
                .sibling_sort_signature
                .as_ref()
                .expect("sort fields only exist for diff-relevant nodes")
                .lower_name
                .as_bytes(),
            SortStringField::Name => cached
                .sibling_sort_signature
                .as_ref()
                .expect("sort fields only exist for diff-relevant nodes")
                .name
                .as_bytes(),
            SortStringField::MappedClass => cached
                .mapped_class
                .as_deref()
                .expect("sort fields only exist for mapped nodes")
                .as_bytes(),
        }
    }

    fn node_id(node: &Value) -> usize {
        node as *const Value as usize
    }

    #[cfg(test)]
    fn stats(&self) -> CacheStats {
        self.stats
    }
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
    pub source_path: String,
    pub class: String,
    pub kind: DiffKind,
    pub source_hash: Option<Hash>,
    pub stream_id: Option<u64>,
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
    let mut cache = ComparisonCache::new(TreeFlavor::Snapshot);
    collect_local_nodes_with_cache(services, &mut cache)
}

fn collect_local_nodes_with_cache(
    services: &[Value],
    cache: &mut ComparisonCache,
) -> BTreeMap<String, DiffNode> {
    let mut out = BTreeMap::new();
    for service in services {
        let Some(name) = service.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(children) = service.get("children").and_then(|v| v.as_array()) {
            cache.prepare_slice(children);
            collect_snapshot_children(children, name, &mut out, cache);
        }
    }
    out
}

#[cfg(test)]
fn collect_local_nodes_with_stats(services: &[Value]) -> (BTreeMap<String, DiffNode>, CacheStats) {
    let mut cache = ComparisonCache::new(TreeFlavor::Snapshot);
    let nodes = collect_local_nodes_with_cache(services, &mut cache);
    (nodes, cache.stats())
}

/// One borrowed filesystem snapshot node plus its exact on-disk ancestry.
///
/// Selective initial sync needs both identities: `logical_path` keys match the
/// generated paths presented in the divergence UI, while `disk_path` remains
/// unambiguous when siblings share a Roblox name or a literal name resembles
/// the generated `Name [N]` grammar. Borrowing the node avoids cloning every
/// descendant subtree into the index (quadratic memory for a deep tree).
pub struct LocalSnapshotValue<'a> {
    pub node: &'a Value,
    pub disk_path: Vec<String>,
}

/// Index the emitted filesystem snapshot by the exact logical path keys used
/// by the initial diff while retaining each node's physical path.
pub fn collect_local_snapshot_entries(
    services: &[Value],
) -> BTreeMap<String, LocalSnapshotValue<'_>> {
    let mut out = BTreeMap::new();
    let mut cache = ComparisonCache::new(TreeFlavor::Snapshot);
    for service in services {
        let Some(name) = service.get("name").and_then(Value::as_str) else {
            continue;
        };
        if let Some(children) = service.get("children").and_then(Value::as_array) {
            cache.prepare_slice(children);
            collect_snapshot_value_children(children, name, &[name.to_string()], &mut out, &cache);
        }
    }
    out
}

pub fn collect_studio_tree_nodes(root: &Value) -> BTreeMap<String, DiffNode> {
    let mut cache = ComparisonCache::new(TreeFlavor::Studio);
    collect_studio_tree_nodes_with_cache(root, &mut cache)
}

fn collect_studio_tree_nodes_with_cache(
    root: &Value,
    cache: &mut ComparisonCache,
) -> BTreeMap<String, DiffNode> {
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
                cache.prepare(service);
                let entries = studio_child_entries(service, name, name, cache);
                collect_studio_entries(entries, &mut out, cache);
            }
        }
    } else {
        // A non-DataModel root historically traverses its children even when
        // the root class itself would be suppressed as a child.
        cache.prepare_children(root);
        cache.prepare(root);
        collect_studio_entries(vec![(root, String::new(), String::new())], &mut out, cache);
    }
    out
}

#[cfg(test)]
fn collect_studio_tree_nodes_with_stats(root: &Value) -> (BTreeMap<String, DiffNode>, CacheStats) {
    let mut cache = ComparisonCache::new(TreeFlavor::Studio);
    let nodes = collect_studio_tree_nodes_with_cache(root, &mut cache);
    (nodes, cache.stats())
}

pub fn studio_script_paths(nodes: &BTreeMap<String, DiffNode>) -> Vec<(String, String)> {
    nodes
        .values()
        .filter(|node| node.kind == DiffKind::Script)
        .map(|node| (node.path.clone(), node.source_path.clone()))
        .collect()
}

pub fn set_node_source(nodes: &mut BTreeMap<String, DiffNode>, path: &str, source: String) {
    if let Some(node) = nodes.get_mut(path) {
        node.source_hash = Some(normalized_source_hash(&source));
    }
}

/// Drop a node the live tree advertised but which no longer exists.
///
/// The comparison enumerates Studio first and reads each Source afterwards, so a
/// runtime-spawned instance can vanish in between. Such a node must leave the
/// Studio side entirely rather than linger without a hash, which would otherwise
/// read as a spurious content difference against the file on disk.
pub fn remove_studio_node(nodes: &mut BTreeMap<String, DiffNode>, path: &str) {
    nodes.remove(path);
}

pub(crate) fn snapshot_sibling_order(children: &[Value]) -> Vec<usize> {
    let mut cache = ComparisonCache::new(TreeFlavor::Snapshot);
    cache.prepare_slice(children);
    cache.sorted_projection_indices(children)
}

pub fn has_truncated_tree(node: &Value) -> bool {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if current.get("truncated").and_then(Value::as_bool) == Some(true) {
            return true;
        }
        if let Some(children) = current.get("children").and_then(Value::as_array) {
            pending.extend(children);
        }
    }
    false
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
            && local_node.source_hash != studio_node.source_hash;
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
    cache: &ComparisonCache,
) {
    let mut pending = snapshot_child_entries(children, parent, cache);
    pending.reverse();
    while let Some((node, path)) = pending.pop() {
        let Some(class) = node.get("class").and_then(Value::as_str) else {
            continue;
        };
        if cache.is_diff_relevant(node) {
            out.insert(
                path.clone(),
                DiffNode {
                    path: path.clone(),
                    source_path: path.clone(),
                    class: class.to_string(),
                    kind: kind_for_class(class),
                    source_hash: SCRIPT_CLASSES
                        .contains(&class)
                        .then(|| source_hash_from_node(node)),
                    stream_id: node.get("streamId").and_then(Value::as_u64),
                },
            );
        }
        if cache.should_descend(node) {
            if let Some(grandchildren) = node.get("children").and_then(Value::as_array) {
                let entries = snapshot_child_entries(grandchildren, &path, cache);
                pending.extend(entries.into_iter().rev());
            }
        }
    }
}

fn snapshot_child_entries<'a>(
    children: &'a [Value],
    parent: &str,
    cache: &ComparisonCache,
) -> Vec<(&'a Value, String)> {
    let mut allocator = PathFragmentAllocator::new();
    let mut entries = Vec::new();
    for child_index in cache.sorted_projection_indices(children) {
        let child = &children[child_index];
        let name = child
            .get("name")
            .and_then(Value::as_str)
            .expect("sorted snapshot nodes have names");
        let segment = allocate_logical_segment(&mut allocator, name);
        let path = join_path(parent, &segment);
        entries.push((child, path));
    }
    entries
}

fn collect_snapshot_value_children<'a>(
    children: &'a [Value],
    parent: &str,
    parent_disk_path: &[String],
    out: &mut BTreeMap<String, LocalSnapshotValue<'a>>,
    cache: &ComparisonCache,
) {
    let mut pending = snapshot_child_entries(children, parent, cache)
        .into_iter()
        .map(|(node, path)| {
            let mut disk_path = parent_disk_path.to_vec();
            disk_path.push(snapshot_disk_fragment(node));
            (node, path, disk_path)
        })
        .collect::<Vec<_>>();
    pending.reverse();
    while let Some((node, path, disk_path)) = pending.pop() {
        out.insert(
            path.clone(),
            LocalSnapshotValue {
                node,
                disk_path: disk_path.clone(),
            },
        );
        if cache.should_descend(node) {
            if let Some(grandchildren) = node.get("children").and_then(Value::as_array) {
                let entries = snapshot_child_entries(grandchildren, &path, cache);
                pending.extend(entries.into_iter().rev().map(|(child, child_path)| {
                    let mut child_disk_path = disk_path.clone();
                    child_disk_path.push(snapshot_disk_fragment(child));
                    (child, child_path, child_disk_path)
                }));
            }
        }
    }
}

fn snapshot_disk_fragment(node: &Value) -> String {
    node.get("diskFragment")
        .and_then(Value::as_str)
        .or_else(|| node.get("name").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

fn studio_child_entries<'a>(
    parent_node: &'a Value,
    parent_path: &str,
    parent_source_path: &str,
    cache: &ComparisonCache,
) -> Vec<(&'a Value, String, String)> {
    let mut allocator = PathFragmentAllocator::new();
    let Some(children) = parent_node.get("children").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut seen_names = HashSet::new();
    let mut duplicate_names = HashSet::new();
    for name in children
        .iter()
        .filter_map(|child| child.get("name").and_then(Value::as_str))
    {
        if !seen_names.insert(name) {
            duplicate_names.insert(name);
        }
    }

    let mut entries = Vec::new();
    for child_index in cache.sorted_projection_indices(children) {
        let child = &children[child_index];
        let name = child
            .get("name")
            .and_then(Value::as_str)
            .expect("sorted Studio nodes have names");
        let segment = allocate_logical_segment(&mut allocator, name);
        let path = join_path(parent_path, &segment);
        let source_segment = if duplicate_names.contains(name) {
            segment
        } else {
            name.to_string()
        };
        let source_path = join_path(parent_source_path, &source_segment);
        entries.push((child, path, source_path));
    }
    entries
}

fn collect_studio_entries(
    entries: Vec<(&Value, String, String)>,
    out: &mut BTreeMap<String, DiffNode>,
    cache: &ComparisonCache,
) {
    let mut pending = entries;
    pending.reverse();
    while let Some((node, path, source_path)) = pending.pop() {
        if node.get("avoidSync").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(class) = node.get("class").and_then(Value::as_str) else {
            continue;
        };
        if cache.is_diff_relevant(node) {
            let mapped_class = cache
                .mapped_class(node)
                .expect("diff-relevant Studio nodes have mapped classes");
            out.insert(
                path.clone(),
                DiffNode {
                    path: path.clone(),
                    source_path: source_path.clone(),
                    class: mapped_class.to_string(),
                    kind: kind_for_class(mapped_class),
                    source_hash: SCRIPT_CLASSES
                        .contains(&class)
                        .then(|| source_hash_from_node(node)),
                    stream_id: node.get("streamId").and_then(Value::as_u64),
                },
            );
        }
        let children = studio_child_entries(node, &path, &source_path, cache);
        pending.extend(children.into_iter().rev());
    }
}

/// Allocate one portable, shape-independent logical sibling segment.
///
/// Filesystem fragments may differ only because of class or representation
/// (`Foo.luau`, `Foo.server.luau`, and `Foo/`). Generated comparison paths
/// cannot strip those suffixes independently without collapsing all three
/// into the same map key. Treat every projected sibling as a directory-shaped
/// name for logical allocation so duplicate Roblox names receive one stable
/// `Name`, `Name [1]`, ... sequence in the already-deterministic sibling order.
/// AvoidSync markers pass through the same allocator before being filtered,
/// preserving their ordinal reservations.
fn allocate_logical_segment(allocator: &mut PathFragmentAllocator, name: &str) -> String {
    allocator
        .allocate(&InstanceDescriptor {
            class: "Folder",
            name,
            has_children: true,
        })
        .fragment
}

fn item_from_node(node: &DiffNode) -> DiffItem {
    DiffItem {
        path: node.path.clone(),
        class: node.class.clone(),
        kind: node.kind,
    }
}

fn source_hash_from_node(node: &Value) -> Hash {
    node.get("properties")
        .and_then(|v| v.get("Source"))
        .and_then(|v| v.as_str())
        .map(normalized_source_hash)
        .unwrap_or_else(|| hash(b""))
}

fn normalized_source_hash(source: &str) -> Hash {
    hash(normalize_line_endings(source.as_bytes()).as_ref())
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn is_sync_class(class: &str) -> bool {
    crate::sync_scope::contains(class)
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

    fn reference_snapshot_sort_key(node: &Value, mapped_class: &str) -> String {
        let name = node.get("name").and_then(Value::as_str).unwrap_or("");
        let lower_name = name.to_ascii_lowercase();
        let mut parts = vec![mapped_class.to_string()];

        if let Some(children) = node.get("children").and_then(Value::as_array) {
            let mut child_keys = children
                .iter()
                .filter(|child| {
                    !child
                        .get("avoidSync")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .filter_map(|child| {
                    let class = child.get("class").and_then(Value::as_str)?;
                    is_sync_class(class).then(|| reference_snapshot_sort_key(child, class))
                })
                .collect::<Vec<_>>();
            child_keys.sort();
            parts.extend(child_keys);
        }

        let signature = parts.join("\u{0}");
        format!("{lower_name}\u{0}{name}\u{0}{mapped_class}\u{0}{signature}")
    }

    #[test]
    fn compact_snapshot_order_matches_recursive_reference_order() {
        let children = vec![
            json!({
                "class": "Folder",
                "name": "Same",
                "children": [{
                    "class": "ModuleScript",
                    "name": "Node",
                    "children": []
                }, {
                    "class": "ModuleScript",
                    "name": "Tail",
                    "children": []
                }]
            }),
            json!({
                "class": "Folder",
                "name": "same",
                "children": [{
                    "class": "ModuleScript",
                    "name": "node",
                    "children": []
                }]
            }),
            json!({
                "class": "Folder",
                "name": "Same",
                "children": [{
                    "class": "ModuleScript",
                    "name": "Node",
                    "children": [{
                        "class": "ModuleScript",
                        "name": "Branch",
                        "children": []
                    }]
                }]
            }),
            json!({
                "class": "Folder",
                "name": "Same",
                "children": [{
                    "class": "ModuleScript",
                    "name": "Node",
                    "children": []
                }]
            }),
        ];

        let mut expected = (0..children.len()).collect::<Vec<_>>();
        expected.sort_by(|left, right| {
            let left_class = children[*left]
                .get("class")
                .and_then(Value::as_str)
                .unwrap();
            let right_class = children[*right]
                .get("class")
                .and_then(Value::as_str)
                .unwrap();
            reference_snapshot_sort_key(&children[*left], left_class)
                .cmp(&reference_snapshot_sort_key(&children[*right], right_class))
        });

        assert_eq!(snapshot_sibling_order(&children), expected);
    }

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
                    { "class": "Camera", "name": "PreviewCamera", "children": [
                        { "class": "ModuleScript", "name": "Bindings", "children": [] }
                    ] }
                ] }
            ]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);
        assert!(!studio.contains_key("ServerStorage/PreviewCamera"));
        assert!(!studio.contains_key("ServerStorage/PreviewCamera/Bindings"));
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
    fn studio_tree_case_only_collision_keeps_real_source_path() {
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [{
                "class": "ReplicatedStorage",
                "name": "ReplicatedStorage",
                "children": [{
                    "class": "Folder",
                    "name": "Packages",
                    "children": [
                        { "class": "ModuleScript", "name": "net", "children": [] },
                        { "class": "ModuleScript", "name": "Net", "children": [] }
                    ]
                }]
            }]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);

        assert!(studio.contains_key("ReplicatedStorage/Packages/Net"));
        assert!(studio.contains_key("ReplicatedStorage/Packages/net [1]"));
        assert_eq!(
            studio["ReplicatedStorage/Packages/net [1]"].source_path,
            "ReplicatedStorage/Packages/net"
        );
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
        assert_eq!(
            studio["Workspace/SellNPC [1]/HumanoidRootPart/DialogueDemo"].source_path,
            "Workspace/SellNPC [1]/HumanoidRootPart/DialogueDemo"
        );
        assert!(compare(&local, &studio).is_clean());
    }

    #[test]
    fn selective_snapshot_index_retains_exact_physical_ancestry() {
        let services = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [{
                "class": "Folder",
                "name": "Parent",
                "diskFragment": "Parent [7]",
                "diskFragmentIsDir": true,
                "properties": {},
                "children": [{
                    "class": "ModuleScript",
                    "name": "Thing [1]",
                    "diskFragment": "Thing %5B1%5D [3].luau",
                    "diskFragmentIsDir": false,
                    "properties": { "Source": "return true" },
                    "children": []
                }]
            }]
        })];

        let entries = collect_local_snapshot_entries(&services);
        let parent = entries
            .get("ReplicatedStorage/Parent")
            .expect("logical parent");
        assert_eq!(parent.disk_path, ["ReplicatedStorage", "Parent [7]"]);

        let child = entries
            .get("ReplicatedStorage/Parent/Thing %5B1%5D")
            .expect("logical child");
        assert_eq!(
            child.disk_path,
            ["ReplicatedStorage", "Parent [7]", "Thing %5B1%5D [3].luau"]
        );
        assert_eq!(child.node["name"], "Thing [1]");
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
    fn indistinguishable_duplicate_scripts_pair_by_source_not_enumeration_order() {
        let local_services = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [{
                "class": "ModuleScript",
                "name": "Same",
                "properties": { "Source": "return 'alpha'\r\n" },
                "children": []
            }, {
                "class": "ModuleScript",
                "name": "Same",
                "properties": { "Source": "return 'beta'\n" },
                "children": []
            }]
        })];
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [{
                "class": "ReplicatedStorage",
                "name": "ReplicatedStorage",
                "children": [{
                    "class": "ModuleScript",
                    "name": "Same",
                    "properties": { "Source": "return 'beta'\r\n" },
                    "children": []
                }, {
                    "class": "ModuleScript",
                    "name": "Same",
                    "properties": { "Source": "return 'alpha'\n" },
                    "children": []
                }]
            }]
        });

        let local = collect_local_nodes(&local_services);
        let studio = collect_studio_tree_nodes(&studio_tree);

        assert_eq!(local.len(), 2);
        assert_eq!(studio.len(), 2);
        assert!(local.contains_key("ReplicatedStorage/Same"));
        assert!(local.contains_key("ReplicatedStorage/Same [1]"));
        assert!(compare(&local, &studio).is_clean());
    }

    #[test]
    fn cross_class_duplicate_scripts_keep_both_source_differences() {
        let local_services = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": [{
                "class": "ModuleScript",
                "name": "Shared",
                "properties": { "Source": "return 'disk module'" },
                "children": []
            }, {
                "class": "Script",
                "name": "Shared",
                "properties": { "Source": "return 'same server'" },
                "children": []
            }]
        })];
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [{
                "class": "Workspace",
                "name": "Workspace",
                "children": [{
                    "class": "ModuleScript",
                    "name": "Shared",
                    "properties": { "Source": "return 'studio module'" },
                    "children": []
                }, {
                    "class": "Script",
                    "name": "Shared",
                    "properties": { "Source": "return 'same server'" },
                    "children": []
                }]
            }]
        });

        let local = collect_local_nodes(&local_services);
        let studio = collect_studio_tree_nodes(&studio_tree);

        assert_eq!(
            local.keys().cloned().collect::<Vec<_>>(),
            vec!["Workspace/Shared", "Workspace/Shared [1]"]
        );
        assert_eq!(
            studio.keys().cloned().collect::<Vec<_>>(),
            vec!["Workspace/Shared", "Workspace/Shared [1]"]
        );
        assert_eq!(studio["Workspace/Shared"].source_path, "Workspace/Shared");
        assert_eq!(
            studio["Workspace/Shared [1]"].source_path,
            "Workspace/Shared [1]"
        );

        let report = compare(&local, &studio);
        assert_eq!(report.summary.changed, 1);
        assert_eq!(report.changed[0].path, "Workspace/Shared");
        assert!(report.changed[0].source_changed);
    }

    #[test]
    fn leaf_and_directory_script_shapes_do_not_collapse() {
        let local_services = vec![json!({
            "class": "ReplicatedStorage",
            "name": "ReplicatedStorage",
            "properties": {},
            "children": [{
                "class": "ModuleScript",
                "name": "Package",
                "properties": { "Source": "return 'same leaf'" },
                "children": []
            }, {
                "class": "ModuleScript",
                "name": "Package",
                "properties": { "Source": "return 'disk package'" },
                "children": [{
                    "class": "ModuleScript",
                    "name": "Child",
                    "properties": { "Source": "return true" },
                    "children": []
                }]
            }]
        })];
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [{
                "class": "ReplicatedStorage",
                "name": "ReplicatedStorage",
                "children": [{
                    "class": "ModuleScript",
                    "name": "Package",
                    "properties": { "Source": "return 'same leaf'" },
                    "children": []
                }, {
                    "class": "ModuleScript",
                    "name": "Package",
                    "properties": { "Source": "return 'studio package'" },
                    "children": [{
                        "class": "ModuleScript",
                        "name": "Child",
                        "properties": { "Source": "return true" },
                        "children": []
                    }]
                }]
            }]
        });

        let local = collect_local_nodes(&local_services);
        let studio = collect_studio_tree_nodes(&studio_tree);
        assert_eq!(local.len(), 3);
        assert_eq!(studio.len(), 3);
        assert!(local.contains_key("ReplicatedStorage/Package"));
        assert!(local.contains_key("ReplicatedStorage/Package [1]"));
        assert!(local.contains_key("ReplicatedStorage/Package [1]/Child"));

        let report = compare(&local, &studio);
        assert_eq!(report.summary.changed, 1);
        assert_eq!(report.changed[0].path, "ReplicatedStorage/Package [1]");
        assert!(report.changed[0].source_changed);
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
    fn wide_duplicate_trees_prepare_each_sort_signature_once() {
        const WIDTH: usize = 1_024;

        let local_children = (0..WIDTH)
            .map(|index| {
                json!({
                    "class": "Folder",
                    "name": "Container",
                    "properties": {},
                    "children": [{
                        "class": "ModuleScript",
                        "name": format!("Script{index:04}"),
                        "properties": {},
                        "children": []
                    }]
                })
            })
            .collect::<Vec<_>>();
        let studio_children = (0..WIDTH)
            .rev()
            .map(|index| {
                json!({
                    "class": "Model",
                    "name": "Container",
                    "properties": {},
                    "children": [{
                        "class": "ModuleScript",
                        "name": format!("Script{index:04}"),
                        "properties": {},
                        "children": []
                    }]
                })
            })
            .collect::<Vec<_>>();

        let local_services = vec![json!({
            "class": "Workspace",
            "name": "Workspace",
            "properties": {},
            "children": local_children
        })];
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [{
                "class": "Workspace",
                "name": "Workspace",
                "children": studio_children
            }]
        });

        let (local, local_stats) = collect_local_nodes_with_stats(&local_services);
        let (studio, studio_stats) = collect_studio_tree_nodes_with_stats(&studio_tree);

        assert_eq!(local.len(), WIDTH * 2);
        assert_eq!(studio.len(), WIDTH * 2);
        assert!(compare(&local, &studio).is_clean());
        assert!(local.contains_key("Workspace/Container/Script0000"));
        assert!(local.contains_key(&format!(
            "Workspace/Container [{}]/Script{:04}",
            WIDTH - 1,
            WIDTH - 1
        )));
        let ordered = snapshot_sibling_order(
            local_services[0]
                .get("children")
                .and_then(Value::as_array)
                .expect("generated service has children"),
        );
        assert_eq!(ordered.len(), WIDTH);
        let ordered_children = local_services[0]
            .get("children")
            .and_then(Value::as_array)
            .expect("generated service has children");
        let ordered_script_name = |position: usize| {
            ordered_children[ordered[position]]
                .get("children")
                .and_then(Value::as_array)
                .and_then(|children| children.first())
                .and_then(|child| child.get("name"))
                .and_then(Value::as_str)
                .expect("generated container has one named script")
        };
        assert_eq!(ordered_script_name(0), "Script0000");
        assert_eq!(
            ordered_script_name(WIDTH - 1),
            format!("Script{:04}", WIDTH - 1)
        );

        assert_eq!(local_stats.nodes_prepared, WIDTH * 2);
        assert_eq!(
            local_stats.relevance_computations,
            local_stats.nodes_prepared
        );
        assert_eq!(
            local_stats.mapped_class_computations,
            local_stats.nodes_prepared
        );
        assert_eq!(
            local_stats.sort_signature_computations,
            local_stats.nodes_prepared
        );
        assert_eq!(local_stats.sort_signature_child_links, WIDTH);

        // Studio also prepares the service node, whose cached signature orders
        // its children without recursively rebuilding their signatures.
        assert_eq!(studio_stats.nodes_prepared, WIDTH * 2 + 1);
        assert_eq!(
            studio_stats.relevance_computations,
            studio_stats.nodes_prepared
        );
        assert_eq!(
            studio_stats.mapped_class_computations,
            studio_stats.nodes_prepared
        );
        assert_eq!(
            studio_stats.sort_signature_computations,
            studio_stats.nodes_prepared
        );
        assert_eq!(studio_stats.sort_signature_child_links, WIDTH * 2);
    }

    #[test]
    fn wide_unique_studio_siblings_collect_without_repeated_name_scans() {
        const WIDTH: usize = 10_000;

        let studio_children = (0..WIDTH)
            .rev()
            .map(|index| {
                json!({
                    "class": "ModuleScript",
                    "name": format!("Script{index:05}"),
                    "children": []
                })
            })
            .collect::<Vec<_>>();
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [{
                "class": "ReplicatedStorage",
                "name": "ReplicatedStorage",
                "children": studio_children
            }]
        });

        let (studio, stats) = collect_studio_tree_nodes_with_stats(&studio_tree);

        assert_eq!(studio.len(), WIDTH);
        assert!(studio.contains_key("ReplicatedStorage/Script00000"));
        assert!(studio.contains_key(&format!("ReplicatedStorage/Script{:05}", WIDTH - 1)));
        assert_eq!(stats.nodes_prepared, WIDTH + 1);
        assert_eq!(stats.relevance_computations, stats.nodes_prepared);
        assert_eq!(stats.mapped_class_computations, stats.nodes_prepared);
        assert_eq!(stats.sort_signature_computations, stats.nodes_prepared);
        assert_eq!(stats.sort_signature_child_links, WIDTH);
    }

    #[test]
    fn deep_passthrough_tree_prepares_each_subtree_once() {
        const DEPTH: usize = 384;

        let mut descendant = json!({
            "class": "Script",
            "name": "Leaf",
            "properties": {},
            "children": []
        });
        for index in (0..DEPTH).rev() {
            descendant = json!({
                "class": "Model",
                "name": format!("Layer{index:04}"),
                "children": [descendant]
            });
        }
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [{
                "class": "Workspace",
                "name": "Workspace",
                "children": [descendant]
            }]
        });

        let (studio, stats) = collect_studio_tree_nodes_with_stats(&studio_tree);
        let mut leaf_path = "Workspace".to_string();
        for index in 0..DEPTH {
            leaf_path.push_str(&format!("/Layer{index:04}"));
        }
        leaf_path.push_str("/Leaf");

        assert_eq!(studio.len(), DEPTH + 1);
        assert_eq!(studio["Workspace/Layer0000"].class, "Folder");
        assert_eq!(studio[&leaf_path].class, "Script");

        // The service, each passthrough container, and the leaf are each
        // analyzed exactly once despite every ancestor depending on the leaf.
        assert_eq!(stats.nodes_prepared, DEPTH + 2);
        assert_eq!(stats.relevance_computations, stats.nodes_prepared);
        assert_eq!(stats.mapped_class_computations, stats.nodes_prepared);
        assert_eq!(stats.sort_signature_computations, stats.nodes_prepared);
        assert_eq!(stats.sort_signature_child_links, stats.nodes_prepared - 1);
    }

    #[test]
    fn iterative_cache_and_collection_handle_a_very_deep_chain() {
        const DEPTH: usize = 4_096;

        fn node(class: &str, name: String, children: Vec<Value>) -> Value {
            let mut object = serde_json::Map::new();
            object.insert("class".to_string(), Value::String(class.to_string()));
            object.insert("name".to_string(), Value::String(name));
            object.insert("children".to_string(), Value::Array(children));
            Value::Object(object)
        }

        let mut descendant = node("Script", "Leaf".to_string(), Vec::new());
        for index in (0..DEPTH).rev() {
            descendant = node("Model", format!("Layer{index:04}"), vec![descendant]);
        }
        let service = node("Workspace", "Workspace".to_string(), vec![descendant]);

        let mut cache = ComparisonCache::new(TreeFlavor::Studio);
        cache.prepare(&service);
        let stats = cache.stats();
        let mut collected = BTreeMap::new();
        let entries = studio_child_entries(&service, "Workspace", "Workspace", &cache);
        collect_studio_entries(entries, &mut collected, &cache);

        assert_eq!(stats.nodes_prepared, DEPTH + 2);
        assert_eq!(stats.relevance_computations, stats.nodes_prepared);
        assert_eq!(stats.mapped_class_computations, stats.nodes_prepared);
        assert_eq!(stats.sort_signature_computations, stats.nodes_prepared);
        assert_eq!(stats.sort_signature_child_links, stats.nodes_prepared - 1);
        assert_eq!(collected.len(), DEPTH + 1);
        assert!(collected.values().any(|node| node.class == "Script"));

        // serde_json::Value itself drops recursively; leaking this generated
        // test-only value keeps that unrelated implementation detail from
        // obscuring the cache's iterative traversal guarantee.
        std::mem::forget(service);
    }

    #[test]
    fn crlf_and_lf_sources_compare_equal() {
        let mut local = BTreeMap::new();
        local.insert(
            "ServerScriptService/Main".into(),
            DiffNode {
                path: "ServerScriptService/Main".into(),
                source_path: "ServerScriptService/Main".into(),
                class: "Script".into(),
                kind: DiffKind::Script,
                source_hash: Some(normalized_source_hash("print(1)\r\n")),
                stream_id: None,
            },
        );
        let mut studio = BTreeMap::new();
        studio.insert(
            "ServerScriptService/Main".into(),
            DiffNode {
                path: "ServerScriptService/Main".into(),
                source_path: "ServerScriptService/Main".into(),
                class: "Script".into(),
                kind: DiffKind::Script,
                source_hash: Some(normalized_source_hash("print(1)\n")),
                stream_id: None,
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
    fn avoid_sync_carrier_reserves_duplicate_sibling_fragment() {
        let studio_tree = json!({
            "class": "DataModel",
            "name": "game",
            "children": [{
                "class": "Workspace",
                "name": "Workspace",
                "children": [{
                    "class": "Folder",
                    "name": "Shared",
                    "children": [{
                        "class": "Script",
                        "name": "Runner",
                        "children": []
                    }]
                }, {
                    "class": "Folder",
                    "name": "Shared",
                    "avoidSyncCarrier": true,
                    "children": [{
                        "class": "Folder",
                        "name": "Ignored",
                        "avoidSync": true,
                        "children": []
                    }]
                }]
            }]
        });

        let studio = collect_studio_tree_nodes(&studio_tree);
        assert!(!studio.contains_key("Workspace/Shared"));
        assert_eq!(studio["Workspace/Shared [1]"].class, "Folder");
        assert_eq!(studio["Workspace/Shared [1]/Runner"].class, "Script");
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
