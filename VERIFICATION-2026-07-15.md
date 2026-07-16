# Ro Sync verification — 2026-07-15

Environment: macOS arm64, Roblox Studio `0.730.0.7300790`, Race Stars 2 on
final daemon port `7879`. Live mutations were restricted to disposable verification
fixtures and removed afterward. The place was never saved by this test run.

## Automated gates

- `cargo fmt --check`: pass.
- `cargo test --locked`: 354 passed after a clean full rerun.
- `cargo clippy --locked --all-targets -- -D warnings`: pass.
- StyLua check for `plugin/Plugin.luau`, `plugin/Photo.luau`, and
  `plugin-src/src/App.luau`: pass.
- Official Luau 0.729 bytecode compilation at `-O0`, `-O1`, and `-O2` for the
  plugin and independently authored native Photo sources: pass. This also
  proves the prior 200-local-register failure is gone.
- Widget JavaScript syntax and cross-platform command checks: pass.
- Command docs regenerated successfully: 56 commands.
- `git diff --check`: pass.
- Case-insensitive source, generated-file, packaged-plugin, installed-skill,
  and binary-string reference scans: zero matches, excluding Git history and
  the explicitly out-of-scope game-owned source tree.

## Capture

- Read-only status did not prompt for permission.
- Explicit authorization detected Studio's current
  `Feature not supported yet` provider result and authorized the scoped macOS
  Roblox Studio-window fallback.
- Full-window `1024x576`, region `200,120,640,360 -> 800x450`, and pixelated
  `512x512 -> 256x256` captures produced valid PNGs with exact dimensions,
  SHA-256 metadata, and the selected Roblox Studio window identity.
- A region outside the Studio window was rejected and wrote no file.
- `--ui none` did not silently fall back to a UI-bearing capture.
- The independently authored, source-blind native Photo engine superseded the
  permission-gated scene path and requires neither screenshot authorization nor
  a place-provided capture dependency. Its public module surface is limited to
  `captureView(options)` and `renderInstance(subject, options)`.
- Final exact-size artifacts include `native-scene-640x360.png`,
  `native-scene-ui-640x360.png`, `native-region-120-80-320x180.png`,
  `native-common-transparent-512.png`, and
  `native-wheels-crate-640x384.png`. The UI/no-UI pair differed by 21,043
  pixels (about 9.13%), proving the UI option changed the captured result.
- Checkerboard QA confirmed real alpha (`0..1`) rather than a baked matte. The
  final `native-wheels-crate-640x384.png` SHA-256 is
  `1f2eec3ec6edeb2ab184a7389ceb440351d0def06d4ce61b0ae00a8dc361e2d8`.
- The maximum-limit `native-common-transparent-4096.png` capture completed in
  `8.00s`, is exactly `4096x4096`, has alpha minimum `0` and maximum `1`, and
  has SHA-256
  `be2d0b99b4dcb37552c221b4412e56644c3bc8bf3aea40c88cc23dc706f5eed9`.
- A focused capture restored camera CFrame and FOV exactly. A focus with no
  renderable parts failed without writing an output file, and post-capture
  inspection found no temporary capture instances.
- After installing and restarting the final daemon, an end-to-end scene capture
  returned `ok: true`, `consumed: true`, and an exact `320x180` PNG.
- Final installed release binary SHA-256:
  `f863e4fcd6a69cdfdcd83a564fd9db3da55efd92f4837d3f38ee2b4bc2883c7e`.
- Deterministic plugin build/install SHA-256:
  `8e1cd6c2864e25f83f5b8a520635771c23866e778e1c8595b198599bb4a87da9`.
- Final UI proofs: `.rosync-artifacts/verification/rosync-studio-final.png`
  (`1224x768`, SHA-256
  `2b84eef8641d8f959ece183b6196b59760d66e04f8d726055d5988f72eefe2c5`)
  and `.rosync-artifacts/verification/rosync-widget-final-native.png`
  (`284x255`, SHA-256
  `8f5fb963b75f0eec7b2a251fc0b2288798c7b50cdf725dcc60cc0418ac395d59`).

## Playtest runtime

- Start/handshake: pass; edit, server, and client host types were distinguished
  without relying on unavailable `plugin.HostDataModelType`.
- Game-identity and plugin-identity execution passed on server and client.
- Typed results, log bounds/filtering, UI inspection, and virtual input passed.
- Deliberately invalid `local =` returned the exact parser error immediately
  instead of hanging for ten seconds.
