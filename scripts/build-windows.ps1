# Builds the Windows release artifacts from a clean checkout.
#
#   pwsh ./scripts/build-windows.ps1
#   pwsh ./scripts/build-windows.ps1 -Version 0.10.0
#   pwsh ./scripts/build-windows.ps1 -Arch amd64 -SkipTests
#
# Produces, under ./dist:
#   LamboPHP-<version>-windows-<arch>/   the portable payload
#   LamboPHP-<version>-windows-<arch>.zip
#   LamboPHP-Setup.exe                   the installer
#   <artifact>.sha256                    one checksum per artifact
#
# This is the entry point CI uses, so a hand-run release and a pipeline release
# produce the same layout. It is deliberately ordinary: no MSBuild, no custom
# toolchain steps, nothing that only works on one machine.
[CmdletBinding()]
param(
    # Release version. Defaults to Cargo.toml, which is the canonical source.
    [string]$Version = "",

    # amd64 or arm64. amd64 is what the release workflow treats as mandatory.
    [ValidateSet("amd64", "arm64")]
    [string]$Arch = "amd64",

    # Skip the test run. For iterating on packaging only; a release build
    # should never use this.
    [switch]$SkipTests,

    # Treat a missing installer toolchain as a failure rather than a note.
    # CI sets this: a release that ships a ZIP but no installer is incomplete,
    # and publishing it quietly is worse than failing the job.
    [switch]$RequireInstaller,

    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

# --- resolve what we are building ------------------------------------------

if (-not $Version) {
    $Version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' |
        Select-Object -First 1).Matches[0].Groups[1].Value
}
$Version = $Version.TrimStart("v")
if (-not $Version) {
    throw "could not read a version from Cargo.toml; pass -Version explicitly"
}

$Target = switch ($Arch) {
    "amd64" { "x86_64-pc-windows-msvc" }
    "arm64" { "aarch64-pc-windows-msvc" }
}

$name = "LamboPHP-$Version-windows-$Arch"
$payloadDir = Join-Path $OutDir $name
$zip = "$payloadDir.zip"
$installer = Join-Path $OutDir "LamboPHP-Setup.exe"

Write-Host "Lambo PHP $Version for $Arch ($Target)"

# --- toolchain --------------------------------------------------------------

foreach ($tool in @("cargo", "rustc")) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool was not found on PATH; install the Rust toolchain first"
    }
}

# A cross-compile needs the target's standard library. Adding it is cheap and
# idempotent, and failing here would otherwise surface as a confusing linker
# error several minutes into the build.
rustup target add $Target 2>$null | Out-Null

# --- tests ------------------------------------------------------------------

if ($SkipTests) {
    Write-Host "skipping tests (-SkipTests)"
} else {
    Write-Host "running the test suite"
    # --no-fail-fast so one failing binary does not hide the others; a pass
    # count read off a fail-fast run is not a pass count.
    cargo test --workspace --all-targets --locked --no-fail-fast
    if ($LASTEXITCODE -ne 0) {
        throw "tests failed; not packaging a broken build"
    }
}

# --- build ------------------------------------------------------------------

Write-Host "building release binaries"
cargo build --release --target $Target --package lambo-cli --package lambo-gui
if ($LASTEXITCODE -ne 0) {
    throw "the release build failed"
}

$binDir = Join-Path "target\$Target\release"
$cli = Join-Path $binDir "lambo.exe"
$gui = Join-Path $binDir "lambo-gui.exe"

if (-not (Test-Path $cli)) {
    throw "missing $cli after a successful build"
}
# The GUI is part of the product on Windows, so its absence is a failure here
# rather than a warning. package.sh stays permissive because it also serves
# Linux and macOS, where there is no GUI to ship.
if (-not (Test-Path $gui)) {
    throw "missing $gui; the Windows artifacts must contain the GUI"
}

# --- portable payload -------------------------------------------------------

# Rebuilt from scratch so a stale file from an earlier build cannot ride along
# in the archive.
if (Test-Path $payloadDir) {
    Remove-Item -Recurse -Force $payloadDir
}
New-Item -ItemType Directory -Force -Path $payloadDir | Out-Null

