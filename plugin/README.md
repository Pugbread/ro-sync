# Ro Sync Studio Plugin

Two-way sync between a Roblox Studio place and the local Ro Sync daemon.

## Install

Install `Plugin.rbxm` into your Roblox Studio plugins folder (Studio → Plugins
→ Plugins Folder…), then enable `HttpService.HttpEnabled` in Game Settings.
The Terminal 64 widget's Settings view does this copy automatically.

`Plugin.luau` remains as the legacy/source artifact. The packaged plugin is
built with Rojo from `plugin-src/`.

## Build

```sh
./plugin/build-plugin.sh
```

The build runs Wally, then Rojo, and writes `plugin/Plugin.rbxm`.

## Use

1. Start the Ro Sync widget (the daemon listens on `http://127.0.0.1:7878` by default; if that port is busy it scans up to `7890`).
2. Open the **Ro Sync** panel from the Plugins toolbar.
3. Paste the daemon URL, click **Connect**. The pill turns green when sync is live.

The plugin watches `ReplicatedStorage`, `ServerScriptService`, `StarterPlayer`, `StarterGui`, `Workspace`, `ReplicatedFirst`, `ServerStorage`, and `Lighting`, pushes Studio edits to disk, and applies file edits back into the DataModel.
