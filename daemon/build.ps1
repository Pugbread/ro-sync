# Build the rosync daemon for Windows x86_64. Output is placed next to
# Cargo.toml so the widget's platform-aware lookup finds it.
$ErrorActionPreference = 'Stop'
Set-Location -Path $PSScriptRoot

$cargo = if ($env:CARGO) { $env:CARGO } else { 'cargo' }
& $cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Copy-Item -Path 'target\release\rosync.exe' -Destination 'rosync-windows-x86_64.exe' -Force
Write-Host "built: $((Resolve-Path 'rosync-windows-x86_64.exe').Path)"

# Source builds opportunistically install the same checksum-pinned compiler
# that release bundles carry. A compiler download failure must not discard a
# successfully built daemon; `rosync doctor` will keep the missing pass visible.
$node = Get-Command node -ErrorAction SilentlyContinue
if ($node) {
    try {
        & $node.Source '..\scripts\install-luau-compiler.mjs'
        if ($LASTEXITCODE -ne 0) { throw "installer exited with $LASTEXITCODE" }
    }
    catch {
        Write-Warning "Luau compiler install failed: $($_.Exception.Message). Run 'node scripts/install-luau-compiler.mjs' from the widget root."
    }
}
else {
    Write-Warning "Node.js not found; run scripts/install-luau-compiler.mjs later to enable compiler lint checks."
}
