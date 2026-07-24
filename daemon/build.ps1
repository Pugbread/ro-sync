# Build the rosync daemon for Windows x86_64. Output is placed next to
# Cargo.toml so the widget's platform-aware lookup finds it.
$ErrorActionPreference = 'Stop'
Set-Location -Path $PSScriptRoot

$cargo = if ($env:CARGO) { $env:CARGO } else { 'cargo' }
$target = 'x86_64-pc-windows-msvc'
& $cargo build --release --locked --target $target
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$built = Join-Path $PSScriptRoot "target\$target\release\rosync.exe"
$destination = Join-Path $PSScriptRoot 'rosync-windows-x86_64.exe'
$staged = Join-Path $PSScriptRoot ('.rosync-windows-x86_64.' + $PID + '.tmp')
try {
    Copy-Item -LiteralPath $built -Destination $staged -Force
    if (Test-Path -LiteralPath $destination) {
        [IO.File]::Replace($staged, $destination, $null, $true)
    }
    else {
        [IO.File]::Move($staged, $destination)
    }
}
finally {
    if (Test-Path -LiteralPath $staged) {
        Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
    }
}
Write-Host "built: $((Resolve-Path 'rosync-windows-x86_64.exe').Path)"

# Source builds opportunistically install the same checksum-pinned compiler
# that release bundles carry. A compiler download failure must not discard a
# successfully built daemon; `rosync doctor` will keep the missing pass visible.
$skipToolDownload = $env:ROSYNC_SKIP_TOOL_DOWNLOAD -eq '1'
if ($skipToolDownload) {
    Write-Host "skipped optional Luau compiler download (ROSYNC_SKIP_TOOL_DOWNLOAD=1)"
}
else {
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
}
