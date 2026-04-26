# Ro Sync plugin source

This directory is the Rojo-managed build project for the Studio plugin.

```sh
cd plugin-src
wally install
rojo build plugin.project.json --output ../plugin/Plugin.rbxm
```

`../plugin/Plugin.luau` remains the plugin's sync/daemon implementation source.
The build script copies it to `src/RoSync.server.luau` before invoking Rojo so
Rojo packages it as a plugin `Script`.

`src/App.luau` is the React Lua / ReactRoblox panel UI. It is bundled into the
same plugin model alongside Wally `Packages`, and `Plugin.luau` requires it at
runtime when the `.rbxm` is installed.
