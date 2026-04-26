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

The binary is not committed by default. Release builds can place the matching
platform binary here, and `rosync lint` will also use the bundled Roblox
definitions file when present. Users can point `rosync lint` at a local install
with `--luau-lsp` / `ROSYNC_LUAU_LSP`.