- Two parallel server/client executions completed together at about `0.10s`
  each without ordered-queue starvation.
- Stop completed through PlayServer in about `1.63s`; no contexts remained.
- Old job/context identifiers were rejected after restart.

## Workflows and guardrails

- Offline validation rejected typoed fields, forward references, dynamic
  `Parent`, and eval/unbounded work in atomic groups before a Studio write.
- An intentionally failing atomic workflow rolled the first write back and
  returned explicit `rolledBack: true` transaction output.
- Idempotent replay returned `replayed: true` with no duplicate side effect;
  reuse of the key with different content was rejected.
- Batched `Parent` writes are rejected before the network unless the explicit
  force flag is present.
- Protocol 2/plugin 2.0.0 capability negotiation, strict mismatch rejection,
  and hexadecimal etags passed automated/live checks.

## Two-way sync and conflicts

- CLI `new` with an initial `Source` and later CLI Source changes reached disk
  byte-for-byte. Disk source edits reached Studio byte-for-byte.
- Studio rename retained content and renamed the existing file; a clean
  follow-up delete removed it from disk without a false conflict.
- A pre-existing empty `Workspace/tools` folder, renamed to
  `Workspace/utilities` before its first script was added, materialized the
  required Studio parent chain and synced the exact script source.
- Source divergence produced a deterministic conflict with the correct local
  and Studio bytes. `resolve --disk` pushed local to Studio; a repeat conflict
  with `--studio` wrote Studio to disk.
- Studio deletion against a local edit reported `studioDeleted: true`.
  `resolve --disk` recreated the ModuleScript with retained local bytes; a
  repeat with `--studio` removed the retained disk file.
- Disk deletion against a divergent Studio source reported
  `localDeleted: true`. `resolve --studio` atomically restored the exact Studio
  bytes; a repeat with `--disk` deleted the live Studio script.
- Disk rename against a divergent Studio source reported the exact
  `localRenamedTo` path. `resolve --studio` restored the original disk path and
  Studio bytes; a repeat with `--disk` renamed Studio and retained the exact
  destination bytes.
- Two daemon restarts ran through content-based initial comparison. Complete
  pre-decision backups and byte-for-byte service-tree comparisons confirmed
  that only the isolated fixture changed; no user script was overwritten.
- Script-with-children rename is transactional, rebases conflict baselines,
  and rolls the outer directory back if the named-init rename fails.
- Portable filename tests cover Unicode case/normalization aliases and legacy
  literal-Unicode script-with-children paths.
- At the conclusion of the earlier two-way-sync regression suite,
  `rosync changes` reported `0 added / 0 removed / 0 changed`, conflicts were
  empty, playtest had no contexts, and all eight synced service trees were
  byte-for-byte identical to the pre-test backup.

## Widget lifecycle

- Rebuild and Restart preserve the active fallback port instead of silently
  returning to `7878` and stranding Studio.
- Restart waits for the old PID/port to be released before probing or
  relaunching, eliminating reuse of a dying daemon.
- Widget state writes are serialized so older asynchronous snapshots cannot
  overwrite a newer daemon project/port target.
- Live regression: a temporary listener occupied `7878`, Ro Sync launched on
  `7879`, the listener was removed, and Restart replaced the daemon PID while
  remaining on `7879`. Studio then reconnected successfully.

## Known provider limitation

Roblox Studio exposes `StudioCaptureService` in this channel but its screenshot
permission call throws `Feature not supported yet`. Ro Sync therefore supports
screen + Studio-UI capture on macOS through the explicitly authorized native
window fallback. At that test point, viewport-only and legacy scene capture
failed closed pending Studio-provider support; Ro Sync did not disguise a
UI-bearing native image as a viewport-only capture.

This provider result describes the original `StudioCaptureService` path tested
in this report. The independently authored native Photo engine supersedes the
scene conclusion: `capture photo` and the `capture scene` alias now work without
Studio screenshot authorization or a place-provided capture dependency.

A whole-directory disk delete cannot currently reconstruct every retained
Studio descendant from a single conflict record. Choosing Studio therefore
returns `DIRECTORY_DELETE_RESTORE_REQUIRES_STUDIO_PULL`, writes no partial
tree, and leaves the conflict parked. File deletes and whole-directory renames
are fully resolvable in both directions.
