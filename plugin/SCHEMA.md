# Ro Sync wire schema

This document describes the current daemon ↔ Studio plugin protocol. The
implementation lives in `plugin/Plugin.luau` and `daemon/src/http.rs` /
`daemon/src/ws.rs`.

Ro Sync only mirrors `Folder`, `Script`, `LocalScript`, and `ModuleScript` to
disk. `.meta.json` files and arbitrary Roblox properties are not part of the
filesystem format.

## WebSocket handshake

The Studio plugin first reads `/hello`, retains the process-local
`pluginCapability` without logging or persisting it, then opens `/ws` and
announces both the protocol version and capability:

```json
{"type":"hello","clientId":"123456789","role":"plugin","protocol":3,"pluginCapability":"<64 hex characters>"}
```

The daemon rejects a missing/incompatible protocol or capability with a
`shutdown` frame. Every socket sends exactly one hello using protocol 3;
`plugin`, command-capable CLI/agent, and read-only widget/watch roles receive
only the traffic appropriate to that role. The daemon replaces caller request
IDs with private correlation IDs before routing them to Studio.
Origin-bearing browser HTTP/WebSocket requests must also carry the owning
widget's capability as `?widgetToken=...`; native loopback Studio/CLI clients
do not send an Origin header.
Protocol 3 corresponds to Studio plugin 2.2.0 and adds selective initial
Disk-to-Studio snapshots. It retains the structured errors, capability
discovery, artifact-backed capture, playtest runtime routing, and workflow
transaction/precondition operations introduced by protocol 2.

## Desktop-authorized project initialization

Ro Sync Desktop's project broker (ports `7867`–`7870`) and a daemon started
with `--projects-root <absolute-directory>` advertise an optional initializer
in `/hello`:

```json
{
  "projectInit": {
    "available": true,
    "projectsRoot": "/Users/example/Roblox",
    "endpoint": "/projects/init"
  }
}
```

Ordinary CLI/manual daemons advertise `available: false` and omit
`projectsRoot`. The plugin first scans project-daemon ports `7878`–`7890` for
the open Studio `GameId`; only when there is no match does it scan the Desktop
broker range. It prefers the lowest-port compatible Desktop broker over a
wrong-game daemon's initializer. Project creation remains an explicit second
click: discovery itself never writes to disk. The plugin posts the endpoint
advertised by that same `/hello`, authenticating it with the process-local
`pluginCapability` and sending metadata rather than a path:

```json
{
  "pluginCapability": "<64 hex characters>",
  "gameName": "Race Stars",
  "placeName": "Main Place",
  "gameId": "123",
  "placeId": "456",
  "creatorType": "Group",
  "creatorId": "789",
  "groupId": "789"
}
```

IDs are positive decimal strings. `creatorType` / `creatorId` are optional but
must be supplied together; a group creator implies a matching `groupId`. The
daemon derives a portable slug and creates exactly one direct, non-symlink
child below its canonical configured root. It never accepts a caller-provided
path. Existing projects with the same `gameId` are idempotent; unrelated name
collisions use a deterministic `-<gameId>` suffix, and a second collision is
refused with `PROJECT_PATH_COLLISION` plus `suggestedDirectoryName`. A repeat
request scans direct children for the universe even if its display name has
changed, merges a newly seen `placeId` plus supplied creator/place metadata,
and preserves unrecognized project settings in `ro-sync.json`.

Success returns `status: "created"` or `"existing"`, the canonical `project`
path, metadata, initialized-file flags, and `reconnectRequired: true`. It also
appends a capability-free `project-init` audit entry and broadcasts a
watcher-visible `project-init` event so Desktop can start the newly created
project's managed daemon. The plugin does not spawn a process; it displays a
waiting state and probes for the matching `GameId` daemon until Desktop makes
it available. Errors use `{ "ok": false, "error": { "code", "message" } }`;
the stable codes are `PROJECT_INIT_UNAVAILABLE`, `UNAUTHORIZED`,
`INVALID_REQUEST`, `INVALID_METADATA`, `INVALID_PROJECTS_ROOT`,
`PROJECT_PATH_COLLISION`, `PROJECT_PATH_ESCAPE`, and `PROJECT_INIT_FAILED`.

