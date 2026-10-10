#!/usr/bin/env bash
# Renders docs/screenshots/*.png with the real UI on a fixture folder (egui_kittest on wgpu,
# so it needs a GPU; not part of CI). See crates/keel-app/src/screenshots.rs.
# Run scripts/fetch-deps first for the PDF preview. Extra arguments go to cargo test.
set -euo pipefail
cd "$(dirname "$0")/.."
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) default='C:/Keel demo' ;;
  *) default='/tmp/Keel demo' ;;
esac
root="${KEEL_SHOTS_ROOT:-$default}"
status=0
cargo test -p keel-app --bin keel "$@" docs_screenshots -- --ignored --nocapture || status=$?
# The test deletes the fixture, but a folder watcher may still have held part of it.
if [ -e "$root/.keel-shots" ]; then rm -rf "$root"; fi
exit "$status"
