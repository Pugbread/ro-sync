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
{"type":"hello","clientId":"123456789","role":"plugin","protocol":2,"pluginCapability":"<64 hex characters>"}
```

The daemon rejects a missing/incompatible protocol or capability with a
`shutdown` frame. Every socket sends exactly one hello using protocol 2;
`plugin`, command-capable CLI/agent, and read-only widget/watch roles receive
only the traffic appropriate to that role. The daemon replaces caller request
IDs with private correlation IDs before routing them to Studio.
Origin-bearing browser HTTP/WebSocket requests must also carry the owning
widget's capability as `?widgetToken=...`; native loopback Studio/CLI clients
do not send an Origin header.
Protocol 2 corresponds to Studio plugin 2.0.0 and adds structured errors,
capability discovery, artifact-backed capture, playtest runtime routing, and
workflow transaction/precondition operations.

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

Its value identifies `pluginVersion` (`2.0.0`), `protocolVersion` (`2`), the
Studio/host DataModel and place/game IDs, limits, current screenshot permission,
and feature flags. `features.photo`, `features.photoTransparent`,
`features.photoUiOnly`, `features.photoCameraCFrame`,
`features.photoUiTarget`, and the `photoAxis`, `photoPixels`, and
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
  `photoCameraCFrameAvailable` / `photoUiTargetAvailable` state,
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
The CLI looks finalized metadata up by the lease ID, verifies bounded bytes,
MIME, dimensions, PNG structure, size, and SHA-256, writes its requested
output, then consumes the transport file. Finalized files that are not consumed
have a TTL, LRU ordering, and a global byte budget. Pending uploads have
separate lease-count and reserved-byte budgets; expired owned crash leftovers
are removed on startup or cleanup, and partial/finalized entries are bounded.

## Locally packaged Photo transport

`rosync capture photo` and the compatibility `capture scene` alias use Ro
Sync's child `Photo` module. This path does not require screenshot permission,
does not load any capture dependency from the open place, and does not use the
screenshot artifact lease:

- `photo_prepare` accepts optional `focus`, `nativeRect`, `outputSize`, `view`,
  `direction`, tagged `cameraCFrame`, `padding`, `fieldOfView`, `background`,
  `alphaBleed`, `isolate`, `uiMode`, `uiTarget`, legacy `hideUI`, `delay`, and
  `timeoutSeconds`. `cameraCFrame` contains the 12 finite components returned by
  `CFrame:GetComponents()` and supplies an exact camera pose in place of
  automatic view/direction/padding framing. `uiTarget` is a Studio path to a
  `ScreenGui` or `GuiObject`, captures only that subtree, and requires
  `uiMode: "only"`. `nativeRect` is a
  viewport-native `{x,y,width,height}` rectangle and cannot accompany `focus`;
  `outputSize` is `{x,y}`. `background` is `transparent` or `scene`. `uiMode`
  is `none`, `overlay`, or `only`; `only` requires a transparent background,
  cannot accompany `focus`, and returns the edit-mode ScreenGui layer without
  the 3D world or Studio chrome. For targeted UI without `nativeRect`, the
  engine returns a tight rendered-alpha crop; an explicit rectangle overrides
  the automatic target bounds.
- A successful prepare returns `sessionId`, `width`, `height`, `byteLength`,
  `background`, `uiMode`, `isolated`, optional `region` / `fullSize`, and target
  or exact-camera metadata when used. Pixel data is
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

## Workflow support operations

The workflow schema is a CLI contract (`rosync run --file`), not a second wire
format. The CLI validates all schema-v1 steps and references, opens one
persistent WebSocket session, then maps each step onto ordinary protocol 2
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
