// views/last-edited.js — persisted per-project "when did this path last change"
// memory. Fed by the sanitized sync-activity frames every watch stream already
// receives, so it works identically for Studio-originated and disk-originated
// edits without new daemon surface.

export const LAST_EDITED_STATE_KEY = "lastEdited";
// Bounds keep the host state file small: the newest 400 paths for the 8 most
// recently active projects is ~100 KB worst case against a 4 MiB state cap.
export const LAST_EDITED_PATH_LIMIT = 400;
export const LAST_EDITED_PROJECT_LIMIT = 8;

const MAX_PATH_CHARS = 512;

// A project is keyed by its filesystem path when known (stable across
// sessions and hosts) and falls back to the widget project id.
export function projectMemoryKey(projectPath, projectId) {
  const path = cleanPath(projectPath);
  if (path) return `path:${path}`;
  const id = cleanPath(projectId);
  return id ? `id:${id}` : null;
}

// Apply one sanitized sync-activity frame to a mutable { path: epochMs } map.
// Returns true when the map changed. Renames carry child stamps to the new
// prefix; deletes drop the whole subtree so stale paths cannot resurface.
export function applySyncActivity(map, frame, at = Date.now()) {
  if (!map || typeof map !== "object" || !frame || typeof frame !== "object") return false;
  if (frame.type && frame.type !== "sync-activity") return false;
  const stamp = clampStamp(at);
  if (!stamp) return false;
  const op = frame.op;
  if (op === "set" || op === "class_change") return stampPath(map, frame.path, stamp);
  if (op === "rename") {
    const moved = moveSubtree(map, frame.from, frame.to);
    const stamped = stampPath(map, frame.to, stamp);
    return moved || stamped;
  }
  if (op === "delete") return dropSubtree(map, frame.path);
  return false;
}

// Newest `limit` entries win. Returns a new plain object.
export function pruneLastEdited(map, limit = LAST_EDITED_PATH_LIMIT) {
  const entries = Object.entries(map || {})
    .filter(([path, ts]) => cleanPath(path) && clampStamp(ts))
    .sort((a, b) => b[1] - a[1])
    .slice(0, Math.max(0, limit));
  return Object.fromEntries(entries);
}

// Attach `editedAt` to each divergence item. An edit anywhere under a folder
// tree credits the folder item, so "Folder tree" rows sort by their most
// recently touched descendant.
export function annotateLastEdited(items, map) {
  const index = new Map();
  for (const item of items || []) {
    if (item && typeof item.path === "string") index.set(item.path, 0);
  }
  for (const [path, raw] of Object.entries(map || {})) {
    const ts = clampStamp(raw);
    if (!ts) continue;
    let current = String(path);
    while (current) {
      const known = index.get(current);
      if (known !== undefined && ts > known) index.set(current, ts);
      const cut = current.lastIndexOf("/");
      if (cut <= 0) break;
      current = current.slice(0, cut);
    }
  }
  return (items || []).map((item) => ({
    ...item,
    editedAt: index.get(item?.path) || null,
  }));
}

// "recent" puts known edits newest-first and leaves never-seen paths in
// alphabetical order after them; "action" groups create → overwrite → remove.
export function sortDivergenceItems(items, mode) {
  const copy = [...(items || [])];
  if (mode === "recent") {
    copy.sort((a, b) => (num(b?.editedAt) - num(a?.editedAt)) || byPath(a, b));
  } else if (mode === "action") {
    const rank = { create: 0, overwrite: 1, remove: 2 };
    copy.sort((a, b) => ((rank[a?.action] ?? 3) - (rank[b?.action] ?? 3)) || byPath(a, b));
  } else {
    copy.sort(byPath);
  }
  return copy;
}

export function formatRelativeEdited(ts, now = Date.now()) {
  const stamp = clampStamp(ts);
  if (!stamp) return null;
  const delta = now - stamp;
  if (delta < 60_000) return "just now";
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m ago`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)}h ago`;
  if (delta < 14 * 86_400_000) return `${Math.floor(delta / 86_400_000)}d ago`;
  try {
    return new Date(stamp).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  } catch {
    return null;
  }
}

