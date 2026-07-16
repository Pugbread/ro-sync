# Ro Sync release verification — 2026-07-16

Environment: macOS arm64, Roblox Studio `0.730.0.7300790`, Race Stars 2,
daemon port `7878`. The release plugin and daemon were rebuilt, installed,
reloaded, and tested through the public CLI against the live edit DataModel.

## Automated release gates

- `cargo fmt --all -- --check`: pass.
- `cargo test --locked`: 382 passed.
- `cargo clippy --locked --all-targets -- -D warnings`: pass.
- Widget JavaScript syntax checks for the app, bridge, platform, and active
  views: pass.
- Command docs regenerated successfully: 56 commands.
- Plugin RBXM build: pass.
- `plugin/Photo.luau` strict Luau analysis: pass with no diagnostics.
- Photo and full plugin bytecode compilation at `-O0`, `-O1`, and `-O2`:
  pass. This includes the compiler register-limit check.
- `git diff --check`: pass.

The full plugin's standalone LSP audit still reports only the known incomplete
Studio definition surface (`Plugin:GetSetting`, `Plugin:SetSetting`,
`Plugin:CreateToolbar`, `Plugin.Unloading`, and related host-only members).
No diagnostic points into the new Photo/capture implementation, and bytecode
compilation succeeds at every optimization level.

## Exact camera CFrame capture

- The capability handshake reports `photoCameraCFrame: true` and capture status
  reports `photoCameraCFrameAvailable: true`.
- `--camera-cframe` accepted an exact 12-component world-space
  `CFrame:GetComponents()` value with a 7-degree roll and returned the same
  tagged value in artifact metadata.
- Isolated rendering preserved the requested subject-relative position,
  orientation, and roll after moving the clone to the remote staging area.
- Repeating the same CFrame at FOV 32 and FOV 60 changed only the expected
  apparent scale. Artifact metadata returned the effective FOV.
- Wide `1024x640` and square `768x768` renders measured car alpha bounds of
  `627x271` and `564x244`; their ratios agree within 0.1%, proving output aspect
  changes crop rather than stretch the model.
- The `--include-world` path used the exact world CFrame directly and also
  returned an aspect-preserving crop.
- Camera CFrame and FOV restored byte-for-byte to their pre-capture values, and
  no `RoSyncNativeTarget` clone remained.
- A reflected/non-right-handed matrix failed in the CLI before network I/O and
  wrote no output.

## Isolated UI-target capture

- The capability handshake reports `photoUiTarget: true` and capture status
  reports `photoUiTargetAvailable: true`.
- A targeted `Frame` (`StarterGui/HUD/Lobby/Top`) produced a tight
  `544x131` transparent crop at native region `588,0,544,131` with
  `regionSource: "target-alpha"`. Only the selected element and descendants
  were present.
- A nested `ImageLabel` target rendered independently with its descendants.
- A whole `ScreenGui` target rendered while every unrelated layer was hidden.
- A disabled nested Settings `ScreenGui` rendered successfully, and its
  original `Enabled = false` state was restored afterward.
- `--size 900x600` aspect-contained the `544x131` target as a `900x217` alpha
  region centered in a transparent exact-size canvas.
- Combining the target with explicit native region `588,0,544,131` and
  `--size 600x600` retained the existing exact-fill behavior and returned
  `regionSource: "explicit"`.
- An empty UI target failed with `UI target rendered no visible pixels`; a
  `BillboardGui` target failed class validation. Neither wrote an output.
- Successful and failed captures left no `RoSyncCaptureUI*` instances, and the
  original HUD/Settings enabled states were unchanged.

## Automatic instance tight crop

- Isolated transparent instance captures now tight-crop visible subject alpha
  by default; `--no-tight-crop` retains the previous camera-framed render.
- Wide and square default captures used the same native `413x521` alpha bounds
  and aspect-contained them into the requested exact output canvas without
  stretching.
- An exact subject-relative camera CFrame and FOV retained the automatic crop;
  the opt-out preserved the full exact-camera framing.
- A 95%-transparent subject remained detectable, while a fully invisible
  subject failed without creating an output file.
- `capture scene` inherited the same default and produced output identical to
  `capture photo` for the equivalent isolated request.
- Scene-background and include-world requests remained framed. The opaque-world
  fallback was exercised against a full-frame wall and returned an opaque PNG.
- Camera CFrame and FOV were restored byte-for-byte after the matrix, no
  temporary capture instances remained, and every temporary Race Stars fixture
  was removed.

## Installed artifacts

- Release daemon and installed widget binary SHA-256:
  `4f0d6760d2746cf2a57831cf8a58d4470e58bd09695be9379310348cc49d8db5`.
- Built and installed Studio plugin SHA-256:
  `3f74239d3d1433a27184db4439e8cc0b40c52649567bfd3eee3af9ac1f7450e8`.
- Live `rosync status` after reload: daemon reachable, plugin connected,
  Race Stars 2 on port `7878`. The disposable port `7879` listener was stopped.
- Visual mode sheet:
  `t64/images/release-capture-matrix-2026-07-16/release-capture-modes-sheet.png`
  (SHA-256
  `2fa8001fcfb2bb3176fa465e8a95c92e5af6bd91beb2e6ca3fc637658c2a123a`).
- Automatic tight-crop sheet:
  `t64/images/race-stars-tightcrop-2026-07-16/race-stars-tightcrop-contact-sheet.png`
  (SHA-256
  `9862a45a43f2cce31c2bafe362c0ebbff88036dd0617ed3027fbc163bf223429`).
