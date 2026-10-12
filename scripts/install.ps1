# Install both CLIs from this checkout on Windows — `smoll`, the coding agent, and
# `smollm`, the model CLI. Everything is local: no release download, no admin
# rights, no telemetry.
#
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -IncludeApp

[CmdletBinding()]
param(
    [string] $Prefix = "$env:LOCALAPPDATA\Programs\smollm",
    [switch] $IncludeApp
)

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not found. Install Rust 1.77+ from https://rustup.rs and reopen your shell."
}

Write-Host "==> Building smollm (release)"
cargo build --release --manifest-path (Join-Path $Root 'Cargo.toml') -p smollm-cli --bin smollm
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }

# The feature the release archives carry, for the same reason as install.sh: with
# no backend this binary can only answer from mock or echo.
Write-Host "==> Building smoll (release, openai-compatible)"
cargo build --release --manifest-path (Join-Path $Root 'Cargo.toml') -p agent-cli --bin smoll --features openai-compatible
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }

$exe = Join-Path $Root 'target\release\smollm.exe'
if (-not (Test-Path $exe)) { throw "expected build output at $exe" }
$agentExe = Join-Path $Root 'target\release\smoll.exe'
if (-not (Test-Path $agentExe)) { throw "expected build output at $agentExe" }

Write-Host "==> Installing to $Prefix"
New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
Copy-Item $exe (Join-Path $Prefix 'smollm.exe') -Force
Copy-Item $agentExe (Join-Path $Prefix 'smoll.exe') -Force

if ($IncludeApp) {
    $bundleDir = Join-Path $Root 'desktop\src-tauri\target\release\bundle'
    $installer = Get-ChildItem -Path $bundleDir -Recurse -Include '*.msi', '*.exe' -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -notmatch 'nsis\\.*smollm\.exe$' } |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $installer) {
        Write-Error "No installer found under $bundleDir. Build it first: cd desktop; pnpm tauri build"
    }
    Write-Host "==> Run the installer yourself: $($installer.FullName)"
    Write-Host "    (this script does not silently start an installer for you)"
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath -notlike "*$Prefix*") {
    Write-Host "==> Adding $Prefix to your user PATH"
    [Environment]::SetEnvironmentVariable('Path', "$Prefix;$userPath", 'User')
    Write-Host "    Reopen your terminal for it to take effect."
}

& (Join-Path $Prefix 'smollm.exe') --version
& (Join-Path $Prefix 'smoll.exe') --version
Write-Host "smollm is installed. Try: smollm hardware; smollm doctor"
Write-Host "smoll is installed. Try: cd a repository; smoll doctor; smoll tools"
