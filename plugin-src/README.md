# Ro Sync plugin source

This directory is the Rojo-managed build project for the Studio plugin.

From the repository root:

```sh
node plugin/build-plugin.mjs
```

`../plugin/Plugin.luau` remains the plugin's sync/daemon implementation source.
The build script copies it to `src/RoSync.server.luau` before invoking Rojo so
Rojo packages it as a plugin `Script`. It also copies `../plugin/Photo.luau`,
`../plugin/Clipboard.luau`, and `../plugin/Playscript.luau` into `src/`; Rojo
packages them as the `RoSync.Photo`, `RoSync.Clipboard`, and
`RoSync.Playscript` child modules required by that Script.

`src/App.luau` is the React Lua / ReactRoblox panel UI. It is bundled into the
same plugin model alongside Wally `Packages`, and `Plugin.luau` requires it at
runtime when the `.rbxm` is installed.
