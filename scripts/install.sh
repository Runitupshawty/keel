#!/usr/bin/env bash
# Install, update or remove Keel for the current user (macOS, Linux). No root needed.
#   curl -fsSL https://raw.githubusercontent.com/Runitupshawty/keel/main/scripts/install.sh | bash
#   ... | bash -s -- --uninstall
# Downloads the latest GitHub release, verifies it against the release's SHA256SUMS, installs to
# ~/.local/share/keel and links ~/.local/bin/keel. Linux gets a launcher entry, macOS a ~/Applications/Keel.app.
# KEEL_PLATFORM (macos-arm64, macos-x64, linux-x64) overrides detection; --skip-verify installs a release without SHA256SUMS.
set -euo pipefail
REPO=Runitupshawty/keel
share="$HOME/.local/share/keel"
bin="$HOME/.local/bin/keel"
dbin="$HOME/.local/bin/keel-daemon"
desktop="$HOME/.local/share/applications/keel.desktop"
icon="$HOME/.local/share/icons/hicolor/256x256/apps/keel.png"
app="$HOME/Applications/Keel.app"

uninstall=0; skip=0
for a in "$@"; do
  case "$a" in
    --uninstall) uninstall=1 ;;
    --skip-verify) skip=1 ;;
    *) echo "unknown option $a (use --uninstall or --skip-verify)" >&2; exit 2 ;;
  esac
done

if [ "$uninstall" = 1 ]; then
  pkill -f "$share/keel" 2>/dev/null || true
  rm -rf "$share" "$app" "$bin" "$dbin" "$desktop" "$icon"
  echo "Keel removed. Your settings and library index are kept."
  exit 0
fi

case "${KEEL_PLATFORM:-$(uname -s)-$(uname -m)}" in
  macos-arm64|macos-x64|linux-x64) plat="$KEEL_PLATFORM" ;;
  Darwin-arm64) plat=macos-arm64 ;;
  Darwin-x86_64) plat=macos-x64 ;;
  Linux-x86_64) plat=linux-x64 ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if command -v sha256sum >/dev/null; then sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else echo "need sha256sum or shasum" >&2; exit 1; fi

json="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")"
urls="$(printf '%s' "$json" | grep -o '"browser_download_url": *"[^"]*"' | cut -d'"' -f4)"
tag="$(printf '%s' "$json" | grep -m1 -o '"tag_name": *"[^"]*"' | cut -d'"' -f4)"
url="$(printf '%s\n' "$urls" | grep -- "-$plat\.tar\.gz\$" | head -n1 || true)"
[ -n "$url" ] || { echo "release $tag has no $plat archive" >&2; exit 1; }
sums_url="$(printf '%s\n' "$urls" | grep '/SHA256SUMS$' | head -n1 || true)"

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
name="${url##*/}"
echo "Downloading Keel $tag ($name)..."
curl -fsSL "$url" -o "$tmp/$name"

if [ -n "$sums_url" ]; then
  curl -fsSL "$sums_url" -o "$tmp/SHA256SUMS"
  want="$(awk -v n="$name" '{f=$2; sub(/^\*/,"",f); sub(/.*\//,"",f); if (f==n) print $1}' "$tmp/SHA256SUMS" | head -n1)"
  [ -n "$want" ] || { echo "SHA256SUMS has no entry for $name" >&2; exit 1; }
  got="$(sha "$tmp/$name")"
  [ "$got" = "$want" ] || { echo "checksum mismatch for $name: expected $want, got $got. Nothing was installed." >&2; exit 1; }
  echo "Checksum OK."
elif [ "$skip" = 1 ]; then
  echo "warning: release $tag has no SHA256SUMS; installing unverified (--skip-verify)" >&2
else
  echo "release $tag has no SHA256SUMS, so the download cannot be verified. Install a newer release, or pass --skip-verify." >&2
  exit 1
fi

mkdir "$tmp/x" && tar xzf "$tmp/$name" -C "$tmp/x"
src="$(find "$tmp/x" -mindepth 1 -maxdepth 1 -type d | head -n1)"
[ -f "$src/keel" ] || { echo "the archive does not contain keel" >&2; exit 1; }
[ -f "$src/keel-daemon" ] || { echo "the archive does not contain keel-daemon" >&2; exit 1; }

rm -rf "$share"; mkdir -p "$share" "$(dirname "$bin")"
cp -R "$src/." "$share/"
chmod +x "$share/keel" "$share/keel-daemon"
ln -sf "$share/keel" "$bin"
ln -sf "$share/keel-daemon" "$dbin"

case "$plat" in
  linux-*)
    mkdir -p "$(dirname "$desktop")" "$(dirname "$icon")"
    cp "$share/keel.png" "$icon"
    sed "s|^Exec=.*|Exec=$share/keel %f|" "$share/keel.desktop" > "$desktop"
    ;;
  macos-*)
    xattr -dr com.apple.quarantine "$share" 2>/dev/null || true
    mkdir -p "$app/Contents/MacOS"
    printf '#!/bin/sh\nexec "%s/keel" "$@"\n' "$share" > "$app/Contents/MacOS/Keel"
    chmod +x "$app/Contents/MacOS/Keel"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>Keel</string>
<key>CFBundleIdentifier</key><string>io.github.runitupshawty.keel</string>
<key>CFBundleExecutable</key><string>Keel</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>${tag#v}</string>
<key>CFBundleShortVersionString</key><string>${tag#v}</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
    ;;
esac

echo "Keel $tag installed to $share."
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Add \$HOME/.local/bin to your PATH to run 'keel' from a terminal." ;; esac
