#!/usr/bin/env bash
# The QA walkthrough: every flow of crates/keel-app/src/walkthrough_tests.rs, the slow ones
# too, each rendering a 1280x800 PNG to target/walkthrough/<flow>.png (egui_kittest on
# wgpu, so it needs a GPU; not part of CI). Run scripts/fetch-deps first for the PDF
# preview. Extra arguments go to the test binary, e.g. a flow name to run only that one.
# The Windows-only Recycle Bin flow moves one file of its own to the Recycle Bin, restores
# it, and deletes it from there again; nothing else in the Recycle Bin is touched. It runs
# in a test process of its own: the shell answers through a thread's message queue, which
# another test's thread would hold without pumping it.
set -euo pipefail
cd "$(dirname "$0")/.."
rm -rf target/walkthrough
export KEEL_WALKTHROUGH_SHOTS=1
cargo test -p keel-app --bin keel walkthrough_tests -- --include-ignored --test-threads 2 \
  --skip recycle_bin "$@"
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    cargo test -p keel-app --bin keel walkthrough_tests::recycle_bin -- --include-ignored "$@" ;;
esac
echo "PNGs in target/walkthrough/"
