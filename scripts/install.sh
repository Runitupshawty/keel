#!/usr/bin/env bash
# Install, update or remove Keel for the current user (macOS, Linux). No root needed.
#   curl -fsSL https://raw.githubusercontent.com/Runitupshawty/keel/main/scripts/install.sh | bash
#   ... | bash -s -- --uninstall
# Downloads the latest GitHub release, verifies it against the release's SHA256SUMS (and that
# file's GPG signature, SHA256SUMS.asc, when the release has one and gpg is installed; on macOS
# also the binaries' code signature when they are signed), installs to ~/.local/share/keel and
# links ~/.local/bin/keel. Linux gets a launcher entry, macOS a ~/Applications/Keel.app.
# Inside WSL, `keel <folder>` then opens the folder in the Windows Keel (keel.exe on the
# Windows PATH), and `keel --here <folder>` in this Linux window; see docs/wsl.md.
# KEEL_PLATFORM (macos-arm64, macos-x64, linux-x64) overrides detection; --skip-verify installs
# a release without SHA256SUMS, or an unsigned one while a signer is pinned below.
set -euo pipefail
REPO=Runitupshawty/keel
# Who signs Keel (docs/releasing.md). Empty: a signature is checked and reported, and an
# unsigned release only warns. Set: a release signed by anyone else is refused, and so is an
# unsigned one unless --skip-verify.
# The release key's full fingerprint (40 hex digits, upper case, no spaces).
KEEL_GPG_FINGERPRINT=""
# The Team ID of the Developer ID Application certificate.
KEEL_APPLE_TEAM_ID=""
skip=0

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# verify_sums DIR NAME: DIR/NAME's SHA-256 must be the one DIR/SHA256SUMS lists for NAME.
verify_sums() {
  local want got
  want="$(awk -v n="$2" '{f=$2; sub(/^\*/,"",f); sub(/.*\//,"",f); if (f==n) print tolower($1)}' "$1/SHA256SUMS" | head -n1)"
  [ -n "$want" ] || { echo "SHA256SUMS has no entry for $2" >&2; return 1; }
  got="$(sha256 "$1/$2")"
  [ "$got" = "$want" ] || { echo "checksum mismatch for $2: expected $want, got $got. Nothing was installed." >&2; return 1; }
}

# verify_sums_sig DIR: checks DIR/SHA256SUMS.asc against DIR/SHA256SUMS with the release's key
# (DIR/SHA256SUMS.key.asc) in a throwaway keyring. Returns 0 when it verifies or there is
# nothing to check (with a warning), 1 to refuse the release.
verify_sums_sig() {
  local dir="$1" home status="" fpr ok=1
  if [ ! -f "$dir/SHA256SUMS.asc" ]; then
    if [ -n "$KEEL_GPG_FINGERPRINT" ] && [ "$skip" != 1 ]; then
      echo "the release's SHA256SUMS is not signed, but this installer expects Keel's signature. Nothing was installed. Pass --skip-verify to install it anyway." >&2
      return 1
    fi
    echo "warning: SHA256SUMS is not signed; the checksum is all that was verified." >&2
    return 0
  fi
  if ! command -v gpg >/dev/null; then
    echo "warning: gpg is not installed, so the signature of SHA256SUMS was not checked." >&2
    return 0
  fi
  [ -f "$dir/SHA256SUMS.key.asc" ] || { echo "the release has SHA256SUMS.asc but no SHA256SUMS.key.asc. Nothing was installed." >&2; return 1; }
  home="$(mktemp -d)"
  GNUPGHOME="$home" gpg --batch --quiet --import "$dir/SHA256SUMS.key.asc" 2>/dev/null &&
    status="$(GNUPGHOME="$home" gpg --batch --status-fd 1 --verify "$dir/SHA256SUMS.asc" "$dir/SHA256SUMS" 2>/dev/null)" || ok=0
  GNUPGHOME="$home" gpgconf --kill all >/dev/null 2>&1 || true
  rm -rf "$home"
  # VALIDSIG's last field is the primary key's fingerprint (also when a subkey signed).
  fpr="$(printf '%s\n' "$status" | awk '$1=="[GNUPG:]" && $2=="VALIDSIG" {print $NF; exit}')"
  if [ "$ok" = 0 ] || [ -z "$fpr" ]; then
    echo "SHA256SUMS.asc does not verify, so the checksums cannot be trusted. Nothing was installed." >&2
    return 1
  fi
  if [ -z "$KEEL_GPG_FINGERPRINT" ]; then
    echo "Signature OK, by key $fpr (this installer pins no key yet; docs/releasing.md lists Keel's)."
  elif [ "$fpr" = "$KEEL_GPG_FINGERPRINT" ]; then
    echo "Signature OK."
  else
    echo "SHA256SUMS is signed by key $fpr, not by Keel's key $KEEL_GPG_FINGERPRINT. Nothing was installed." >&2
    return 1
  fi
}

