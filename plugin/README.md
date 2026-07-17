# Ro Sync Studio Plugin

Two-way sync between a Roblox Studio place and the local Ro Sync daemon.

## Install

Install `Plugin.rbxm` into your Roblox Studio plugins folder (Studio → Plugins
→ Plugins Folder…), then enable `HttpService.HttpEnabled` in Game Settings.
The Terminal 64 widget's Settings view does this copy automatically.

`Plugin.luau` remains as the legacy/source artifact. The packaged plugin is
built with Rojo from `plugin-src/`.

`Photo.luau` is Ro Sync's dependency-free native viewport/subject capture
module. The builder packages it as `RoSync.Photo`; it never requires a capture
binding or source folder from the open place. It returns bounded
`{ buffer, size }` RGBA records, keeping screenshot `EditableImage` objects
private and destroying them as soon as their pixels have been read. Its UI
mode can exclude ScreenGuis, preserve them over the scene, or extract the
edit-mode ScreenGui layer alone as transparent RGBA. Isolated transparent
instance captures tight-crop the rendered subject alpha by default and
aspect-contain it in any exact requested output size, including exact-CFrame
views. Callers can disable that behavior to retain the full camera-framed
render; `capture photo` scene-background and include-world captures remain
framed, while the isolated transparent `capture scene` alias inherits the
default crop.

`Playscript.luau` is the isolated runtime coordinator for playscript-owned
playtests. The builder packages it as `RoSync.Playscript`; `Plugin.luau`
requires that child module for authenticated boot, event streaming,
cross-context signals, completion, and bounded result transport.

## Build

```sh
node plugin/build-plugin.mjs
```

Run this from the repository root. On macOS / Linux,
`./plugin/build-plugin.sh` is also available and delegates to the same Node
builder. The build runs Wally, then Rojo, and writes `plugin/Plugin.rbxm`.

## Use

1. Start the Ro Sync widget (the daemon listens on `http://127.0.0.1:7878` by default; if that port is busy it scans up to `7890`).
2. Open the **Ro Sync** panel from the Plugins toolbar.
3. Paste the daemon URL, click **Connect**. The pill turns green when sync is live.

The plugin watches `ReplicatedStorage`, `ServerScriptService`, `StarterPlayer`, `StarterGui`, `Workspace`, `ReplicatedFirst`, `ServerStorage`, and `Lighting`, pushes Studio edits to disk, and applies file edits back into the DataModel.
