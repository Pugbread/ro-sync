# Ro Sync — manual verification (sync + widget workspace)

Covers reflection-driven `.meta.json`, `[N]` sibling disambiguation, and the
Terminal 64 widget workspace: sidebar navigation, project detail/activity,
duplicate-name chips, session spawning, and throttled live logs.

## Prep

1. Build the daemon and run tests — both must pass:
   - macOS/Linux: `bash daemon/build.sh`, then `cargo test` in `daemon/`
   - Windows PowerShell: `daemon\build.ps1`, then `cargo test` in `daemon\`
2. Use the widget Settings tab to install `plugin/Plugin.rbxm`, or copy it to
   `~/Documents/Roblox/Plugins/RoSync.rbxm`.
3. Create a scratch project folder:
   - macOS/Linux: `mkdir -p /tmp/rosync-v2`
   - Windows PowerShell: `New-Item -ItemType Directory -Force "$env:TEMP\rosync-v2" | Out-Null`
4. Open the Ro Sync widget, go to Projects from the sidebar, add project
   `/tmp/rosync-v2` on macOS/Linux or `$env:TEMP\rosync-v2` on Windows, and
   turn on its serving switch. Daemon dot should go green with `:7878`.
5. Open Roblox Studio → Plugins → RoSync → **Connect**. Plugin stat in the
   Active view should read `connected`.
6. Run widget JS smoke checks after editing UI files:
   `node --check app.js bridge.js views/active.js views/projects.js`.

## Checklist

| # | Scenario | Expected | Pass |
|---|----------|----------|------|
| 1 | In Studio, insert a `Part` into `Workspace`, set `Size = 5,1,5`, `Color = 1,0,0`, `Material = Neon`. Save/Connect. | `<scratch>/Workspace/Part.meta.json` exists with `className: "Part"` AND keys `Size`, `Color3uint8` (or `Color`), `Material` each shaped `{__type: "<TypeName>", value: ...}`. Default props (Anchored=false, CanCollide=true, etc.) are absent. | ☐ |
| 2 | In Studio, rename that `Part` to `Baseplate`. | Within 500 ms (watch Active log), `Part.luau`/`Part.meta.json` are gone and `Baseplate.meta.json` appears with the same properties. Only one pair of files exists; no orphan. | ☐ |
| 3 | In Studio, delete `Baseplate`. | Within 500 ms, `Baseplate.meta.json` is removed from disk. No error in the Active log. | ☐ |
| 4 | Recreate a `Part` named `Baseplate`. Then on disk, edit `Baseplate.meta.json` to set `"Transparency": {"__type":"f32","value":0.8}`. Save. | Within 500 ms, Studio's `Baseplate.Transparency` becomes `0.8`. The Active log shows a line with a `meta` chip reading `…/Baseplate.meta.json • Transparency + 0.8` (or `→ 0.8` if a prior value existed). | ☐ |
| 5 | In Studio, create two sibling parts under `Workspace` both named `Foo`. | Disk has `Workspace/Foo.luau`/`.meta.json` **and** `Workspace/Foo [1].luau`/`.meta.json`. No `(ClassName)`-style names anywhere. | ☐ |
| 6 | Add a third sibling `Foo`. | `Workspace/Foo [2].luau`/`.meta.json` appears. Existing `Foo` and `Foo [1]` files are untouched. | ☐ |
| 7 | Delete the middle `Foo [1]` in Studio. | `Foo [1].meta.json` is removed. `Foo` and `Foo [2]` remain (stable — no cascading rename is required; loose ordering is OK). | ☐ |
| 8 | Back on the Projects tab, wait for status to refresh. | Project card for the scratch folder shows a warning chip `1 duplicate-name group` from the remaining `Foo` siblings. Switch views / return to confirm the chip persists until duplicates resolve. | ☐ |
| 9 | Select the project card, then click **Edit** in the detail pane. | The detail pane opens the editable Game ID / Group ID / Place IDs fields plus Local Path, Plugin, Refresh Status, View Diff, and two-click Delete actions. | ☐ |
| 10 | Click **Spawn Session** from the project detail header. | Terminal 64 opens a new session with cwd set to the scratch folder. If the host lacks `t64:create-session`, the widget shows `Spawn session failed` without breaking navigation. | ☐ |
| 11 | Go to Activity and exercise a burst such as creating/deleting many Studio children. | The log remains responsive, shows op counts/last sync, and collapses saturated daemon events instead of rendering every frame. **Stop live log** pauses the stream and **Start live log** resumes it. | ☐ |
| 12 | Grep the widget JS for stale disambiguation references: `grep -n "(ClassName)" app.js views/*.js bridge.js` | No matches. | ☐ |

## UX gotchas noted while wiring the widget

* **`/snapshot` shape isn't formally documented.** `countDupeGroups()` in
  `views/projects.js` walks either `children[]`, `services[]`, or a bare
  array — if Agent 1/2 settle on a different tree key, update that visitor.
  The current daemon returns `{bootstrap, services, strict}`, so the
  duplicate-name chip stays hidden (0 groups) until the tree is populated — that's
  fine for the empty-project state.
* **Widget streams are intentionally lossy for display.** Control events still
  reach the app-level prompt modals, but high-volume raw `op` frames are
  throttled/collapsed before JSON parsing in the app relay and live logs. The
  daemon/plugin sync path is unchanged; this only affects what the widget
  chooses to render.
* **SSE content is a byte-array.** The inspector decodes via
  `String.fromCharCode(...content)`, which is fine for ASCII/UTF-8 up to a
  few KB but will silently truncate on very long `.meta.json` content
  (e.g. Lighting with every ColorSequence serialised). If we ever go that
  big, switch to `TextDecoder`.
* **`Color` vs `Color3uint8`.** Roblox exposes parts' colour as two
  properties with the same underlying storage. Whichever one Agent 1's
  reflection list picks is what shows up on disk — the verification cell
  accepts either.
