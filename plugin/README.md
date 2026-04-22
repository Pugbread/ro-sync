# Ro Sync Studio Plugin

Two-way sync between a Roblox Studio place and the local Ro Sync daemon.

## Install

Copy `Plugin.luau` into your Roblox Studio plugins folder (Studio → Plugins → Plugins Folder…), then enable `HttpService.HttpEnabled` in Game Settings.

## Use

1. Start the Ro Sync widget (the daemon listens on `http://127.0.0.1:8484` by default).
2. Open the **Ro Sync** panel from the Plugins toolbar.
3. Paste the daemon URL, click **Connect**. The pill turns green when sync is live.

The plugin watches `ReplicatedStorage`, `ServerScriptService`, `StarterPlayer`, `StarterGui`, `Workspace`, `ReplicatedFirst`, `ServerStorage`, and `Lighting`, pushes Studio edits to disk, and applies file edits back into the DataModel.
