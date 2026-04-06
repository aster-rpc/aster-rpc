# Setup script for self-hosted GitHub Actions runner on Windows.
#
# Run this in an ELEVATED PowerShell prompt on the Windows machine:
#   Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
#   .\setup-windows.ps1 -Token "<your-runner-token>"
#
# Generate a token with:
#   gh api -X POST repos/emrul/iroh-python/actions/runners/registration-token --jq .token

param(
    [Parameter(Mandatory=$true)]
    [string]$Token,

    [string]$RunnerDir = "C:\actions-runner",
    [string]$RunnerName = "aster-windows-x64",
    [string]$GitHubUrl = "https://github.com/emrul/iroh-python",
    [string]$RunnerVersion = "2.333.1"
)

$ErrorActionPreference = "Stop"

Write-Host "=== Setting up runner directory ===" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $RunnerDir | Out-Null
Set-Location $RunnerDir

Write-Host "=== Downloading GitHub Actions runner ===" -ForegroundColor Cyan
if (-not (Test-Path ".\config.cmd")) {
    $RunnerZip = "actions-runner-win-x64-$RunnerVersion.zip"
    Invoke-WebRequest -Uri "https://github.com/actions/runner/releases/download/v$RunnerVersion/$RunnerZip" -OutFile $RunnerZip
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory("$PWD\$RunnerZip", $PWD)
    Remove-Item $RunnerZip
}

Write-Host "=== Configuring runner ===" -ForegroundColor Cyan
.\config.cmd --unattended `
    --url $GitHubUrl `
    --token $Token `
    --name $RunnerName `
    --labels "self-hosted,Windows,X64" `
    --work _work `
    --replace `
    --runasservice

Write-Host ""
Write-Host "=== Runner $RunnerName configured ===" -ForegroundColor Green
Write-Host ""
Write-Host "Now install prerequisites:" -ForegroundColor Yellow
Write-Host "  1. Rust:    winget install Rustlang.Rustup" -ForegroundColor White
Write-Host "  2. Python:  winget install Python.Python.3.13" -ForegroundColor White
Write-Host "  3. uv:      irm https://astral.sh/uv/install.ps1 | iex" -ForegroundColor White
Write-Host "  4. sccache: cargo install sccache --locked" -ForegroundColor White
Write-Host ""
Write-Host "Then start the service:" -ForegroundColor Yellow
Write-Host "  Start-Service actions.runner.*" -ForegroundColor White
