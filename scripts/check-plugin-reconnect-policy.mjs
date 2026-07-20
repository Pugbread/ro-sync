import assert from "node:assert/strict";
import fs from "node:fs";

const source = fs.readFileSync(new URL("../plugin/Plugin.luau", import.meta.url), "utf8");

const wsLoop = source.slice(
  source.indexOf("local function wsLoop(gen)"),
  source.indexOf("reconnectState.retryInitialCompare = function"),
);
assert.ok(wsLoop.length > 0, "WebSocket loop must remain discoverable");
assert.ok(
  wsLoop.indexOf("reconnectState.refreshHello()")
    < wsLoop.indexOf("HttpService:CreateWebStreamClient"),
  "every WebSocket attempt must refresh /hello before opening a socket",
);
assert.ok(
  wsLoop.indexOf("reconnectState.comparedCapability ~= reconnectState.pluginCapability")
    < wsLoop.indexOf("HttpService:CreateWebStreamClient"),
  "a daemon process change after initial comparison must resync before opening a socket",
);
assert.match(
  wsLoop,
  /pluginCapability = reconnectState\.pluginCapability/,
  "the authenticated hello must use the freshly fetched process capability",
);
assert.match(
  wsLoop,
  /if\s+retryable == false[\s\S]*?terminalReason = reason/,
  "non-retryable daemon rejections must stop automatic reconnect",
);
assert.match(
  wsLoop,
  /local needResyncAfter = true/,
  "every failed or closed transport attempt, including pre-ack rejection, must trigger a full resync",
);
assert.match(
  wsLoop,
  /pendingOps = \{\}[\s\S]*?reconnectState\.retryInitialCompare\(recoveryContext\)/,
  "gap recovery must replace stale point ops with a fresh initial comparison",
);
assert.equal(
  wsLoop.includes("ws: reconnecting in"),
  false,
  "WebSocket failures must not bypass full initial comparison with an in-place socket retry",
);
assert.match(
  wsLoop,
  /not reconnectState\.desired or not connected or gen ~= wsGeneration/,
  "late WebSocket callbacks must not revive an explicitly cancelled connection",
);

const shutdownBranch = wsLoop.slice(
  wsLoop.indexOf('elseif kind == "shutdown"'),
  wsLoop.indexOf('elseif kind == "pong"'),
);
assert.equal(
  shutdownBranch.includes("connected = false"),
  false,
  "a reason-only daemon shutdown is transient and must not permanently disconnect",
);
assert.match(
  shutdownBranch,
  /msg\.retryable/,
  "structured retryability should be honored when a newer daemon supplies it",
);

const refreshHello = source.slice(
  source.indexOf("reconnectState.refreshHello = function"),
  source.indexOf("local function wsLoop(gen)"),
);
assert.match(
  refreshHello,
  /reconnectState\.discoverDaemon\(expectedGameId, true, false\)/,
  "recovery must rediscover a matching daemon when Desktop changes ports",
);
assert.match(
  refreshHello,
  /reconnectState\.pluginCapability = capability/,
  "each successful /hello must rotate the in-memory plugin capability",
);

const startHooks = source.slice(
  source.indexOf("local function startHooksAndWs()"),
  source.indexOf("local function doPushPath"),
);
assert.equal(
  startHooks.includes('setPill("connected"'),
  false,
  "the UI must not claim connection before WebSocket authentication succeeds",
);
assert.match(
  startHooks,
  /if not reconnectState\.desired or not busy then\s+return/,
  "late initial-sync responses must not install hooks after user cancellation",
);
assert.equal(
  startHooks.includes("wsBackoff = WS_BACKOFF_START"),
  false,
  "pre-auth initial comparisons must not reset transport recovery backoff",
);

const initialCompare = source.slice(
  source.indexOf("runInitialCompare = function()"),
  source.indexOf("-- Port probing / auto-discovery"),
);
assert.match(
  initialCompare,
  /reconnectState\.comparedCapability = compareCapability[\s\S]*?startHooksAndWs\(\)/,
  "live hooks may start only after recording which daemon capability was fully compared",
);

const disconnect = source.slice(
  source.indexOf("local function disconnect(reason)"),
  source.indexOf("-- Initial-sync decision handshake"),
);
assert.match(
  disconnect,
  /reconnectState\.desired = false/,
  "explicit user disconnect and plugin unload must remain terminal",
);

assert.match(source, /local PLUGIN_VERSION_STRING = "2\.2\.0"/);
assert.match(source, /local PLUGIN_PROTOCOL_VERSION = 3/);
assert.match(
  source,
  /return tostring\(decoded\.choice\), decoded\.paths/,
  "the initial decision poll must retain the user's selective disk paths",
);
assert.match(
  source,
  /httpJson\("\/snapshot\/selective", "POST", \{/,
  "selective initial sync must request the bounded disk snapshot endpoint",
);

const ensureBranch = source.slice(
  source.indexOf('if kind == "ensure" then'),
  source.indexOf('elseif kind == "set" or kind == "replace" then'),
);
assert.match(
  ensureBranch,
  /if parent:FindFirstChild\(node\.name\) then\s+return/,
  "selective parent materialization must leave an existing unselected ancestor untouched",
);

console.log("Studio plugin reconnect policy checks passed");
