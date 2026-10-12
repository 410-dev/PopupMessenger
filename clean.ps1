# PopupMessenger build-output cleanup.
# Usage:  .\clean.ps1 [-DryRun] [-Yes] [-NodeModules] [-KeepInstallers N]
#   -DryRun          Only show what would be removed and how much space it frees.
#   -Yes             Skip the confirmation prompt.
#   -NodeModules     Also remove node_modules (root and mobile); `npm install` restores them.
#   -KeepInstallers  Prune production\ to the newest N installers per extension (default: keep all).
# relay\target\dev-data (the local relay's DB, blobs and TLS certs) is always kept.
param(
    [switch]$DryRun,
    [switch]$Yes,
    [switch]$NodeModules,
    [int]$KeepInstallers = -1
)

$ErrorActionPreference = "Stop"
$Root = $PSScriptRoot

# Build output directories, relative to the repo root.
$Targets = @(
    "src-tauri\target",
    "src-tauri\gen",
    "target-dist",
    "mobile\src-tauri\target",
    "mobile\src-tauri\gen\android\app\build",
    "mobile\src-tauri\gen\android\build",
    "mobile\src-tauri\gen\android\buildSrc\build",
    "mobile\src-tauri\gen\android\buildSrc\.gradle",
    "mobile\src-tauri\gen\android\.gradle",
    "mobile\plugin\target",
    "mobile\plugin\android\build",
    "mobile\plugin\android\.gradle",
    "mobile\plugin\android\.tauri\tauri-api\build",
    "mobile\plugin\ios\.build"
)
if ($NodeModules) { $Targets += "node_modules", "mobile\node_modules" }

# Directories cleaned of everything except the listed entries.
$PartialTargets = @{
    "relay\target" = @("dev-data")
}

function Get-Size([string]$Path) {
    if (Test-Path -LiteralPath $Path -PathType Leaf) { return (Get-Item -LiteralPath $Path -Force).Length }
    $sum = (Get-ChildItem -LiteralPath $Path -Recurse -Force -File -ErrorAction SilentlyContinue |
        Measure-Object Length -Sum).Sum
    if ($sum) { [long]$sum } else { 0L }
}

function Format-Size([long]$Bytes) {
    if ($Bytes -ge 1GB) { "{0:N2} GB" -f ($Bytes / 1GB) }
    elseif ($Bytes -ge 1MB) { "{0:N1} MB" -f ($Bytes / 1MB) }
    else { "{0:N0} KB" -f ($Bytes / 1KB) }
}

# Remove-Item chokes on Gradle's deep paths; robocopy-mirroring an empty dir does not.
function Remove-Tree([string]$Path) {
    if (Test-Path -LiteralPath $Path -PathType Leaf) { Remove-Item -LiteralPath $Path -Force; return }
    cmd /c rd /s /q "`"$Path`"" 2>$null
    if (Test-Path -LiteralPath $Path) {
        $empty = Join-Path ([IO.Path]::GetTempPath()) ("pm-clean-" + [guid]::NewGuid())
        New-Item -ItemType Directory $empty | Out-Null
        robocopy $empty $Path /MIR /NFL /NDL /NJH /NJS /NP /R:1 /W:1 | Out-Null
        Remove-Item -LiteralPath $empty -Force
        cmd /c rd /s /q "`"$Path`"" 2>$null
    }
    if (Test-Path -LiteralPath $Path) { throw "could not fully remove (file in use?)" }
}

# Collect what to remove.
$items = New-Object System.Collections.Generic.List[object]
foreach ($rel in $Targets) {
    $full = Join-Path $Root $rel
    if (Test-Path -LiteralPath $full) { $items.Add([pscustomobject]@{ Label = $rel; Path = $full }) }
}
foreach ($rel in $PartialTargets.Keys) {
    $dir = Join-Path $Root $rel
    if (-not (Test-Path -LiteralPath $dir)) { continue }
    Get-ChildItem -LiteralPath $dir -Force | Where-Object { $PartialTargets[$rel] -notcontains $_.Name } |
        ForEach-Object { $items.Add([pscustomobject]@{ Label = "$rel\$($_.Name)"; Path = $_.FullName }) }
}
if ($KeepInstallers -ge 0) {
    $prod = Join-Path $Root "production"
    if (Test-Path -LiteralPath $prod) {
        Get-ChildItem -LiteralPath $prod -File | Group-Object Extension | ForEach-Object {
            $_.Group | Sort-Object LastWriteTime -Descending | Select-Object -Skip $KeepInstallers |
                ForEach-Object { $items.Add([pscustomobject]@{ Label = "production\$($_.Name)"; Path = $_.FullName }) }
        }
    }
}

if ($items.Count -eq 0) {
    Write-Host "Nothing to clean." -ForegroundColor Green
    return
}

Write-Host "Measuring..." -ForegroundColor DarkGray
$total = 0L
foreach ($it in $items) {
    $it | Add-Member Size (Get-Size $it.Path)
    $total += $it.Size
}
$items | Sort-Object Size -Descending | ForEach-Object { "  {0,10}  {1}" -f (Format-Size $_.Size), $_.Label }
Write-Host ("  {0,10}  total" -f (Format-Size $total)) -ForegroundColor Cyan

if ($DryRun) { Write-Host "Dry run: nothing removed."; return }
if (-not $Yes) {
    $answer = Read-Host "Remove these? [y/N]"
    if ($answer -notmatch '^(y|yes)$') { Write-Host "Cancelled."; return }
}

# Gradle daemons keep files under .gradle/build open.
if ($items | Where-Object { $_.Label -like "mobile\*android*" }) {
    $gradlew = Join-Path $Root "mobile\src-tauri\gen\android\gradlew.bat"
    if (Test-Path -LiteralPath $gradlew) {
        Push-Location (Split-Path $gradlew)
        try { & $gradlew --stop *> $null } catch { } finally { Pop-Location }
    }
}

$freed = 0L
$failed = 0
foreach ($it in $items) {
    try {
        Remove-Tree $it.Path
        $freed += $it.Size
        Write-Host "  removed  $($it.Label)" -ForegroundColor DarkGray
    }
    catch {
        $failed++
        Write-Host "  FAILED   $($it.Label): $($_.Exception.Message)" -ForegroundColor Yellow
    }
}

Write-Host ""
Write-Host ("Freed {0}." -f (Format-Size $freed)) -ForegroundColor Green
if ($failed) {
    Write-Host "$failed item(s) could not be removed. Stop running relay/app/IDE processes and re-run." -ForegroundColor Yellow
    exit 1
}