## Filesystem sync operations

Plugin → daemon operations are sent in a WebSocket `push` frame. Daemon →
plugin operations are sent one at a time in an `op` frame:

```json
{"type":"push","ops":[{"op":"update","path":["Workspace","Main"],"properties":{"Source":"print('hi')"}}]}
{"type":"op","op":{"op":"delete","path":["Workspace","Old"]}}
```

Supported sync operation payloads:

- `set`: `path` is the parent instance path and `node` contains `class`,
  `name`, `properties`, and `children`.
- `update`: `path` identifies an existing script; the only mirrored property
  is `Source`.
- `delete`: `path` identifies the removed instance.
- `rename`: plugin → daemon uses `path` plus `name`; daemon → plugin uses
  `from` plus the full destination path in `to`.
- `move`: `from` and `to` are full instance paths in the sync pipeline.
- `class_change`: daemon → plugin replaces a script class while preserving
  its destination identity and `Source`.

Nodes outside the four mirrored classes may appear as pass-through containers
while carrying mirrored descendants, but their properties remain
Studio-authoritative.

## Remote-control request/response

CLI commands use correlated WebSocket frames:

```json
{"type":"request","request_id":42,"op":"get","args":{"path":"Workspace/Part"}}
{"type":"response","request_id":42,"ok":true,"value":{"class":"Part"},"meta":{"op":"get","durationMs":1,"protocol":2}}
{"type":"response","request_id":43,"ok":false,"error":{"code":"NOT_FOUND","message":"instance not found: Workspace/Missing","retryable":false,"details":{"op":"get"}},"meta":{"op":"get","durationMs":0,"protocol":2}}
```

Every response repeats the numeric `request_id`. Successful responses carry a
`value`; failed responses carry an error object with stable `code`, readable
`message`, `retryable`, and optional `details`. Current plugin error codes are
`UNKNOWN_OP`, `NOT_FOUND`, `PERMISSION_REQUIRED`, `TIMEOUT`,
`INVALID_ARGUMENT`, `CONFLICT`, and the fallback `PLUGIN_ERROR`. The daemon
preserves the whole envelope, so `--raw` callers can branch on codes rather
than parsing error prose.

The plugin dispatch table is `remoteHandlers`. Tagged values used by commands
such as `set`, `new`, and attribute writes are decoded by
`decodeRemoteValue`; values returned to the CLI are encoded by
`encodeRemoteValue`.

Common tagged shapes include:

| Roblox value | JSON shape |
| --- | --- |
| `Vector2` | `{"__type":"Vector2","x":0,"y":0}` |
| `Vector3` | `{"__type":"Vector3","x":0,"y":0,"z":0}` |
| `Color3` | `{"__type":"Color3","r":1,"g":1,"b":1}` |
| `UDim` | `{"__type":"UDim","scale":0,"offset":0}` |
| `UDim2` | `{"__type":"UDim2","x":{"scale":0,"offset":0},"y":{"scale":0,"offset":0}}` |
| `CFrame` | `{"__type":"CFrame","components":[12 numbers]}` |
| `BrickColor` | `{"__type":"BrickColor","name":"Medium stone grey"}` |
| `EnumItem` | `{"__type":"EnumItem","enum":"Material","name":"Plastic","value":256}` |
| `NumberRange` | `{"__type":"NumberRange","min":0,"max":1}` |
| `Instance` | `{"__type":"Instance","path":"Workspace/Part","class":"Part"}` |

Primitive booleans, finite numbers, and strings pass through directly. A
decoder error is returned to the requesting CLI rather than silently
substituting a default.

## Capability discovery

The read-only `capabilities` operation is the feature-negotiation entrypoint:

```json
{"type":"request","request_id":1,"op":"capabilities","args":{}}
```

Its value identifies `pluginVersion` (`2.1.0`), `protocolVersion` (`2`), the
Studio/host DataModel and place/game IDs, limits, current screenshot permission,
and feature flags. `features.photo`, `features.photoTransparent`,
`features.photoUiOnly`, `features.photoCameraCFrame`,
`features.photoUiTarget`, `features.photoInstanceTightCrop`, and the `photoAxis`,
`photoPixels`, and
`photoChunkBytes` limits describe the locally packaged Photo engine
independently of `features.capture` and Studio screenshot permission. Agents
should check this document before using an optional Studio API instead of
inferring support from a version string.

