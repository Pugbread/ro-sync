import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";

const source = fs.readFileSync(new URL("../plugin/Plugin.luau", import.meta.url), "utf8");
const pathHelpersSource = fs.readFileSync(
  new URL("../plugin/PathHelpers.luau", import.meta.url),
  "utf8",
);
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
assert.ok(
  wsLoop.indexOf("client.Opened:Connect")
    < wsLoop.indexOf('type = "hello"'),
  "the plugin must observe Roblox's asynchronous WebSocket open before sending hello",
);
assert.match(
  wsLoop,
  /client\.ConnectionState == Enum\.WebStreamClientState\.Open[\s\S]*?os\.clock\(\) < openDeadline[\s\S]*?if opened and not closed[\s\S]*?type = "hello"/,
  "the plugin must wait boundedly for an open, writable WebSocket before authenticating",
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
assert.match(
  wsLoop,
  /local heartbeatSuspectClock = nil[\s\S]*?lastWatchdogClock = lastPing[\s\S]*?schedulerGap > WS_SCHEDULER_STALL_THRESHOLD[\s\S]*?lastPongClock = now[\s\S]*?not closed and now - lastPongClock > WS_HEARTBEAT_TIMEOUT[\s\S]*?heartbeatSuspectClock = now[\s\S]*?now - heartbeatSuspectClock > WS_POST_WAKE_GRACE/,
  "a scheduler stall or silent heartbeat must receive a bounded post-wake probe window before timeout",
);
assert.ok(
  wsLoop.indexOf("schedulerGap > WS_SCHEDULER_STALL_THRESHOLD")
    < wsLoop.indexOf("not closed and now - lastPongClock > WS_HEARTBEAT_TIMEOUT"),
  "wake/stall grace must run before the ordinary heartbeat timeout check",
);
assert.match(
  wsLoop,
  /lastPongClock = os\.clock\(\)[\s\S]*?heartbeatSuspectClock = nil[\s\S]*?heartbeat remained silent after post-wake grace/,
  "any inbound daemon traffic must clear the heartbeat suspect window",
);

const closeClient = wsLoop.slice(
  wsLoop.indexOf("local function closeClient()"),
  wsLoop.indexOf("local function acceptTransport()"),
);
assert.match(
  closeClient,
  /closed = true[\s\S]*?task\.spawn\(function\(\)[\s\S]*?client:Close\(\)/,
  "WebSocket close must publish local completion before isolating the engine call in another task",
);
assert.ok(
  closeClient.indexOf("closed = true") < closeClient.indexOf("client:Close()"),
  "a blocking WebSocket Close call must never delay local retry state",
);

const laggedBranch = wsLoop.slice(
  wsLoop.indexOf('elseif kind == "lagged"'),
  wsLoop.indexOf('elseif kind == "shutdown"'),
);
assert.match(
  laggedBranch,
  /recoveryContext = "daemon broadcast lagged"[\s\S]*?closeClient\(\)/,
  "broadcast lag recovery must enter the nonblocking close path",
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
assert.match(
  shutdownBranch,
  /recoveryContext = "daemon rejected WebSocket: " \.\. tostring\(msg\.code or reason\)[\s\S]*?closeClient\(\)/,
  "retryable shutdowns such as WATCHER_LAGGED must start nonblocking close and retain a resync reason",
);
assert.match(
  wsLoop,
  /elseif kind == "shutdown"[\s\S]*?closeClient\(\)[\s\S]*?if needResyncAfter then[\s\S]*?disconnectHooks\(\)[\s\S]*?reconnectState\.retryInitialCompare\(recoveryContext\)/,
  "WATCHER_LAGGED shutdown recovery must tear down hooks and re-enter initial comparison",
);
const pushResultBranch = wsLoop.slice(
  wsLoop.indexOf('elseif kind == "push-result"'),
  wsLoop.indexOf('elseif kind == "request"'),
);
assert.match(
  pushResultBranch,
  /skipped > 0 or conflictCount > 0 or errorCount > 0/,
  "any partial live push result must be treated as a failure",
);
assert.match(
  pushResultBranch,
  /setBanner\s*\(\s*"[\s\S]*not fully written to disk[\s\S]*closeClient\(\)/,
  "partial push failure must be visible and start nonblocking close into full reconciliation",
);

const refreshHello = source.slice(
  source.indexOf("reconnectState.refreshHello = function"),
  source.indexOf("local function wsLoop(gen)"),
);
assert.match(
  refreshHello,
  /local pinnedProject = reconnectState\.pinnedProject[\s\S]*?matchesPinnedProject[\s\S]*?reconnectState\.discoverDaemon\(expectedGameId, true, false, pinnedProject\)/,
  "recovery must rediscover only the explicitly selected canonical project when Desktop changes ports",
);
assert.doesNotMatch(
  refreshHello,
  /discoverDaemon[\s\S]{0,120}expectedGameId ~= "0"/,
  "an unpublished place must still follow its exact pinned project to a new port",
);
assert.match(
  refreshHello,
  /reconnectState\.pluginCapability = capability/,
  "each successful /hello must rotate the in-memory plugin capability",
);
assert.match(
  refreshHello,
  /hello ~= nil and not matchesPinnedProject\(hello\)[\s\S]*?hello = nil[\s\S]*?not matchesPinnedProject\(hello\)[\s\S]*?different project during discovery/,
  "both the current port and a rediscovered port must fail closed on a project identity mismatch",
);
assert.match(
  refreshHello,
  /discovered\.ambiguous == true[\s\S]*?multiple Ro Sync projects match this place[\s\S]*?true/,
  "ambiguous recovery discovery must stop instead of silently switching projects",
);

const daemonDiscovery = source.slice(
  source.indexOf("reconnectState.discoverDaemon = function"),
  source.indexOf("local function positiveIdString"),
);
assert.match(
  daemonDiscovery,
  /function\(gameId, quiet, includeInitializer, expectedProject, shouldContinue\)[\s\S]*?for port = firstPort, lastPort do[\s\S]*?not shouldContinue\(\)[\s\S]*?result\.cancelled = true[\s\S]*?probePort\(port, gameId\)/,
  "daemon discovery must stop between deterministic port probes when its connection attempt is cancelled",
);
assert.doesNotMatch(
  daemonDiscovery,
  /for port = firstPort, lastPort do[\s\S]{0,160}?task\.spawn/,
  "daemon discovery must not launch an uncancellable RequestAsync burst across every port",
);
assert.match(
  daemonDiscovery,
  /currentPlaceId = tostring\(game\.PlaceId\)[\s\S]*?candidate\.hello\.placeIds[\s\S]*?#exactPlaceMatches > 0 then exactPlaceMatches else gameMatches/,
  "daemon discovery must prefer candidates whose configured placeIds include the current PlaceId",
);
assert.match(
  daemonDiscovery,
  /function\(gameId, quiet, includeInitializer, expectedProject, shouldContinue\)[\s\S]*?probe\.hello\.project == expectedProjectIdentity[\s\S]*?candidate\.hello\.project == expectedProjectIdentity[\s\S]*?gameMatches = projectMatches[\s\S]*?local currentPlaceId/,
  "automatic discovery must restrict candidates by canonical project before applying PlaceId preference",
);
assert.match(
  daemonDiscovery,
  /#candidates == 1[\s\S]*?result\.found = true[\s\S]*?#candidates > 1[\s\S]*?result\.ambiguous = true/,
  "daemon discovery must require a unique candidate instead of choosing the lowest port",
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
assert.match(
  startHooks,
  /local installGeneration = wsGeneration[\s\S]*?installHooks\(shouldContinueInstall\)[\s\S]*?disconnectHooks\(\)[\s\S]*?connected = true/,
  "time-sliced hook installation must be generation-cancellable and clean up before publishing connected=true",
);
assert.ok(
  startHooks.indexOf("if not shouldContinueInstall()")
    < startHooks.indexOf("connected = true"),
  "hook installation must re-check cancellation after its last possible yield",
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
assert.match(
  initialCompare,
  /action == "in-sync"[\s\S]*?comparedDiskIdentityServices\[serviceName\] ~= true[\s\S]*?snapshotApplyState\.seedDiskPathsForService\(service, comparedDiskIdentities\)[\s\S]*?completeInitialCompare\(\)/,
  "the in-sync fast path must install complete daemon-authored physical identities before live hooks start",
);
assert.match(
  initialCompare,
  /resp\.phase ~= "identities"[\s\S]*?identityCount > scaleState\.maxStreamNodes[\s\S]*?postCompareChunk\([\s\S]*?"identities"[\s\S]*?PathHelpers\.isPortableDiskFragment\(fragment\)[\s\S]*?colliding sibling disk identities[\s\S]*?resp\.nextPhase ~= "hashes"/,
  "initial comparison must page, bound, and validate exact daemon-authored disk identity receipts",
);
assert.match(
  initialCompare,
  /local terminalCompareFailure = nil[\s\S]*?retryOrStopStreamedCompare[\s\S]*?reconnectState\.stopTerminal\(message\)/,
  "streamed comparison must have a terminal path that does not enter retry backoff",
);
assert.match(
  initialCompare,
  /if nextResp\.retryable == false then[\s\S]*?terminalCompareFailure = \{[\s\S]*?code = tostring\(nextResp\.code[\s\S]*?break/,
  "a non-retryable streamed comparison response must retain its typed terminal failure",
);
assert.match(
  initialCompare,
  /retryOrStopStreamedCompare\(streamErr[\s\S]*?retryOrStopStreamedCompare\(prepareErr[\s\S]*?retryOrStopStreamedCompare\(identityErr[\s\S]*?retryOrStopStreamedCompare\(hashErr/,
  "every streamed comparison transport phase must honor a terminal daemon response",
);
assert.match(
  source,
  /message:lower\(\):find\("protocol"[\s\S]*?setPill\("error", "update Ro Sync"\)[\s\S]*?else[\s\S]*?setPill\("error", "action required"\)/,
  "non-protocol terminal failures must not misleadingly ask the user to update Ro Sync",
);
assert.match(
  source,
  /reconnectState\.retryInitialCompare = function\(context\)[\s\S]*?setPill\("reconnecting", waitSec\)[\s\S]*?Waiting for the matching project daemon[\s\S]*?Last check:/,
  "offline recovery must show a truthful retry state and the latest daemon error",
);

const disconnect = source.slice(
  source.indexOf("local function disconnect(reason)"),
  source.indexOf("-- Initial-sync decision handshake"),
);
assert.match(
  disconnect,
  /reconnectState\.desired = false[\s\S]*?reconnectState\.pinnedProject = nil/,
  "explicit user disconnect and plugin unload must remain terminal and release the project pin",
);

const daemonValidation = source.slice(
  source.indexOf("local function validateDaemonProtocol(url)"),
  source.indexOf("-- Project-bootstrap constants"),
);
assert.match(
  daemonValidation,
  /daemonProject = hello\.project[\s\S]*?canonical project identity[\s\S]*?return true, nil, capability, daemonProject/,
  "the initial Connect action must obtain a non-empty canonical project identity from /hello",
);

const beginDaemonConnection = source.slice(
  source.indexOf("local function beginDaemonConnection"),
  source.indexOf("local function projectInitErrorMessage"),
);
assert.match(
  beginDaemonConnection,
  /daemonPluginCapability, daemonProject = validateDaemonProtocol\(chosenUrl\)[\s\S]*?reconnectState\.pinnedProject = daemonProject[\s\S]*?runInitialCompare\(\)/,
  "the selected canonical project must be pinned before the first comparison begins",
);

const startConnect = source.slice(
  source.indexOf("startConnect = function()"),
  source.indexOf("plugin.Unloading:Connect"),
);
assert.match(
  startConnect,
  /if busy then[\s\S]*?reconnectState\.pinnedProject = nil[\s\S]*?return[\s\S]*?Every explicit Connect\/Create Project action[\s\S]*?reconnectState\.pinnedProject = nil/,
  "cancel and every subsequent explicit Connect action must reset the automatic-recovery project pin",
);
assert.match(
  startConnect,
  /local cancelledChoiceId = activeInitialChoiceId[\s\S]*?activeInitialChoiceId = nil[\s\S]*?choiceId = cancelledChoiceId[\s\S]*?choice = "cancel"/,
  "Connect-button cancellation must clear the exact pending daemon overwrite decision",
);
assert.match(
  startConnect,
  /discoveryDeadline = os\.clock\(\) \+ DAEMON_STARTUP_GRACE_SECONDS[\s\S]*?discoverDaemon\(gameId, scanCount > 1, nil, nil, function\(\)[\s\S]*?attempt == connectionAttempt and busy[\s\S]*?result\.found or result\.ambiguous[\s\S]*?setPill\("daemon_waiting", remaining\)[\s\S]*?until os\.clock\(\) >= discoveryDeadline[\s\S]*?if not result\.found then/,
  "an explicit Connect must retry generation-safely through Desktop startup before reporting no daemon",
);

assert.match(source, /local PLUGIN_VERSION_STRING = "2\.4\.1"/);
assert.match(source, /local PLUGIN_PROTOCOL_VERSION = 6/);
assert.match(
  source,
  /return choice, true, selectedCount/,
  "the initial decision poll must retain only the bounded selective grant metadata",
);
assert.match(
  source,
  /startBody\.choiceId = selectionChoiceId[\s\S]*?postExact\(startBody, "snapshot stream start"\)/,
  "selective initial sync must redeem its short choice grant through the bounded snapshot stream",
);
assert.equal(
  source.includes('httpJson("/snapshot/selective"'),
  false,
  "initial decisions must not decode a monolithic selective Source snapshot",
);

const ensureBranch = source.slice(
  source.indexOf('if kind == "ensure" then'),
  source.indexOf('elseif kind == "set" or kind == "replace" then'),
);
assert.match(
  ensureBranch,
  /snapshotApplyState\.buildAssignments\(parent, \{ node \}, nodeOpts\._applyContext\)/,
  "selective parent materialization must resolve one unclaimed sibling identity",
);
assert.match(
  ensureBranch,
  /claimedInstances\[assignment\.candidate\] = true[\s\S]*?return/,
  "an existing unselected ancestor must be claimed but otherwise left untouched",
);
assert.equal(
  ensureBranch.includes("parent:FindFirstChild(node.name)"),
  false,
  "selective parent materialization must not collapse duplicate sibling names",
);

assert.match(
  source,
  /GetPropertyChangedSignal\("Parent"\):Connect[\s\S]*?scaleState\.onParentChanged\(inst\)/,
  "same-service reparents must remain observable without per-instance child signals",
);
assert.match(
  source,
  /if oldExcluded then\s+onDescendantAdded\(inst\)\s+connectTree\(inst\)/,
  "moving an excluded subtree into live scope must upgrade its lightweight hooks",
);
assert.match(
  source,
  /or propertyName == "Parent"/,
  "initial scans must reject a snapshot raced by a same-service reparent",
);
assert.match(
  source,
  /avoidSyncPushInFlight = true[\s\S]*?local revision = scaleState\.avoidSyncRevision/,
  "compact AvoidSync path updates must be single-flight and revisioned",
);
assert.match(
  source,
  /if scaleState\.avoidSyncRevision == revision then\s+scaleState\.avoidSyncPathsDirty = false/,
  "an older AvoidSync response must not clear a newer path update",
);
const serializer = source.slice(
  source.indexOf("local function serializeInstance(inst, shouldContinue)"),
  source.indexOf("local function avoidSyncMarker"),
);
assert.match(
  serializer,
  /while #stack > 0 do/,
  "large live subtree serialization must use an iterative stack",
);
assert.doesNotMatch(
  serializer,
  /serializeInstance\(child\)/,
  "large live subtree serialization must not recurse through every descendant",
);
assert.match(
  serializer,
  /visited % 500 == 0[\s\S]*?task\.wait\(\)[\s\S]*?shouldContinue[\s\S]*?return nil, "cancelled"/,
  "large live subtree preflight must yield and honor connection-generation cancellation",
);
assert.match(
  serializer,
  /estimatedBytes \+= #source[\s\S]*?estimatedBytes > LIVE_PUSH_MAX_BYTES[\s\S]*?return nil, "oversized"/,
  "large live Sources/subtrees must fail into streamed resync before building an unbounded nested operation",
);
assert.match(
  pathHelpersSource,
  /local leadingSpace = index == 1 and byte == 32[\s\S]*?if leadingDot or leadingSpace or trailingDotOrSpace or unsafe then/,
  "Windows-safe physical names must encode a leading ASCII space",
);
assert.match(
  pathHelpersSource,
  /function PathHelpers\.isReservedInitStem[\s\S]*?lower == "init"[\s\S]*?function PathHelpers\.leafScriptStem[\s\S]*?string\.format\("%%%02X"/,
  "literal leaf scripts matching init-marker grammar must escape their first byte",
);
assert.ok(
  pathHelpersSource.includes('return lower:match("^init %((.+)%)$") ~= nil'),
  "named init reservation must require a non-empty parenthesized name",
);
assert.match(
  pathHelpersSource,
  /allocatePhysicalFragment[\s\S]*?not isDirectory and deps\.scriptClasses\[className\][\s\S]*?PathHelpers\.leafScriptStem/,
  "reserved init escaping must apply only to leaf scripts",
);
assert.match(
  pathHelpersSource,
  /function PathHelpers\.portableInitFileName[\s\S]*?#named <= 255[\s\S]*?return "init" \.\. suffix/,
  "directory-backed scripts must mirror Rust's portable named/plain init marker selection",
);

function modelLeafStem(name, isDirectory) {
  const reserved = /^init$/i.test(name) || /^init \(.+\)$/i.test(name);
  if (!isDirectory && reserved) {
    return `%${name.charCodeAt(0).toString(16).toUpperCase().padStart(2, "0")}${name.slice(1)}`;
  }
  return name;
}
assert.equal(modelLeafStem("init", false), "%69nit");
assert.equal(modelLeafStem("Init (Notifications)", false), "%49nit (Notifications)");
assert.equal(modelLeafStem("init ()", false), "init ()");
assert.equal(modelLeafStem("init (Notifications)", true), "init (Notifications)");

const httpRequestHelper = source.slice(
  source.indexOf("local function httpRequestTo"),
  source.indexOf("local function httpRequest("),
);
assert.match(
  httpRequestHelper,
  /if internalJsonSafe == true then body else sanitizeJson\(body\)/,
  "trusted snapshot bodies may skip the whole-tree sanitization copy",
);
assert.match(
  httpRequestHelper,
  /HttpService:JSONEncode\(value\)/,
  "the trusted snapshot path must still catch direct JSON encoding failures",
);
const httpJsonHelper = source.slice(
  source.indexOf("local function httpJson"),
  source.indexOf("-- Pushing local changes"),
);
assert.match(
  httpJsonHelper,
  /maxResponseBytes and #resp\.Body > maxResponseBytes[\s\S]*?HttpService:JSONDecode/,
  "bounded protocol responses must be size-checked before JSON decoding",
);

const livePush = source.slice(
  source.indexOf("scaleState.preflightLiveOp = function"),
  source.indexOf("local function queueScriptUpdateOp"),
);
assert.match(
  livePush,
  /local stack = \{ \{ value = op, depth = 0 \} \}[\s\S]*?while #stack > 0 do[\s\S]*?estimated > LIVE_PUSH_MAX_BYTES[\s\S]*?task\.wait\(\)[\s\S]*?shouldContinue/,
  "live operations must be iteratively preflighted and cancellable before JSON allocation",
);
assert.match(
  livePush,
  /Preflight every operation before sending the first frame[\s\S]*?for index, op in ipairs\(ops\)[\s\S]*?local prefix =[\s\S]*?for index, op in ipairs\(ops\)[\s\S]*?table\.concat\(batch, ","\)/,
  "live pushes must use a discard-only pass before retaining only one bounded frame in pass two",
);
assert.equal(
  livePush.includes("encodedOps"),
  false,
  "two-pass live preflight must not retain an encoded string for every queued operation",
);
const sourceUpdateQueue = source.slice(
  source.indexOf("local function queueScriptUpdateOp"),
  source.indexOf("-- AvoidSync path push"),
);
assert.match(
  sourceUpdateQueue,
  /snapshotApplyState\.sourceDiskPathForInstance\(inst\)/,
  "live Source updates must target a leaf file, including a script-with-children init marker",
);
assert.doesNotMatch(
  sourceUpdateQueue,
  /queueScriptUpdateOp[\s\S]*?snapshotApplyState\.diskPathForInstance\(inst\)/,
  "live Source updates must not send a directory path as the Source target",
);
assert.match(
  sourceUpdateQueue,
  /generation = sourceSyncState\.generation\[scriptInst\] or 0[\s\S]*?latest\.generation ~= \(sourceSyncState\.generation\[scriptInst\] or 0\)[\s\S]*?tryReadScriptSource\(scriptInst\)[\s\S]*?queueScriptUpdateOp\(scriptInst, currentSource\)/,
  "debounced editor writes must be generation-checked and re-read current editor text at commit",
);
assert.match(
  sourceUpdateQueue,
  /queueOp\(op, true\)/,
  "per-script Source handling must bypass the unrelated global structure echo clock",
);
assert.doesNotMatch(
  sourceUpdateQueue,
  /task\.delay\(EDITOR_SOURCE_DEBOUNCE[\s\S]*?queueScriptUpdateOp\(scriptInst, latest\.source\)/,
  "debounced editor writes must never commit captured stale text",
);
const sourceExpectedGuard = source.slice(
  source.indexOf("function sourceSyncState.expectRemoteSource"),
  source.indexOf("local function applyScriptSource"),
);
assert.match(
  sourceExpectedGuard,
  /function sourceSyncState\.expectRemoteSource[\s\S]*?sourceSyncState\.pending\[inst\] = nil[\s\S]*?sourceHash = sha256Hex\(source\)[\s\S]*?function sourceSyncState\.consumeExpected[\s\S]*?observedHash == expected\.sourceHash/,
  "filesystem Source applies must invalidate stale pending edits and suppress only the expected text for that script",
);
assert.doesNotMatch(
  sourceExpectedGuard,
  /sourceSyncState\.expected\[inst\] = \{[\s\S]*?source = source/,
  "expected-source guards must retain bounded hashes rather than every full script Source",
);
assert.match(
  source,
  /local function tryReadScriptSource[\s\S]*?return nil, "ScriptEditorService:GetEditorSource failed: " \.\. tostring\(src\)/,
  "authoritative editor reads must surface GetEditorSource failure instead of manufacturing empty Source",
);
const authoritativeSourceWrites = source.slice(
  source.indexOf("local function writeScriptSource"),
  source.indexOf("local function flushScriptSourceWrites"),
);
assert.match(
  authoritativeSourceWrites,
  /local function writeScriptSource[\s\S]*?local currentSource = tryReadScriptSource\(inst\)[\s\S]*?currentSource ~= nil and sourcesMatchForApply\(currentSource, newSource\)[\s\S]*?return applyScriptSource\(inst, newSource, suppressStudioEcho\)/,
  "direct Source equality must use the authoritative editor buffer and force an apply when it is unreadable",
);
assert.match(
  authoritativeSourceWrites,
  /local function queueScriptSourceWrite[\s\S]*?Always retain the desired value[\s\S]*?table\.insert\(queue,[\s\S]*?return true/,
  "queued Source writes must retain every desired value until last-write-wins coalescing",
);
assert.doesNotMatch(
  authoritativeSourceWrites,
  /sourcesMatchForApply\(readScriptSource\(inst\), newSource\)/,
  "authoritative Source write paths must never accept a stale raw Source fallback as equality",
);
const sourceWriteFlush = source.slice(
  source.indexOf("local function flushScriptSourceWrites"),
  source.indexOf("-- Source is the only property we round-trip"),
);
assert.match(
  sourceWriteFlush,
  /local coalesced = \{\}[\s\S]*?indexByInstance\[job\.inst\][\s\S]*?coalesced\[existingIndex\] = job[\s\S]*?queue = coalesced[\s\S]*?tryReadScriptSource\(job\.inst\)[\s\S]*?sourcesMatchForApply\(currentSource, job\.source\)[\s\S]*?queue = pending/,
  "Source drains must coalesce repeated writes per Instance before checking the final desired value",
);
assert.match(
  source,
  /push Source %s could not read the Studio editor buffer[\s\S]*?return false/,
  "snapshot push must abort coherently when an editor Source cannot be read",
);
assert.match(
  source,
  /scaleState\.requestControlledLiveResync = function\(reason\)[\s\S]*?liveResyncPending = true[\s\S]*?pendingOps = \{\}[\s\S]*?disconnectHooks\(\)[\s\S]*?wsGeneration \+= 1[\s\S]*?retryInitialCompare\(reason\)/,
  "an oversized live update must trigger exactly one controlled bounded resync instead of requeueing its frame",
);

function modelLiveFrames(opSizes, limit = 512 * 1024) {
  if (opSizes.some((size) => size + 32 > limit)) {
    return { frames: [], resyncs: 1, peakRetained: 0 };
  }
  const envelope = 26;
  const frames = [];
  let current = envelope;
  let peakRetained = 0;
  let count = 0;
  for (const size of opSizes) {
    const comma = count === 0 ? 0 : 1;
    if (count > 0 && current + comma + size > limit) {
      frames.push(current);
      current = envelope;
      count = 0;
    }
    current += (count === 0 ? 0 : 1) + size;
    count += 1;
    peakRetained = Math.max(peakRetained, current);
  }
  if (count > 0) frames.push(current);
  return { frames, resyncs: 0, peakRetained };
}

{
  const modeled = modelLiveFrames(Array(25_000).fill(48));
  assert.equal(modeled.resyncs, 0);
  assert.ok(modeled.frames.length > 1, "25k small operations must split into several frames");
  assert.ok(modeled.frames.every((bytes) => bytes <= 512 * 1024));
  assert.ok(modeled.peakRetained <= 512 * 1024, "only one bounded candidate frame may be retained");
}
{
  const modeled = modelLiveFrames([48, 512 * 1024]);
  assert.deepEqual(modeled.frames, [], "an oversized op must prevent any prefix frame from being sent");
  assert.equal(modeled.resyncs, 1, "an oversized burst must request one controlled resync");
}

const snapshotMatcher = source.slice(
  source.indexOf("function snapshotApplyState.takeCandidate"),
  source.indexOf("function snapshotApplyState.createInstance"),
);
assert.match(
  snapshotMatcher,
  /not used\[candidate\] and not claimed\[candidate\][\s\S]*?used\[candidate\] = true/,
  "snapshot sibling matching must consume each Instance identity at most once",
);
assert.match(
  snapshotMatcher,
  /projection\.boundary and SCRIPT_CLASSES\[projection\.mappedClass\][\s\S]*?not projection\.directory/,
  "AvoidSync scripts must reserve both leaf and directory physical shapes",
);
assert.match(
  snapshotMatcher,
  /PathHelpers\.allocatePhysicalFragment[\s\S]*?byFragment\[string\.lower\(fragment\)\]/,
  "snapshot identity must use the complete physical fragment allocator",
);
assert.match(
  snapshotMatcher,
  /local logicalAllocator = \{[\s\S]*?allocatePhysicalFragment\(logicalAllocator, entry\.name, "Folder", true\)[\s\S]*?byLookupSegment\[logicalSegment\]/,
  "generated lookup paths must use a class-independent encoded sibling allocator",
);
assert.doesNotMatch(
  snapshotMatcher,
  /diskFragmentInfo|decodeDiskFragment|stripDiskScriptSuffix/,
  "exact disk fragments must remain opaque instead of being reduced to name ordinals",
);
assert.match(
  snapshotMatcher,
  /groupEnd > groupStart and SCRIPT_CLASSES[\s\S]*?sha256Hex\(\(source:gsub\("\\r\\n", "\\n"\)\)\)[\s\S]*?left\.sourceKey < right\.sourceKey[\s\S]*?left\.entry\.index < right\.entry\.index/,
  "tied script siblings must use normalized Source identity before GetChildren order",
);
assert.match(
  snapshotMatcher,
  /local cachedFragmentByInstance = \{\}[\s\S]*?parentDiskPath and not override[\s\S]*?allocator\.taken\[fragmentKey\] = true[\s\S]*?local fragment = cachedFragmentByInstance\[entry\.child\][\s\S]*?or PathHelpers\.allocatePhysicalFragment/,
  "cached physical identities must be reserved before newly sorted siblings allocate fragments",
);
assert.match(
  snapshotMatcher,
  /local placementOwner = \{\}[\s\S]*?priorOwner and priorOwner ~= entry\.child[\s\S]*?physical sibling fragment collision/,
  "physical sibling placements must reject duplicate byInstance fragments",
);
assert.match(
  snapshotMatcher,
  /cachedFragmentOwner\[fragmentKey\][\s\S]*?clearDiskPathSubtree\(entry\.child\)[\s\S]*?repaired duplicate cached disk fragment/,
  "colliding cached fragments must be repaired instead of overlaid",
);
assert.match(
  snapshotMatcher,
  /Exact physical identities are assigned as a batch[\s\S]*?for _, assignment in ipairs\(assignments\)/,
  "all exact identities must win before same-name safety fallbacks",
);
assert.match(
  snapshotMatcher,
  /takeExactFallbackCandidate\([\s\S]*?bucket\.boundaries[\s\S]*?ctx\.claimedInstances[\s\S]*?false[\s\S]*?\)/,
  "a changed-shape AvoidSync boundary must still consume one same-name disk node",
);
assert.equal(
  snapshotMatcher.includes("FindFirstChild"),
  false,
  "wide snapshot apply must index siblings instead of searching once per node",
);

const streamedStructure = source.slice(
  source.indexOf("scaleState.streamStudioServiceStructure = function"),
  source.indexOf("scaleState.normalizedSourceHash = function"),
);
assert.match(
  streamedStructure,
  /local orderingContext = snapshotApplyState\.newContext[\s\S]*?buildPhysicalSiblingIndex\(frame\.inst, orderingContext, \{\}\)[\s\S]*?order = placement\.order[\s\S]*?left\.order < right\.order/,
  "streamed structure IDs must follow the canonical physical sibling allocator with stale cache overlays disabled",
);
assert.match(
  streamedStructure,
  /local childEntry = frame\.children\[frame\.nextChild\][\s\S]*?inst = childEntry\.inst[\s\S]*?childIndex = childIndex/,
  "canonical child ordering must be applied before streamed IDs and childIndex values are assigned",
);
assert.match(
  streamedStructure,
  /local instanceById = \{\}[\s\S]*?instanceById\[frame\.id \+ 1\] = frame\.inst[\s\S]*?return sourceInstances, nil, instanceById/,
  "streamed comparison IDs must retain their exact Studio Instance identity until daemon receipts arrive",
);

const seedDiskPaths = source.slice(
  source.indexOf("function snapshotApplyState.seedDiskPathsForService"),
  source.indexOf("function snapshotApplyState.buildAssignments"),
);
assert.match(
  seedDiskPaths,
  /if exactIdentities then[\s\S]*?clearDiskPathSubtree\(service\)[\s\S]*?identity\.fragment[\s\S]*?buildPhysicalSiblingIndex\(frame\.parent, ctx\)[\s\S]*?placement\.fragment ~= exact\.fragment/,
  "clean reconnects must preseed exact fragments, reserve them during allocation, and fail closed on a shape mismatch",
);

function modelStreamedTiedScriptOrder(entries) {
  const canonical = [...entries].sort((left, right) => {
    const leftKey = crypto.createHash("sha256").update(left.source.replaceAll("\r\n", "\n")).digest("hex");
    const rightKey = crypto.createHash("sha256").update(right.source.replaceAll("\r\n", "\n")).digest("hex");
    return leftKey.localeCompare(rightKey) || left.index - right.index;
  });
  return canonical
    .map((entry, index) => ({ ...entry, fragment: index === 0 ? "Twin.luau" : `Twin [${index}].luau` }))
    .map((entry, index) => ({ ...entry, physicalOrder: index + 1 }))
    .sort((left, right) => left.physicalOrder - right.physicalOrder);
}
{
  const forward = modelStreamedTiedScriptOrder([
    { source: "return 'A'\n", index: 1 },
    { source: "return 'B'\n", index: 2 },
  ]).map((entry) => entry.source);
  const reversed = modelStreamedTiedScriptOrder([
    { source: "return 'B'\n", index: 1 },
    { source: "return 'A'\n", index: 2 },
  ]).map((entry) => entry.source);
  assert.deepEqual(
    forward,
    reversed,
    "reversing GetChildren order must not change distinct duplicate-script physical identity",
  );
}

function modelCachedDuplicateInsertion(entries) {
  const canonical = [...entries].sort((left, right) => {
    const leftKey = crypto.createHash("sha256").update(left.source.replaceAll("\r\n", "\n")).digest("hex");
    const rightKey = crypto.createHash("sha256").update(right.source.replaceAll("\r\n", "\n")).digest("hex");
    return leftKey.localeCompare(rightKey) || left.index - right.index;
  });
  const taken = new Set(
    canonical.filter((entry) => entry.cachedFragment).map((entry) => entry.cachedFragment.toLowerCase()),
  );
  const allocate = () => {
    if (!taken.has("twin.luau")) {
      taken.add("twin.luau");
      return "Twin.luau";
    }
    let ordinal = 1;
    while (taken.has(`twin [${ordinal}].luau`)) ordinal += 1;
    const fragment = `Twin [${ordinal}].luau`;
    taken.add(fragment.toLowerCase());
    return fragment;
  };
  return canonical.map((entry) => ({
    ...entry,
    fragment: entry.cachedFragment ?? allocate(),
  }));
}
{
  const modeled = modelCachedDuplicateInsertion([
    { id: "cached-A", source: "return 'A'\n", index: 1, cachedFragment: "Twin.luau" },
    { id: "new-earlier", source: "return 1\n", index: 2 },
  ]);
  assert.equal(modeled[0].id, "new-earlier", "the regression requires the new sibling to Source-sort first");
  assert.deepEqual(
    Object.fromEntries(modeled.map((entry) => [entry.id, entry.fragment])),
    {
      "new-earlier": "Twin [1].luau",
      "cached-A": "Twin.luau",
    },
    "a newly inserted duplicate must not take an existing sibling's cached base fragment",
  );
  assert.equal(
    new Set(modeled.map((entry) => entry.fragment.toLowerCase())).size,
    modeled.length,
    "every duplicate Instance must retain a one-to-one physical fragment",
  );
}
const generatedLookup = pathHelpersSource.slice(
  pathHelpersSource.indexOf("function PathHelpers.findGeneratedPathChild"),
  pathHelpersSource.indexOf("function PathHelpers.findRawDisambiguatedChild"),
);
assert.match(
  generatedLookup,
  /local state = deps\.getSnapshotApplyState\(\)[\s\S]*?state\.buildPhysicalSiblingIndex\(parent, ctx\)[\s\S]*?byLookupSegment\[seg\]/,
  "generated lookup must reuse the iterative projection index",
);
const pathHelpersInstall = source.slice(
  source.indexOf('script:FindFirstChild("PathHelpers")'),
  source.indexOf("local function pathStartsWith"),
);
assert.match(
  pathHelpersInstall,
  /getSnapshotApplyState = function\(\)[\s\S]*?return snapshotApplyState/,
  "PathHelpers must lazily share the one snapshot identity state instead of duplicating it",
);
assert.equal(
  pathHelpersSource.includes("syncRelevantSignature"),
  false,
  "generated lookup must not recursively recompute descendant signatures",
);
const snapshotPrepare = source.slice(
  source.indexOf("function snapshotApplyState.prepareInstance"),
  source.indexOf("function snapshotApplyState.pruneUnkept"),
);
const appliedProjectionRegistration = source.slice(
  source.indexOf("function snapshotApplyState.registerAppliedProjection"),
  source.indexOf("function snapshotApplyState.prepareInstance"),
);
assert.match(
  appliedProjectionRegistration,
  /ctx\.syncableInstances\[inst\] = true[\s\S]*?ctx\.directoryInstances\[inst\] = if node\.class == "Folder"[\s\S]*?local ancestor = inst\.Parent[\s\S]*?ctx\.syncableInstances\[ancestor\] = true[\s\S]*?ctx\.directoryInstances\[ancestor\] = true/,
  "a set must immediately add its materialized node and ancestors to the shared projection index",
);
assert.match(
  snapshotPrepare,
  /ctx\.claimedInstances\[inst\] = true[\s\S]*?registerAppliedProjection\(inst, node, ctx\)[\s\S]*?diskPathCache\[inst\] = diskPath/,
  "projection registration must happen in the same apply step that caches the exact disk identity",
);
assert.match(
  snapshotPrepare,
  /local existingBlocked = existing[\s\S]*?if existingBlocked then[\s\S]*?return existing, true/,
  "a matched AvoidSync boundary must consume its disk identity without applying descendants",
);
assert.match(
  snapshotPrepare,
  /ctx\.avoidSyncCarriers\[existing\][\s\S]*?applyNodeProperties = false/,
  "an AvoidSync carrier's own source must remain Studio-authoritative",
);

function modelBackToBackFolderAndScriptSet(registerProjection) {
  const projection = new Set();
  const children = new Map([["ReplicatedStorage", []]]);
  const set = (parent, name, kind) => {
    const path = `${parent}/${name}`;
    const siblings = children.get(parent) ?? [];
    children.set(parent, [...siblings, path]);
    children.set(path, []);
    if (registerProjection) projection.add(path);
    if (kind === "ModuleScript") projection.add(path);
    return path;
  };
  const resolveParent = (parent) =>
    (children.get("ReplicatedStorage") ?? []).find(
      (candidate) => candidate === parent && projection.has(candidate),
    );

  const folder = set("ReplicatedStorage", "Fresh", "Folder");
  const parent = resolveParent(folder);
  if (!parent) return false;
  set(parent, "Config.luau", "ModuleScript");
  return true;
}
assert.equal(
  modelBackToBackFolderAndScriptSet(false),
  false,
  "the pre-fix stale projection index must reproduce the dropped child set",
);
assert.equal(
  modelBackToBackFolderAndScriptSet(true),
  true,
  "registering the Folder during its set must make the immediately following child resolvable",
);

const physicalAllocator = pathHelpersSource.slice(
  pathHelpersSource.indexOf("function PathHelpers.allocatePhysicalFragment"),
  pathHelpersSource.indexOf("function PathHelpers.mappedSyncClass"),
);
assert.match(
  physicalAllocator,
  /while true do[\s\S]*?ordinal \+= 1/,
  "duplicate allocation must support ordinals beyond four digits",
);
assert.equal(
  physicalAllocator.includes("9999"),
  false,
  "duplicate allocation must not have the old 9,999 sibling ceiling",
);

const snapshotWalker = source.slice(
  source.indexOf("function snapshotApplyState.runChildren"),
  source.indexOf("local function applyNode"),
);
assert.match(
  snapshotWalker,
  /while #stack > 0 do/,
  "large disk snapshots must use an iterative DFS",
);
assert.match(
  snapshotWalker,
  /snapshotApplyState\.checkpoint\(ctx\)/,
  "large disk snapshots must cooperatively yield and observe cancellation",
);
assert.doesNotMatch(
  snapshotWalker,
  /applyNode\(/,
  "the disk snapshot walker must not recurse through descendants",
);

const strictPruner = source.slice(
  source.indexOf("function snapshotApplyState.pruneUnkept"),
  source.indexOf("function snapshotApplyState.runChildren"),
);
assert.match(
  strictPruner,
  /keptInstances\[child\]/,
  "strict snapshot pruning must key wanted children by Instance identity",
);
assert.match(
  strictPruner,
  /isAvoidSyncBlocked\(frame\.inst\)/,
  "strict snapshot pruning must preserve AvoidSync boundaries",
);
assert.match(
  strictPruner,
  /remaining\.Parent = targetParent[\s\S]*?inst:Destroy\(\)/,
  "deleting a stale script must preserve its Studio-authoritative descendants",
);

const snapshotApply = source.slice(
  source.indexOf("local function applySnapshot"),
  source.indexOf("-- HTTP helpers"),
);
const sourceComparison = source.slice(
  source.indexOf("local function normalizeSourceForCompare"),
  source.indexOf("local function applyScriptSource"),
);
assert.match(
  sourceComparison,
  /source:find\("\\r\\n"[\s\S]*?source:gsub\("\\r\\n", "\\n"\)/,
  "Source apply equivalence must use the protocol's CRLF-only normalization",
);
assert.equal(
  sourceComparison.includes('gsub("\\r", "\\n")'),
  false,
  "Source apply equivalence must preserve a lone CR as real content",
);
assert.match(
  snapshotApply,
  /snapshotApplyState\.runChildren\(svc, node\.children, applyOpts, true\)/,
  "a service snapshot must reconcile all siblings in one duplicate-safe batch",
);
assert.match(
  snapshotApply,
  /failedSources > 0[\s\S]*?error\(/,
  "partial ScriptEditorService failures must fail the initial pull",
);
assert.match(
  snapshotApply,
  /return false, tostring\(err\)[\s\S]*?return true, nil/,
  "snapshot application must report failure or success to its caller",
);

const pullPath = source.slice(
  source.indexOf("local function doPullPath"),
  source.indexOf("local function doPullSelectedChoice"),
);
assert.match(
  pullPath,
  /httpRequest\("\/snapshot\/stream", "POST", body, true\)/,
  "full initial pulls must use the protocol-6 bounded snapshot stream",
);
assert.match(
  pullPath,
  /#\(raw\.Body or ""\) > scaleState\.maxStreamRequestBytes/,
  "snapshot responses must be rejected before decoding when they exceed the bounded wire limit",
);
assert.match(
  pullPath,
  /id ~= #state\.records[\s\S]*?disk structure is not dense depth-first preorder/,
  "streamed disk structure must use dense, validated preorder IDs",
);
assert.match(
  pullPath,
  /#records > scaleState\.structureChunkRecords/,
  "pull structure chunks must enforce the shared 512-record bound",
);
assert.match(
  pullPath,
  /#part\.data > scaleState\.sourceChunkBytes[\s\S]*?totalBytes > scaleState\.maxScriptSourceBytes/,
  "pull Source parts must enforce both piece and per-script byte bounds",
);
assert.match(
  source,
  /maxStagedSourceBytesPerService = 64 \* 1024 \* 1024[\s\S]*?maxStagedSourceBytesTotal = 128 \* 1024 \* 1024/,
  "detached Source staging must publish defensible per-service and all-service memory caps",
);
assert.match(
  pullPath,
  /state\.stagedSourceBytes \+ totalBytes > scaleState\.maxStagedSourceBytesPerService[\s\S]*?totalStagedSourceBytes \+ totalBytes > scaleState\.maxStagedSourceBytesTotal[\s\S]*?state\.stagedSourceBytes \+= totalBytes[\s\S]*?totalStagedSourceBytes \+= totalBytes/,
  "pull must reserve aggregate Source bytes once before retaining each detached script",
);
function modelStageSource(serviceBytes, totalBytes, incoming) {
  const perServiceCap = 64 * 1024 * 1024;
  const totalCap = 128 * 1024 * 1024;
  if (serviceBytes + incoming > perServiceCap) return "service";
  if (totalBytes + incoming > totalCap) return "total";
  return [serviceBytes + incoming, totalBytes + incoming];
}
assert.equal(modelStageSource(64 * 1024 * 1024, 64 * 1024 * 1024, 1), "service");
assert.equal(
  modelStageSource(32 * 1024 * 1024, 128 * 1024 * 1024, 1),
  "total",
);
assert.deepEqual(modelStageSource(0, 0, 1024), [1024, 1024]);
assert.match(
  pullPath,
  /#sources > scaleState\.sourcePartChunkRecords/,
  "pull must cap each Source response at 64 part records",
);
assert.match(
  pullPath,
  /#sources == 0 and not finalChunk[\s\S]*?beforeFirstSource[\s\S]*?afterEverySource[\s\S]*?Source fence tick arrived between ordinary Source parts/,
  "empty non-final Source responses must be no-progress fence ticks only before the first or after every Source",
);
assert.match(
  pullPath,
  /offset ~= active\.offset[\s\S]*?sha256Hex\(source\) ~= digest/,
  "pull Source assembly must validate contiguous offsets and full raw SHA-256",
);
assert.match(
  pullPath,
  /state\.activeSource = \{[\s\S]*?parts = \{\}[\s\S]*?state\.activeSource = nil/,
  "pull buffering must retain at most one script Source at a time",
);
assert.match(
  pullPath,
  /stageStructure\(serviceState\)[\s\S]*?acceptSources\([\s\S]*?commitAllServices\(stagedServiceStates\)/,
  "pull must stage source-free structure, validate every Source, and only then commit all services",
);
assert.match(
  pullPath,
  /if strict then[\s\S]*?snapshotApplyState\.pruneUnkept/,
  "strict pruning must be deferred until the complete service Source phase",
);
assert.match(
  pullPath,
  /snapshotApplyState\.activeMutationGuard = mutationGuard[\s\S]*?not mutationGuard\.dirty/,
  "long pull application must install an exact expected-mutation guard while still rejecting external changes",
);
assert.match(
  pullPath,
  /local exactCoordinate = response\.service == expectedService[\s\S]*?response\.phase == expectedPhase[\s\S]*?responseChunk == expectedChunk[\s\S]*?not exactCoordinate and not diskPrepareReady/,
  "pull responses must match the exact requested coordinate or the one authenticated diskPrepare-to-structure transition",
);
assert.equal(
  pullPath.includes('"/snapshot?service="'),
  false,
  "full pull must not fall back to legacy nested per-service snapshots",
);
assert.match(
  pullPath,
  /avoidSyncPaths = scaleState\.collectAvoidSyncPaths\(\)/,
  "snapshot stream start must carry current compact AvoidSync paths before any strict pull",
);
assert.match(
  pullPath,
  /startBody\.choiceId = selectionChoiceId/,
  "selective initial decisions must remain server-authorized inside the bounded stream",
);
assert.match(
  pullPath,
  /local selective = selectiveSelectedCount ~= nil[\s\S]*?selectiveSelectedCount <= 0[\s\S]*?selectiveSelectedCount % 1 ~= 0[\s\S]*?selectiveSelectedCount > scaleState\.maxStreamNodes/,
  "selective snapshot pulls must require a positive bounded integer selectedCount",
);
assert.equal(
  pullPath.includes("startBody.paths"),
  false,
  "an all-selected decision must not resend a potentially unbounded paths array",
);
assert.match(
  pullPath,
  /type\(part\.finalPart\) ~= "boolean"/,
  "pull Source validation must reject non-boolean finalPart metadata",
);
assert.match(
  pullPath,
  /record\.sourceIncluded ~= false[\s\S]*?state\.sourceIds/,
  "selective ancestor script shells must not cause unselected Sources to be requested or applied",
);
assert.match(
  pullPath,
  /local function acceptDeletes[\s\S]*?#deletes > scaleState\.deleteChunkRecords[\s\S]*?deletion\.pathMode ~= "generated"[\s\S]*?path\[1\] ~= state\.serviceName/,
  "selective delete chunks must be bounded and contain only generated paths rooted at the active service",
);
assert.match(
  pullPath,
  /acceptDeletes[\s\S]*?empty non-final chunk is an authenticated continuation tick[\s\S]*?for deleteIndex in pairs\(deletes\)/,
  "selective delete streams must allow bounded no-op ticks while exact disk revalidation runs",
);
assert.match(
  pullPath,
  /expectedPhase == "deletes" and selective[\s\S]*?acceptDeletes\(serviceState[\s\S]*?commitAllServices\(stagedServiceStates\)/,
  "selective pulls must validate every delete chunk before the all-service commit",
);

const selectiveDeletePlanner = pullPath.slice(
  pullPath.indexOf("local function buildSelectiveDeletePlan"),
  pullPath.indexOf("local function cancelRecording"),
);
assert.match(
  selectiveDeletePlanner,
  /local siblingIndexes = \{\}[\s\S]*?local siblings = siblingIndexes\[target\][\s\S]*?if not siblings then[\s\S]*?buildPhysicalSiblingIndex\(target, resolveContext\)[\s\S]*?siblingIndexes\[target\] = siblings/,
  "selective delete planning must build each unchanged parent's generated sibling index at most once",
);
assert.match(
  selectiveDeletePlanner,
  /if resolvedTargets\[target\] then[\s\S]*?same Studio target[\s\S]*?resolvedTargets\[target\] = true/,
  "distinct authorized paths must not alias the same Studio target",
);
assert.match(
  selectiveDeletePlanner,
  /while ancestor and ancestor ~= service do[\s\S]*?if resolvedTargets\[ancestor\] then[\s\S]*?targets overlap by ancestry/,
  "ancestor and descendant delete targets must be rejected before mutation",
);
assert.match(
  selectiveDeletePlanner,
  /local group = groupsByParent\[parent\][\s\S]*?groupsByParent\[parent\] = group[\s\S]*?group\.targets\[entry\.target\] = true/,
  "immutable delete targets must be grouped by parent before pruning",
);
assert.equal(
  selectiveDeletePlanner.includes("pruneUnkept"),
  false,
  "selective delete planning must remain read-only",
);

const selectiveDeleteCommit = pullPath.slice(
  pullPath.indexOf("for _, group in ipairs(deletePlan.groups)"),
  pullPath.indexOf("if strict then"),
);
assert.match(
  pullPath,
  /buildSelectiveDeletePlan\(state, service\)[\s\S]*?ChangeHistoryService:TryBeginRecording/,
  "every selective target must resolve against one immutable projection before ChangeHistory mutation starts",
);
assert.match(
  selectiveDeleteCommit,
  /for target in pairs\(group\.targets\)[\s\S]*?claimedInstances\[target\][\s\S]*?target\.Parent ~= group\.parent/,
  "planned targets must retain identity and remain disjoint from streamed structure assignments",
);
assert.match(
  selectiveDeleteCommit,
  /for _, sibling in ipairs\(group\.parent:GetChildren\(\)\)[\s\S]*?not group\.targets\[sibling\][\s\S]*?isAvoidSyncBlocked\(sibling\)[\s\S]*?pruneUnkept\(group\.parent, kept, state\.applyContext\)/,
  "one parent-level prune must delete the immutable target set while preserving unselected and AvoidSync siblings",
);
assert.equal(
  selectiveDeleteCommit.includes("resolveGeneratedPath"),
  false,
  "generated ordinals must never be re-resolved after selective deletion begins",
);
assert.match(
  pullPath,
  /local terminalResponse =[\s\S]*?response\.action ~= "complete"[\s\S]*?response\.action ~= nil[\s\S]*?commitAllServices\(stagedServiceStates\)/,
  "terminal action and phase state must be validated before the only live all-service commit",
);

// Executable policy model for the two adversarial cases that broke the old
// resolve-then-delete loop. Static assertions above bind these invariants to
// Plugin.luau; this model makes the identity/complexity expectations explicit.
function modeledDeleteNode(segment, children = []) {
  const node = { segment, children, parent: null };
  for (const child of children) child.parent = node;
  return node;
}

function modelImmutableDeletePlan(service, paths) {
  const siblingIndexes = new Map();
  const resolvedTargets = new Set();
  const entries = [];
  let indexBuilds = 0;

  const indexFor = (parent) => {
    if (!siblingIndexes.has(parent)) {
      const index = new Map();
      for (const child of parent.children) index.set(child.segment, child);
      siblingIndexes.set(parent, index);
      indexBuilds += 1;
    }
    return siblingIndexes.get(parent);
  };

  for (const path of paths) {
    let target = service;
    for (let index = 1; index < path.length && target; index += 1) {
      target = indexFor(target).get(path[index]);
    }
    if (!target) continue;
    if (resolvedTargets.has(target)) throw new Error("duplicate target");
    resolvedTargets.add(target);
    entries.push(target);
  }

  for (const target of entries) {
    let ancestor = target.parent;
    while (ancestor && ancestor !== service) {
      if (resolvedTargets.has(ancestor)) throw new Error("overlapping target");
      ancestor = ancestor.parent;
    }
    if (ancestor !== service) throw new Error("escaped service");
  }

  const groupsByParent = new Map();
  for (const target of entries) {
    if (!groupsByParent.has(target.parent)) groupsByParent.set(target.parent, new Set());
    groupsByParent.get(target.parent).add(target);
  }
  return { groupsByParent, indexBuilds };
}

{
  const twins = [
    modeledDeleteNode("Twin"),
    modeledDeleteNode("Twin [1]"),
    modeledDeleteNode("Twin [2]"),
  ];
  const service = modeledDeleteNode("Workspace", twins);
  const plan = modelImmutableDeletePlan(service, [
    ["Workspace", "Twin [1]"],
    ["Workspace", "Twin [2]"],
  ]);
  const targets = plan.groupsByParent.get(service);
  assert.equal(targets.has(twins[1]), true);
  assert.equal(targets.has(twins[2]), true);
  service.children = service.children.filter((child) => !targets.has(child));
  assert.deepEqual(service.children, [twins[0]], "duplicate ordinals must retain pre-mutation Instance identity");
}

{
  const leaf = modeledDeleteNode("Leaf");
  const folder = modeledDeleteNode("Folder", [leaf]);
  const service = modeledDeleteNode("Workspace", [folder]);
  assert.throws(
    () => modelImmutableDeletePlan(service, [
      ["Workspace", "Folder"],
      ["Workspace", "Folder", "Leaf"],
    ]),
    /overlapping target/,
  );
  assert.throws(
    () => modelImmutableDeletePlan(service, [
      ["Workspace", "Folder"],
      ["Workspace", "Folder"],
    ]),
    /duplicate target/,
  );
}

{
  const width = 25_000;
  const children = Array.from(
    { length: width },
    (_, index) => modeledDeleteNode(index === 0 ? "Wide" : `Wide [${index}]`),
  );
  const service = modeledDeleteNode("Workspace", children);
  const paths = children.map((child) => ["Workspace", child.segment]);
  const plan = modelImmutableDeletePlan(service, paths);
  assert.equal(plan.indexBuilds, 1, "a 25k-wide selection must index its parent once");
  assert.equal(plan.groupsByParent.size, 1);
  assert.equal(plan.groupsByParent.get(service).size, width);
}

const detachedStage = pullPath.slice(
  pullPath.indexOf("local function stageStructure"),
  pullPath.indexOf("local function acceptSources"),
);
assert.match(
  detachedStage,
  /stagingRoot = Instance\.new\("Folder"\)[\s\S]*?staged\.Parent = parent/,
  "pull structure must be built under a detached staging root",
);
assert.equal(
  detachedStage.includes("game:GetService"),
  false,
  "structure validation/staging must not mutate a live Studio service",
);

const sourceStage = pullPath.slice(
  pullPath.indexOf("local function acceptSources"),
  pullPath.indexOf("local function applyStagedService"),
);
assert.match(
  sourceStage,
  /sha256Hex\(source\) ~= digest[\s\S]*?inst\.Source = source/,
  "a Source must validate fully before being written only to its detached script",
);
assert.equal(
  sourceStage.includes("writeScriptSource"),
  false,
  "no live Script Source may change before the full service validates",
);

const streamedCommit = pullPath.slice(
  pullPath.indexOf("local function applyStagedService"),
  pullPath.indexOf("local response, startErr"),
);
assert.match(
  streamedCommit,
  /prepareInstance[\s\S]*?writeScriptSource[\s\S]*?pruneUnkept[\s\S]*?local function commitAllServices[\s\S]*?TryBeginRecording/,
  "the all-service transaction must apply structure and Sources before strict prune under one recording",
);
assert.match(
  streamedCommit,
  /writeScriptSource\(target, source\)[\s\S]*?rawReadbackMatches[\s\S]*?normalizeSourceForCompare\(readback\)[\s\S]*?streamed Source readback SHA-256/,
  "streamed Source writes must verify raw or CRLF-contract-normalized readback before commit",
);
const allServiceCommit = pullPath.slice(
  pullPath.indexOf("local function commitAllServices"),
  pullPath.indexOf("local startBody"),
);
assert.match(
  allServiceCommit,
  /for _, state in ipairs\(states\)[\s\S]*?buildSelectiveDeletePlan\(state, service\)[\s\S]*?TryBeginRecording[\s\S]*?for _, plan in ipairs\(plans\)[\s\S]*?applyStagedService/,
  "every service/delete plan must validate before one all-service ChangeHistory recording mutates Studio",
);
assert.equal(
  (allServiceCommit.match(/TryBeginRecording/g) || []).length,
  1,
  "the complete streamed pull must open exactly one ChangeHistory recording",
);
assert.equal(
  pullPath.includes("commitService"),
  false,
  "a pull must never expose a successfully committed service before the whole stream is terminal",
);
assert.match(
  pullPath,
  /for _, state in ipairs\(stagedServiceStates\)[\s\S]*?state\.stagingRoot:Destroy\(\)/,
  "success, cancellation, and failure must dispose every retained detached service",
);

const streamedRollback = pullPath.slice(
  pullPath.indexOf("local function cancelRecording"),
  pullPath.indexOf("local response, startErr"),
);
assert.match(
  streamedRollback,
  /FinishRecording\(recordingId, Enum\.FinishRecordingOperation\.Cancel\)[\s\S]*?if cancelled then[\s\S]*?method = "cancel"[\s\S]*?ChangeHistoryService:Undo\(\)[\s\S]*?ok = undone[\s\S]*?cancelError = tostring\(cancelErr\)[\s\S]*?undoError = if undone then nil else tostring\(undoErr\)/,
  "stream rollback must report Cancel success or preserve the fallback Undo outcome and both errors",
);
assert.match(
  streamedRollback,
  /TryBeginRecording[\s\S]*?pcall\(function\(\)[\s\S]*?applyStagedService[\s\S]*?if not committed then[\s\S]*?cancelRecording\(recordingId\)/,
  "any cancellation or mutation after TryBeginRecording must use the same Cancel/Undo rollback path",
);
assert.match(
  streamedRollback,
  /if not rollback\.ok then[\s\S]*?may be partially applied[\s\S]*?rollback\.cancelError[\s\S]*?rollback\.undoError[\s\S]*?reconnectState\.stopTerminal\(fatalMessage\)/,
  "Cancel+Undo double failure must report possible partial state and terminally halt bootstrap",
);
assert.match(
  streamedRollback,
  /rollback\.method == "undo"[\s\S]*?rolled back via %s%s/,
  "a successful single fallback Undo must remain an ordinary verified rollback and retry path",
);

function modelRecordingRollback(cancelOk, undoOk) {
  if (cancelOk) {
    return { ok: true, method: "cancel", undoCalls: 0 };
  }
  return {
    ok: undoOk,
    method: undoOk ? "undo" : null,
    undoCalls: 1,
    cancelError: "cancel failed",
    undoError: undoOk ? null : "undo failed",
  };
}
assert.deepEqual(modelRecordingRollback(false, true), {
  ok: true,
  method: "undo",
  undoCalls: 1,
  cancelError: "cancel failed",
  undoError: null,
});
assert.deepEqual(modelRecordingRollback(false, false), {
  ok: false,
  method: null,
  undoCalls: 1,
  cancelError: "cancel failed",
  undoError: "undo failed",
});
assert.deepEqual(modelRecordingRollback(true, false), {
  ok: true,
  method: "cancel",
  undoCalls: 0,
});

const scanGuard = source.slice(
  source.indexOf("scaleState.beginStudioScanGuard"),
  source.indexOf("scaleState.disconnectStudioScanGuard"),
);
assert.equal(
  source.includes("streamInternalDepth"),
  false,
  "streamed pulls must not suppress mutations through a process-global write depth",
);
assert.match(
  source,
  /runInternalWrite\(callback, expectedMutation\)[\s\S]*?guard\.pushExpectedMutation\(expectedMutation\)[\s\S]*?guard\.popExpectedMutation\(expectedMutation\)/,
  "internal write scopes must register and retire an exact expected mutation",
);
assert.match(
  scanGuard,
  /item == expected\.instance[\s\S]*?eventKind == "structure"[\s\S]*?eventKind == "Source"[\s\S]*?markDirty/,
  "scan guards must match internal structure/source events to their exact expected Instance",
);

const applyOp = source.slice(
  source.indexOf("local function applyOp(op, sharedApplyOpts)"),
  source.indexOf("local function setWaypoint"),
);
assert.match(
  applyOp,
  /resolveDiskPath\(op\.diskPath, opContext\)/,
  "delete/update operations must prefer exact physical disk paths",
);
assert.match(
  applyOp,
  /resolveDiskPath\(op\.fromDiskPath, opContext\)/,
  "rename/move operations must prefer their exact source identity",
);
assert.match(
  applyOp,
  /resolveDiskParent\(destinationDiskPath, opContext\)/,
  "rename/move operations must resolve exact destination ancestry",
);
assert.match(
  applyOp,
  /else resolveGeneratedPath\(op\.path, opContext\)/,
  "legacy selective deletes must use generated lookup precedence",
);
assert.match(
  applyOp,
  /generatedTarget = resolveGeneratedPath\(op\.targetPath, opContext\)[\s\S]*?seedDiskPathAncestry\(generatedTarget, op\.diskPath\)/,
  "selective targetPath must seed noncanonical exact disk identity",
);

const sourceAck = source.slice(
  source.indexOf("local function sourceAckForAppliedOp"),
  source.indexOf("local function applySnapshot"),
);
assert.match(
  sourceAck,
  /ack\.diskPath = diskPath/,
  "source acknowledgements must preserve exact duplicate identity",
);

const liveApply = source.slice(
  source.indexOf("local function applyOps(ops, shouldContinue)"),
  source.indexOf("local function sourceAckForAppliedOp"),
);
assert.match(
  liveApply,
  // The trailing argument list is deliberately open-ended: flushScriptSourceWrites
  // also takes an optional failed-job collector so a single rejected Source can be
  // reported against its own op instead of failing the whole coalesced batch. The
  // invariant being asserted is that the same `shouldContinue` generation
  // predicate reaches both the structure pass and the Source pass.
  /TryBeginRecording[\s\S]*?shouldContinue[\s\S]*?snapshotApplyState\.checkpoint[\s\S]*?flushScriptSourceWrites\(sourceWrites, shouldContinue[,)]/,
  "yielding live filesystem applies must share one generation predicate across structure and Source writes",
);
assert.match(
  liveApply,
  /FinishRecording\(recordingId, Enum\.FinishRecordingOperation\.Commit\)[\s\S]*?FinishRecording\(recordingId, Enum\.FinishRecordingOperation\.Cancel\)[\s\S]*?ChangeHistoryService:Undo\(\)/,
  "a disconnected or failed live apply must roll back its recording before returning failure",
);
// Inbound ops are queued and drained by a single worker rather than applied
// inline. applyOps yields (UpdateSourceAsync) while holding the plugin's one
// ChangeHistoryService recording, so a second op frame arriving mid-apply used
// to fail its TryBeginRecording, drop the socket, and force a Studio-authoritative
// resync that discarded local edits. The predicate still has to reach the apply.
assert.match(
  wsLoop,
  /local function shouldContinueApply\(\)[\s\S]*?not closed[\s\S]*?gen == wsGeneration[\s\S]*?ws == client[\s\S]*?applyOps\(batch, shouldContinueApply\)[\s\S]*?elseif shouldContinueApply\(\) then/,
  "WebSocket operation apply must cancel on generation/transport loss and acknowledge only a committed result",
);
assert.match(
  wsLoop,
  /kind == "op" or kind == "ops"[\s\S]*?type\(msg\.ops\) == "table"[\s\S]*?#opQueue \+ #incoming > OP_QUEUE_MAX[\s\S]*?opQueueBytes \+ payloadBytes > OP_QUEUE_MAX_BYTES[\s\S]*?table\.insert\(opQueue, op\)[\s\S]*?opQueueBytes \+= payloadBytes[\s\S]*?runOpWorker\(\)/,
  "single and bounded batched inbound ops must share one count- and byte-bounded apply lane",
);
assert.doesNotMatch(
  wsLoop,
  /applyOps\(\{ msg\.op \}/,
  "applying a single op frame inline reintroduces the recording-contention disconnect",
);
assert.match(
  wsLoop,
  /elseif shouldContinueApply\(\) then[\s\S]*?requestControlledLiveResync\("live filesystem apply failed"\)/,
  "an unacknowledged filesystem apply failure must force an exact comparison instead of leaving Studio stale",
);
assert.match(
  wsLoop,
  /failedOps ~= nil and next\(failedOps\) ~= nil[\s\S]*?requestControlledLiveResync\("one or more live Source writes were rejected"\)/,
  "individually rejected Source writes must force an exact comparison after successful siblings are acknowledged",
);
assert.match(
  wsLoop,
  /operation frame was malformed or exceeded its bounded count[\s\S]*?requestControlledLiveResync\("malformed or oversized inbound operation frame"\)/,
  "malformed or oversized inbound op frames must fail closed into exact comparison",
);

const liveHooks = source.slice(
  source.indexOf("onDescendantAdded = function"),
  source.indexOf("disconnectInstance = function"),
);
assert.match(
  liveHooks,
  /queueRepresentationSet\(representationParent, parentNode, oldDiskPath, newDiskPath\)[\s\S]*?decorateSetIdentity\(inst/,
  "the parent leaf-to-directory migration must be queued before its first child set",
);
assert.match(
  liveHooks,
  /deleteOp\.diskPath = diskPath/,
  "live deletes must carry exact physical identity",
);
assert.match(
  liveHooks,
  /queueRepresentationSet\([\s\S]*?representationParent,[\s\S]*?leafNode,[\s\S]*?oldParentDiskPath,[\s\S]*?leafDiskPath/,
  "last-child removal must collapse a script directory back to its leaf file",
);
assert.match(
  liveHooks,
  /moveOp\.fromDiskPath = oldDiskPath[\s\S]*?moveOp\.toDiskPath = newDiskPath/,
  "same-service moves must carry exact physical source and destination paths",
);
assert.match(
  liveHooks,
  /renameOp\.fromDiskPath = oldDiskPath[\s\S]*?renameOp\.toDiskPath = newDiskPath/,
  "renames must carry exact physical source and destination paths",
);

const hookInstaller = source.slice(
  source.indexOf("connectInstance = function"),
  source.indexOf("-- Studio snapshot + stats"),
);
assert.match(
  hookInstaller,
  /game\.ItemChanged:Connect[\s\S]*?scaleState\.itemChangedAvailable = true/,
  "large projects must use one DataModel-wide property signal when Studio exposes it",
);
assert.match(
  hookInstaller,
  /if scaleState\.itemChangedAvailable then[\s\S]*?return[\s\S]*?if not relevant then[\s\S]*?return/,
  "irrelevant instances must not receive per-instance fallback property connections",
);
assert.match(
  hookInstaller,
  /bucket = scaleState\.emptyConnectionBucket[\s\S]*?bucket == scaleState\.emptyConnectionBucket[\s\S]*?bucket = \{\}/,
  "tracked instances must share one empty bucket and allocate only when fallback hooks are needed",
);
assert.doesNotMatch(
  hookInstaller,
  /GetAttributeChangedSignal\(AVOID_SYNC_ATTRIBUTE\)/,
  "ItemChanged-less channels must not allocate one AvoidSync signal per Instance",
);
assert.match(
  hookInstaller,
  /scaleState\.startAvoidSyncFallbackScanner = function\(\)[\s\S]*?local generation = scaleState\.hookGeneration[\s\S]*?for inst in pairs\(connections\)[\s\S]*?hasAvoidSyncAttribute\(inst\)[\s\S]*?onAvoidSyncChanged\(inst\)[\s\S]*?task\.wait\(0\.5\)/,
  "one generation-scoped low-memory scan must preserve dynamic AvoidSync changes on fallback channels",
);
assert.match(
  hookInstaller,
  /connectTree = function\(inst, progress, shouldContinue\): boolean[\s\S]*?task\.wait\(\)[\s\S]*?not shouldContinue\(\)[\s\S]*?return false/,
  "time-sliced hook indexing must stop promptly when its connection generation is cancelled",
);
assert.match(
  hookInstaller,
  /local function installHooks\(shouldContinue\): boolean[\s\S]*?if not hookService\(svc, progress, shouldContinue\) then[\s\S]*?disconnectHooks\(\)[\s\S]*?return false/,
  "a cancelled large-project hook install must tear down its partial index",
);
assert.match(
  source,
  /local function recordMaterializedAncestors\(inst\)[\s\S]*?connectInstance\(current, false, true\)[\s\S]*?recordMaterializedAncestors\(inst\.Parent\)/,
  "a dynamically added script must upgrade every newly projected fallback ancestor",
);

const initialCompareFlow = source.slice(
  source.indexOf("runInitialCompare = function()"),
  source.indexOf("-- Port probing / auto-discovery"),
);
assert.match(
  initialCompareFlow,
  /collectStudioStatsWithGuard\(scanGuard\)[\s\S]*?studioSnapshot = \{\}/,
  "initial compare must begin with a time-sliced stats-only request",
);
assert.match(
  initialCompareFlow,
  /resp\.action\) == "compare"[\s\S]*?records = records or \{\}[\s\S]*?streamStudioServiceStructure\(/,
  "two-sided comparison must stream source-free flat structure by service",
);
assert.match(
  initialCompareFlow,
  /compareId = compareId,[\s\S]*?service = serviceName,[\s\S]*?for attempt = 1, 2 do[\s\S]*?httpJson\("\/initial-compare", "POST", requestBody, true, PROTOCOL_STREAM_MAX_BYTES\)/,
  "streamed comparison must use the trusted JSON path with bounded idempotent retries",
);
assert.match(
  initialCompareFlow,
  /encodedCompareBodySize[\s\S]*?HttpService:JSONEncode\(requestBody\)[\s\S]*?bodyBytes > scaleState\.maxStreamRequestBytes/,
  "every streamed comparison request must enforce its actual encoded 512 KiB wire size",
);
assert.match(
  initialCompareFlow,
  /while take >= 1 do[\s\S]*?take = math\.floor\(take \/ 2\)[\s\S]*?one Studio comparison structure record exceeds/,
  "comparison structure chunks must split adaptively and fail clearly when one record cannot fit",
);
assert.match(
  initialCompareFlow,
  /tonumber\(resp\.nextChunk\) ~= chunkIndex \+ 1[\s\S]*?exact comparison structure cursor/,
  "adaptively split comparison requests must still acknowledge the exact next cursor",
);
assert.match(
  initialCompareFlow,
  /resp\.phase ~= "diskPrepare"[\s\S]*?tonumber\(resp\.nextChunk\) ~= 0[\s\S]*?while resp\.phase == "diskPrepare" do/,
  "every completed comparison structure stream must enter the bounded diskPrepare continuation at cursor zero",
);
assert.match(
  initialCompareFlow,
  /postCompareChunk\(serviceName, "diskPrepare", prepareChunk, false, \{\}, \{\}\)/,
  "diskPrepare polls must send an explicitly empty, non-final, exact-replay comparison envelope",
);
assert.match(
  initialCompareFlow,
  /resp\.phase == "diskPrepare"[\s\S]*?tonumber\(resp\.nextChunk\) ~= prepareChunk \+ 1[\s\S]*?resp\.phase ~= "hashes"[\s\S]*?tonumber\(resp\.nextChunk\) ~= 0/,
  "diskPrepare may only advance to its exact next poll cursor or the same service's hashes cursor zero",
);
assert.match(
  initialCompareFlow,
  /#hashes < scaleState\.compareHashChunkRecords/,
  "streamed comparison must use the shared 64-hash batch bound",
);
assert.equal(
  initialCompareFlow.includes("collectStudioSnapshotWithGuard"),
  false,
  "initial comparison must never retain a whole-place Source snapshot",
);
assert.match(
  pullPath,
  /local expectedPhase = "diskPrepare"[\s\S]*?local allowDiskPrepareReady = false[\s\S]*?local diskPrepareReady = allowDiskPrepareReady[\s\S]*?response\.phase == "structure"[\s\S]*?responseChunk == 0/,
  "snapshot pull must require the start diskPrepare response and accept structure/0 only after an explicit diskPrepare poll",
);
assert.match(
  pullPath,
  /if expectedPhase == "diskPrepare" then[\s\S]*?response\.finalChunk ~= false[\s\S]*?response\.records ~= nil[\s\S]*?expectedChunk \+= 1/,
  "pending pull diskPrepare responses must be payload-free, non-final exact cursor ticks",
);
assert.match(
  pullPath,
  /expectedService = WATCHED_SERVICES\[expectedServiceIndex\][\s\S]*?expectedPhase = "diskPrepare"[\s\S]*?serviceState = nil/,
  "every committed pull service must advance through the next service's diskPrepare cursor",
);
assert.match(
  pullPath,
  /allowDiskPrepareReady = expectedPhase == "diskPrepare"[\s\S]*?postExact\(nextBody/,
  "only a request actually sent at a diskPrepare cursor may authorize its direct structure response",
);

const selectedChoicePull = source.slice(
  source.indexOf("local function doPullSelectedChoice"),
  source.indexOf("-- A daemon or Desktop broker may advertise"),
);
assert.match(
  selectedChoicePull,
  /selectedCount <= 0[\s\S]*?selectedCount % 1 ~= 0[\s\S]*?selectedCount > scaleState\.maxStreamNodes[\s\S]*?return false/,
  "the selective pull entry point must fail closed on malformed or excessive selectedCount metadata",
);
assert.match(
  selectedChoicePull,
  /type\(choiceId\) ~= "string" or choiceId == ""[\s\S]*?return false[\s\S]*?doPullPath\(true, selectedCount, choiceId\)/,
  "the selective pull entry point must require the daemon-authorized choiceId without a path vector",
);
assert.equal(
  selectedChoicePull.includes("paths"),
  false,
  "the selective pull entry point must not receive or retain selected paths",
);

const decisionHandshake = source.slice(
  source.indexOf("local function waitForDecision"),
  source.indexOf("local function countArray"),
);
assert.match(
  decisionHandshake,
  /#\(resp\.Body or ""\) > PROTOCOL_STREAM_MAX_BYTES[\s\S]*?HttpService:JSONDecode/,
  "initial-decision responses must be capped before decode",
);
assert.match(
  decisionHandshake,
  /decoded\.paths ~= nil[\s\S]*?return "malformed"/,
  "obsolete initial-decision path vectors must be rejected rather than interpreted as a full pull",
);
assert.match(
  decisionHandshake,
  /decoded\.pending == true[\s\S]*?decoded\.selective ~= nil or decoded\.selectedCount ~= nil[\s\S]*?return "malformed"/,
  "pending decision polls must reject premature selective grant metadata",
);
assert.match(
  decisionHandshake,
  /choice == "disk" and decoded\.selective == true[\s\S]*?selectedCount <= 0[\s\S]*?selectedCount % 1 ~= 0[\s\S]*?selectedCount > scaleState\.maxStreamNodes[\s\S]*?return choice, true, selectedCount/,
  "selective Disk decisions must carry only a positive bounded integer selectedCount",
);
assert.match(
  decisionHandshake,
  /elseif choice == "disk" then[\s\S]*?if hasSelective or hasSelectedCount then[\s\S]*?return "malformed"[\s\S]*?return choice, false, nil/,
  "full Disk decisions must omit selective metadata completely",
);

assert.match(
  initialCompareFlow,
  /local choice, selectiveDiskChoice, selectedDiskCount = waitForDecision[\s\S]*?if selectiveDiskChoice == true[\s\S]*?doPullSelectedChoice\(selectedDiskCount, tostring\(choiceId\)\)[\s\S]*?else doPullPath\(true\)/,
  "the decision caller must enter selective pull from the boolean grant and count, while absent metadata means full Disk",
);
assert.match(
  initialCompareFlow,
  /choice == "malformed"[\s\S]*?disconnect\("malformed initial decision"\)/,
  "malformed bounded decisions must abort instead of falling back to a full overwrite",
);
assert.equal(
  source.includes("selectedDiskPaths") || source.includes("doPullSelectedPaths"),
  false,
  "the plugin must not retain the obsolete selected path vector decision flow",
);

const pushPath = source.slice(
  source.indexOf("local function doPushPath"),
  source.indexOf("local function doPullPath"),
);
assert.match(
  source,
  /compareHashChunkRecords = 512/,
  "initial comparison should hash a medium place in one bounded request",
);
assert.match(
  pushPath,
  /strict and \(type\(initialChoiceId\) ~= "string" or initialChoiceId == ""\)[\s\S]*?return false/,
  "a destructive Studio push must require the exact initial-choice authorization",
);
assert.match(
  pushPath,
  /choiceId = if strict then initialChoiceId else nil/,
  "every strict streamed-push chunk must retain its initial-choice authorization",
);
assert.match(
  pushPath,
  /response\.ok == false and response\.stale == true[\s\S]*?return nil/,
  "a stale Studio choice must restart comparison without replaying its rejected push",
);
assert.match(
  initialCompareFlow,
  /choice == "studio"[\s\S]*?doPushPath\(true, scanGuard, choiceId\)/,
  "the Studio decision must pass its choiceId into the strict streamed push",
);
const partialReceiptValidator = pushPath.slice(
  pushPath.indexOf("local function validatePartialReceipt"),
  pushPath.indexOf("local function postExact"),
);
assert.match(
  partialReceiptValidator,
  /allowedFields = \{[\s\S]*?failedService = true[\s\S]*?committedServices = true[\s\S]*?unexpected field/,
  "partial push receipts must reject fields outside the exact daemon response shape",
);
assert.match(
  partialReceiptValidator,
  /response\.ok ~= false[\s\S]*?response\.action ~= "partial"[\s\S]*?response\.recoveryRequired ~= true[\s\S]*?response\.streamId ~= streamId/,
  "only an exact matching recovery-required partial receipt may become terminal",
);
assert.match(
  partialReceiptValidator,
  /#response\.error > scaleState\.maxRecoveryReceiptStringBytes[\s\S]*?#response\.backups > #WATCHED_SERVICES[\s\S]*?backups must be a dense array[\s\S]*?#backup > scaleState\.maxRecoveryReceiptStringBytes/,
  "partial receipt errors and dense backup arrays must remain explicitly bounded",
);
assert.match(
  partialReceiptValidator,
  /committedServices must be a dense array[\s\S]*?type\(entry\) ~= "table"[\s\S]*?field ~= "service"[\s\S]*?field ~= "created"[\s\S]*?field ~= "backup"[\s\S]*?field ~= "recoveryAction"[\s\S]*?unexpected field/,
  "every committed-service recovery entry must be a dense exact-shape object",
);
assert.match(
  partialReceiptValidator,
  /entry\.service ~= WATCHED_SERVICES\[index\][\s\S]*?type\(entry\.created\) ~= "boolean"[\s\S]*?if entry\.created then[\s\S]*?entry\.backup ~= nil[\s\S]*?entry\.recoveryAction ~= "removeCreatedService"[\s\S]*?type\(entry\.backup\) ~= "string"[\s\S]*?entry\.recoveryAction ~= "restoreBackup"/,
  "created and replaced service entries must carry their one exact recovery action",
);
assert.match(
  partialReceiptValidator,
  /committedBackupCount \+= 1[\s\S]*?response\.backups\[committedBackupCount\] ~= entry\.backup[\s\S]*?response\.failedService ~= WATCHED_SERVICES\[#response\.committedServices \+ 1\][\s\S]*?#response\.backups < committedBackupCount[\s\S]*?#response\.backups > committedBackupCount \+ 1/,
  "backups must equal committed restore paths in order plus at most the failed service's retained backup",
);
assert.match(
  pushPath,
  /push recovery restore %s from: %s[\s\S]*?push recovery remove-created-service: %s[\s\S]*?push recovery restore failed service %s from: %s/,
  "terminal handling must log every restore path and every remove-created-service action",
);
assert.match(
  pushPath,
  /Restore %s from: %s[\s\S]*?Remove newly created service directory: %s[\s\S]*?Restore failed service %s from retained backup: %s[\s\S]*?Required recovery actions:\\n%s[\s\S]*?reconnectState\.stopTerminal\(fatalMessage\)/,
  "terminal handling must surface every exact recovery action before stopping reconnects",
);

const pushPostExact = pushPath.slice(
  pushPath.indexOf("local function postExact"),
  pushPath.indexOf("local function acceptAdvance"),
);
assert.match(
  pushPostExact,
  /response\.ok == false[\s\S]*?response\.action == "partial"[\s\S]*?response\.recoveryRequired == true[\s\S]*?response\.streamId == streamId[\s\S]*?validatePartialReceipt\(response\)/,
  "postExact must recognize only the matching daemon partial-receipt identity",
);
assert.ok(
  pushPostExact.indexOf("return nil, stopForPartialReceipt(response, label)")
    < pushPostExact.indexOf("if attempt < 2 then"),
  "a valid partial receipt must terminally return before the exact-chunk retry branch",
);
assert.match(
  source,
  /reconnectState\.stopTerminal = function\(reason\)[\s\S]*?reconnectState\.desired = false[\s\S]*?reconnectState\.retryInitialCompare = function\(context\)[\s\S]*?if not reconnectState\.desired then/,
  "terminal partial recovery must disable the outer comparison retry path",
);

const modeledWatchedServices = [
  "ReplicatedStorage",
  "ServerScriptService",
  "StarterPlayer",
  "StarterGui",
  "Workspace",
  "ReplicatedFirst",
  "ServerStorage",
  "Lighting",
];
function modelValidPartialReceipt(receipt, streamId) {
  const allowed = new Set([
    "ok",
    "action",
    "streamId",
    "error",
    "failedService",
    "recoveryRequired",
    "backups",
    "committedServices",
  ]);
  if (
    receipt === null
    || typeof receipt !== "object"
    || Array.isArray(receipt)
    || Object.keys(receipt).length !== allowed.size
    || Object.keys(receipt).some((field) => !allowed.has(field))
  ) return false;
  if (
    receipt.ok !== false
    || receipt.action !== "partial"
    || receipt.recoveryRequired !== true
    || receipt.streamId !== streamId
  ) return false;
  if (
    typeof receipt.error !== "string"
    || receipt.error.length === 0
    || receipt.error.length > 64 * 1024
    || receipt.error.includes("\0")
  ) return false;
  if (
    !Array.isArray(receipt.backups)
    || receipt.backups.length > modeledWatchedServices.length
  ) return false;
  const backupSeen = new Set();
  for (let index = 0; index < receipt.backups.length; index += 1) {
    if (!Object.hasOwn(receipt.backups, index)) return false;
    const path = receipt.backups[index];
    if (
      typeof path !== "string"
      || path.length === 0
      || path.length > 64 * 1024
      || path.includes("\0")
      || backupSeen.has(path)
    ) return false;
    backupSeen.add(path);
  }
  if (
    !Array.isArray(receipt.committedServices)
    || receipt.committedServices.length > modeledWatchedServices.length
  ) return false;
  const entryFields = new Set(["service", "created", "backup", "recoveryAction"]);
  const committedBackups = [];
  for (let index = 0; index < receipt.committedServices.length; index += 1) {
    if (!Object.hasOwn(receipt.committedServices, index)) return false;
    const entry = receipt.committedServices[index];
    if (
      entry === null
      || typeof entry !== "object"
      || Array.isArray(entry)
      || Object.keys(entry).length !== entryFields.size
      || Object.keys(entry).some((field) => !entryFields.has(field))
      || entry.service !== modeledWatchedServices[index]
      || typeof entry.created !== "boolean"
    ) return false;
    if (entry.created) {
      if (entry.backup !== null || entry.recoveryAction !== "removeCreatedService") return false;
    } else {
      if (
        typeof entry.backup !== "string"
        || entry.backup.length === 0
        || entry.backup.length > 64 * 1024
        || entry.backup.includes("\0")
        || entry.recoveryAction !== "restoreBackup"
      ) return false;
      committedBackups.push(entry.backup);
    }
  }
  if (receipt.failedService !== modeledWatchedServices[receipt.committedServices.length]) return false;
  if (
    receipt.backups.length < committedBackups.length
    || receipt.backups.length > committedBackups.length + 1
    || committedBackups.some((path, index) => receipt.backups[index] !== path)
  ) return false;
  return true;
}
function modelRecoveryLines(receipt) {
  const lines = [];
  let committedBackupCount = 0;
  for (const entry of receipt.committedServices) {
    if (entry.recoveryAction === "restoreBackup") {
      committedBackupCount += 1;
      lines.push(`Restore ${entry.service} from: ${entry.backup}`);
    } else {
      lines.push(`Remove newly created service directory: ${entry.service}`);
    }
  }
  if (receipt.backups.length === committedBackupCount + 1) {
    lines.push(
      `Restore failed service ${receipt.failedService} from retained backup: ${receipt.backups.at(-1)}`,
    );
  }
  return lines;
}
function modelPartialPost(receipts, streamId) {
  for (let attempt = 0; attempt < Math.min(2, receipts.length); attempt += 1) {
    const receipt = receipts[attempt];
    const matchingCore = receipt
      && receipt.ok === false
      && receipt.action === "partial"
      && receipt.recoveryRequired === true
      && receipt.streamId === streamId;
    if (matchingCore && modelValidPartialReceipt(receipt, streamId)) {
      return {
        terminal: true,
        attempts: attempt + 1,
        visible: `${receipt.error}\n${modelRecoveryLines(receipt).join("\n")}`,
      };
    }
  }
  return { terminal: false, attempts: Math.min(2, receipts.length) };
}
const modeledPartialReceipt = {
  ok: false,
  action: "partial",
  recoveryRequired: true,
  streamId: "same-stream",
  error: "installed service changed; rollback refused",
  failedService: "StarterPlayer",
  backups: [
    "C:\\project\\.rosync-backups\\replicated",
    "C:\\project\\.rosync-backups\\starter-player-retained",
  ],
  committedServices: [
    {
      service: "ReplicatedStorage",
      created: false,
      backup: "C:\\project\\.rosync-backups\\replicated",
      recoveryAction: "restoreBackup",
    },
    {
      service: "ServerScriptService",
      created: true,
      backup: null,
      recoveryAction: "removeCreatedService",
    },
  ],
};
{
  const result = modelPartialPost([modeledPartialReceipt], "same-stream");
  assert.equal(result.terminal, true);
  assert.equal(result.attempts, 1, "a valid matching partial receipt must never retry");
  assert.match(result.visible, /installed service changed/);
  assert.match(result.visible, /Restore ReplicatedStorage from: .*rosync-backups\\replicated/);
  assert.match(result.visible, /Remove newly created service directory: ServerScriptService/);
  assert.match(result.visible, /Restore failed service StarterPlayer .*starter-player-retained/);
}
{
  const result = modelPartialPost(
    [{ ...modeledPartialReceipt, unexpected: true }, { ...modeledPartialReceipt, unexpected: true }],
    "same-stream",
  );
  assert.equal(result.terminal, false, "a malformed negative receipt must not stop the stream");
  assert.equal(result.attempts, 2, "a malformed negative receipt remains a normal two-attempt failure");
}
assert.equal(
  modelPartialPost(
    [{ ...modeledPartialReceipt, streamId: "other" }, { ...modeledPartialReceipt, streamId: "other" }],
    "same-stream",
  ).terminal,
  false,
  "a mismatched negative receipt must not terminally halt the current stream",
);
assert.equal(
  modelValidPartialReceipt(
    { ...modeledPartialReceipt, error: "x".repeat(64 * 1024 + 1) },
    "same-stream",
  ),
  false,
  "partial receipt error strings must be bounded",
);
const modeledCreatedOnlyReceipt = {
  ...modeledPartialReceipt,
  failedService: "ServerScriptService",
  backups: [],
  committedServices: [
    {
      service: "ReplicatedStorage",
      created: true,
      backup: null,
      recoveryAction: "removeCreatedService",
    },
  ],
};
assert.equal(
  modelValidPartialReceipt(modeledCreatedOnlyReceipt, "same-stream"),
  true,
  "created services require an explicit removal action and no backup path",
);
assert.match(
  modelPartialPost([modeledCreatedOnlyReceipt], "same-stream").visible,
  /Remove newly created service directory: ReplicatedStorage/,
);
for (const [name, mutate] of [
  ["created with backup", (entry) => ({ ...entry, backup: "C:\\unexpected" })],
  ["created with restore action", (entry) => ({ ...entry, recoveryAction: "restoreBackup" })],
  ["replaced without backup", (entry) => ({ ...entry, created: false, backup: null })],
  ["replaced with remove action", (entry) => ({
    ...entry,
    created: false,
    backup: "C:\\expected",
    recoveryAction: "removeCreatedService",
  })],
  ["entry with extra field", (entry) => ({ ...entry, unexpected: true })],
]) {
  const malformed = {
    ...modeledCreatedOnlyReceipt,
    committedServices: [mutate(modeledCreatedOnlyReceipt.committedServices[0])],
  };
  assert.equal(modelValidPartialReceipt(malformed, "same-stream"), false, name);
}
assert.equal(
  modelValidPartialReceipt(
    {
      ...modeledPartialReceipt,
      backups: [
        modeledPartialReceipt.backups[1],
        modeledPartialReceipt.backups[0],
      ],
    },
    "same-stream",
  ),
  false,
  "committed backup paths must appear first and in service order",
);
assert.equal(
  modelValidPartialReceipt(
    {
      ...modeledPartialReceipt,
      backups: [
        modeledPartialReceipt.backups[0],
        modeledPartialReceipt.backups[1],
        "C:\\project\\.rosync-backups\\second-extra",
      ],
    },
    "same-stream",
  ),
  false,
  "only one extra retained path may belong to the failed service",
);
assert.equal(
  modelValidPartialReceipt(
    { ...modeledPartialReceipt, failedService: "StarterGui" },
    "same-stream",
  ),
  false,
  "failedService must immediately follow the committed service prefix",
);
assert.equal(
  modelValidPartialReceipt(
    {
      ...modeledPartialReceipt,
      backups: ["C:\\project\\bad\0path", modeledPartialReceipt.backups[1]],
      committedServices: [
        {
          ...modeledPartialReceipt.committedServices[0],
          backup: "C:\\project\\bad\0path",
        },
        modeledPartialReceipt.committedServices[1],
      ],
    },
    "same-stream",
  ),
  false,
  "recovery paths must reject embedded NUL bytes",
);
assert.match(
  pushPath,
  /streamStudioServiceStructure\([\s\S]*?candidate\.records = batch/,
  "bootstrap push must stream flat source-free structure instead of nested service snapshots",
);
assert.match(
  pushPath,
  /for attempt = 1, 2 do[\s\S]*?httpJson\("\/push", "POST", body, true, PROTOCOL_STREAM_MAX_BYTES\)/,
  "push chunks must use exact bounded idempotent retries",
);
assert.match(
  initialCompareFlow,
  /httpJson\("\/initial-compare", "POST", requestBody, true, PROTOCOL_STREAM_MAX_BYTES\)/,
  "every streamed comparison response must be capped before decode",
);
assert.match(
  initialCompareFlow,
  /studioSnapshot = \{\}[\s\S]*?PROTOCOL_STREAM_MAX_BYTES/,
  "the stats-first comparison response must also be capped before decode",
);
assert.match(
  pushPath,
  /bodyBytes > scaleState\.maxStreamRequestBytes/,
  "push must enforce actual encoded request size before every send",
);
assert.match(
  pushPath,
  /utf8ChunkEnd[\s\S]*?nextByte >= 0x80 and nextByte < 0xC0/,
  "push must never split a raw UTF-8 Source inside a multibyte sequence",
);
assert.match(
  pushPath,
  /#source > scaleState\.maxScriptSourceBytes[\s\S]*?#part\.data/,
  "push must cap one script and advance offsets using raw Source bytes",
);
assert.match(
  pushPath,
  /partIndex = partIndex,[\s\S]*?offset = offset,[\s\S]*?totalBytes = #source,[\s\S]*?sha256 = sourceHash/,
  "every push Source part must carry contiguous coordinates and full raw SHA-256",
);
assert.match(
  pushPath,
  /response\.nextService == expectedService[\s\S]*?response\.phase == expectedPhase[\s\S]*?response\.nextChunk == expectedChunk/,
  "push must reject daemon responses that do not advance to the exact next coordinate",
);
assert.match(
  pushPath,
  /response\.action == "complete"[\s\S]*?response\.phase ~= nil or response\.nextService ~= nil or response\.nextChunk ~= nil[\s\S]*?response\.action ~= nil[\s\S]*?exactExpected and expectedPhase == "complete"/,
  "push completion must be the sole explicit terminal action, never an unknown or coordinate-only synthetic terminal",
);
assert.match(
  pushPath,
  /phase == "diskFence"[\s\S]*?phase == "diskRevalidate"[\s\S]*?drainContinuation/,
  "push must drive bounded daemon disk-fence and revalidation continuation ticks",
);
assert.match(
  pushPath,
  /body\.records = \{\}[\s\S]*?body\.sources = \{\}/,
  "push continuation ticks must be empty and retry-safe",
);
assert.equal(
  pushPath.includes("services = {"),
  false,
  "protocol-6 push must never construct a legacy nested services payload",
);

const daemonWs = fs.readFileSync(new URL("../daemon/src/ws.rs", import.meta.url), "utf8");
const bridge = fs.readFileSync(new URL("../bridge.js", import.meta.url), "utf8");
const settings = fs.readFileSync(new URL("../views/settings.js", import.meta.url), "utf8");
const projectBroker = fs.readFileSync(
  new URL("../desktop/src-tauri/src/project_broker.rs", import.meta.url),
  "utf8",
);
const readme = fs.readFileSync(new URL("../README.md", import.meta.url), "utf8");
const schema = fs.readFileSync(new URL("../plugin/SCHEMA.md", import.meta.url), "utf8");
const capabilityTemplate = fs.readFileSync(new URL("../daemon/src/snapshot.rs", import.meta.url), "utf8");
assert.match(daemonWs, /PLUGIN_PROTOCOL_VERSION: u64 = 6/);
assert.match(
  bridge,
  /role: "watch",\s*protocol: 6/,
  "the widget event stream must use the same current protocol as the daemon and plugin",
);
assert.match(
  settings,
  /const EXPECTED_PLUGIN_PROTOCOL = 6;/,
  "the settings compatibility UI must use the same current protocol",
);
assert.match(
  projectBroker,
  /"pluginProtocol": 6,/,
  "the Desktop project broker must advertise the same current protocol",
);
assert.match(
  projectBroker,
  /assert_eq!\(unavailable\["pluginProtocol"\], 6\);/,
  "the Desktop project broker compatibility test must pin the current protocol",
);
assert.match(readme, /plugin_protocol-6-/);
assert.match(schema, /Protocol 6 corresponds to Studio plugin 2\.4\.1/);
assert.match(
  schema,
  /"type":"hello","clientId":"123456789","role":"plugin","protocol":6/,
  "the documented WebSocket hello must match the runtime protocol",
);
assert.equal(
  schema.includes('"protocol":5'),
  false,
  "the wire schema must not advertise a stale protocol",
);
assert.match(
  schema,
  /"meta":\{"op":"get","durationMs":1,"protocol":6\}/,
  "documented remote response metadata must use the current protocol",
);
assert.match(capabilityTemplate, /Protocol 6 \/ plugin 2\.4\.1 exposes optional Studio features/);

console.log("Studio plugin reconnect policy checks passed");
