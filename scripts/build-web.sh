#!/usr/bin/env bash
# Builds the web client (crates/keel-web) into crates/keel-web/dist; keel-daemon embeds that
# folder at build time, so build keel-daemon afterwards. Needs the wasm32-unknown-unknown
# target and wasm-bindgen-cli of the wasm-bindgen version in Cargo.lock:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version "$(cargo pkgid wasm-bindgen | sed 's/.*@//')"
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/target}"
dist="$root/crates/keel-web/dist"
cargo build --release --locked -p keel-web --target wasm32-unknown-unknown --manifest-path "$root/Cargo.toml"
rm -rf "$dist"
mkdir -p "$dist"
wasm-bindgen --target web --no-typescript --out-dir "$dist" "$target/wasm32-unknown-unknown/release/keel_web.wasm"
cp "$root"/crates/keel-web/static/* "$dist/"
echo "web client in $dist; now build keel-daemon"
