#Requires -Version 5.1
<#
.SYNOPSIS
    Build the ai-psd-thumbnails bootstrapper bundle (setup exe).

.DESCRIPTION
    1. Builds the MSI first via build-msi.ps1 (payload assembly + wix build).
    2. Compiles bundle.wxs into ai-psd-thumbnails-setup-<version>.exe (WiX Burn).

    The bundle chains the official Ghostscript installer (downloaded from the
    Artifex GitHub release at INSTALL time -- never embedded; skipped when
    Ghostscript is already present) followed by ai-psd-thumbnails.msi.

.PARAMETER Version
    Bundle/MSI version (default: 0.5.2). Must match bundle.wxs and
    ai-psd-thumbnails.wxs.

.EXAMPLE
    .\installer\build-bundle.ps1
#>
param(
    [string]$Version = "0.5.2",
    [string]$OutDir  = $PSScriptRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$BundleOut = Join-Path $OutDir "ai-psd-thumbnails-setup-$Version.exe"

Write-Host "==> Building MSI first..."
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "build-msi.ps1") -Version $Version
if ($LASTEXITCODE -ne 0) {
    Write-Error "build-msi.ps1 failed with exit code $LASTEXITCODE"
}

if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
    Write-Error "WiX v4 CLI not found on PATH. Install: dotnet tool install --global wix --version 4.0.6"
}

Write-Host "==> Building bundle: $BundleOut"
Push-Location $PSScriptRoot
try {
    & wix build `
        (Join-Path $PSScriptRoot "bundle.wxs") `
        -ext WixToolset.Bal.wixext `
        -ext WixToolset.Util.wixext `
        -o $BundleOut

    if ($LASTEXITCODE -ne 0) {
        Write-Error "wix build (bundle) failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

Write-Host ""
Write-Host "==> Bundle built successfully: $BundleOut"
Write-Host "    Chain: [Ghostscript 10.07.1 via DownloadUrl, skipped if installed] -> [ai-psd-thumbnails.msi]"
