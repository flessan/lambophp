# Builds LamboPHP-Setup.exe from a release build.
#
#   pwsh ./scripts/installer.ps1 [-Version 0.2.0] [-Binary path\to\lambo.exe]
#
# Needs NSIS (`makensis`). On a GitHub Windows runner it is preinstalled;
# elsewhere `winget install NSIS.NSIS` or `choco install nsis` gets it.
#
# The script deliberately does no building of its own: it packages whatever
# binary it is handed, so the artifact in the installer is exactly the one the
# test suite ran against.
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Binary = "",
    [string]$Gui = "",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

if (-not $Version) {
    $Version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
}
$Version = $Version.TrimStart("v")

if (-not $Binary) {
    $Binary = "target\x86_64-pc-windows-msvc\release\lambo.exe"
}
if (-not (Test-Path $Binary)) {
    throw "missing $Binary - run 'cargo build --release --target x86_64-pc-windows-msvc' first"
}

# The GUI is optional: a CLI-only build still produces a working installer.
# Defaulting to the sibling of the CLI means a normal release build is picked up
# without the caller having to spell out a second path.
if (-not $Gui) {
    $Gui = Join-Path (Split-Path $Binary) "lambo-gui.exe"
}
$haveGui = Test-Path $Gui
if (-not $haveGui) {
    Write-Host "note: $Gui not found; building a CLI-only installer"
}

$makensis = Get-Command makensis -ErrorAction SilentlyContinue
if (-not $makensis) {
    throw "makensis was not found on PATH; install NSIS (winget install NSIS.NSIS)"
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$out = Join-Path $OutDir "LamboPHP-Setup.exe"

Write-Host "building $out for Lambo PHP $Version"
$defines = @(
    "/DVERSION=$Version",
    "/DBINARY=$Binary",
    "/DOUTFILE=$out"
)
if ($haveGui) {
    $defines += "/DGUI=$Gui"
}

& makensis @defines "packaging\lambo.nsi"

if ($LASTEXITCODE -ne 0) {
    throw "makensis failed with exit code $LASTEXITCODE"
}

# An installer nobody can verify is an installer nobody should run.
$sum = (Get-FileHash -Algorithm SHA256 $out).Hash.ToLower()
Write-Host "sha256 $sum  $out"
Get-ChildItem $out | Format-Table Name, Length, LastWriteTime
