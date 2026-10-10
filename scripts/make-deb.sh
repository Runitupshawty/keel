#!/usr/bin/env bash
# Build a .deb from an unpacked linux-x64 release directory (the one Package release creates).
#   scripts/make-deb.sh <release-dir> <tag> <out-dir>
# Layout: everything in /usr/lib/keel (keel finds libpdfium.so next to its executable),
# /usr/bin/keel is a one-line launcher. Needs dpkg-deb only, no cargo-deb.
set -euo pipefail
src="$1"; tag="$2"; out="$3"
# Debian orders "0.7.0~rc1" before "0.7.0"; a "-" in the tag is a pre-release.
ver="${tag#v}"; ver="${ver/-/'~'}"
root="$(mktemp -d)"; trap 'rm -rf "$root"' EXIT
install -d "$root/DEBIAN" "$root/usr/lib/keel" "$root/usr/bin" "$root/usr/share/applications" \
  "$root/usr/share/icons/hicolor/256x256/apps" "$root/usr/share/doc/keel"
install -m 755 "$src/keel" "$src/keel-daemon" "$src/libpdfium.so" "$root/usr/lib/keel/"
cp -R "$src/licenses" "$root/usr/lib/keel/licenses"
printf '#!/bin/sh\nexec /usr/lib/keel/keel "$@"\n' > "$root/usr/bin/keel"; chmod 755 "$root/usr/bin/keel"
printf '#!/bin/sh\nexec /usr/lib/keel/keel-daemon "$@"\n' > "$root/usr/bin/keel-daemon"; chmod 755 "$root/usr/bin/keel-daemon"
install -m 644 "$src/keel.desktop" "$root/usr/share/applications/keel.desktop"
install -m 644 "$src/keel.png" "$root/usr/share/icons/hicolor/256x256/apps/keel.png"
install -m 644 "$src/LICENSE-MIT" "$src/LICENSE-APACHE" "$src/THIRD_PARTY.md" "$src/CHANGELOG.md" "$root/usr/share/doc/keel/"
size="$(du -sk "$root" | cut -f1)"
cat > "$root/DEBIAN/control" <<CTL
Package: keel
Version: $ver
Architecture: amd64
Maintainer: Keel contributors <noreply@users.noreply.github.com>
Section: utils
Priority: optional
Installed-Size: $size
Depends: libc6 (>= 2.35), libgtk-3-0, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libasound2 | libasound2t64
Recommends: mesa-vulkan-drivers | libvulkan1, ffmpeg
Homepage: https://github.com/Runitupshawty/keel
Description: Fast cross-platform file manager
 Keel browses and manages local, remote and cloud files, with a library index,
 duplicate finder and safe copy, move and delete previews.
CTL
mkdir -p "$out"
dpkg-deb --root-owner-group --build "$root" "$out/keel_${ver}_amd64.deb"