# mac_signed FILE...: 0 when every FILE has a valid Developer ID code signature (of
# KEEL_APPLE_TEAM_ID when set), 1 when none is signed (or only ad-hoc), 2 to refuse: a broken
# signature, another team, or only some files signed.
mac_signed() {
  local f team n=0
  command -v codesign >/dev/null || return 1
  for f in "$@"; do
    team="$(codesign -dv "$f" 2>&1 | sed -n 's/^TeamIdentifier=//p' || true)"
    case "$team" in "" | "not set") continue ;; esac
    codesign --verify --strict "$f" 2>/dev/null || { echo "${f##*/} has a broken code signature. Nothing was installed." >&2; return 2; }
    if [ -n "$KEEL_APPLE_TEAM_ID" ] && [ "$team" != "$KEEL_APPLE_TEAM_ID" ]; then
      echo "${f##*/} is signed by team $team, not by Keel's team $KEEL_APPLE_TEAM_ID. Nothing was installed." >&2
      return 2
    fi
    n=$((n + 1))
  done
  [ "$n" != 0 ] || return 1
  [ "$n" = "$#" ] || { echo "only some of the programs are signed. Nothing was installed." >&2; return 2; }
}

# An update replaces running daemons (the phone web client, mounts, attached windows).
# stop_daemons SHARE: stops every keel-daemon of the install in SHARE, the default profile's
# with `keel daemon stop` and any other profile's by signal; prints 1 when one ran, else 0.
stop_daemons() {
  if pgrep -f "$1/keel-daemon" >/dev/null 2>&1; then
    "$1/keel" daemon stop >/dev/null 2>&1 || true
    pkill -f "$1/keel-daemon" >/dev/null 2>&1 || true
    echo 1
  else
    echo 0
  fi
}
# restart_daemon SHARE: starts the default profile's daemon again; other profiles' are
# left to `keel --profile <name> daemon start`.
restart_daemon() {
  if "$1/keel" daemon start >/dev/null 2>&1; then
    echo "keel-daemon restarted (the default profile's; for another profile run: keel --profile <name> daemon start)."
  else
    echo "keel-daemon was running and could not be restarted; run: keel daemon start" >&2
  fi
}

# scripts/test-install.sh sources this file for the functions above, with this set.
if [ "${KEEL_INSTALL_FUNCTIONS_ONLY:-}" = 1 ]; then return 0; fi

share="$HOME/.local/share/keel"
bin="$HOME/.local/bin/keel"
dbin="$HOME/.local/bin/keel-daemon"
desktop="$HOME/.local/share/applications/keel.desktop"
icon="$HOME/.local/share/icons/hicolor/256x256/apps/keel.png"
app="$HOME/Applications/Keel.app"

uninstall=0
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

command -v sha256sum >/dev/null || command -v shasum >/dev/null || { echo "need sha256sum or shasum" >&2; exit 1; }

