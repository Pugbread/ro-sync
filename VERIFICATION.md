# Ro Sync manual verification

Use a scratch place and project. Automated Rust tests cover the daemon paths;
these checks exercise Roblox Studio signals and `ScriptEditorService`, which
cannot be reproduced outside Studio.

## Prep

1. Run `cargo test --locked` and
   `cargo clippy --locked --all-targets -- -D warnings` in `daemon/`.
2. Run `node scripts/check-luau-bytecode.mjs` with the official
   `luau-compile` on `PATH`, then rebuild with `node plugin/build-plugin.mjs`.
3. Install `plugin/Plugin.rbxm` from the widget Settings tab.
4. Start a scratch project with `rosync serve --project <path>` and connect the
   matching Studio place.

## Sync checks

| # | Scenario | Expected |
| --- | --- | --- |
| 1 | Create a `ModuleScript` named `Config` in `ReplicatedStorage`. | `ReplicatedStorage/Config.luau` appears with the current editor source. |
| 2 | Edit and save that local file. | The open Studio editor updates through `UpdateSourceAsync`; no duplicate script is created. |
| 3 | Rename the script in Studio. | The existing file is renamed without losing its contents. |
| 4 | Delete the script in Studio without local edits. | The corresponding local file is removed. |
| 5 | Recreate it, make divergent local and Studio edits, then delete it in Studio. | The local file remains and `rosync conflicts` reports a Studio deletion conflict. |
| 6 | Resolve that conflict with `rosync resolve <path> --disk`. | The script is recreated in Studio with the retained local source. |
| 7 | Repeat and resolve with `--studio`. | The retained local file is deleted. |
| 8 | Create a script, disconnect/restart the daemon, change both sides before reconnecting. | Initial compare or the conflict engine asks for a decision; neither side silently overwrites the other. |
| 9 | Delete a Studio script and a Studio folder containing scripts. | Delete operations reach disk; the plugin log includes the cached pre-removal path. |
| 10 | Open a script editor draft, make the source read-only/unwritable for `UpdateSourceAsync`, then trigger a local change. | The plugin reports the failed apply and does not assign raw `.Source` over the draft. |

## Protocol and CLI checks

| # | Scenario | Expected |
| --- | --- | --- |
| 11 | Connect an older plugin that omits protocol version 2. | The daemon closes it with an incompatible-protocol message asking for plugin reinstall. |
| 12 | Run `rosync set --batch` with an entry whose `prop` is `Parent`. | The whole batch is rejected before any network write unless `--force-parent` is explicit. |
| 13 | Create `Workspace/tools/Test.luau` locally. | It syncs normally; only a project-root `tools/` directory is watcher-ignored. |
| 14 | Run `node scripts/build-command-docs.mjs` and inspect `git diff`. | Generated command docs remain unchanged. |

## Widget smoke checks

After editing widget JavaScript, run:

```sh
node --check app.js
node --check bridge.js
node --check views/active.js
node --check views/projects.js
node --check views/settings.js
```

Confirm project switching, plugin status, initial-sync decisions, conflict
navigation, and daemon shutdown all remain responsive.

## Capture and playtest checks

| # | Scenario | Expected |
| --- | --- | --- |
| 15 | Run `rosync capabilities --project . --raw`. | Protocol 3, plugin 2.2.0, and explicit capture/playtest/runtime feature flags and limits are returned. |
| 16 | Run `rosync capture status --project . --raw` before authorization. | Status is read-only and no permission prompt appears. |
| 17 | Run `rosync capture authorize`, then capture a custom `x,y,width,height` region and a resized output. | One explicit Studio prompt is shown; the CLI writes a verified PNG with matching dimensions, size, and SHA-256 metadata. |
| 18 | Before screenshot authorization, run `rosync capture photo` for a viewport region and a focused target from each named view; repeat one focused capture through the `capture scene` alias. | No permission prompt appears; exact dimensions and valid RGBA/PNG metadata are returned, the target remains fully framed, and camera, UI, Lighting, and temporary-clone state are cleaned up after every success/failure. |
| 19 | Start Play with `--wait`, list contexts, and execute `return game.PlaceId` at game identity on `server`. | A generation-scoped `server` context appears and the typed result is returned; the temporary harness script is removed. |
| 20 | Execute a deliberately invalid/runtime-failing script and request logs from its context. | The command returns a structured error and bounded output/log entries identify the failure without hanging the test. |
| 21 | Inspect UI, send a short input sequence to `client:1`, and capture that client. | Resolved GUI visibility/geometry is reported, bounded input completes, and a verified PNG is written. |
| 22 | Stop and restart the test, then address a context from the old job. | The stale generation is rejected and cannot satisfy `wait` or receive runtime requests. |

## Workflow checks

| # | Scenario | Expected |
| --- | --- | --- |
| 23 | Dry-run a workflow containing a typoed field, forward reference, dynamic `Parent` property, or unbounded operation in an atomic group. | Validation fails before a WebSocket or Studio mutation. |
| 24 | Run an atomic workflow whose second write intentionally fails. | The whole Studio recording is canceled; the first write is rolled back and rollback status is present in raw output. |
| 25 | Run a successful workflow twice with one `idempotencyKey`. | The second run returns the stored result with `replayed: true` and performs no side effects. Reusing that key with different workflow content is rejected. |