## Projected live query

`query` matches a `/`-separated selector inside Studio (`*` for one segment,
`**` for zero or more) and returns only requested properties, attributes, and
tags. Matching is memoized and bounded by selector length, 128 segments, 32
projected properties, 10,000 matches, a traversal-node budget, a wall-clock
budget, and a 4 MiB encoded response budget. Partial results set `truncated`
and a machine-readable `truncationReason` (`matches`, `nodes`, `time`, or
`response-bytes`) plus visited-node/response-byte counters.

## Screenshot and artifact transport

Permission-gated screen capture uses the following correlated operations:

- `capture_status` reports API availability, current permission, packaged
  `photoAvailable` / `photoUiOnlyAvailable` /
  `photoCameraCFrameAvailable` / `photoUiTargetAvailable` /
  `photoInstanceTightCropAvailable` state,
  `photoAuthorizationRequired` (`false`), and the cached `providerUnsupported`
  / `providerError` result without prompting.
- `capture_authorize` explicitly calls Studio's permission request and may
  show a user prompt. If Studio returns its exact `Feature not supported yet`
  stub error, the plugin caches and returns that state instead of treating an
  ordinary missing authorization as provider failure. The macOS CLI then uses
  this same explicit command to request native screen-capture permission.
- `capture_prepare` accepts optional `position`, `captureSize`, `outputSize`,
  `ui`, `resample`, and legacy scene `focus` / `view` / `padding`; it returns a
  short-lived `sessionId`, dimensions, position, and `byteLength`. New subject
  and viewport renders use the Photo operations below.
- `capture_export` streams one prepared PNG to an artifact lease;
  `capture_close` releases a session early. `capture_read` remains the bounded
  chunk primitive.

Artifact bytes use a separate localhost HTTP channel so a 4K image is not one
huge WebSocket JSON frame:

```text
POST /artifacts/lease
POST /artifacts/:id/chunk
POST /artifacts/:id/read
POST /artifacts/:id/finalize
POST /artifacts/:id/abort
POST /artifacts/:id/consume
GET  /artifacts/:id
```

A lease contains an opaque random ID and token. Chunks are base64 at the HTTP
edge, must append at the exact next offset, and are bounded in size and total
bytes. The token is valid only until finalize/abort/expiry. Finalization checks
the expected size and optional SHA-256, atomically promotes the private staging
file, and returns absolute path, MIME, size, and digest metadata. Tokens are
never returned by lookup or final artifact metadata.
The bounded `read` route serves finalized chunks by unguessable artifact id and
echoes exact offsets, total length, EOF state, and digest. It lets a destination
Studio plugin consume a CLI-uploaded native clipboard without putting large
binary bodies on the command WebSocket.
The CLI looks finalized metadata up by the lease ID, verifies bounded bytes,
MIME, dimensions, PNG structure, size, and SHA-256, writes its requested
output, then consumes the transport file. Finalized files that are not consumed
have a TTL, LRU ordering, and a global byte budget. Pending uploads have
separate lease-count and reserved-byte budgets; expired owned crash leftovers
are removed on startup or cleanup, and partial/finalized entries are bounded.

## Native cross-project clipboard

`clipboard_copy` resolves explicit `paths` or the current Studio Selection,
removes duplicate and nested roots, refreshes open Script Editor text, and
serializes every root together with
`SerializationService:SerializeInstancesAsync`. Serializing roots in one call
preserves references between them. The plugin uploads the opaque `.rbxm`
buffer through an artifact lease and returns only bounded root metadata, size,
and digest information; binary bytes and script source never enter command
responses or activity frames.

