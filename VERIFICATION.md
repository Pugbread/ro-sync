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
| 11 | Connect an older plugin that omits protocol version 6 or reports protocol 5. | The daemon closes it with an incompatible-protocol message asking for plugin reinstall. |
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

## Windows checks

| # | Scenario | Expected |
| --- | --- | --- |
| W1 | Run `daemon/build.ps1` on Windows with `ROSYNC_SKIP_TOOL_DOWNLOAD=1`. | Cargo builds the explicit `x86_64-pc-windows-msvc` target and atomically replaces `daemon/rosync-windows-x86_64.exe` without deleting the previous working binary first. |
| W2 | Add and switch among projects whose paths contain spaces, Unicode, mixed separators, an extended `\\?\` drive path, and a UNC share. | `/hello` reattaches only to the same canonical project and exact PID/port/boot; a daemon for another clone or path is never claimed or stopped. |
| W3 | Serve several projects in the desktop app, then exit while an unrelated listener occupies another port. | Native cleanup uses at most four close workers, authenticates the exact immutable identity of every managed daemon, finishes within the shared deadline, and leaves the unrelated listener untouched. |
| W4 | Put a junction/reparse point or case-colliding alias at a synced service, descendant, config, tooling path, or Desktop-authorized project path; also retarget an ancestor between validation and use. | Daemon and Desktop handle-relative operations fail closed before reading or mutating through it; the external target remains unchanged and the error identifies the unsafe physical path. |
| W5 | Run the Windows CI and a tagged release with the Authenticode PFX secrets configured. | CI builds the source script plus unsigned MSI and NSIS installers. The tagged release signs and RFC3161-timestamps the standalone daemon, MSI, and NSIS installer, strictly verifies every signature, and publishes verified SHA-256 files. |

## Capture and playtest checks

| # | Scenario | Expected |
| --- | --- | --- |
| 15 | Run `rosync capabilities --project . --raw`. | Protocol 6, plugin 2.4.1, and explicit capture/playtest/runtime feature flags and limits are returned. |
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

## Large-project bootstrap checks

| # | Scenario | Expected |
| --- | --- | --- |
| 26 | Connect a place with at least 25,000 projected instances and thousands of scripts to a populated project. | Comparison starts with aggregate stats, then advances in service order through source-free flat structure chunks of at most 512 records and 512 KiB encoded JSON, followed by script-hash chunks of at most 64 records. Studio remains responsive while the scan yields cooperatively. |
| 27 | Choose Studio for a divergent large project and edit a local file after that service's `diskFence` is captured but before `diskRevalidate` completes. Repeat with a concurrent edit after the staged service is installed, a failure after one existing and one newly created service committed, and lookalike/replaced backup directories during retention. | Sources arrive separately in bounded, ordered, SHA-256-checked parts, with 32-MiB/script, 64-MiB/service, and 128-MiB/session limits charged atomically. A pre-commit disk change fails revalidation without replacement. Every post-backup failure restores the exact backup when safe; otherwise the terminal receipt identifies every ordered restore/remove recovery action, the plugin displays it, and reconnect remains stopped. Only canonical, completion-marked, generation-matched successful backups are pruned; lookalike, replaced, and partial recovery paths remain untouched. |
| 28 | Choose Disk for a divergent large project, then force one Source apply or later-service mutation to fail. Repeat with injected failures in both ChangeHistory cancellation and its Undo fallback. | Every service is validated and staged before the only live commit. Cancellation (with exactly one Undo only as its fallback) rolls back every service. A double rollback failure stops sync terminally and reports possible partial Studio state; it never reports a false rollback or retries the transfer. |
| 29 | Select a disk-side deletion together with an update while leaving neighboring Studio paths unselected. | The stream emits bounded generated-path delete chunks, validates the complete selected plan, commits the selected update and deletion in the same all-service recording, and preserves unselected and `AvoidSync` neighbors. |
| 30 | Rename, reparent, add, remove, or edit a watched Studio instance during stats, comparison, pull validation, or the hook-install handoff. | The mutation guard rejects the stale transfer and restarts initial comparison before live sync is enabled; Ro Sync never declares the stale view in sync. |
| 31 | Open a first-connect decision containing at least 25,000 divergent paths, page the whole list, and submit a sparse selection while retrying one chunk verbatim. | Status remains below 64 KiB, each immutable detail page remains below 512 KiB, no more than 300 file rows are live in the widget, selection requests remain below 64 KiB, the exact retry returns the same receipt, and only the selected stable IDs are authorized. |
| 32 | Deliver 25,000 modify events for files under one stable wide service directory, then deliver `A→B`, modify `B`, and `B→C` in one batch. Repeat with `A→B` then remove `B`, a swap/cycle, a cross-boundary or competing destination, a Source over 32 MiB, raw-ingress overflow, and a watcher backend error. | The directory is indexed once for the batch (plus the project-root index), every reuse is fenced by no-follow generation/identity checks, and the safe chain reaches Studio as `A→C` followed by the final `C` content update. The nonblocking raw queue retains at most four metadata batches rather than Source bodies; terminal removal becomes removal of the original identity. Each unsafe condition enters a generation-tagged quarantine and produces exactly one typed full-resync request, with raw and broadcast tails discarded before reconnect. |
