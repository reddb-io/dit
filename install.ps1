# dit installer (Windows / PowerShell)
#
# Usage:
#   irm https://raw.githubusercontent.com/reddb-io/dit/main/install.ps1 | iex
#
#   # pin a version or change the install dir:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/reddb-io/dit/main/install.ps1))) -Version v0.1.0

[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\dit"
)

$ErrorActionPreference = "Stop"
$Repo = "reddb-io/dit"

function Info($m) { Write-Host "› $m" -ForegroundColor Cyan }
function Warn($m) { Write-Host "! $m" -ForegroundColor Yellow }

function Test-AssetExists($u) {
    try { Invoke-WebRequest $u -Method Head -UseBasicParsing | Out-Null; return $true }
    catch { return $false }
}

# --- resolve the release tag -----------------------------------------------
if ([string]::IsNullOrEmpty($Version)) {
    Info "resolving latest release…"
    $headers = @{ "User-Agent" = "dit-install" }
    $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    $tag = $rel.tag_name
} else {
    $tag = $Version
}
if ([string]::IsNullOrEmpty($tag)) { throw "Could not determine a release tag for $Repo" }

# --- detect arch → asset name ----------------------------------------------
# ARM64 gets the native build when the release published one; older releases
# (and any release whose best-effort ARM64 leg failed) fall back to x86_64,
# which Windows runs under emulation.
$base = "https://github.com/$Repo/releases/download/$tag"
$osArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($osArch -eq "Arm64") {
    if (Test-AssetExists "$base/dit-windows-aarch64.exe") {
        $asset = "dit-windows-aarch64.exe"
    } else {
        Warn "no ARM64 build in $tag; using the x86_64 build (runs under emulation)."
        $asset = "dit-windows-x86_64.exe"
    }
} else {
    $asset = "dit-windows-x86_64.exe"
}
$platform = ($asset -replace '^dit-', '') -replace '\.exe$', ''

# --- download ---------------------------------------------------------------
$url = "$base/$asset"
$tmp = New-TemporaryFile
Info "installing dit $tag ($platform)"
Invoke-WebRequest $url -OutFile $tmp -UseBasicParsing

# --- verify checksum (skips if sidecar missing) -----------------------------
$sumText = $null
try {
    $sumText = (Invoke-WebRequest "$url.sha256" -UseBasicParsing).Content
} catch {
    Warn "no checksum published; skipping verification"
}
if ($sumText) {
    # Sidecars are `<digest>  <asset>`, but releases up to v0.3.0 shipped the raw
    # three-line `certutil -hashfile` report on Windows, whose first token is the
    # literal "SHA256". Pull the first 64-char hex run instead of the first token.
    $m = [regex]::Match($sumText, '(?im)\b[0-9a-f]{64}\b')
    if (-not $m.Success) {
        Warn "checksum sidecar for $asset is unreadable; skipping verification"
    } else {
        $expected = $m.Value.ToLower()
        $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            Remove-Item $tmp -Force
            throw "Checksum mismatch for $asset (expected $expected, got $actual)"
        }
        Info "checksum OK"
    }
}

# --- install ----------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$dest = Join-Path $InstallDir "dit.exe"
Move-Item -Force $tmp $dest
Info "installed → $dest"

# --- add to the user PATH ---------------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
    Warn "added $InstallDir to your PATH — restart the terminal to pick it up"
}

Write-Host ""
Write-Host "✓ done" -ForegroundColor Green
Write-Host "Next:"
Write-Host "  New-Item -ItemType Directory -Force `"$env:USERPROFILE\.red\dit`" | Out-Null"
Write-Host "  'ELEVENLABS_API_KEY=sk_your_key_here' | Out-File -Encoding ascii `"$env:USERPROFILE\.red\dit\.env`""
Write-Host "  dit --help     # press F9 to start/stop dictation"
