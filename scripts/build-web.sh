#!/usr/bin/env bash
# Builds the web client (crates/keel-web) into crates/keel-web/dist; keel-daemon embeds that
# folder at build time, so build keel-daemon afterwards. Needs the wasm32-unknown-unknown
# target and wasm-bindgen-cli of the wasm-bindgen version in Cargo.lock:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version "$(cargo pkgid wasm-bindgen | sed 's/.*@//')"
# Arguments go to cargo: `--features e2e` builds the browser-test bundle (tests/web-e2e)
# with its `window.__keel` hook; the release bundle (no arguments) must not carry it.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/target}"
dist="$root/crates/keel-web/dist"
cargo build --release --locked -p keel-web --target wasm32-unknown-unknown --manifest-path "$root/Cargo.toml" "$@"
rm -rf "$dist"
mkdir -p "$dist"
wasm-bindgen --target web --no-typescript --out-dir "$dist" "$target/wasm32-unknown-unknown/release/keel_web.wasm"
cp "$root"/crates/keel-web/static/* "$dist/"
# The service worker's cache key: a hash of every file of the build (a change to any shell
# file, not only the wasm, replaces the old cache).
hash256() { sha256sum 2>/dev/null || shasum -a 256; }
ver="$(cd "$dist" && LC_ALL=C ls | LC_ALL=C sort | while read -r f; do cat "$f"; done | hash256 | cut -c1-16)"
sed -i.bak "s/__KEEL_BUILD__/$ver/" "$dist/sw.js" && rm -f "$dist/sw.js.bak"
grep -q "const VERSION = \"$ver\";" "$dist/sw.js"
case " $* " in
*e2e*) echo "browser-test bundle: window.__keel is on with ?e2e=1" ;;
*) if grep -qa "__keel" "$dist"/*; then echo "the release bundle carries the e2e hook" >&2; exit 1; fi ;;
esac
echo "web client in $dist; now build keel-daemon"
