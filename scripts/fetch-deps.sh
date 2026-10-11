#!/usr/bin/env bash
# Fetch pdfium for macOS/Linux into target/deps. Pinned to the release pdfium-render 0.8.37 targets (pdfium_7543).
set -euo pipefail
# The pdfium-binaries release and the SHA-256 of each of its assets (GitHub's digest of the
# asset, checked against a download). The release tag and every hash below change together,
# deliberately, in this script and in fetch-deps.ps1: download each asset of the new tag and
# `sha256sum` it. A download that does not match is refused.
TAG="chromium/7543"
root="$(cd "$(dirname "$0")/.." && pwd)"
deps="$root/target/deps"; tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$deps/licenses/pdfium/third-party"
# KEEL_PDFIUM_ASSET overrides the host guess (release CI builds macOS x64 on an arm64 runner).
case "${KEEL_PDFIUM_ASSET:-$(uname -s)-$(uname -m)}" in
  pdfium-*) asset="$KEEL_PDFIUM_ASSET" ;;
  Darwin-arm64) asset=pdfium-mac-arm64 ;;
  Darwin-x86_64) asset=pdfium-mac-x64 ;;
  Linux-x86_64) asset=pdfium-linux-x64 ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
case "$asset" in
  pdfium-mac-arm64) want=41c269723b4711793de70ff34e65c00fa79907d6c023741837579e906b846faa ;;
  pdfium-mac-x64) want=2510460ac106f14b884598a0da3f53a99e23d79512acf027c5e101c2bb2f26cb ;;
  pdfium-linux-x64) want=9329a3c4b19b3c8d0a93af5440f44be84e4bd879a204e47b1a7a160e96809da4 ;;
  *) echo "no pinned SHA-256 for $asset of pdfium-binaries $TAG" >&2; exit 1 ;;
esac
curl -fsSL "https://github.com/bblanchon/pdfium-binaries/releases/download/$TAG/$asset.tgz" -o "$tmp/pdfium.tgz"
if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/pdfium.tgz")"; else got="$(shasum -a 256 "$tmp/pdfium.tgz")"; fi
got="${got%% *}"
if [ "$got" != "$want" ]; then
  echo "$asset.tgz of pdfium-binaries $TAG: SHA-256 $got, expected $want; nothing installed" >&2
  exit 1
fi
tar -xzf "$tmp/pdfium.tgz" -C "$tmp"
cp "$tmp"/lib/libpdfium.* "$deps/"
cp "$tmp/LICENSE" "$deps/licenses/pdfium/LICENSE"
[ -d "$tmp/licenses" ] && cp -R "$tmp/licenses/." "$deps/licenses/pdfium/third-party/"
echo "deps in $deps"
