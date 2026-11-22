# pa2pa installer for Windows (PowerShell)
# Usage:
#   irm https://raw.githubusercontent.com/theflowp/pa2pa/main/scripts/install.ps1 | iex
# Or from a local git clone:
#   powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1

$ErrorActionPreference = "Stop"

$InstallDir = if ($env:PA2PA_INSTALL_DIR) { $env:PA2PA_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "pa2pa\bin" }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$Repo = if ($env:PA2PA_REPO) { $env:PA2PA_REPO } else { "theflowp/pa2pa" }
$Target = "x86_64-pc-windows-msvc"
$Installed = $false

if ($Repo) {
    $Url = "https://github.com/$Repo/releases/latest/download/pa2pa-$Target.zip"
    $TmpZip = Join-Path $env:TEMP "pa2pa-$Target.zip"
    $TmpDir = Join-Path $env:TEMP "pa2pa-extract-$(Get-Random)"
    try {
        Write-Host "==> Downloading prebuilt pa2pa from $Url..."
        Invoke-WebRequest -Uri $Url -OutFile $TmpZip -UseBasicParsing
        Expand-Archive -Path $TmpZip -DestinationPath $TmpDir -Force
        Copy-Item (Join-Path $TmpDir "pa2pa.exe") (Join-Path $InstallDir "pa2pa.exe") -Force
        if (Test-Path (Join-Path $TmpDir "pa2pa-pill.exe")) {
            Copy-Item (Join-Path $TmpDir "pa2pa-pill.exe") (Join-Path $InstallDir "pa2pa-pill.exe") -Force
        }
        $Installed = $true
    } catch {
        Write-Host "Prebuilt release not found, falling back to building from source..."
    } finally {
        Remove-Item $TmpZip -ErrorAction SilentlyContinue
        Remove-Item $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if (-not $Installed) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host "==> Installing Rust via winget..."
        winget install --id Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements
        $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    }

    if ($PSScriptRoot -and (Test-Path (Join-Path (Join-Path $PSScriptRoot "..") "Cargo.toml"))) {
        $RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
        $CleanupSrc = $false
    } else {
        $RepoRoot = Join-Path $env:TEMP "pa2pa-src-$(Get-Random)"
        Write-Host "==> Cloning https://github.com/$Repo.git..."
        git clone --depth 1 "https://github.com/$Repo.git" $RepoRoot
        $CleanupSrc = $true
    }
    Write-Host "==> Building pa2pa.exe and pa2pa-pill.exe from source..."
    cargo build --manifest-path (Join-Path $RepoRoot "Cargo.toml") --release --bin pa2pa --bin pa2pa-pill
    Copy-Item (Join-Path $RepoRoot "target\release\pa2pa.exe") (Join-Path $InstallDir "pa2pa.exe") -Force
    Copy-Item (Join-Path $RepoRoot "target\release\pa2pa-pill.exe") (Join-Path $InstallDir "pa2pa-pill.exe") -Force
    if ($CleanupSrc) {
        Remove-Item $RepoRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$InstallDir*") {
    $NewPath = if ($UserPath) { "$UserPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    $env:PATH = "$InstallDir;$env:PATH"
    Write-Host "Added $InstallDir to your User PATH."
}

Write-Host ""
Write-Host "Installed pa2pa.exe and pa2pa-pill.exe into: $InstallDir"
Write-Host ""
Write-Host "Next steps (restart your terminal if needed):"
Write-Host "  pa2pa init --name <your-name> --relay <wss://your-relay/ws>"
Write-Host "  pa2pa setup-mcp --apply"
Write-Host "  pa2pa service install"
