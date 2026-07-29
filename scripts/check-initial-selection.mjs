import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  INITIAL_DETAIL_PAGE_LIMIT,
  INITIAL_DISPLAY_PATH_MAX,
  INITIAL_LIST_RENDER_LIMIT,
  INITIAL_SELECTION_CHUNK_IDS,
  acceptChoiceDetailPage,
  boundedRenderItems,
  boundedRenderPair,
  choiceDetailsPath,
  choiceSummaryCounts,
  createChoiceDetailAccumulator,
  createChoiceReplayEpochGuard,
  divergenceItems,
  itemClassLabel,
  itemStateLabel,
  selectedTransferIds,
  selectionIdChunks,
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

assert.deepEqual(divergenceItems(null), []);
assert.deepEqual(divergenceItems({ newFiles: [{ path: "" }] }), []);

assert.deepEqual(splitDisplayPath("ReplicatedStorage/Shared/CarPhysics"), {
  parent: "ReplicatedStorage/Shared",
  name: "CarPhysics",
});
assert.deepEqual(splitDisplayPath("Loader"), { parent: "", name: "Loader" });
assert.equal(itemClassLabel({ localClass: "Script" }), "Script");
assert.equal(itemClassLabel({ kind: "folder" }), "Folder");

const largeRenderSet = Array.from(
  { length: INITIAL_LIST_RENDER_LIMIT + 25 },
  (_, index) => ({ path: `Workspace/Item${index}` }),
);
assert.deepEqual(boundedRenderItems(largeRenderSet), {
  items: largeRenderSet.slice(0, INITIAL_LIST_RENDER_LIMIT),
  hidden: 25,
});
assert.deepEqual(boundedRenderItems(largeRenderSet, 0), {
  items: [],
  hidden: largeRenderSet.length,
});

// ---- bounded initial-choice detail model --------------------------------

assert.deepEqual(choiceSummaryCounts({
  detailCount: 9,
  comparison: {
    summary: { newFiles: 2, changedFiles: 3, removedFiles: 4 },
  },
}), { create: 2, overwrite: 3, remove: 4, total: 9 });
assert.deepEqual(choiceSummaryCounts({
  comparison: {
    summary: { newFiles: 2, changedFiles: 3, removedFiles: 4 },
  },
}), { create: 2, overwrite: 3, remove: 4, total: 9 });
assert.equal(
  choiceDetailsPath("choice/one", "opaque+cursor"),
  `/initial-choice/details?choiceId=choice%2Fone&limit=${INITIAL_DETAIL_PAGE_LIMIT}&cursor=opaque%2Bcursor`,
);
const extendedWindowsPath = `Workspace/${"LongSegment".repeat(500)}`;
assert.ok(extendedWindowsPath.length > 4096);
const extendedAccumulator = createChoiceDetailAccumulator("choice-long-path", 1);
const [extendedItem] = acceptChoiceDetailPage(extendedAccumulator, {
  ok: true,
  choiceId: "choice-long-path",
  totalCount: 1,
  items: [{
    id: 0,
    action: "overwrite",
    path: extendedWindowsPath,
    kind: "script",
  }],
  nextCursor: null,
  complete: true,
});
assert.equal(extendedItem.path, extendedWindowsPath);
assert.ok(extendedItem.path.length <= INITIAL_DISPLAY_PATH_MAX);

const LARGE_DETAIL_COUNT = 25_000;
const largeDetails = Array.from({ length: LARGE_DETAIL_COUNT }, (_, id) => ({
  id,
  action: ["create", "overwrite", "remove"][id % 3],
  path: `ReplicatedStorage/Generated/Branch${String(id).padStart(5, "0")}/${"LongName".repeat(12)}`,
  kind: "script",
  class: "ModuleScript",
  localClass: "ModuleScript",
  studioClass: "ModuleScript",
  classChanged: false,
  sourceChanged: id % 3 === 1,
}));
const accumulator = createChoiceDetailAccumulator("choice-25k", LARGE_DETAIL_COUNT);
let pageIndex = 0;
for (let offset = 0; offset < largeDetails.length; offset += INITIAL_DETAIL_PAGE_LIMIT) {
  const pageItems = largeDetails.slice(offset, offset + INITIAL_DETAIL_PAGE_LIMIT);
  const complete = offset + pageItems.length === largeDetails.length;
  const page = {
    ok: true,
    choiceId: "choice-25k",
    totalCount: LARGE_DETAIL_COUNT,
    items: pageItems,
    nextCursor: complete ? null : `cursor-${pageIndex + 1}`,
    complete,
  };
  // Representative 512-item pages stay far below the daemon's 512 KiB
  // response ceiling even with deliberately long paths.
  assert.ok(Buffer.byteLength(JSON.stringify(page)) < 512 * 1024);
  acceptChoiceDetailPage(accumulator, page);
  pageIndex += 1;
}
assert.equal(accumulator.complete, true);
assert.equal(accumulator.receivedCount, LARGE_DETAIL_COUNT);
assert.equal(accumulator.paths.size, LARGE_DETAIL_COUNT);