Copy-Item $cli (Join-Path $payloadDir "lambo.exe")
Copy-Item $gui (Join-Path $payloadDir "lambo-gui.exe")

# The licences are not optional. Shipping a binary without the terms it is
# distributed under is a packaging failure, so a missing licence file stops
# the build instead of quietly producing an incomplete archive.
foreach ($licence in @("LICENSE-APACHE", "LICENSE-MIT")) {
    if (-not (Test-Path $licence)) {
        throw "missing $licence; the package cannot ship without its licences"
    }
    Copy-Item $licence $payloadDir
}

@"
Lambo PHP $Version (windows-$Arch)

Install
-------
Run LamboPHP-Setup.exe for a Start Menu install, or use this folder as a
portable copy: put lambo.exe somewhere on your PATH, or run it in place.
Nothing else is required - Lambo downloads PHP, Apache and MariaDB itself,
into %USERPROFILE%\Lambo.

Getting started
---------------
  cd your-project
  lambo init      detect the project and write lambo.yml
  lambo up        start the services
  open http://localhost
  open http://localhost/phpmyadmin
  lambo down      stop everything Lambo started

lambo-gui.exe is the same engine with a window on it. Its sidebar is Services,
Projects, Editor, Vhosts and Settings; the log panel under every page is the
same log the CLI prints.

Everything Lambo writes lives under LAMBO_HOME (%USERPROFILE%\Lambo by
default). No administrator rights are needed, and uninstalling removes the
application while leaving your projects and databases alone.

Documentation: https://github.com/flessan/lambophp/tree/main/docs
"@ | Set-Content -Encoding utf8 (Join-Path $payloadDir "README.txt")

# Nothing from the test tree may ship. This is a guard rather than a cleanup
# step: the payload above is built from an explicit list, so a fixture could
# only arrive here if someone added a copy line.
# Test fixtures, debug leftovers, partial downloads and checksum sidecars all
# belong to the build, never to the shipped payload. The payload is assembled
# from an explicit list above, so this is a guard against a future edit rather
# than a cleanup step.
$leaked = Get-ChildItem -Recurse $payloadDir |
    Where-Object { $_.Name -match "fixture|catalogue\.json|\.sha256$|\.part$|\.tmp$|\.pdb$|\.d$" -and $_.Name -notmatch "^(LICENSE|README)" }
if ($leaked) {
    throw "build leftovers leaked into the payload: $($leaked.Name -join ', ')"
}

Write-Host "compressing $zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $payloadDir -DestinationPath $zip

# --- installer --------------------------------------------------------------

# makensis is optional here but expected in CI: a developer packaging a portable
# zip on a machine without NSIS should still get their zip.
$artifacts = @($zip)
if (Get-Command makensis -ErrorAction SilentlyContinue) {
    Write-Host "building $installer"
    & (Join-Path $PSScriptRoot "installer.ps1") -Version $Version -Binary $cli -Gui $gui -OutDir $OutDir
    if ($LASTEXITCODE -ne 0) {
        throw "makensis failed; not publishing an installer that did not build"
    }
    if (-not (Test-Path $installer)) {
        throw "makensis reported success but $installer does not exist"
    }
    $artifacts += $installer
} elseif ($RequireInstaller) {
    throw "makensis was not found and -RequireInstaller was set; a release needs " +
        "LamboPHP-Setup.exe (winget install NSIS.NSIS)"
} else {
    Write-Host "note: makensis not found; skipping the installer (winget install NSIS.NSIS)"
}

# --- checksums --------------------------------------------------------------

# An artifact nobody can verify is an artifact nobody should download.
foreach ($artifact in $artifacts) {
    $sum = (Get-FileHash -Algorithm SHA256 $artifact).Hash.ToLower()
    $leaf = Split-Path $artifact -Leaf
    "$sum  $leaf" | Set-Content -Encoding ascii "$artifact.sha256"
    Write-Host "sha256 $sum  $leaf"
}

Write-Host ""
Get-ChildItem $OutDir | Where-Object { $_.Name -like "LamboPHP*" } |
    Format-Table Name, Length, LastWriteTime
Write-Host "built $name"