The CLI verifies and atomically installs that payload in Ro Sync's private,
platform-native state directory. `clipboard_paste` receives a short-lived
artifact id on the destination daemon, reads exact bounded chunks, verifies the
complete SHA-256 with `EncodingService`, and calls
`SerializationService:DeserializeInstancesAsync`. Every destination parent is
resolved before mutation. Default destinations use a segment route containing
name, class, and same-class/name sibling ordinal, so `/` inside a legal Studio
name and duplicate sibling names remain unambiguous; `--to` remains an explicit
human path override. Parenting and default Selection replacement happen
inside one `ChangeHistoryService` recording; errors destroy all detached or
partly inserted roots and cancel the recording.

Services and other non-creatable roots are rejected. References outside the
copied roots cannot cross places, matching native Studio copy/paste. Copy and
paste audit records contain counts and paths only—never lease tokens, base64,
artifact contents, or script text.

## Locally packaged Photo transport

`rosync capture photo` and the compatibility `capture scene` alias use Ro
Sync's child `Photo` module. This path does not require screenshot permission,
does not load any capture dependency from the open place, and does not use the
screenshot artifact lease:

- `photo_prepare` accepts optional `focus`, `nativeRect`, `outputSize`, `view`,
  `direction`, tagged `cameraCFrame`, `padding`, `fieldOfView`, `background`,
  `alphaBleed`, `isolate`, `tightCrop`, `uiMode`, `uiTarget`, legacy `hideUI`,
  `delay`, and `timeoutSeconds`. `cameraCFrame` contains the 12 finite
  components returned by `CFrame:GetComponents()` and supplies an exact camera pose in place of
  automatic view/direction/padding framing. `uiTarget` is a Studio path to a
  `ScreenGui` or `GuiObject`, captures only that subtree, and requires
  `uiMode: "only"`. `nativeRect` is a
  viewport-native `{x,y,width,height}` rectangle and cannot accompany `focus`;
  `outputSize` is `{x,y}`. `background` is `transparent` or `scene`. `uiMode`
  is `none`, `overlay`, or `only`; `only` requires a transparent background,
  cannot accompany `focus`, and returns the edit-mode ScreenGui layer without
  the 3D world or Studio chrome. For targeted UI without `nativeRect`, the
  engine returns a tight rendered-alpha crop; an explicit rectangle overrides
  the automatic target bounds. For an isolated focus with a transparent
  background, `tightCrop` defaults to `true` and crops the rendered subject's
  alpha bounds. `outputSize` aspect-contains that crop in the exact transparent
  canvas, including when `cameraCFrame` supplies the view. Set `tightCrop` to
  `false` (CLI `--no-tight-crop`) to retain the full camera-framed render.
  `capture photo` requests with a scene background and non-isolated/include-world
  captures remain framed and are not subject-alpha cropped. The isolated,
  transparent `capture scene` compatibility alias inherits the tight-crop
  default and the same opt-out.
- A successful prepare returns `sessionId`, `width`, `height`, `byteLength`,
  `background`, `uiMode`, `isolated`, `tightCrop`, optional `region` /
  `fullSize`, and target or exact-camera metadata when used. A focused
  automatic crop reports `tightCrop: true` and
  `regionSource: "subject-alpha"`. Pixel data is
  tightly packed RGBA8, so `byteLength` must equal `width * height * 4`.
- `photo_read` accepts `sessionId`, the exact next `offset`, and optional
  `maxBytes`. It returns `offset`, `nextOffset`, `eof`, and `bytesBase64`.
  Chunks are contiguous and bounded to 512 KiB before base64 encoding.
- `photo_close` releases the RGBA session. Sessions expire after 120 seconds,
  only two may remain active, and Photo preparation is serialized.

Photo dimensions are limited to 4096 pixels per axis and 16,777,216 pixels
total. The CLI revalidates dimensions, exact RGBA length, chunk offsets, and
EOF, then encodes and validates the PNG locally. Subject capture clones without
scripts and isolates by default; protected cleanup restores camera, in-game UI,
and Lighting state and destroys temporary clones after both success and failure.

## Playtest runtime routing