assert.throws(
  () => acceptChoiceDetailPage(
    createChoiceDetailAccumulator("choice-a", 1),
    {
      ok: true,
      choiceId: "choice-b",
      totalCount: 1,
      items: [largeDetails[0]],
      nextCursor: null,
      complete: true,
    },
  ),
  /another choice/,
);
assert.throws(
  () => acceptChoiceDetailPage(
    createChoiceDetailAccumulator("choice-a", 1),
    {
      ok: true,
      choiceId: "choice-a",
      totalCount: 1,
      items: [{ ...largeDetails[0], id: 7 }],
      nextCursor: null,
      complete: true,
    },
  ),
  /not sequential/,
);

const allSelectedIds = selectedTransferIds(
  largeDetails,
  new Set([...largeDetails.map((item) => item.id), LARGE_DETAIL_COUNT + 1]),
);
assert.equal(allSelectedIds.length, LARGE_DETAIL_COUNT);
assert.equal(allSelectedIds[0], 0);
assert.equal(allSelectedIds.at(-1), LARGE_DETAIL_COUNT - 1);
const chunks = selectionIdChunks(allSelectedIds);
assert.equal(chunks.length, Math.ceil(LARGE_DETAIL_COUNT / INITIAL_SELECTION_CHUNK_IDS));
assert.ok(chunks.every((chunk) => chunk.length <= INITIAL_SELECTION_CHUNK_IDS));
assert.deepEqual(chunks.flat(), allSelectedIds);
for (let chunkIndex = 0; chunkIndex < chunks.length; chunkIndex += 1) {
  const body = {
    choiceId: "choice-25k",
    submissionId: "submission-25k",
    chunkIndex,
    finalChunk: chunkIndex === chunks.length - 1,
    ids: chunks[chunkIndex],
    ...(chunkIndex === 0 ? { restart: true } : {}),
  };
  assert.ok(Buffer.byteLength(JSON.stringify(body)) < 64 * 1024);
  assert.equal(Object.hasOwn(body, "paths"), false);
}

const pairWindow = boundedRenderPair(largeDetails, largeDetails);
assert.equal(
  pairWindow.available.items.length + pairWindow.staged.items.length,
  INITIAL_LIST_RENDER_LIMIT,
);
assert.equal(pairWindow.available.hidden + pairWindow.available.items.length, LARGE_DETAIL_COUNT);
assert.equal(pairWindow.staged.hidden + pairWindow.staged.items.length, LARGE_DETAIL_COUNT);
const onePaneWindow = boundedRenderPair(largeDetails, []);
assert.equal(onePaneWindow.available.items.length, INITIAL_LIST_RENDER_LIMIT);
assert.equal(onePaneWindow.staged.items.length, 0);
const overviewPathRows = 18;
const modalWindow = boundedRenderPair(
  largeDetails,
  largeDetails,
  INITIAL_LIST_RENDER_LIMIT - overviewPathRows,
);
assert.equal(
  overviewPathRows + modalWindow.available.items.length + modalWindow.staged.items.length,
  INITIAL_LIST_RENDER_LIMIT,
);

