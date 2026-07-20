import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  divergenceItems,
  itemClassLabel,
  itemStateLabel,
  selectedTransferPaths,
  splitDisplayPath,
  transferMeta,
  transferVerb,
} from "../views/initial-selection.js";
import {
  annotateLastEdited,
  applySyncActivity,
  createLastEditedStore,
  formatRelativeEdited,
  projectMemoryKey,
  pruneLastEdited,
  sortDivergenceItems,
} from "../views/last-edited.js";

const items = divergenceItems({
  newFiles: [
    { path: "ReplicatedStorage/NewModule", class: "ModuleScript", kind: "script" },
    { path: "Workspace/NewFolder", class: "Folder", kind: "folder" },
  ],
  changedFiles: [{
    path: "ServerScriptService/Main",
    kind: "script",
    localClass: "Script",
    studioClass: "LocalScript",
    classChanged: true,
    sourceChanged: true,
  }],
  removedFiles: [{
    path: "StarterGui/StudioOnly",
    class: "LocalScript",
    kind: "script",
  }],
});

assert.deepEqual(items.map((item) => [item.path, item.action]), [
  ["ReplicatedStorage/NewModule", "create"],
  ["ServerScriptService/Main", "overwrite"],
  ["StarterGui/StudioOnly", "remove"],
  ["Workspace/NewFolder", "create"],
]);
assert.equal(transferVerb(items[0]), "Create in Studio");
// A differs-item is two-sided: the verb must state that staging makes the
// DISK copy win, so a user who edited in Studio is not misled.
assert.equal(transferVerb(items[1]), "Replace Studio version with disk");
assert.equal(transferVerb(items[2]), "Remove from Studio");
assert.equal(itemStateLabel(items[0]), "Only on disk");
assert.equal(itemStateLabel(items[1]), "Differs from Studio");
assert.equal(itemStateLabel(items[2]), "Missing on disk");
assert.equal(transferMeta(items[1]), "LocalScript → Script");
assert.equal(transferMeta(items[2]), "LocalScript · absent on disk");
assert.equal(transferMeta(items[3]), "Folder tree");

const selected = selectedTransferPaths(items, new Set([
  "Workspace/NewFolder",
  "not/in/the/current/divergence",
  "ReplicatedStorage/NewModule",
]));
assert.deepEqual(selected, ["ReplicatedStorage/NewModule", "Workspace/NewFolder"]);

assert.deepEqual(divergenceItems(null), []);
assert.deepEqual(divergenceItems({ newFiles: [{ path: "" }] }), []);

assert.deepEqual(splitDisplayPath("ReplicatedStorage/Shared/CarPhysics"), {
  parent: "ReplicatedStorage/Shared",
  name: "CarPhysics",
});
assert.deepEqual(splitDisplayPath("Loader"), { parent: "", name: "Loader" });
assert.equal(itemClassLabel({ localClass: "Script" }), "Script");
assert.equal(itemClassLabel({ kind: "folder" }), "Folder");

// ---- last-edited memory --------------------------------------------------

const edits = {};
assert.equal(applySyncActivity(edits, { type: "sync-activity", op: "set", path: "Workspace/A" }, 1000), true);
assert.equal(applySyncActivity(edits, { type: "sync-activity", op: "set", path: "Workspace/A/Child" }, 2000), true);
assert.equal(applySyncActivity(edits, { type: "sync-activity", op: "class_change", path: "Workspace/B" }, 3000), true);
// Renames carry descendant stamps to the new prefix.
assert.equal(applySyncActivity(edits, { type: "sync-activity", op: "rename", from: "Workspace/A", to: "Workspace/Z" }, 4000), true);
assert.equal(edits["Workspace/Z"], 4000);
assert.equal(edits["Workspace/Z/Child"], 2000);
assert.equal(edits["Workspace/A"], undefined);
// Deletes drop the whole subtree.
assert.equal(applySyncActivity(edits, { type: "sync-activity", op: "delete", path: "Workspace/Z" }, 5000), true);
assert.equal(edits["Workspace/Z"], undefined);
assert.equal(edits["Workspace/Z/Child"], undefined);
assert.equal(edits["Workspace/B"], 3000);
// Non-activity frames and unknown ops must be ignored.
assert.equal(applySyncActivity(edits, { type: "op", op: "set", path: "Workspace/C" }, 6000), false);
assert.equal(applySyncActivity(edits, { type: "sync-activity", op: "noop", path: "Workspace/C" }, 6000), false);

assert.deepEqual(
  pruneLastEdited({ a: 1, b: 3, c: 2 }, 2),
  { b: 3, c: 2 },
);

