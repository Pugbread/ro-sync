import assert from "node:assert/strict";
import {
  formatActivity,
  pushActivity,
  scrubActivityFrame,
} from "../views/activity-format.js";

const entries = [];
pushActivity(entries, {
  type: "command-activity",
  activityId: "request:7",
  phase: "started",
  op: "set",
  detail: {
    path: "Workspace/Camera",
    property: "FieldOfView",
    source: "SECRET_SOURCE",
    token: "SECRET_TOKEN",
  },
}, 1000);
pushActivity(entries, {
  type: "command-activity",
  activityId: "request:7",
  phase: "completed",
  op: "set",
  detail: { path: "Workspace/Camera", property: "FieldOfView" },
  durationMs: 84,
  ok: true,
}, 1084);

assert.equal(entries.length, 1, "start and completion must merge by activityId");
assert.equal(entries[0].at, 1000, "merged activities keep their original position and start time");
const setModel = formatActivity(entries[0]);
assert.equal(setModel.title, "Change Field of view");
assert.equal(setModel.stateLabel, "Done");
assert.equal(setModel.duration, "84 ms");
assert.equal(setModel.target, "Workspace/Camera");
assert.doesNotMatch(JSON.stringify(setModel.technical), /SECRET|source|token/i);

pushActivity(entries, {
  type: "command-activity",
  activityId: "request:7",
  phase: "started",
  op: "get",
  detail: { path: "Workspace/Camera", property: "CFrame" },
}, 2000);
assert.equal(entries.length, 1, "a repeated legacy ID should replace, not merge with, a terminal card");
assert.equal(entries[0].at, 2000);
const restartedModel = formatActivity(entries[0]);
assert.equal(restartedModel.stateLabel, "Running");
assert.equal(restartedModel.duration, "");
assert.equal(restartedModel.title, "Read CFrame");
assert.equal("ok" in entries[0].frame, false, "a new start must clear the prior terminal outcome");

const scopedId = scrubActivityFrame({
  type: "command-activity",
  activityId: "request:0123456789abcdef:9",
  phase: "started",
  op: "get",
  detail: {},
});
assert.equal(scopedId.activityId, "request:0123456789abcdef:9");

const aborted = [];
pushActivity(aborted, {
  type: "command-activity",
  activityId: "request:8",
  phase: "aborted",
  op: "eval",
  detail: { sourceBytes: 2048, source: "print('secret')" },
  ok: false,
  error: "Requester disconnected",
}, 2000);
const abortedModel = formatActivity(aborted[0]);
assert.equal(abortedModel.title, "Run a Studio check");
assert.equal(abortedModel.stateLabel, "Failed");
assert.deepEqual(abortedModel.facts, [{ label: "Source", value: "2.0 KB" }]);
assert.match(abortedModel.intent, /sandbox\. The action did not complete\.$/);

const sync = [];
pushActivity(sync, {
  type: "sync-activity",
  op: "set",
  path: "Workspace",
  class: "Script",
  name: "Hello",
  properties: { Source: "print('secret')" },
}, 3000);
const syncModel = formatActivity(sync[0]);
assert.equal(syncModel.title, "Synced Hello");
assert.equal(syncModel.target, "Workspace/Hello");
assert.doesNotMatch(JSON.stringify(syncModel.technical), /Source|secret/);

const legacy = scrubActivityFrame({
  type: "op",
  op: {
    op: "class_change",
    path: ["Workspace", "Hello"],
    class: "LocalScript",
    properties: { Source: "SECRET" },
  },
});
assert.deepEqual(legacy, {
  type: "sync-activity",
  op: "class_change",
  path: "Workspace/Hello",
  from: "",
  to: "",
  class: "LocalScript",
  name: "",
});

const terminalShutdown = scrubActivityFrame({
  type: "shutdown",
  reason: "legacy leaf script ReplicatedStorage/Misc/init (Notice).luau must be renamed",
  code: "WATCHER_PROJECTION_MIGRATION_REQUIRED",
  retryable: false,
  token: "SECRET",
});
assert.deepEqual(terminalShutdown, {
  type: "shutdown",
  reason: "legacy leaf script ReplicatedStorage/Misc/init (Notice).luau must be renamed",
  code: "WATCHER_PROJECTION_MIGRATION_REQUIRED",
  retryable: false,
});
const terminalShutdownModel = formatActivity({
  key: "shutdown:terminal",
  at: 5000,
  updatedAt: 5000,
  frame: terminalShutdown,
});
assert.equal(terminalShutdownModel.stateLabel, "Action required");
assert.equal(terminalShutdownModel.tone, "danger");
assert.match(terminalShutdownModel.intent, /must be renamed/);
assert.deepEqual(terminalShutdownModel.facts, [{
  label: "Code",
  value: "WATCHER_PROJECTION_MIGRATION_REQUIRED",
}]);

