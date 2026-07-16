# Bundled luau-lsp

Ro-Sync looks for a bundled `luau-lsp` executable here before falling back to
`luau-lsp` on `PATH`.

Expected layout:

```text
tools/luau-lsp/darwin-arm64/luau-lsp
tools/luau-lsp/darwin-x86_64/luau-lsp
tools/luau-lsp/linux-x86_64/luau-lsp
tools/luau-lsp/windows-x86_64/luau-lsp.exe
tools/luau-lsp/roblox/globalTypes.d.luau
```

The macOS widget currently carries the official universal `luau-lsp` 1.68.1
release in the `darwin-arm64` slot. It came from
`luau-lsp-macos.zip` in the upstream 1.68.1 GitHub release; the archive SHA-256
is `e32a71823ee47471d931a03e4186ced2b4c43bb785c8fe05de901fe54c6ebe21`
and the extracted binary SHA-256 is
`a669e117af9fc28efbb7ba3fb5237ba7e579cc09d6e3e4e6841ef700c7f0dbff`.

The Roblox definitions are the upstream `None` security snapshot paired with
this update. Their SHA-256 is
`08fbcafcf6d17643886d8fe0ec297fc9bfab33d3bf8d96d88b6eefe29f6d5490`
after normalizing the generated file to one trailing newline.
Ro Sync still accepts `--luau-lsp` / `ROSYNC_LUAU_LSP` overrides. luau-lsp is
distributed under its upstream MIT license: <https://github.com/JohnnyMorganz/luau-lsp>.
