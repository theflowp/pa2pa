# pa2pa installer for Windows (PowerShell)
# Usage:
#   irm https://raw.githubusercontent.com/theflowp/pa2pa/main/scripts/install.ps1 | iex
# Or from a local git clone:
#   powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 -FromSource

param(
    [switch]$FromSource
)

$ErrorActionPreference = "Stop"

if ($env:PA2PA_FROM_SOURCE -eq "1") {
    $FromSource = $true
}

$InstallDir = if ($env:PA2PA_INSTALL_DIR) { $env:PA2PA_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "pa2pa\bin" }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$OldVersion = ""
$ExistingExe = Join-Path $InstallDir "pa2pa.exe"
if (Test-Path $ExistingExe) {
    try {
        $OldVersion = (& $ExistingExe --version 2>$null | Select-Object -First 1)
    } catch {
        $OldVersion = ""
    }
}

$Repo = if ($env:PA2PA_REPO) { $env:PA2PA_REPO } else { "theflowp/pa2pa" }
$Target = "x86_64-pc-windows-msvc"
$Installed = $false
$InLocalClone = ($PSScriptRoot -and (Test-Path (Join-Path (Join-Path $PSScriptRoot "..") "Cargo.toml")))

if ((-not $FromSource) -and $Repo) {
    if ($InLocalClone) {
        Write-Host "Note: Downloading latest GitHub release (pass -FromSource to build from this local checkout instead)."
    }
    $ArchiveName = "pa2pa-$Target.zip"
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
    $Url = "$BaseUrl/$ArchiveName"
    $SumsUrl = "$BaseUrl/SHA256SUMS.txt"
    $TmpZip = Join-Path $env:TEMP $ArchiveName
    $TmpSums = Join-Path $env:TEMP "pa2pa-SHA256SUMS.txt"
    $TmpDir = Join-Path $env:TEMP "pa2pa-extract-$(Get-Random)"
    try {
        Write-Host "==> Downloading prebuilt pa2pa from $Url..."
        Invoke-WebRequest -Uri $Url -OutFile $TmpZip -UseBasicParsing

        if ($env:PA2PA_SKIP_CHECKSUM -eq "1") {
            Write-Warning "PA2PA_SKIP_CHECKSUM=1 set — skipping SHA-256 verification for $ArchiveName!"
        } else {
            try {
                Invoke-WebRequest -Uri $SumsUrl -OutFile $TmpSums -UseBasicParsing
            } catch {
                throw "SHA256 verification failed: could not download $SumsUrl (set `$env:PA2PA_SKIP_CHECKSUM='1' to override)."
            }
            $Match = Select-String -Path $TmpSums -Pattern " $ArchiveName$" | Select-Object -First 1
            if (-not $Match) {
                throw "SHA256 verification failed: $ArchiveName missing from SHA256SUMS.txt (set `$env:PA2PA_SKIP_CHECKSUM='1' to override)."
            }
            $Expected = ($Match.Line -split '\s+')[0].ToLower()
            $Actual = (Get-FileHash -Path $TmpZip -Algorithm SHA256).Hash.ToLower()
            if ($Expected -ne $Actual) {
                throw "SHA256 checksum mismatch for $ArchiveName (expected $Expected, got $Actual)"
            }
            Write-Host "==> Verified SHA256 checksum ($($Actual.Substring(0,12))...)"
        }

        Expand-Archive -Path $TmpZip -DestinationPath $TmpDir -Force
        Copy-Item (Join-Path $TmpDir "pa2pa.exe") (Join-Path $InstallDir "pa2pa.exe") -Force
        if (Test-Path (Join-Path $TmpDir "pa2pa-pill.exe")) {
            Copy-Item (Join-Path $TmpDir "pa2pa-pill.exe") (Join-Path $InstallDir "pa2pa-pill.exe") -Force
        }
        $Installed = $true
    } catch {
        if ($_.Exception.Message -like "*SHA256*") { throw }
        Write-Host "Prebuilt release not found, falling back to building from source..."
    } finally {
        Remove-Item $TmpZip -ErrorAction SilentlyContinue
        Remove-Item $TmpSums -ErrorAction SilentlyContinue
        Remove-Item $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if (-not $Installed) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host "==> Installing Rust via winget..."
        winget install --id Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements
        $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    }

    if ($InLocalClone) {
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

$NewVersion = ""
try {
    $NewVersion = (& (Join-Path $InstallDir "pa2pa.exe") --version 2>$null | Select-Object -First 1)
} catch {
    $NewVersion = "pa2pa"
}

Write-Host ""
if ($OldVersion) {
    if ($OldVersion -ne $NewVersion) {
        Write-Host "Upgraded: $OldVersion -> $NewVersion (in $InstallDir)"
    } else {
        Write-Host "Reinstalled: $NewVersion (in $InstallDir)"
    }
    Write-Host "Note: If pa2pa daemon/pill is already running, restart it so the new version takes effect."
} else {
    Write-Host "Installed: $NewVersion (in $InstallDir)"
}
Write-Host ""
Write-Host "Next steps (restart your terminal if needed):"
Write-Host "  pa2pa init --name <your-name>            # zero-server P2P by default (add --relay wss://... optionally)"
Write-Host "  pa2pa setup-mcp --apply                  # register pa2pa in detected AI agents"
Write-Host "  pa2pa service install                    # start daemon + system tray icon + floating pill"
Write-Host "  pa2pa doctor                             # verify identity, MCP registrations, and services"