Only the edit-mode plugin connects to the daemon. When Studio clones the plugin
into a PlayServer or PlayClient DataModel, that copy starts a lightweight
runtime agent and connects to edit mode through `PluginConnectionService`.
Runtime messages carry their own request IDs inside the plugin-to-plugin link;
each CLI-started generation also carries a random session token injected through
`StudioTestService` test args. Runtime hello/request/response frames are SHA-256
authenticated, replayed request IDs are ignored, and contexts are scoped to the
matching playtest job. The raw session token is never included in runtime hello
or response metadata. The edit plugin exposes runtime operations externally through:

- `playtest_start`, `playtest_status`, `playtest_contexts`, `playtest_wait`,
  and `playtest_stop` for asynchronous job lifecycle.
- `playtest_request` with `{context, op, args, timeout}` for a named `server`
  or `client:N` runtime.
- `playtest_capture` for artifact-backed runtime screenshots.
- `playtest_run_start`, `playtest_run_poll`, and `playtest_run_cancel` for a
  foreground, playscript-owned job lifecycle.

Supported runtime operations include `exec`, `logs`, `ui_tree`, `input`,
`capture_prepare`, `capture_read`, and `capture_close`. Runtime hello metadata
includes the role, host DataModel type, target/runtime IDs, place/game, and
client player identity when available. Game-identity execution uses a temporary
Script/LocalScript in the playtest clone; it is destroyed after completion.
Plugin-identity execution is opt-in. Runtime changes never enter the edit-mode
sync pipeline. Input sequences, capture dimensions/bytes/session counts, and
runtime serialization breadth are bounded. Plugin-identity timeouts cancel the
cooperating execution thread, but tasks deliberately spawned by user code are
outside that cooperative cancellation boundary.

### Playscript-owned playtest sessions

`playtest_run_start` composes job creation and boot-time source injection. Its
request is:

```json
{
  "clientRunId":"8b1f...128-bit-client-token...",
  "mode":"multiplayer",
  "players":2,
  "context":"server",
  "identity":"game",
  "script":{"path":"bench.server.luau","source":"..."},
  "clientScript":{"path":"join.client.luau","source":"..."},
  "scriptArgsJson":"{\"laps\":3}",
  "timeout":600,
  "logs":"warn",
  "keepOpen":false
}
```

The plugin hashes the received sources itself for the audit record and never
puts source text in that record. `scriptArgsJson` preserves the exact validated
JSON value, including top-level `null`; `scriptArgs` is accepted as the decoded
fallback. `clientRunId` is a required caller-generated token of at most 128
bytes. A session-local, content-fingerprinted mapping makes start idempotent:
the first call returns `{job,run,reused:false,clientRunId}`, while an equivalent
replay returns the same job with `reused:true` and does not launch or audit a
second session. Reusing a key with different source hashes, args, paths, mode,
context, identity, timeout, logs, players, keep-open state, or test args is an
error. The mapping is limited to 64 entries; inactive entries expire after ten
minutes and active entries are never evicted.

`playtest_run_poll` accepts
`{jobId,afterSeq,waitSeconds,maxEvents,maxBytes}` and returns
`{frames,nextSeq,lastSeq,hasMore,heartbeats,heartbeatStale,run,job}`. The cursor
is the last sequence actually delivered, so the caller can pass `nextSeq`
unchanged. `playtest_run_cancel` accepts `{jobId,reason,outcome,force}` and does
not return until the cleanup attempt settles. Its `cancelled` and
`cleanupConfirmed` fields are false when runner cleanup or, unless a non-forced
keep-open run is being retained, playtest teardown cannot be confirmed; the
returned terminal is then the canonical `aborted` outcome.
Status, poll, and cancel also accept `clientRunId` when `jobId` is unavailable;
a supplied unknown key never falls back to the latest job. Canceling an unknown
key records a 120-second cancellation tombstone (also capped at 64), so a start
that was still queued when cleanup arrived is rejected before launching.
A forced cancel by key also tears down an already-terminal keep-open job, which
prevents a fast completion plus lost start response from leaving an ownerless
Studio session.

