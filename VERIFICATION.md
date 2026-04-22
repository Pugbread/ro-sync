# Ro Sync — manual verification (full-property + rename/delete + [N] suffix)

Covers the combined feature set after the three parallel patches land:
reflection-driven `.meta.json` (Agent 1, daemon + plugin), `[N]` sibling
disambiguation (Agent 2, daemon + plugin), and widget log enrichment +
project-row duplicate-group subtitle (Agent 3, this branch).

## Prep

1. `bash daemon/build.sh` and `cargo test` (in `daemon/`) — must both pass.
2. Copy `plugin/Plugin.luau` to `~/Documents/Roblox/Plugins/RoSync.lua`.
3. Create a scratch project folder: `mkdir -p /tmp/rosync-v2`.
4. Open the Ro Sync widget, Add project `/tmp/rosync-v2`, Activate it.
   Daemon dot should go green with `:7878`.
5. Open Roblox Studio → Plugins → RoSync → **Connect**. Plugin stat in the
   Active view should read `connected`.

## Checklist

| # | Scenario | Expected | Pass |
|---|----------|----------|------|
| 1 | In Studio, insert a `Part` into `Workspace`, set `Size = 5,1,5`, `Color = 1,0,0`, `Material = Neon`. Save/Connect. | `/tmp/rosync-v2/Workspace/Part.meta.json` exists with `className: "Part"` AND keys `Size`, `Color3uint8` (or `Color`), `Material` each shaped `{__type: "<TypeName>", value: ...}`. Default props (Anchored=false, CanCollide=true, etc.) are absent. | ☐ |
| 2 | In Studio, rename that `Part` to `Baseplate`. | Within 500 ms (watch Active log), `Part.luau`/`Part.meta.json` are gone and `Baseplate.meta.json` appears with the same properties. Only one pair of files exists; no orphan. | ☐ |
| 3 | In Studio, delete `Baseplate`. | Within 500 ms, `Baseplate.meta.json` is removed from disk. No error in the Active log. | ☐ |
| 4 | Recreate a `Part` named `Baseplate`. Then on disk, edit `Baseplate.meta.json` to set `"Transparency": {"__type":"f32","value":0.8}`. Save. | Within 500 ms, Studio's `Baseplate.Transparency` becomes `0.8`. The Active log shows a line with a `meta` chip reading `…/Baseplate.meta.json • Transparency + 0.8` (or `→ 0.8` if a prior value existed). | ☐ |
| 5 | In Studio, create two sibling parts under `Workspace` both named `Foo`. | Disk has `Workspace/Foo.luau`/`.meta.json` **and** `Workspace/Foo [1].luau`/`.meta.json`. No `(ClassName)`-style names anywhere. | ☐ |
| 6 | Add a third sibling `Foo`. | `Workspace/Foo [2].luau`/`.meta.json` appears. Existing `Foo` and `Foo [1]` files are untouched. | ☐ |
| 7 | Delete the middle `Foo [1]` in Studio. | `Foo [1].meta.json` is removed. `Foo` and `Foo [2]` remain (stable — no cascading rename is required; loose ordering is OK). | ☐ |
| 8 | Back on the Projects tab, wait for the status dot to refresh. | Project row for `/tmp/rosync-v2` shows a muted subtitle `1 duplicate-name group` (from the remaining `Foo` siblings). Switch projects / return to confirm subtitle persists until duplicates resolved. | ☐ |
| 9 | Grep the widget JS for stale disambiguation references: `grep -n "(ClassName)" app.js views/*.js bridge.js` | No matches. | ☐ |

## UX gotchas noted while wiring the widget

* **`/snapshot` shape isn't formally documented.** `countDupeGroups()` in
  `views/projects.js` walks either `children[]`, `services[]`, or a bare
  array — if Agent 1/2 settle on a different tree key, update that visitor.
  The current daemon returns `{bootstrap, services, strict}`, so the
  subtitle stays hidden (0 groups) until the tree is populated — that's
  fine for the empty-project state.
* **Meta diff chip depends on in-session state.** The first `op:update` for
  a `.meta.json` after the widget opens has no prior snapshot to diff
  against, so it renders as `+` additions only. That is intentional — it
  still tells you *which* keys are in play without pretending to know a
  baseline we never saw.
* **SSE content is a byte-array.** The inspector decodes via
  `String.fromCharCode(...content)`, which is fine for ASCII/UTF-8 up to a
  few KB but will silently truncate on very long `.meta.json` content
  (e.g. Lighting with every ColorSequence serialised). If we ever go that
  big, switch to `TextDecoder`.
* **`Color` vs `Color3uint8`.** Roblox exposes parts' colour as two
  properties with the same underlying storage. Whichever one Agent 1's
  reflection list picks is what shows up on disk — the verification cell
  accepts either.
