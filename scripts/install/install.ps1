#requires -Version 5.1
<#
.SYNOPSIS
    One-shot installer for the Aster CLI on Windows.

.DESCRIPTION
    Downloads the Aster CLI standalone distribution (or onefile binary), verifies
    SHA256, installs to %LOCALAPPDATA%\Programs\Aster (no admin required), and
    adds it to the user PATH.

.PARAMETER Version
    Specific release to install (e.g. "0.1.2"). Defaults to latest.

.PARAMETER Prefix
    Install root. Defaults to "$env:LOCALAPPDATA\Programs\Aster".

.PARAMETER OneFile
    Install the single-file onefile binary instead of the standalone dist.
    Faster to download, slower first-run startup.

.EXAMPLE
    iwr -useb https://aster.site/install.ps1 | iex

.EXAMPLE
    & ([scriptblock]::Create((iwr -useb https://aster.site/install.ps1).Content)) -Version 0.1.2
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\Aster'),
    [switch]$OneFile
)

$ErrorActionPreference = 'Stop'
$Repo = if ($env:ASTER_REPO) { $env:ASTER_REPO } else { 'aster-rpc/aster-rpc' }

function Say  ($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Warn ($msg) { Write-Warning $msg }
function Die  ($msg) { Write-Error $msg; exit 1 }

# ── Platform check ─────────────────────────────────────────────────────────
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64') {
    Die "Unsupported architecture: $arch. The Aster CLI ships x86_64-only on Windows."
}
$Suffix = 'windows-x86_64'

# ── Resolve version ────────────────────────────────────────────────────────
if (-not $Version) {
    Say "Resolving latest aster-cli release..."
    $latest = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    if ($latest.tag_name -notmatch '^cli-v(.+)$') {
        Die "Could not resolve latest release. Pass -Version explicitly."
    }
    $Version = $matches[1]
}
$Tag = "cli-v$Version"
Say "Installing aster-cli $Version ($Suffix)"

# ── Resolve archive ────────────────────────────────────────────────────────
$BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
if ($OneFile) {
    $Archive = "aster-$Suffix.exe"
} else {
    $Archive = "aster-dist-$Suffix.zip"
}

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

try {
    # ── Download ───────────────────────────────────────────────────────────
    Say "Downloading $Archive..."
    Invoke-WebRequest "$BaseUrl/$Archive" -OutFile "$tempDir\$Archive" -UseBasicParsing
    Invoke-WebRequest "$BaseUrl/SHA256SUMS" -OutFile "$tempDir\SHA256SUMS" -UseBasicParsing

    # ── Verify ─────────────────────────────────────────────────────────────
    Say "Verifying SHA256..."
    $expected = (Get-Content "$tempDir\SHA256SUMS" | Where-Object { $_ -match "  $Archive$" } | ForEach-Object { ($_ -split '\s+')[0] }) | Select-Object -First 1
    if (-not $expected) { Die "$Archive not found in SHA256SUMS" }
    $actual = (Get-FileHash "$tempDir\$Archive" -Algorithm SHA256).Hash.ToLower()
    if ($expected.ToLower() -ne $actual) { Die "Checksum mismatch (expected $expected, got $actual)" }

    # ── Install ────────────────────────────────────────────────────────────
    $shareDir = Join-Path $Prefix 'app'
    $binDir   = Join-Path $Prefix 'bin'
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    New-Item -ItemType Directory -Path (Split-Path $shareDir) -Force | Out-Null

    if ($OneFile) {
        Copy-Item "$tempDir\$Archive" -Destination (Join-Path $binDir 'aster.exe') -Force
        Say "Warming onefile cache..."
        & (Join-Path $binDir 'aster.exe') --version | Out-Null
    } else {
        Say "Extracting to $shareDir..."
        Expand-Archive -Path "$tempDir\$Archive" -DestinationPath $tempDir -Force
        $extracted = Join-Path $tempDir "aster-$Suffix"
        if (-not (Test-Path $extracted)) { Die "Archive layout unexpected (no aster-$Suffix\)" }

        # Atomic replace
        if (Test-Path $shareDir) {
            $old = "$shareDir.old"
            if (Test-Path $old) { Remove-Item -Recurse -Force $old }
            Move-Item $shareDir $old
        }
        Move-Item $extracted $shareDir
        if (Test-Path "$shareDir.old") { Remove-Item -Recurse -Force "$shareDir.old" }

        # Windows has no symlinks-by-default for unprivileged users; use a launcher .cmd.
        $launcher = Join-Path $binDir 'aster.cmd'
        @"
@echo off
"$shareDir\aster.exe" %*
"@ | Set-Content -Path $launcher -Encoding ASCII
    }

    # ── Verify install ─────────────────────────────────────────────────────
    $entry = if ($OneFile) { Join-Path $binDir 'aster.exe' } else { Join-Path $binDir 'aster.cmd' }
    & $entry --version | Out-Null
    if ($LASTEXITCODE -ne 0) { Die "Installed binary failed --version" }

    Say "Installed: $entry"

    # ── PATH update ────────────────────────────────────────────────────────
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $binDir)) {
        Say "Adding $binDir to user PATH..."
        $newPath = if ($userPath) { "$userPath;$binDir" } else { $binDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Warn "Restart your shell (or open a new terminal) for PATH changes to take effect."
    }

    Say "Run: aster --help"
}
finally {
    Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
}
