[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$RepositoryRoot,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$CacheRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [string]$FilePath,

        [Parameter(ValueFromRemainingArguments = $true)]
        [string[]]$ArgumentList
    )

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: $FilePath $ArgumentList"
    }
}

function Remove-DirectoryLink {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    $item = Get-Item -Force -LiteralPath $Path
    if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -eq 0) {
        throw "Refusing to remove non-link path: $Path"
    }

    # Windows PowerShell 5.1 may try to prompt when Remove-Item sees a
    # non-empty directory junction. cmd.exe's rmdir removes the junction
    # itself without traversing or deleting its target.
    Invoke-Checked "cmd.exe" "/d" "/c" "rmdir" $Path
}

$repo = (Resolve-Path -LiteralPath $RepositoryRoot).Path
$salvoCheckout = Join-Path $repo "_deps\salvo"
$salvoLink = Join-Path (Split-Path -Parent $repo) "salvo"

if (-not (Test-Path -LiteralPath (Join-Path $salvoCheckout "Cargo.toml"))) {
    throw "Pinned Salvo checkout is missing at $salvoCheckout"
}

New-Item -ItemType Directory -Force -Path $CacheRoot | Out-Null
$env:CARGO_HOME = Join-Path $CacheRoot "cargo"
$env:UV_CACHE_DIR = Join-Path $CacheRoot "uv"
$env:CARGO_INCREMENTAL = "0"
Remove-Item Env:RUSTC_WRAPPER -ErrorAction SilentlyContinue

if (Test-Path -LiteralPath $salvoLink) {
    Remove-DirectoryLink $salvoLink
}

New-Item -ItemType Junction -Path $salvoLink -Target $salvoCheckout | Out-Null

try {
    Set-Location -LiteralPath $repo
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue "dist", ".venv", ".venv-smoke"

    Invoke-Checked "rustc.exe" "--version"
    Invoke-Checked "cargo.exe" "--version"
    Invoke-Checked "python.exe" "--version"
    Invoke-Checked "uv.exe" "--version"

    Invoke-Checked "uv.exe" "venv" "--python" "3.13" ".venv"
    $buildPython = Join-Path $repo ".venv\Scripts\python.exe"
    Invoke-Checked "uv.exe" "pip" "install" "--python" $buildPython "maturin"
    Invoke-Checked $buildPython "-m" "maturin" "build" `
        "--release" "--out" "dist" "-m" "bindings/python/rust/Cargo.toml"

    $wheels = @(Get-ChildItem -LiteralPath (Join-Path $repo "dist") -Filter "*.whl")
    if ($wheels.Count -ne 1) {
        throw "Expected exactly one Windows wheel, found $($wheels.Count)"
    }

    Invoke-Checked "uv.exe" "venv" "--python" "3.13" ".venv-smoke"
    $smokePython = Join-Path $repo ".venv-smoke\Scripts\python.exe"
    Invoke-Checked "uv.exe" "pip" "install" "--python" $smokePython $wheels[0].FullName
    Invoke-Checked $smokePython "-c" "import aster; import aster._aster; print('Aster Windows wheel import OK')"
}
finally {
    if (Test-Path -LiteralPath $salvoLink) {
        Remove-DirectoryLink $salvoLink
    }
}
