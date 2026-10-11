#!/usr/bin/env bash
# Self-test of install.sh's checks (no download, nothing installed; temp files only):
#   bash scripts/test-install.sh
# Fake gpg and codesign programs on PATH stand in for a signed release; with a real gpg the
# signature check also runs against a throwaway key.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bash -n "$here/install.sh"
KEEL_INSTALL_FUNCTIONS_ONLY=1
# shellcheck source=install.sh
. "$here/install.sh"

failed=0
ok() { echo "ok   $1"; }
fail() { echo "FAIL $1"; failed=$((failed + 1)); }
# check NAME WANT CMD...: CMD's exit status must be WANT.
check() {
  local name="$1" want="$2" rc=0
  shift 2
  "$@" >"$t/out" 2>&1 || rc=$?
  if [ "$rc" = "$want" ]; then ok "$name"; else fail "$name (exit $rc, wanted $want): $(cat "$t/out")"; fi
}
said() { grep -q -- "$1" "$t/out" || fail "output lacks '$1': $(cat "$t/out")"; }

t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
real_path="$PATH"
real_gpg="$(command -v gpg || true)"

# --- checksums ---
mkdir "$t/rel"
printf hello > "$t/rel/keel-v1.0.0-linux-x64.tar.gz"
hash=2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824 # SHA-256 of "hello"
printf '%s  keel-v1.0.0-macos-x64.tar.gz\n%s  keel-v1.0.0-linux-x64.tar.gz\n' "$(printf 'f%.0s' {1..64})" "$hash" > "$t/rel/SHA256SUMS"
check "checksum: the listed hash" 0 verify_sums "$t/rel" keel-v1.0.0-linux-x64.tar.gz
printf '%s *dist/keel-v1.0.0-linux-x64.tar.gz\n' "$(printf '%s' "$hash" | tr a-f A-F)" > "$t/rel/SHA256SUMS"
check "checksum: binary-mode, upper-case and path entries" 0 verify_sums "$t/rel" keel-v1.0.0-linux-x64.tar.gz
printf '%s  keel-v1.0.0-linux-x64.tar.gz\n' "$(printf '0%.0s' {1..64})" > "$t/rel/SHA256SUMS"
check "checksum: a mismatch is refused" 1 verify_sums "$t/rel" keel-v1.0.0-linux-x64.tar.gz
said "checksum mismatch"
printf '%s  keel-v1.0.0-linux-x64.tar.gz.old\n' "$hash" > "$t/rel/SHA256SUMS"
check "checksum: no entry is refused" 1 verify_sums "$t/rel" keel-v1.0.0-linux-x64.tar.gz

# --- SHA256SUMS.asc with a fake gpg: FAKE_FPR is the signer, FAKE_VERIFY its exit status ---
mkdir "$t/fake" "$t/empty"
cat > "$t/fake/gpg" <<'GPG'
#!/bin/sh
case " $* " in
  *" --import "*) exit 0 ;;
  *" --verify "*)
    [ "${FAKE_VERIFY:-0}" = 0 ] || { echo "[GNUPG:] BADSIG 0123 test"; exit 1; }
    echo "[GNUPG:] GOODSIG 0123 test"
    echo "[GNUPG:] VALIDSIG SUBKEYFPR 2026-10-11 0 0 4 0 22 10 00 $FAKE_FPR"
    exit 0 ;;
esac
exit 2
GPG
printf '#!/bin/sh\nexit 0\n' > "$t/fake/gpgconf"
chmod +x "$t/fake/gpg" "$t/fake/gpgconf"
PATH="$t/fake:$real_path"
export FAKE_FPR=AAAABBBBCCCCDDDDEEEEFFFF0000111122223333 FAKE_VERIFY=0
skip=0 KEEL_GPG_FINGERPRINT=""
printf 'sums\n' > "$t/rel/SHA256SUMS"

check "signature: none, no key pinned: warns" 0 verify_sums_sig "$t/rel"
said "not signed"
KEEL_GPG_FINGERPRINT=$FAKE_FPR
check "signature: none while a key is pinned is refused" 1 verify_sums_sig "$t/rel"
skip=1
check "signature: none while a key is pinned, --skip-verify" 0 verify_sums_sig "$t/rel"
skip=0
printf 'sig\n' > "$t/rel/SHA256SUMS.asc"
check "signature: without the release key is refused" 1 verify_sums_sig "$t/rel"
printf 'key\n' > "$t/rel/SHA256SUMS.key.asc"
check "signature: the pinned key" 0 verify_sums_sig "$t/rel"
said "Signature OK."
KEEL_GPG_FINGERPRINT=""
check "signature: valid, no key pinned: names the key" 0 verify_sums_sig "$t/rel"
said "$FAKE_FPR"
KEEL_GPG_FINGERPRINT=9999BBBBCCCCDDDDEEEEFFFF0000111122223333
check "signature: another key is refused" 1 verify_sums_sig "$t/rel"
said "not by Keel's key"
KEEL_GPG_FINGERPRINT="" FAKE_VERIFY=1
check "signature: a bad signature is refused" 1 verify_sums_sig "$t/rel"
FAKE_VERIFY=0
PATH="$t/empty"
check "signature: no gpg installed: warns" 0 verify_sums_sig "$t/rel"
PATH="$real_path"
said "gpg is not installed"

