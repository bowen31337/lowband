#!/usr/bin/env pwsh
# Build, sign, and (optionally) timestamp-stamp the LowBand Windows .msi installer.
#
# Required environment variables when signing:
#   SIGN_THUMBPRINT       Certificate SHA-1 thumbprint in the local machine store
#                         (preferred — exact-match, no ambiguity).
#                         Retrieve with:
#                           Get-ChildItem Cert:\LocalMachine\My |
#                             Select-Object Thumbprint, Subject
#   SIGN_SUBJECT          Partial subject string — used when SIGN_THUMBPRINT is
#                         not set (e.g. "LowBand Project").  Passed to signtool
#                         as /n "<subject>".
#   SIGN_TIMESTAMP_URL    RFC 3161 timestamp authority URL.
#                         Default: http://timestamp.digicert.com
#
# Optional:
#   VERSION               Package version string, e.g. "1.2.3" (default: 0.1.0).
#                         Must be a dotted triple or quad to be valid in an MSI.
#   SKIP_SIGN             Set to "1" to produce an unsigned MSI (dev/CI builds).
#   CARGO_PROFILE         release | debug  (default: release)
#   WIX_BIN               Override path to the WiX bin directory if candle/light
#                         are not on %PATH% and not in the default install locations.
#
# Usage:
#   .\packaging\windows\build_msi.ps1
#   $env:SKIP_SIGN="1"; .\packaging\windows\build_msi.ps1       # dev build
#   $env:VERSION="1.2.3"; .\packaging\windows\build_msi.ps1     # release build
#
# Prerequisites:
#   - Rust toolchain with target x86_64-pc-windows-msvc
#   - WiX Toolset 3.x  (https://wixtoolset.org) — candle.exe + light.exe on PATH
#     or installed to the default path (%ProgramFiles(x86)%\WiX Toolset v3.x\bin\)
#   - Windows SDK signtool.exe on PATH (included with Visual Studio Build Tools)
#   - A code-signing certificate in Cert:\LocalMachine\My (for signing)
#
# Output:
#   dist\lowband-<VERSION>-windows.msi

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ── Configuration ──────────────────────────────────────────────────────────────
$Version           = if ($env:VERSION)           { $env:VERSION }           else { "0.1.0" }
$CargoProfile      = if ($env:CARGO_PROFILE)      { $env:CARGO_PROFILE }      else { "release" }
$SkipSign          = $env:SKIP_SIGN -eq "1"
$SignThumbprint    = $env:SIGN_THUMBPRINT
$SignSubject       = $env:SIGN_SUBJECT
$TimestampUrl      = if ($env:SIGN_TIMESTAMP_URL) { $env:SIGN_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }

$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot    = (Resolve-Path (Join-Path $ScriptDir ".." "..")).Path
$WxsSource   = Join-Path $ScriptDir "lowband.wxs"
$BuildTmp    = Join-Path $ScriptDir ".build"
$DistDir     = Join-Path $RepoRoot "dist"
$OutputMsi   = Join-Path $DistDir "lowband-$Version-windows.msi"
$WixObj      = Join-Path $BuildTmp "lowband.wixobj"

$Target      = "x86_64-pc-windows-msvc"
$BinDir      = Join-Path $RepoRoot "target" $Target $CargoProfile

# ── Helpers ─────────────────────────────────────────────────────────────────────
function Log  { Write-Host "  [build_msi] $args" }
function Die  { Write-Error "  [build_msi] ERROR: $args"; exit 1 }