// Store bound to the host state surface. `record` is called for every
// sync-activity frame during bursts, so it only mutates memory and defers the
// pruned write behind a debounce.
export function createLastEditedStore({ stateGet, stateSet, now = () => Date.now(), debounceMs = 1500 } = {}) {
  let projects = {};
  let dirty = false;
  let timer = null;
  let writeChain = Promise.resolve();

  async function load() {
    try {
      const value = await stateGet(LAST_EDITED_STATE_KEY);
      projects = normalizePersisted(value);
    } catch {
      projects = {};
    }
  }

  function forProject(key) {
    const map = key && projects[key];
    return map ? { ...map } : {};
  }

  function record(key, frame, at = now()) {
    if (!key) return false;
    const map = projects[key] || (projects[key] = {});
    const changed = applySyncActivity(map, frame, at);
    if (changed) {
      dirty = true;
      if (!timer) {
        timer = setTimeout(() => {
          timer = null;
          void flush();
        }, debounceMs);
      }
    }
    return changed;
  }

  function flush() {
    if (!dirty) return writeChain;
    dirty = false;
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
    projects = prunedProjects(projects);
    const payload = { v: 1, projects };
    const write = () => stateSet(LAST_EDITED_STATE_KEY, payload);
    writeChain = writeChain.then(write, write).catch(() => {});
    return writeChain;
  }

  return { load, forProject, record, flush };
}

function prunedProjects(projects) {
  const kept = Object.entries(projects)
    .map(([key, map]) => [key, pruneLastEdited(map)])
    .filter(([, map]) => Object.keys(map).length > 0)
    .sort((a, b) => newestStamp(b[1]) - newestStamp(a[1]))
    .slice(0, LAST_EDITED_PROJECT_LIMIT);
  return Object.fromEntries(kept);
}

function normalizePersisted(value) {
  const source = value && typeof value === "object" ? value.projects : null;
  if (!source || typeof source !== "object") return {};
  const out = {};
  for (const [key, map] of Object.entries(source)) {
    if (!cleanPath(key) || !map || typeof map !== "object") continue;
    const pruned = pruneLastEdited(map);
    if (Object.keys(pruned).length) out[key] = pruned;
  }
  return prunedProjects(out);
}

function newestStamp(map) {
  let newest = 0;
  for (const ts of Object.values(map)) {
    if (ts > newest) newest = ts;
  }
  return newest;
}

function stampPath(map, path, stamp) {
  const key = cleanPath(path);
  if (!key) return false;
  if (map[key] === stamp) return false;
  map[key] = stamp;
  return true;
}

function moveSubtree(map, from, to) {
  const source = cleanPath(from);
  const target = cleanPath(to);
  if (!source || source === target) return false;
  if (!target) return dropSubtree(map, source);
  let changed = false;
  const prefix = `${source}/`;
  for (const [key, ts] of Object.entries(map)) {
    if (key !== source && !key.startsWith(prefix)) continue;
    delete map[key];
    const moved = key === source ? target : `${target}/${key.slice(prefix.length)}`;
    if (moved.length <= MAX_PATH_CHARS && !(map[moved] >= ts)) map[moved] = ts;
    changed = true;
  }
  return changed;
}

function dropSubtree(map, path) {
  const root = cleanPath(path);
  if (!root) return false;
  const prefix = `${root}/`;
  let changed = false;
  for (const key of Object.keys(map)) {
    if (key === root || key.startsWith(prefix)) {
      delete map[key];
      changed = true;
    }
  }
  return changed;
}

function byPath(a, b) {
  return String(a?.path || "").localeCompare(String(b?.path || ""), undefined, { numeric: true });
}

function num(value) {
  return Number.isFinite(value) ? value : 0;
}

function clampStamp(value) {
  const ts = Number(value);
  if (!Number.isFinite(ts) || ts <= 0) return null;
  return Math.round(ts);
}

function cleanPath(value) {
  return String(value ?? "")
    .replace(/[\u0000-\u001f\u007f]/g, " ")
    .trim()
    .slice(0, MAX_PATH_CHARS) || null;
}
