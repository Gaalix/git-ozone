# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Single-command installer for Ozone (oz.exe, git-ozone.exe, oz-vfsd.exe) on Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/install.ps1 | iex
#   .\packaging\install.ps1 -Version 1.0.0 -Prefix "$env:LOCALAPPDATA\Ozone" -DryRun

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:OZ_VERSION) { $env:OZ_VERSION } else { "1.0.0" }),
    [string]$Prefix = $(if ($env:OZ_INSTALL_PREFIX) { $env:OZ_INSTALL_PREFIX } else { Join-Path $env:LOCALAPPDATA "Ozone" }),
    [string]$Repo = $(if ($env:OZ_REPO) { $env:OZ_REPO } else { "Gaalix/git-ozone" }),
    [switch]$DryRun,
    [switch]$NoPathUpdate,
    [switch]$NoDoctor
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Write-Info {
    param([string]$Message)
    Write-Host "[ozone-install] $Message"
}

function Write-Warn {
    param([string]$Message)
    Write-Warning "[ozone-install] $Message"
}

function Get-OzoneTarget {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) {
        $arch = $env:PROCESSOR_ARCHITEW6432
    }
    switch ($arch.ToUpperInvariant()) {
        "AMD64" { return "x86_64-pc-windows-msvc" }
        "X64"   { return "x86_64-pc-windows-msvc" }
        default {
            throw "Unsupported Windows architecture '$arch'. Prebuilt Windows binaries currently target x86_64-pc-windows-msvc."
        }
    }
}

function Resolve-LatestVersion {
    param([string]$RepoSlug)
    $apiUrl = "https://api.github.com/repos/$RepoSlug/releases/latest"
    $release = Invoke-RestMethod -Uri $apiUrl -Headers @{ "User-Agent" = "ozone-installer" }
    if (-not $release.tag_name) {
        throw "Failed to resolve latest release tag from $apiUrl"
    }
    return ($release.tag_name -replace '^v', '')
}

# Ensure TLS 1.2 on Windows PowerShell 5.1
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # Ignore on platforms where SecurityProtocol is managed by the runtime
}

$Target = Get-OzoneTarget

if ($Version -eq "latest") {
    if ($DryRun) {
        $Version = "1.0.0"
        Write-Info "dry-run: using default version $Version instead of querying GitHub API for 'latest'"
    } else {
        $Version = Resolve-LatestVersion -RepoSlug $Repo
    }
}

$CleanVersion = $Version -replace '^v', ''
$Tag = "v$CleanVersion"
$PkgName = "oz-$Tag-$Target"
$ArchiveName = "$PkgName.zip"
$BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
$ArchiveUrl = "$BaseUrl/$ArchiveName"
$ChecksumsUrl = "$BaseUrl/SHA256SUMS.txt"
$BinDir = Join-Path $Prefix "bin"

if ($DryRun) {
    Write-Info "dry-run mode enabled (no files will be downloaded or written)"
    Write-Info "target:       $Target"
    Write-Info "version:      $CleanVersion ($Tag)"
    Write-Info "archive url:  $ArchiveUrl"
    Write-Info "checksum url: $ChecksumsUrl"
    Write-Info "install dir:  $BinDir"
    Write-Info "binaries:     oz.exe, git-ozone.exe, oz-vfsd.exe"
    Write-Info "path update:  $(-not $NoPathUpdate)"
    return
}

$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("ozone-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null

try {
    $ArchivePath = Join-Path $TempDir $ArchiveName
    $ChecksumsPath = Join-Path $TempDir "SHA256SUMS.txt"

    Write-Info "Downloading $ArchiveName ($Tag)..."
    Invoke-WebRequest -Uri $ArchiveUrl -OutFile $ArchivePath -UseBasicParsing

    Write-Info "Downloading SHA256SUMS.txt..."
    Invoke-WebRequest -Uri $ChecksumsUrl -OutFile $ChecksumsPath -UseBasicParsing

    $ExpectedHash = $null
    foreach ($line in (Get-Content -Path $ChecksumsPath)) {
        if ($line -match '^\s*([0-9a-fA-F]{64})\s+\*?(.+?)\s*$') {
            if ($Matches[2] -eq $ArchiveName) {
                $ExpectedHash = $Matches[1].ToLowerInvariant()
                break
            }
        }
    }
    if (-not $ExpectedHash) {
        throw "Checksum entry for $ArchiveName not found in SHA256SUMS.txt"
    }

    $ActualHash = (Get-FileHash -Path $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($ActualHash -ne $ExpectedHash) {
        throw "SHA256 checksum mismatch for $ArchiveName (expected $ExpectedHash, got $ActualHash)"
    }
    Write-Info "Verified SHA256 checksum ($ActualHash)"

    $ExtractDir = Join-Path $TempDir "extract"
    Expand-Archive -Path $ArchivePath -DestinationPath $ExtractDir -Force

    $SrcDir = Join-Path $ExtractDir $PkgName
    if (-not (Test-Path $SrcDir)) {
        $SrcDir = $ExtractDir
    }

    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    foreach ($binName in @("oz.exe", "git-ozone.exe", "oz-vfsd.exe")) {
        $srcBin = Join-Path $SrcDir $binName
        if (-not (Test-Path $srcBin)) {
            throw "Expected binary '$binName' not found inside $ArchiveName"
        }
        Copy-Item -Path $srcBin -Destination (Join-Path $BinDir $binName) -Force
    }
    Write-Info "Installed oz.exe, git-ozone.exe, and oz-vfsd.exe into $BinDir"
} finally {
    if (Test-Path $TempDir) {
        Remove-Item -Path $TempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if (-not $NoPathUpdate) {
    $CurrentUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = @()
    if ($CurrentUserPath) {
        $PathEntries = $CurrentUserPath -split ';' | Where-Object { $_ -and $_.Trim() -ne '' }
    }
    $NormalizedBinDir = $BinDir.TrimEnd('\')
    $AlreadyPresent = $false
    foreach ($entry in $PathEntries) {
        if ($entry.TrimEnd('\').Equals($NormalizedBinDir, [System.StringComparison]::OrdinalIgnoreCase)) {
            $AlreadyPresent = $true
            break
        }
    }
    if (-not $AlreadyPresent) {
        $NewUserPath = if ($CurrentUserPath) { "$CurrentUserPath;$BinDir" } else { $BinDir }
        [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
        Write-Info "Added $BinDir to User PATH (restart new terminals to pick up changes)"
    }
    if (-not ($env:Path -like "*$NormalizedBinDir*")) {
        $env:Path = "$BinDir;$env:Path"
    }
}

$ProjFsDll = Join-Path $env:SystemRoot "System32\ProjectedFSLib.dll"
if (-not (Test-Path $ProjFsDll)) {
    Write-Info "Note: Windows Projected File System (Client-ProjFS) is not yet enabled."
    Write-Info "For instant zero-copy NTFS workspaces, run ``oz setup`` in an Administrator terminal (no reboot required)."
}

if (-not $NoDoctor) {
    Write-Info "Running ``oz doctor``..."
    & (Join-Path $BinDir "oz.exe") doctor
    if ($LASTEXITCODE -ne 0) {
        Write-Warn "``oz doctor`` reported actionable setup items; see output above."
    }
}

Write-Info "Ozone $Tag installation complete."