// A status response can resolve after the realtime stream has already
// delivered initial-choice-made. The replay epoch must make that stale
// continuation fail closed rather than reopen the modal.
const replayEpoch = createChoiceReplayEpochGuard();
let resolveReplay;
const deferredReplay = new Promise((resolve) => { resolveReplay = resolve; });
const replayToken = replayEpoch.begin("project-a", "choice-a");
const staleReplayAccepted = deferredReplay.then((pending) => (
  replayEpoch.accepts(replayToken, pending.choiceId)
));
replayEpoch.invalidate("project-a");
resolveReplay({ choiceId: "choice-a" });
assert.equal(await staleReplayAccepted, false);
const currentReplayToken = replayEpoch.begin("project-a", "choice-b");
assert.equal(replayEpoch.accepts(currentReplayToken, "choice-b"), true);
assert.equal(replayEpoch.accepts(currentReplayToken, "choice-c"), false);
replayEpoch.invalidateAll();
assert.equal(replayEpoch.accepts(currentReplayToken, "choice-b"), false);

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
  /role: "watch",\s*protocol: 6/,
  "the app event stream must speak the current daemon protocol",
);
assert.match(
  bridgeSource,
  /if \(!res\.ok\) throw new Error\(`\$\{path\} -> \$\{res\.status\}`\)/,
  "the modal transport must surface 409 stale/conflict responses instead of treating them as data",
);
assert.match(
  appSource,
  /async function replayPendingInitialChoice[\s\S]*?daemonJson\(base, "\/initial-choice"\)/,
  "a late-attaching app must replay a pending initial choice",
);
assert.match(
  appSource,
  /open: \(\) => \{[\s\S]*?replayPendingInitialChoice\(projectId, base, event\)[\s\S]*?\},\s*message:/,
  "every successful event-stream reconnect must recover a missed choice",
);
assert.match(
  appSource,
  /if \(t === "initial-choice-needed"\) \{[\s\S]*?replayPendingInitialChoice\(projectId, base,[\s\S]*?outcome !== INITIAL_CHOICE_REPLAY\.UNAVAILABLE\) return;[\s\S]*?emit\(t,/,
  "a bounded choice event must revalidate bounded server-held status before opening the transfer UI",
);
assert.match(
  appSource,
  /if \(!pending\?\.pending \|\| pending\.choice \|\| !pending\.choiceId\) \{\s*return INITIAL_CHOICE_REPLAY\.RESOLVED;[\s\S]*?catch \{\s*[\s\S]*?return INITIAL_CHOICE_REPLAY\.UNAVAILABLE;/,
  "authoritative resolution must suppress a stale choice event while transport failure may fall back",
);
assert.match(
  appSource,
  /const replayEpoch = initialChoiceReplayEpoch\.begin\(projectId, event\.choiceId\);[\s\S]*?await daemonJson\(base, "\/initial-choice"\);[\s\S]*?!initialChoiceReplayEpoch\.accepts\(replayEpoch, pending\?\.choiceId\)/,
  "a delayed status replay must validate its project epoch and expected choice after awaiting",
);
assert.match(
  appSource,
  /if \(t === "initial-choice-made"\) initialChoiceReplayEpoch\.invalidate\(projectId\);[\s\S]*?on\("daemon:down",[\s\S]*?initialChoiceReplayEpoch\.invalidate/,
  "choice resolution and daemon teardown must invalidate in-flight status replays",
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
  /acceptChoiceDetailPage\(detailAccumulator, page\)[\s\S]*?annotateLastEdited\(added, currentEdits\)/,
  "paged stable-ID detail items must carry last-edited stamps",
);
assert.match(
  overwriteSource,
  /new AbortController\(\)[\s\S]*?choiceDetailsPath\([\s\S]*?signal: controller\.signal/,
  "detail paging must be abortable",
);
assert.match(
  overwriteSource,
  /sortDivergenceItems\(currentItems, sortMode\)/,
  "the transfer list must honor the selected sort mode",
);
assert.match(
  overwriteSource,
  /boundedRenderPair\(visible, staged, transferRowBudget\)/,
  "both transfer panes together must cap live DOM rows",
);
assert.match(
  overwriteSource,
  /visibleCache/,
  "selection changes must not repeatedly sort a large divergence",
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
assert.match(
  overwriteSource,
  /body\.mode = "all"/,
  "the full-disk choice must use a constant-size mode:all request",
);
assert.match(
  overwriteSource,
  /selectionIdChunks\(ids\)[\s\S]*?"\/initial-choice\/selection"/,
  "selective disk choices must submit bounded stable-ID chunks",
);
assert.match(
  overwriteSource,
  /if \(chunkIndex === 0\) body\.restart = true/,
  "a fresh chunk zero must explicitly replace an abandoned uncommitted submission",
);
assert.match(
  overwriteSource,
  /body: JSON\.stringify\(\{ op: "abort", choiceId, submissionId \}\)/,
  "failed selective submissions must explicitly abort their accumulator",
);
assert.doesNotMatch(
  overwriteSource,
  /selectedTransferPaths|body\.paths|data-(?:drag|toggle|remove|unstage)-path/,
  "the UI must never send or use paths as selective-write authority",
);
assert.match(
  overwriteSource,
  /data-detail-progress[\s\S]*?data-act="retry-details"[\s\S]*?data-act="all-disk"/,
  "detail loading must expose progress/retry while keeping full Disk available",
);

console.log("initial selection policy ok");
