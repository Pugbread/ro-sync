// Pure initial-divergence helpers shared by the modal and policy tests.

export function divergenceItems(comparison) {
  if (!comparison || typeof comparison !== "object") return [];
  const items = [];
  append(items, comparison.newFiles, "create");
  append(items, comparison.changedFiles, "overwrite");
  append(items, comparison.removedFiles, "remove");
  const unique = new Map();
  for (const item of items) {
    if (!unique.has(item.path)) unique.set(item.path, item);
  }
  return [...unique.values()].sort((left, right) =>
    left.path.localeCompare(right.path, undefined, { numeric: true })
  );
}

export function selectedTransferPaths(items, selected) {
  const allowed = new Set(items.map((item) => item.path));
  return [...selected]
    .filter((path) => allowed.has(path))
    .sort((left, right) => left.localeCompare(right, undefined, { numeric: true }));
}

// "ReplicatedStorage/Shared/CarPhysics" → parent "ReplicatedStorage/Shared",
// name "CarPhysics". The name is what users scan for, so rows render it
// prominently and ellipsize the parent instead.
export function splitDisplayPath(path) {
  const value = String(path || "");
  const cut = value.lastIndexOf("/");
  if (cut <= 0) return { parent: "", name: value };
  return { parent: value.slice(0, cut), name: value.slice(cut + 1) };
}

export function itemClassLabel(item) {
  return (
    item?.localClass
    || item?.class
    || item?.studioClass
    || (item?.kind === "folder" ? "Folder" : "Synced item")
  );
}

// The action a staged item performs in Studio. "Differs" items are two-sided
// — the compare cannot know whether Studio or disk made the edit — so the
// verb must say that staging makes the DISK copy win.
export function transferVerb(item) {
  if (item?.action === "create") return "Create in Studio";
  if (item?.action === "remove") return "Remove from Studio";
  return "Replace Studio version with disk";
}

// The state of a divergent path, without implying which side edited it.
export function itemStateLabel(item) {
  if (item?.action === "create") return "Only on disk";
  if (item?.action === "remove") return "Missing on disk";
  return "Differs from Studio";
}

export function transferMeta(item) {
  const cls = item?.localClass || item?.class || item?.studioClass || "Synced item";
  if (item?.kind === "folder") return `${cls} tree`;
  if (item?.action === "remove") return `${cls} · absent on disk`;
  if (item?.classChanged) return `${item.studioClass || "Studio type"} → ${item.localClass || cls}`;
  if (item?.sourceChanged) return `${cls} · source differs`;
  return cls;
}

function append(target, source, action) {
  if (!Array.isArray(source)) return;
  for (const raw of source) {
    if (!raw || typeof raw !== "object") continue;
    const path = cleanPath(raw.path);
    if (!path) continue;
    target.push({
      action,
      path,
      kind: cleanText(raw.kind),
      class: cleanText(raw.class),
      localClass: cleanText(raw.localClass),
      studioClass: cleanText(raw.studioClass),
      classChanged: raw.classChanged === true,
      sourceChanged: raw.sourceChanged === true,
    });
  }
}

function cleanPath(value) {
  return String(value || "").replace(/[\u0000-\u001f\u007f]/g, " ").trim().slice(0, 4096);
}

function cleanText(value) {
  return String(value || "").replace(/[\u0000-\u001f\u007f]/g, " ").trim().slice(0, 128);
}