const annotated = annotateLastEdited(
  [
    { path: "ReplicatedStorage/Alpha", action: "create" },
    { path: "ReplicatedStorage/Packages", action: "create", kind: "folder" },
    { path: "ServerScriptService/Beta", action: "overwrite" },
  ],
  {
    "ReplicatedStorage/Alpha": 500,
    // Descendant edits credit the folder item that contains them.
    "ReplicatedStorage/Packages/net/init": 900,
  },
);
assert.equal(annotated[0].editedAt, 500);
assert.equal(annotated[1].editedAt, 900);
assert.equal(annotated[2].editedAt, null);

const sorted = sortDivergenceItems(annotated, "recent").map((item) => item.path);
assert.deepEqual(sorted, [
  "ReplicatedStorage/Packages",
  "ReplicatedStorage/Alpha",
  "ServerScriptService/Beta",
]);
assert.deepEqual(
  sortDivergenceItems(annotated, "action").map((item) => item.action),
  ["create", "create", "overwrite"],
);
assert.deepEqual(
  sortDivergenceItems(annotated, "path").map((item) => item.path),
  ["ReplicatedStorage/Alpha", "ReplicatedStorage/Packages", "ServerScriptService/Beta"],
);

const NOW = 1_800_000_000_000;
assert.equal(formatRelativeEdited(NOW - 5_000, NOW), "just now");
assert.equal(formatRelativeEdited(NOW - 5 * 60_000, NOW), "5m ago");
assert.equal(formatRelativeEdited(NOW - 3 * 3_600_000, NOW), "3h ago");
assert.equal(formatRelativeEdited(NOW - 2 * 86_400_000, NOW), "2d ago");
assert.equal(formatRelativeEdited(null, NOW), null);
assert.equal(formatRelativeEdited("garbage", NOW), null);

assert.equal(projectMemoryKey("/proj/path", "id-1"), "path:/proj/path");
assert.equal(projectMemoryKey("", "id-1"), "id:id-1");
assert.equal(projectMemoryKey("", ""), null);

{
  const writes = [];
  const store = createLastEditedStore({
    stateGet: async () => ({
      v: 1,
      projects: {
        "path:/old": { "Workspace/Kept": 100, "": 50, bogus: "NaN" },
        123: null,
      },
    }),
    stateSet: async (key, value) => writes.push([key, value]),
    now: () => 42,
    debounceMs: 0,
  });
  await store.load();
  assert.deepEqual(store.forProject("path:/old"), { "Workspace/Kept": 100 });
  assert.deepEqual(store.forProject("path:/missing"), {});
  assert.equal(store.record("path:/old", { type: "sync-activity", op: "set", path: "Workspace/New" }), true);
  assert.equal(store.record(null, { type: "sync-activity", op: "set", path: "Workspace/New" }), false);
  await store.flush();
  assert.equal(writes.length, 1);
  assert.equal(writes[0][0], "lastEdited");
  assert.deepEqual(writes[0][1].projects["path:/old"], { "Workspace/Kept": 100, "Workspace/New": 42 });
}

const appSource = await readFile(new URL("../app.js", import.meta.url), "utf8");
const bridgeSource = await readFile(new URL("../bridge.js", import.meta.url), "utf8");
assert.match(
  bridgeSource,
  /role: "watch",\s*protocol: 3/,
  "the app event stream must speak the current daemon protocol",
);
assert.match(
  appSource,
  /async function replayPendingInitialChoice[\s\S]*?daemonJson\(base, "\/initial-choice"\)/,
  "a late-attaching app must replay a pending initial choice",
);
assert.match(
  appSource,
  /open: \(\) => \{ void replayPendingInitialChoice\(projectId, base, event\); \}/,
  "every successful event-stream reconnect must recover a missed choice",
);
assert.match(
  appSource,
  /if \(t === "sync-activity"\) \{\s*lastEditedStore\.record\(resolveMemoryKey\(\), data\);/,
  "the app event stream must feed sync activity into the last-edited store",
);
assert.match(
  appSource,
  /mountOverwriteModal\(\{[\s\S]*?lastEdited: lastEditedStore,[\s\S]*?\}\)/,
  "the divergence modal must receive the last-edited store",
);

const overwriteSource = await readFile(new URL("../views/overwrite.js", import.meta.url), "utf8");
assert.match(
  overwriteSource,
  /annotateLastEdited\(divergenceItems\(data\.comparison\)/,
  "divergence items must carry last-edited stamps",
);
assert.match(
  overwriteSource,
  /sortDivergenceItems\(currentItems, sortMode\)/,
  "the transfer list must honor the selected sort mode",
);
// Tauri's native drag-drop interception (kept on for the projects view)
// swallows HTML5 drag events inside the desktop webview, so the transfer
// view must drag with pointer events instead.
assert.doesNotMatch(
  overwriteSource,
  /draggable=|dragstart|dataTransfer/,
  "the transfer view must not rely on HTML5 drag events",
);
assert.match(
  overwriteSource,
  /addEventListener\("pointerdown"/,
  "the transfer view must implement pointer-based dragging",
);

console.log("initial selection policy ok");