json="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")"
urls="$(printf '%s' "$json" | grep -o '"browser_download_url": *"[^"]*"' | cut -d'"' -f4)"
tag="$(printf '%s' "$json" | grep -m1 -o '"tag_name": *"[^"]*"' | cut -d'"' -f4)"
url="$(printf '%s\n' "$urls" | grep -- "-$plat\.tar\.gz\$" | head -n1 || true)"
[ -n "$url" ] || { echo "release $tag has no $plat archive" >&2; exit 1; }
asset() { printf '%s\n' "$urls" | grep "/$1\$" | head -n1 || true; }
sums_url="$(asset SHA256SUMS)"

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
name="${url##*/}"
echo "Downloading Keel $tag ($name)..."
curl -fsSL "$url" -o "$tmp/$name"

if [ -n "$sums_url" ]; then
  curl -fsSL "$sums_url" -o "$tmp/SHA256SUMS"
  for f in SHA256SUMS.asc SHA256SUMS.key.asc; do
    u="$(asset "$f")"
    [ -z "$u" ] || curl -fsSL "$u" -o "$tmp/$f"
  done
  verify_sums_sig "$tmp" || exit 1
  verify_sums "$tmp" "$name" || exit 1
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

# macOS: a Developer ID signed and notarized build passes Gatekeeper as it is; an unsigned one
# has its download quarantine removed below (curl sets none, a browser download does).
signed=1
case "$plat" in
  macos-*)
    signed=0; mac_signed "$src/keel" "$src/keel-daemon" "$src/libpdfium.dylib" || signed=$?
    case "$signed" in
      0) echo "Code signature OK." ;;
      1)
        if [ -n "$KEEL_APPLE_TEAM_ID" ] && [ "$skip" != 1 ]; then
          echo "release $tag is not signed, but this installer expects Keel's signature. Nothing was installed. Pass --skip-verify to install it anyway." >&2
          exit 1
        fi
        echo "warning: release $tag is not signed (Developer ID); see Gatekeeper in docs/getting-started.md." >&2
        ;;
      *) exit 1 ;;
    esac
    ;;
esac

# Stop the old daemons before the files go and start the new default one after.
daemon_was_running=0
if [ -x "$share/keel" ]; then daemon_was_running="$(stop_daemons "$share")"; fi
rm -rf "$share"; mkdir -p "$share" "$(dirname "$bin")"
cp -R "$src/." "$share/"
chmod +x "$share/keel" "$share/keel-daemon"
ln -sf "$share/keel" "$bin"
ln -sf "$share/keel-daemon" "$dbin"
if [ "$daemon_was_running" = 1 ]; then restart_daemon "$share"; fi

case "$plat" in
  linux-*)
    mkdir -p "$(dirname "$desktop")" "$(dirname "$icon")"
    cp "$share/keel.png" "$icon"
    sed "s|^Exec=.*|Exec=$share/keel %f|" "$share/keel.desktop" > "$desktop"
    ;;
  macos-*)
    [ "$signed" = 0 ] || xattr -dr com.apple.quarantine "$share" 2>/dev/null || true
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

case "$plat" in linux-*) ldconfig -p 2>/dev/null | grep -q "libxkbcommon-x11.so.0" || echo "Note: libxkbcommon-x11 is not installed; the window needs it on an X11 session (Debian/Ubuntu: sudo apt install libxkbcommon-x11-0)." ;; esac
echo "Keel $tag installed to $share."
[ -n "${WSL_DISTRO_NAME:-}" ] && echo "WSL: 'keel <folder>' opens it in the Windows Keel when keel.exe is on the Windows PATH (or KEEL_EXE names it); 'keel --here <folder>' opens this Linux window. See docs/wsl.md."
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Add \$HOME/.local/bin to your PATH to run 'keel' from a terminal." ;; esac