# --- the same with a real gpg and a throwaway key, when gpg is installed ---
if [ -n "$real_gpg" ]; then
  gh="$(mktemp -d)"
  if GNUPGHOME="$gh" gpg --batch --quiet --passphrase '' --quick-gen-key "Keel test <test@example.invalid>" ed25519 sign never 2>/dev/null; then
    fpr="$(GNUPGHOME="$gh" gpg --batch --with-colons --list-keys 2>/dev/null | awk -F: '$1=="fpr"{print $10; exit}')"
    mkdir "$t/real"
    printf '%s  keel.tar.gz\n' "$hash" > "$t/real/SHA256SUMS"
    GNUPGHOME="$gh" gpg --batch --quiet --armor --detach-sign --output "$t/real/SHA256SUMS.asc" "$t/real/SHA256SUMS" 2>/dev/null
    GNUPGHOME="$gh" gpg --batch --armor --export "$fpr" > "$t/real/SHA256SUMS.key.asc"
    KEEL_GPG_FINGERPRINT="$fpr"
    check "real gpg: a good signature by the pinned key" 0 verify_sums_sig "$t/real"
    printf 'tampered\n' >> "$t/real/SHA256SUMS"
    check "real gpg: a changed SHA256SUMS is refused" 1 verify_sums_sig "$t/real"
    KEEL_GPG_FINGERPRINT=""
  else
    echo "skip real gpg: could not create a test key"
  fi
  GNUPGHOME="$gh" gpgconf --kill all >/dev/null 2>&1 || true
  rm -rf "$gh"
else
  echo "skip real gpg: not installed"
fi

# --- macOS code signatures with a fake codesign: FAKE_TEAM per file, FAKE_CS_VERIFY ---
cat > "$t/fake/codesign" <<'CS'
#!/bin/sh
for f; do :; done
case "$1" in
  -dv)
    team="$(cat "$f.team" 2>/dev/null)"
    [ -n "$team" ] || { echo "$f: code object is not signed at all" >&2; exit 1; }
    echo "Executable=$f" >&2; echo "TeamIdentifier=$team" >&2; exit 0 ;;
  --verify) exit "${FAKE_CS_VERIFY:-0}" ;;
esac
exit 2
CS
chmod +x "$t/fake/codesign"
PATH="$t/fake:$real_path"
mkdir "$t/mac"
for f in keel keel-daemon; do : > "$t/mac/$f"; done
check "codesign: unsigned" 1 mac_signed "$t/mac/keel" "$t/mac/keel-daemon"
for f in keel keel-daemon; do echo "not set" > "$t/mac/$f.team"; done
check "codesign: ad-hoc counts as unsigned" 1 mac_signed "$t/mac/keel" "$t/mac/keel-daemon"
for f in keel keel-daemon; do echo TEAM123456 > "$t/mac/$f.team"; done
check "codesign: Developer ID, no team pinned" 0 mac_signed "$t/mac/keel" "$t/mac/keel-daemon"
KEEL_APPLE_TEAM_ID=TEAM123456
check "codesign: the pinned team" 0 mac_signed "$t/mac/keel" "$t/mac/keel-daemon"
KEEL_APPLE_TEAM_ID=OTHER00000
check "codesign: another team is refused" 2 mac_signed "$t/mac/keel" "$t/mac/keel-daemon"
KEEL_APPLE_TEAM_ID="" FAKE_CS_VERIFY=1
export FAKE_CS_VERIFY
check "codesign: a broken signature is refused" 2 mac_signed "$t/mac/keel" "$t/mac/keel-daemon"
FAKE_CS_VERIFY=0
rm "$t/mac/keel-daemon.team"
check "codesign: partly signed is refused" 2 mac_signed "$t/mac/keel" "$t/mac/keel-daemon"

# --- daemons on update: fake keel, pgrep and pkill record what they were asked ---
mkdir "$t/share"
CALLS="$t/calls"; export CALLS
printf '#!/usr/bin/env bash\necho "keel $*" >> "$CALLS"\n' > "$t/share/keel"
printf '#!/usr/bin/env bash\necho "pgrep $*" >> "$CALLS"; exit "${FAKE_PGREP:-0}"\n' > "$t/fake/pgrep"
printf '#!/usr/bin/env bash\necho "pkill $*" >> "$CALLS"\n' > "$t/fake/pkill"
chmod +x "$t/share/keel" "$t/fake/pgrep" "$t/fake/pkill"
: > "$CALLS"
ran="$(stop_daemons "$t/share")"
if [ "$ran" = 1 ] && grep -qx "keel daemon stop" "$CALLS" && grep -qx "pkill -f $t/share/keel-daemon" "$CALLS"; then
  ok "update: every daemon of the install stops (other profiles' too)"
else fail "update: stop said $ran: $(cat "$CALLS")"; fi
: > "$CALLS"
check "update: the default profile's daemon starts again" 0 restart_daemon "$t/share"
said "keel --profile <name> daemon start"
if [ "$(cat "$CALLS")" = "keel daemon start" ]; then ok "update: one start, default profile"
else fail "update: start ran: $(cat "$CALLS")"; fi
: > "$CALLS"
FAKE_PGREP=1; export FAKE_PGREP
ran="$(stop_daemons "$t/share")"
if [ "$ran" = 0 ] && ! grep -q "^keel\|^pkill" "$CALLS"; then ok "update: no daemon, nothing stopped"
else fail "update: idle stop said $ran: $(cat "$CALLS")"; fi
PATH="$real_path"

[ "$failed" = 0 ] || { echo "$failed check(s) failed"; exit 1; }
echo "install.sh checks: all passed"
