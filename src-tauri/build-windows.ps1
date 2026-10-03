# PopupMessenger Windows build (MSI + NSIS installers).
# Usage:  .\build-windows.ps1 [-Bundles msi,nsis] [-DebugBuild] [-- extra tauri build args]
param(
    [string[]]$Bundles = @("msi", "nsis"),
    [switch]$DebugBuild,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest = @()
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot

foreach ($cmd in "cargo", "node", "npm") {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        throw "'$cmd' not found. Install Rust (stable) and Node.js first."
    }
}

Push-Location $Root
try {
    if (-not (Test-Path "node_modules")) {
        npm install
        if ($LASTEXITCODE -ne 0) { throw "npm install failed" }
    }

    $tauriArgs = @("tauri", "build", "--bundles", ($Bundles -join ","))
    if ($DebugBuild) { $tauriArgs += "--debug" }
    $tauriArgs += $Rest

    npx @tauriArgs
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }

    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot "target" }
    $buildProfile = if ($DebugBuild) { "debug" } else { "release" }
    $bundleDir = Join-Path $targetDir "$buildProfile\bundle"
    Write-Host ""
    Write-Host "Build complete. Installers:" -ForegroundColor Green
    Get-ChildItem $bundleDir -Recurse -Include *.msi, *-setup.exe -ErrorAction SilentlyContinue |
        ForEach-Object { Write-Host "  $($_.FullName)" }
}
finally {
    Pop-Location
}
