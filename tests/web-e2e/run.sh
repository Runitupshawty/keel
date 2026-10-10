#!/usr/bin/env bash
# Runs the browser test against a fixture keel-daemon on 127.0.0.1:7421. Needs node 22+, a
# keel-daemon built after `scripts/build-web.sh --features e2e` (it embeds the web bundle;
# the test drives the app through the bundle's `window.__keel` hook), and Chromium for
# Playwright (`npx playwright install chromium`, once, in this folder).
#   DAEMON=path/to/keel-daemon bash tests/web-e2e/run.sh      (default: target/debug/keel-daemon)
# Everything it creates (profile, library, fixture files) is under one temp folder, never
# your real Keel configuration; screenshots of the run go to tests/web-e2e/out/.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
daemon="${DAEMON:-$root/target/debug/keel-daemon}"
addr="127.0.0.1:7421"
work="$(mktemp -d)"
export KEEL_CONFIG_DIR="$work/config" KEEL_DATA_DIR="$work/data" KEEL_NET_SECRET=memory
mkdir -p "$work/fixture" "$here/out"
printf 'alpha notes: the quick brown fox\n' > "$work/fixture/alpha-notes.txt"
printf 'bravo readme\n' > "$work/fixture/bravo-readme.txt"
printf 'charlie log\n' > "$work/fixture/charlie-log.txt"

"$daemon" --web "$addr" > "$here/out/daemon.log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; rm -rf "$work"' EXIT
for _ in $(seq 1 100); do
  if [ -s "$KEEL_CONFIG_DIR/daemon.token" ] && curl -fs "http://$addr/" > /dev/null; then break; fi
  kill -0 "$pid" 2> /dev/null || { cat "$here/out/daemon.log"; echo "keel-daemon exited" >&2; exit 1; }
  sleep 0.3
done
curl -fs "http://$addr/" > /dev/null || { cat "$here/out/daemon.log"; echo "keel-daemon did not serve the web client (built without the bundle?)" >&2; exit 1; }
curl -fs "http://$addr/keel_web_bg.wasm" -o "$work/bundle.wasm"
grep -qa __keel "$work/bundle.wasm" || { echo "the served bundle has no e2e hook: scripts/build-web.sh --features e2e, then build keel-daemon" >&2; exit 1; }

cd "$here"
[ -d node_modules ] || npm ci --no-audit --no-fund
node seed.mjs "ws://$addr/rpc" "$KEEL_CONFIG_DIR/daemon.token" "$work/fixture"
node e2e.mjs "http://$addr" "$KEEL_CONFIG_DIR/daemon.token" "$work/fixture"
