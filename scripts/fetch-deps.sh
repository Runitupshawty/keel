#!/usr/bin/env bash
# Fetch pdfium for macOS/Linux into target/deps. Pinned to the release pdfium-render 0.8.37 targets (pdfium_7543).
set -euo pipefail
TAG="chromium/7543"
root="$(cd "$(dirname "$0")/.." && pwd)"
deps="$root/target/deps"; tmp="$(mktemp -d)"
mkdir -p "$deps/licenses/pdfium/third-party"
# KEEL_PDFIUM_ASSET overrides the host guess (release CI builds macOS x64 on an arm64 runner).
case "${KEEL_PDFIUM_ASSET:-$(uname -s)-$(uname -m)}" in
  pdfium-*) asset="$KEEL_PDFIUM_ASSET" ;;
  Darwin-arm64) asset=pdfium-mac-arm64 ;;
  Darwin-x86_64) asset=pdfium-mac-x64 ;;
  Linux-x86_64) asset=pdfium-linux-x64 ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
curl -fsSL "https://github.com/bblanchon/pdfium-binaries/releases/download/$TAG/$asset.tgz" -o "$tmp/pdfium.tgz"
tar -xzf "$tmp/pdfium.tgz" -C "$tmp"
cp "$tmp"/lib/libpdfium.* "$deps/"
cp "$tmp/LICENSE" "$deps/licenses/pdfium/LICENSE"
[ -d "$tmp/licenses" ] && cp -R "$tmp/licenses/." "$deps/licenses/pdfium/third-party/"
rm -rf "$tmp"
echo "deps in $deps"
