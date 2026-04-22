# Build the rosync daemon for Windows x86_64. Output is placed next to
# Cargo.toml so the widget's platform-aware lookup finds it.
$ErrorActionPreference = 'Stop'
Set-Location -Path $PSScriptRoot

$cargo = if ($env:CARGO) { $env:CARGO } else { 'cargo' }
& $cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Copy-Item -Path 'target\release\rosync.exe' -Destination 'rosync-windows-x86_64.exe' -Force
Write-Host "built: $((Resolve-Path 'rosync-windows-x86_64.exe').Path)"
