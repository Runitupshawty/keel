#!/usr/bin/env bash
# Runs the browser test against a fixture keel-daemon on 127.0.0.1:7421. Needs node 22+, a
# keel-daemon built after `scripts/build-web.sh --features e2e` (it embeds the web bundle;
# the test drives the app through the bundle's `window.__keel` hook), and Chromium for
# Playwright (`npx playwright install chromium`, once, in this folder).
#   DAEMON=path/to/keel-daemon bash tests/web-e2e/run.sh      (default: target/debug/keel-daemon)
#   SPEC=desktop|phone bash tests/web-e2e/run.sh             (one of the two flow files)
# Everything it creates (profile, library, fixture files) is under one temp folder, never
# your real Keel configuration; screenshots of the run go to tests/web-e2e/out/.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
daemon="${DAEMON:-$root/target/debug/keel-daemon}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
export KEEL_CONFIG_DIR="$work/config" KEEL_DATA_DIR="$work/data" KEEL_NET_SECRET=memory
rm -rf "$here/out"
mkdir -p "$here/out" "$KEEL_CONFIG_DIR/profiles/default"
# Devices on (an identity in memory, no relays, nothing paired) for the Devices page and
# the Spacedrop inbox.
printf '[devices]\nenabled = true\nexplicit = true\nrelay = false\n' > "$KEEL_CONFIG_DIR/profiles/default/config.toml"
fx="$work/fixture"
mkdir -p "$fx/docs/deep" "$fx/copies" "$work/media"
printf 'alpha notes: the quick brown fox\n' > "$fx/alpha-notes.txt"
printf 'bravo readme\n' > "$fx/bravo-readme.txt"
printf 'charlie log\n' > "$fx/charlie-log.txt"
printf 'delta\n' > "$fx/docs/deep/delta.txt"
printf 'an older alpha\n' > "$fx/copies/alpha-notes.txt"
cp "$root/crates/keel-preview/tests/fixtures/sample.pdf" "$fx/docs/report.pdf"
cp "$root/fuzz/corpus/archive/deflate.zip" "$fx/docs/bundle.zip"
cp "$root/assets/keel.png" "$work/media/keel.png"

cd "$here"
[ -d node_modules ] || npm ci --no-audit --no-fund
node e2e.mjs "$daemon" 127.0.0.1:7421 "$work"
