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

# --- detect arch → asset name ----------------------------------------------
$osArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($osArch -eq "Arm64") {
    Warn "Windows on ARM detected; using the x86_64 build (runs under emulation)."
}
# We currently ship a single Windows asset.
$asset = "dit-windows-x86_64.exe"

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

# --- download ---------------------------------------------------------------
$url = "https://github.com/$Repo/releases/download/$tag/$asset"
$tmp = New-TemporaryFile
Info "installing dit $tag (windows-x86_64)"
Invoke-WebRequest $url -OutFile $tmp -UseBasicParsing

# --- verify checksum (skips if sidecar missing) -----------------------------
$sumLine = $null
try {
    $sumLine = (Invoke-WebRequest "$url.sha256" -UseBasicParsing).Content
} catch {
    Warn "no checksum published; skipping verification"
}
if ($sumLine) {
    $expected = ($sumLine -split '\s+')[0].Trim().ToLower()
    $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
    if ($expected -and ($expected -ne $actual)) {
        Remove-Item $tmp -Force
        throw "Checksum mismatch for $asset (expected $expected, got $actual)"
    }
    Info "checksum OK"
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
Write-Host "  'ELEVENLABS_API_KEY=sk_your_key_here' | Out-File -Encoding ascii `"$env:USERPROFILE\.dit.env`""
Write-Host "  dit --help     # press F9 to start/stop dictation"
