#!/usr/bin/env bash
# Build, sign, and (optionally) notarize the LowBand macOS .pkg installer.
#
# Required environment variables when signing/notarizing:
#   SIGN_APP_IDENTITY   "Developer ID Application: Acme Corp (TEAMID)"
#   SIGN_PKG_IDENTITY   "Developer ID Installer: Acme Corp (TEAMID)"
#   NOTARIZE_APPLE_ID   Apple ID used for notarization
#   NOTARIZE_TEAM_ID    10-character Apple Developer team ID
#   NOTARIZE_PASSWORD   App-specific password from appleid.apple.com
#
# Optional:
#   VERSION             Package version string (default: 0.1.0)
#   BUILD_ARCH          arm64 | x86_64 | universal (default: universal)
#   SKIP_NOTARIZE       Set to 1 to skip notarization (default: 0)
#   CARGO_PROFILE       release | debug (default: release)
#
# Usage:
#   ./packaging/macos/build_pkg.sh
#   SKIP_NOTARIZE=1 ./packaging/macos/build_pkg.sh          # local dev build
#   VERSION=1.2.3 ./packaging/macos/build_pkg.sh            # release build
set -euo pipefail

# ── Configuration ──────────────────────────────────────────────────────────────
VERSION="${VERSION:-0.1.0}"
BUILD_ARCH="${BUILD_ARCH:-universal}"
CARGO_PROFILE="${CARGO_PROFILE:-release}"
SKIP_NOTARIZE="${SKIP_NOTARIZE:-0}"
SIGN_APP_IDENTITY="${SIGN_APP_IDENTITY:-}"
SIGN_PKG_IDENTITY="${SIGN_PKG_IDENTITY:-}"
NOTARIZE_APPLE_ID="${NOTARIZE_APPLE_ID:-}"
NOTARIZE_TEAM_ID="${NOTARIZE_TEAM_ID:-}"
NOTARIZE_PASSWORD="${NOTARIZE_PASSWORD:-}"

BUNDLE_ID="com.lowband.app"
DAEMON_BUNDLE_ID="com.lowband.lowbandd"
INSTALLER_BUNDLE_ID="com.lowband.pkg"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ENTITLEMENTS_DIR="$SCRIPT_DIR/entitlements"
LAUNCHD_DIR="$SCRIPT_DIR/launchd"
INSTALLER_SCRIPTS="$SCRIPT_DIR/scripts"
DISTRIBUTION_XML="$SCRIPT_DIR/distribution.xml"

BUILD_TMP="$SCRIPT_DIR/.build"
PKG_ROOT="$BUILD_TMP/pkg_root"
COMPONENT_PKG="$BUILD_TMP/lowband-component.pkg"
DIST_PKG="$BUILD_TMP/lowband-unsigned.pkg"
OUTPUT_PKG="$REPO_ROOT/dist/lowband-${VERSION}-macos.pkg"

# ── Helpers ────────────────────────────────────────────────────────────────────
log()  { echo "  [build_pkg] $*"; }
die()  { echo "  [build_pkg] ERROR: $*" >&2; exit 1; }

require_cmd() {
    command -v "$1" &>/dev/null || die "Required command not found: $1"
}

signing_available() {
    [[ -n "$SIGN_APP_IDENTITY" && -n "$SIGN_PKG_IDENTITY" ]]
}

notarize_available() {
    [[ "$SKIP_NOTARIZE" != "1" && -n "$NOTARIZE_APPLE_ID" && -n "$NOTARIZE_TEAM_ID" && -n "$NOTARIZE_PASSWORD" ]]
}

# ── Preflight checks ──────────────────────────────────────────────────────────
require_cmd cargo
require_cmd pkgbuild
require_cmd productbuild

if signing_available; then
    require_cmd codesign
    require_cmd productsign
fi

if notarize_available; then
    require_cmd xcrun
fi

# ── Step 1: Compile Rust binaries ─────────────────────────────────────────────
log "Compiling Rust binaries (profile=$CARGO_PROFILE, arch=$BUILD_ARCH)"
cd "$REPO_ROOT"

case "$BUILD_ARCH" in
    arm64)
        cargo build --profile "$CARGO_PROFILE" --target aarch64-apple-darwin
        LOWBANDD_BIN="$REPO_ROOT/target/aarch64-apple-darwin/$CARGO_PROFILE/lowbandd"
        ;;
    x86_64)
        cargo build --profile "$CARGO_PROFILE" --target x86_64-apple-darwin
        LOWBANDD_BIN="$REPO_ROOT/target/x86_64-apple-darwin/$CARGO_PROFILE/lowbandd"
        ;;
    universal)
        cargo build --profile "$CARGO_PROFILE" --target aarch64-apple-darwin
        cargo build --profile "$CARGO_PROFILE" --target x86_64-apple-darwin
        mkdir -p "$BUILD_TMP/universal"
        lipo -create \
            "$REPO_ROOT/target/aarch64-apple-darwin/$CARGO_PROFILE/lowbandd" \
            "$REPO_ROOT/target/x86_64-apple-darwin/$CARGO_PROFILE/lowbandd" \
            -output "$BUILD_TMP/universal/lowbandd"
        LOWBANDD_BIN="$BUILD_TMP/universal/lowbandd"
        log "Universal binary created via lipo"
        ;;
    *)
        die "Unknown BUILD_ARCH: $BUILD_ARCH (use arm64, x86_64, or universal)"
        ;;