function Find-Exe {
    param([string]$Name)
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

function Require-Exe {
    param([string]$Name, [string]$InstallHint = "")
    $path = Find-Exe $Name
    if (-not $path) {
        $msg = "Required command not found: $Name"
        if ($InstallHint) { $msg += "`n  $InstallHint" }
        Die $msg
    }
    return $path
}

function Signing-Available {
    return -not $SkipSign -and ($SignThumbprint -or $SignSubject)
}

# ── Locate WiX tools ────────────────────────────────────────────────────────────
function Find-WixBin {
    # 1. Caller override
    if ($env:WIX_BIN -and (Test-Path (Join-Path $env:WIX_BIN "candle.exe"))) {
        return $env:WIX_BIN
    }
    # 2. %WIX% environment variable set by WiX installer
    if ($env:WIX -and (Test-Path (Join-Path $env:WIX "bin" "candle.exe"))) {
        return Join-Path $env:WIX "bin"
    }
    # 3. Already on PATH
    if (Find-Exe "candle.exe") {
        return $null  # null means "use PATH"
    }
    # 4. Scan default WiX 3.x install location
    $pf86 = ${env:ProgramFiles(x86)}
    if ($pf86) {
        $candidates = Get-ChildItem -Path $pf86 -Filter "WiX Toolset v3*" -Directory -ErrorAction SilentlyContinue |
                      Sort-Object Name -Descending
        foreach ($dir in $candidates) {
            $bin = Join-Path $dir.FullName "bin"
            if (Test-Path (Join-Path $bin "candle.exe")) {
                return $bin
            }
        }
    }
    return $null
}

$WixBin = Find-WixBin
if ($WixBin) {
    $env:PATH = "$WixBin;$env:PATH"
    Log "Using WiX from: $WixBin"
} else {
    # Verify candle is actually on PATH now
    $null = Require-Exe "candle.exe" `
        "Install WiX Toolset 3.x from https://wixtoolset.org or add it to PATH."
}

# ── Preflight checks ────────────────────────────────────────────────────────────
$null = Require-Exe "cargo" "Install the Rust toolchain from https://rustup.rs"
$null = Require-Exe "candle.exe" "Install WiX Toolset 3.x from https://wixtoolset.org"
$null = Require-Exe "light.exe"  "Install WiX Toolset 3.x from https://wixtoolset.org"

if (Signing-Available) {
    $null = Require-Exe "signtool.exe" `
        "Install Windows SDK or Visual Studio Build Tools, then ensure signtool.exe is on PATH."
}

# Version must be a dotted triple or quad (MSI requirement)
if ($Version -notmatch '^\d+\.\d+\.\d+(\.\d+)?$') {
    Die "VERSION '$Version' is not a valid MSI version string (must be N.N.N or N.N.N.N)."
}

Log "Version=$Version  Profile=$CargoProfile  Sign=$(if (Signing-Available) { 'yes' } else { 'no (WARN: unsigned MSI)' })"

# ── Step 1: Compile Rust binary ─────────────────────────────────────────────────
Log "Compiling Rust binary (profile=$CargoProfile, target=$Target)"
Push-Location $RepoRoot
try {
    cargo build --profile $CargoProfile --target $Target
    if ($LASTEXITCODE -ne 0) { Die "cargo build failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

$DaemonBin = Join-Path $BinDir "lowbandd.exe"
if (-not (Test-Path $DaemonBin)) {
    Die "Compiled binary not found at: $DaemonBin"
}
$BinSize = (Get-Item $DaemonBin).Length / 1MB
Log ("Binary size: {0:N1} MB" -f $BinSize)

# ── Step 2: Sign binary ─────────────────────────────────────────────────────────
if (Signing-Available) {
    Log "Signing lowbandd.exe..."
    $signArgs = @("sign")

    if ($SignThumbprint) {
        $signArgs += @("/sha1", $SignThumbprint)
    } elseif ($SignSubject) {
        $signArgs += @("/n", $SignSubject)
    }

    $signArgs += @(
        "/fd",  "sha256",
        "/tr",  $TimestampUrl,
        "/td",  "sha256",
        "/d",   "LowBand Remote-Assist Daemon",
        $DaemonBin
    )

    & signtool.exe @signArgs
    if ($LASTEXITCODE -ne 0) { Die "signtool sign (binary) failed (exit $LASTEXITCODE)" }

    Log "Verifying binary signature..."
    & signtool.exe verify /pa /v $DaemonBin
    if ($LASTEXITCODE -ne 0) { Die "signtool verify (binary) failed (exit $LASTEXITCODE)" }
} else {
    Log "WARN: Signing skipped — producing unsigned binary"
    Log "      An unsigned binary may trigger Windows Defender SmartScreen warnings."
}

# ── Step 3: Compile WiX source ──────────────────────────────────────────────────
Log "Compiling WiX source ($WxsSource)..."
New-Item -ItemType Directory -Force -Path $BuildTmp | Out-Null

$candleArgs = @(
    "-nologo",
    "-arch", "x64",
    "-dVersion=$Version",
    "-dBinDir=$BinDir",
    "-ext", "WixUtilExtension",
    "-out", $WixObj,
    $WxsSource
)

& candle.exe @candleArgs
if ($LASTEXITCODE -ne 0) { Die "candle.exe failed (exit $LASTEXITCODE)" }
Log "WiX object: $WixObj"

# ── Step 4: Link MSI ─────────────────────────────────────────────────────────────
Log "Linking MSI..."
New-Item -ItemType Directory -Force -Path $DistDir | Out-Null

$UnsignedMsi = Join-Path $BuildTmp "lowband-unsigned.msi"

$lightArgs = @(
    "-nologo",
    "-ext", "WixUtilExtension",
    "-cultures:en-us",
    "-out", $UnsignedMsi,
    $WixObj
)

& light.exe @lightArgs
if ($LASTEXITCODE -ne 0) { Die "light.exe failed (exit $LASTEXITCODE)" }
Log "Unsigned MSI: $UnsignedMsi"

# ── Step 5: Sign MSI ─────────────────────────────────────────────────────────────
if (Signing-Available) {
    Log "Signing MSI package..."
    $msiSignArgs = @("sign")

    if ($SignThumbprint) {
        $msiSignArgs += @("/sha1", $SignThumbprint)
    } elseif ($SignSubject) {
        $msiSignArgs += @("/n", $SignSubject)
    }

    $msiSignArgs += @(
        "/fd",  "sha256",
        "/tr",  $TimestampUrl,
        "/td",  "sha256",
        "/d",   "LowBand $Version",
        "/du",  "https://lowband.example.com",
        $UnsignedMsi
    )

    & signtool.exe @msiSignArgs
    if ($LASTEXITCODE -ne 0) { Die "signtool sign (MSI) failed (exit $LASTEXITCODE)" }

    Log "Verifying MSI signature..."
    & signtool.exe verify /pa /v $UnsignedMsi
    if ($LASTEXITCODE -ne 0) { Die "signtool verify (MSI) failed (exit $LASTEXITCODE)" }

    Copy-Item -Path $UnsignedMsi -Destination $OutputMsi -Force
    Log "Signed MSI: $OutputMsi"
} else {
    Copy-Item -Path $UnsignedMsi -Destination $OutputMsi -Force
    Log "Unsigned MSI: $OutputMsi"
    Log "WARN: Unsigned MSI will trigger UAC 'Unknown publisher' warnings."
    Log "      Set SIGN_THUMBPRINT or SIGN_SUBJECT to produce a signed package."
}

# ── Done ─────────────────────────────────────────────────────────────────────────
$MsiSize = (Get-Item $OutputMsi).Length / 1MB
Log ""
Log ("Done. Package: $OutputMsi  ({0:N1} MB)" -f $MsiSize)
Log ""
Log "Silent install:      msiexec /i `"$OutputMsi`" /qn /norestart"
Log "Silent uninstall:    msiexec /x `"$OutputMsi`" /qn /norestart"
Log "MDM deployment:      upload to Intune/SCCM and target the device group"
Log "Verify install:      sc query LowBandDaemon"
Log "Service logs:        Get-Content `"%ProgramData%\LowBand\Logs\lowbandd.log`""