Edit/runtime coordination uses `kind: "rosync-playscript-frame"`. Each frame
carries `playtestJobId`, `runtimeId`, a monotonic per-direction `seq`, an encoded
`bodyJson`, and an authenticator derived from the private generation token,
direction, runtime ID, sequence, and SHA-256 of the body. Stale generations,
replayed sequences, mismatched runtime IDs, and invalid authenticators are
ignored. Edit-to-runtime frame types are `boot`, `signal`, `clients`, and
`cancel`; runtime-to-edit types are `booted`, `bootFailure`, `heartbeat`,
`event`, `log`, `signal`, `dropped`, `clientResult`, and `complete`. Result
payloads are fetched separately with the internal `playscript_result_read` and
`playscript_result_close` bounded-chunk operations; `playscript_cancel`
provides confirmed runner cancellation.
Boot sends are retried until an authenticated `booted` or `bootFailure` frame.
The runtime distinguishes installing, installed, and failed runners: duplicate
boot frames re-ack only a fully installed runner (including one that already
finished), never execute source twice, and replay a retained installation
failure. A companion boot failure is terminal for that companion attempt and
produces one `clientResult` instead of a retry flood.

The injected source receives this runtime surface:

```lua
playtest.args
playtest.mode
playtest.context
playtest.jobId
playtest.emit(value)
playtest.log(message)
playtest.done(value)
playtest.fail(message)
playtest.signal(name, payload)
playtest.await(name, timeoutSeconds)
playtest.awaitClients(count, timeoutSeconds)
```

The first main return/error, explicit `done`/`fail`, timeout, boot failure, or
external end claims completion. A companion client's ordinary return/error is
only a `clientResult`; explicit `done`/`fail` is global. Ordinary completion is
not exposed to polling until owned teardown finishes. If teardown or keep-open
runner cancellation cannot be confirmed, the terminal outcome is `aborted`
with the observed job status and cleanup error instead of a false success.
`keepOpen` retains the Studio job only after every connected runtime confirms
its playscript runner was cancelled.

Source events and signals use a 20/s token bucket with burst 40 and a 64 KiB
encoded-value ceiling. Loss is reported with counted `dropped` frames. Pending
signals are limited to 100 total values per runner (not 100 per signal name),
and the generation backlog is also limited to 100 values and 30 seconds.
Heartbeats run every two seconds. A missing/disconnected selected context
aborts the run; a connected but stale heartbeat is reported without overriding
the hard timeout while the Studio job is still active. Polls return at most 64
frames / 512 KiB, the coordinator queue is bounded, and encoded final values
are capped at 1 MiB with an explicit `{truncated:true,bytes:N}` marker. Result
sessions are length- and SHA-checked, chunked at 96 KiB, limited to 16, and
expire after 120 seconds.

Start and completion each produce a write audit record. Completion records the
outcome, mapped exit code, final job status, and elapsed time. Playscripts run
only in the disposable playtest DataModel and never enter edit mode or disk
sync.
The first launch audit is attempted synchronously before its start response;
if the audit endpoint rejects it, the generic write lane retries without
abandoning an already-started playtest. Idempotent start replays suppress an
extra generic write audit. Completion audit is likewise attempted before the
terminal becomes visible to poll/cancel, so the healthy path cannot race the
required completion record.

## Workflow support operations

The workflow schema is a CLI contract (`rosync run --file`), not a second wire
format. The CLI validates all schema-v1 steps and references, opens one
persistent WebSocket session, then maps each step onto ordinary protocol 3
requests. Three operations add safe workflow semantics:

- `inspect_ref` checks a live target and rejects mismatched `expectedClass` or
  `etag` before a step executes.
- `transaction_begin` starts a named Studio change-history recording.
- `transaction_finish` commits or cancels that recording; cancel rolls the
  in-progress changes back.

Workflow result references are exact JSON strings such as
`$stepId.value.properties.Name` and preserve the selected JSON type. A leading
`$$` is an escape. Atomic groups must be contiguous and may contain only
bounded operations with understood change-history behavior. `verify: true`
causes supported writes to be read back before the step is reported successful.
Assertions and waits are evaluated by the workflow executor, while capture,
playtest, and upload steps use their dedicated transports.

## Source writes

Filesystem source is applied with
`ScriptEditorService:UpdateSourceAsync()`. Failure is reported as a failed
apply; the plugin does not fall back to assigning `.Source`, because doing so
can overwrite an open Studio editor draft.