esac

[[ -f "$LOWBANDD_BIN" ]] || die "Compiled binary not found at $LOWBANDD_BIN"
log "Binary size: $(du -sh "$LOWBANDD_BIN" | cut -f1)"

# ── Step 2: Sign binaries ─────────────────────────────────────────────────────
if signing_available; then
    log "Signing binary with hardened runtime..."
    codesign \
        --sign "$SIGN_APP_IDENTITY" \
        --entitlements "$ENTITLEMENTS_DIR/lowbandd.entitlements" \
        --options runtime \
        --timestamp \
        --force \
        "$LOWBANDD_BIN"
    log "Verifying signature..."
    codesign --verify --verbose=2 "$LOWBANDD_BIN"
else
    log "WARN: SIGN_APP_IDENTITY/SIGN_PKG_IDENTITY not set — producing unsigned package"
    log "      An unsigned package cannot be installed on systems with Gatekeeper enabled."
fi

# ── Step 3: Stage the package root ───────────────────────────────────────────
log "Staging package root..."
rm -rf "$PKG_ROOT"

# Daemon binary → /usr/local/bin/
DAEMON_BIN_DIR="$PKG_ROOT/usr/local/bin"
mkdir -p "$DAEMON_BIN_DIR"
cp "$LOWBANDD_BIN" "$DAEMON_BIN_DIR/lowbandd"
chmod 755 "$DAEMON_BIN_DIR/lowbandd"

# LaunchDaemon plist → /Library/LaunchDaemons/
LAUNCHD_DEST="$PKG_ROOT/Library/LaunchDaemons"
mkdir -p "$LAUNCHD_DEST"
cp "$LAUNCHD_DIR/com.lowband.lowbandd.plist" "$LAUNCHD_DEST/"
chmod 644 "$LAUNCHD_DEST/com.lowband.lowbandd.plist"

# Data directory → /Library/Application Support/LowBand/
APPDATA_DIR="$PKG_ROOT/Library/Application Support/LowBand"
mkdir -p "$APPDATA_DIR"

# Log directory → /var/log/lowband/
LOG_DIR="$PKG_ROOT/var/log/lowband"
mkdir -p "$LOG_DIR"

# ── Step 4: Build component package ──────────────────────────────────────────
log "Building component package..."
mkdir -p "$(dirname "$COMPONENT_PKG")"

PKGBUILD_ARGS=(
    --root "$PKG_ROOT"
    --install-location "/"
    --scripts "$INSTALLER_SCRIPTS"
    --identifier "$DAEMON_BUNDLE_ID"
    --version "$VERSION"
    --ownership recommended
)

pkgbuild "${PKGBUILD_ARGS[@]}" "$COMPONENT_PKG"
log "Component package: $COMPONENT_PKG"

# ── Step 5: Build distribution package ───────────────────────────────────────
log "Building distribution package..."

PRODUCTBUILD_ARGS=(
    --distribution "$DISTRIBUTION_XML"
    --package-path "$(dirname "$COMPONENT_PKG")"
    --resources "$SCRIPT_DIR/Resources"
    --version "$VERSION"
)

if signing_available; then
    PRODUCTBUILD_ARGS+=(--sign "$SIGN_PKG_IDENTITY" --timestamp)
    productbuild "${PRODUCTBUILD_ARGS[@]}" "$OUTPUT_PKG"
    log "Signed distribution package: $OUTPUT_PKG"
else
    productbuild "${PRODUCTBUILD_ARGS[@]}" "$DIST_PKG"
    mkdir -p "$(dirname "$OUTPUT_PKG")"
    cp "$DIST_PKG" "$OUTPUT_PKG"
    log "Unsigned distribution package: $OUTPUT_PKG"
fi

# ── Step 6: Notarize and staple ───────────────────────────────────────────────
if notarize_available && signing_available; then
    log "Submitting to Apple notary service (this may take several minutes)..."
    xcrun notarytool submit "$OUTPUT_PKG" \
        --apple-id "$NOTARIZE_APPLE_ID" \
        --team-id "$NOTARIZE_TEAM_ID" \
        --password "$NOTARIZE_PASSWORD" \
        --wait \
        --timeout 900

    log "Stapling notarization ticket..."
    xcrun stapler staple "$OUTPUT_PKG"
    log "Notarization complete"
else
    if [[ "$SKIP_NOTARIZE" == "1" ]]; then
        log "Notarization skipped (SKIP_NOTARIZE=1)"
    elif ! signing_available; then
        log "Notarization skipped (no signing identity)"
    else
        log "WARN: Notarization variables not set — package will trigger Gatekeeper on macOS 10.15+"
        log "      Set NOTARIZE_APPLE_ID, NOTARIZE_TEAM_ID, and NOTARIZE_PASSWORD to notarize."
    fi
fi

# ── Done ──────────────────────────────────────────────────────────────────────
PKG_SIZE="$(du -sh "$OUTPUT_PKG" | cut -f1)"
log "Done. Package: $OUTPUT_PKG  ($PKG_SIZE)"
log ""
log "Silent install:     sudo installer -pkg $OUTPUT_PKG -target /"
log "MDM deployment:     upload to Jamf/Mosyle/Kandji and push via MDM policy"
log "Verify install:     pkgutil --pkg-info $DAEMON_BUNDLE_ID"
