#!/usr/bin/env bash
# Build release archives for every target buildable from a Linux box and
# drop them in dist/.
#
# Native target (x86_64-unknown-linux-musl) builds with plain cargo; every
# other target cross-compiles with cargo-zigbuild (zig provides the C
# toolchain for zstd-sys and friends).
#
# Platform coverage from Linux:
#   - Linux x86_64/aarch64 (musl, static)     : full (both binaries)
#   - Windows x86_64/aarch64 (gnullvm)        : full (both binaries)
#   - macOS arm64/x86_64                      : lowband-signaling ONLY.
#     lowbandd links Apple frameworks (ScreenCaptureKit, AudioToolbox, …)
#     which need the real macOS SDK — Apple's license ties it to Apple
#     hardware, so full macOS archives come from the macOS runners in
#     .github/workflows/release.yml, never from this script.
#
# Prerequisites:
#   - rustup with the target std libs installed:
#       rustup target add aarch64-unknown-linux-musl \
#                         x86_64-pc-windows-gnullvm aarch64-pc-windows-gnullvm \
#                         aarch64-apple-darwin x86_64-apple-darwin
#   - cargo-zigbuild + zig on PATH (only for cross targets)
#
# Usage:
#   packaging/build-dist.sh                 # all targets
#   packaging/build-dist.sh x86_64-unknown-linux-musl
#
# Output: dist/lowband-<version>-<target>.{tar.gz|zip} + dist/SHA256SUMS

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ALL_TARGETS=(
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-musl
  x86_64-pc-windows-gnullvm
  aarch64-pc-windows-gnullvm
  x86_64-apple-darwin
  aarch64-apple-darwin
)
TARGETS=("${@:-${ALL_TARGETS[@]}}")

mkdir -p dist

for target in "${TARGETS[@]}"; do
  echo "==> $target"

  bins=(lowbandd lowband-signaling)
  pkgs=(-p lowbandd -p lowband-signaling)
  suffix=""
  if [[ "$target" == *apple* ]]; then
    echo "    NOTE: signaling-only (lowbandd needs the macOS SDK; built in CI)"
    bins=(lowband-signaling)
    pkgs=(-p lowband-signaling)
    suffix="-signaling-only"
  fi

  if [[ "$target" == "x86_64-unknown-linux-musl" ]]; then
    cargo build --release --target "$target" "${pkgs[@]}"
  else
    cargo zigbuild --release --target "$target" "${pkgs[@]}"
  fi

  name="lowband-${VERSION}-${target}${suffix}"
  stage=$(mktemp -d)
  case "$target" in
    *windows*)
      for b in "${bins[@]}"; do cp "target/$target/release/$b.exe" "$stage/"; done
      rm -f "dist/$name.zip"
      (cd "$stage" && zip -q "$OLDPWD/dist/$name.zip" ./*)
      echo "    dist/$name.zip"
      ;;
    *)
      for b in "${bins[@]}"; do cp "target/$target/release/$b" "$stage/"; done
      tar czf "dist/$name.tar.gz" -C "$stage" .
      echo "    dist/$name.tar.gz"
      ;;
  esac
  rm -rf "$stage"
done

(cd dist && sha256sum lowband-* > SHA256SUMS)
echo "==> dist/SHA256SUMS"
cat dist/SHA256SUMS