const retryableShutdownModel = formatActivity({
  key: "shutdown:retryable",
  at: 5001,
  updatedAt: 5001,
  frame: scrubActivityFrame({
    type: "shutdown",
    reason: "filesystem watcher lagged; reconnect to rebuild exact sync state",
    code: "WATCHER_LAGGED",
    retryable: true,
  }),
});
assert.equal(retryableShutdownModel.stateLabel, "Reconnecting");
assert.equal(retryableShutdownModel.state, "running");
assert.match(retryableShutdownModel.intent, /rebuild exact sync state/);

let semanticId = 100;
for (const [op, expectedTitle] of [
  ["capabilities", "Check Studio capabilities"],
  ["capture_status", "Check capture readiness"],
  ["capture_authorize", "Authorize screen capture"],
  ["capture_prepare", "Prepare screen capture"],
  ["photo_prepare", "Prepare Studio photo"],
  ["capture_export", "Export captured image"],
  ["save", "Save the place"],
  ["undo", "Undo the last change"],
  ["redo", "Redo the last change"],
  ["select_get", "Read Studio selection"],
  ["enums", "Inspect Roblox types"],
  ["playtest_contexts", "Inspect playtest contexts"],
  ["playtest_run_cancel", "Cancel playtest run"],
  ["transmit_prepare", "Prepare image transfer"],
]) {
  const list = [];
  pushActivity(list, {
    type: "command-activity",
    activityId: `request:${semanticId++}`,
    phase: "completed",
    op,
    detail: {},
    ok: true,
  }, 4000);
  assert.equal(formatActivity(list[0]).title, expectedTitle, op);
}

const query = [];
pushActivity(query, {
  type: "command-activity",
  activityId: "request:200",
  phase: "completed",
  op: "query",
  detail: { selector: "Workspace/**/Checkpoint", limit: 25 },
  ok: true,
}, 4500);
const queryModel = formatActivity(query[0]);
assert.equal(queryModel.title, "Search for Workspace/**/Checkpoint");
assert.deepEqual(queryModel.facts, [
  { label: "Selector", value: "Workspace/**/Checkpoint" },
  { label: "Limit", value: "25" },
]);

for (const [frame, expected] of [
  [{
    type: "command-activity",
    activityId: "request:300",
    phase: "completed",
    op: "clipboard_copy",
    detail: {
      itemCount: 2,
      selectionMode: "paths",
      token: "SECRET_TOKEN",
      artifactId: "SECRET_ARTIFACT",
    },
    ok: true,
  }, { title: "Copy 2 Studio instances", facts: [
    { label: "Items", value: "2" },
  ] }],
  [{
    type: "command-activity",
    activityId: "request:301",
    phase: "completed",
    op: "clipboard_paste",
    detail: {
      itemCount: 2,
      byteLength: 4096,
      parent: "Workspace/Imported",
      sha256: "SECRET_SHA",
      roots: ["SECRET_ROOT"],
    },
    ok: true,
  }, { title: "Paste 2 instances", facts: [
    { label: "Items", value: "2" },
    { label: "Size", value: "4.0 KB" },
  ] }],
]) {
  const list = [];
  pushActivity(list, frame, 4700);
  const model = formatActivity(list[0]);
  assert.equal(model.title, expected.title);
  assert.deepEqual(model.facts, expected.facts);
  assert.doesNotMatch(JSON.stringify(model.technical), /SECRET|token|artifactId|sha256|roots/i);
}

for (const frame of [
  { type: "plugin", connected: true },
  { type: "project-init", status: "created", name: "Race Stars", project: "/Games/Race Stars" },
  { type: "config-changed", gameId: "123", groupId: "9" },
  { type: "conflict", path: "ReplicatedStorage/Foo.luau", source: "SECRET" },
  { type: "sync-error", path: "Workspace/Foo", error: "SECRET" },
  { type: "initial-choice-needed", choiceId: "choice-1" },
  { type: "initial-choice-made", choice: "studio" },
]) {
  const list = [];
  pushActivity(list, frame, 5000);
  const model = formatActivity(list[0]);
  assert.ok(model.title && model.intent, frame.type);
  assert.doesNotMatch(model.title, /^\s*\{|SECRET/);
  assert.doesNotMatch(JSON.stringify(model.technical), /SECRET/);
}

console.log("activity format policy checks passed");
