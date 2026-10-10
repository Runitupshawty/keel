#!/usr/bin/env bash
# Runs every ignored release-mode performance measurement (docs/performance.md) and prints a
# table: crate, test, result, wall time, and what the test printed.
#   scripts/perf.sh             all of them
#   scripts/perf.sh search      only the ones whose name contains "search"
# Settings and libraries go to temporary folders (KEEL_CONFIG_DIR / KEEL_DATA_DIR), never the
# user's own; big fixtures are made once under target/ (about 15 GB with media_grid_perf).
# Tests that need a GPU (keel-app first frame, media grid) are skipped with KEEL_PERF_NO_GPU=1.
set -uo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
only="${1:-}"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/keel-perf.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
export KEEL_CONFIG_DIR="$scratch/config" KEEL_DATA_DIR="$scratch/data" KEEL_NET_SECRET=memory
jobs="${CARGO_BUILD_JOBS:-2}"

# crate | cargo test target args | test name filter | needs a GPU
tests=(
  "keel-core|--lib|perf_index_200k_files|"
  "keel-core|--lib|perf_watcher_burst_10k|"
  "keel-core|--lib|perf_hash_10k_files_of_1mib|"
  "keel-core|--lib|ten_thousand_small_files_hash_fast|"
  "keel-core|--lib|perf_search_200k|"
  "keel-core|--lib|perf_duplicates_and_recount_200k|"
  "keel-core|--lib|recount_100k|"
  "keel-core|--lib|perf_sidecar_5k_jpegs|"
  "keel-core|--lib|sidecar_perf_1000_images|"
  "keel-core|--lib|three_thousand_items_copy_fast|"
  "keel-core|--lib|two_million_entries_index_fast_in_bounded_memory|"
  "keel-core|--lib|two_million_row_search_is_fast|"
  "keel-core|--lib|realistic_paths_search_is_fast|"
  "keel-search|--lib|perf_two_million_entries|"
  "keel-search|--lib|bench_synthetic_tree|"
  "keel-vfs|--test local|perf_list_50k|"
  "keel-vfs|--test archive|perf_zip_list_10k_under_200ms|"
  "keel-vfs|--test archive|perf_zip_extract_3k_under_2s|"
  "keel-vfs|--test archive|perf_zip_extract_10k|"
  "keel-vfs|--test zip_edit|perf_zip_delete_one_entry_of_1gib|"
  "keel-net|--lib|two_thousand_files_arrive_in_linear_time|"
  "keel-daemon|--lib|perf_list_1000_requests|"
  "keel-app|--test cli|perf_version_startup|"
  "keel-app|--bin keel|perf_100k|"
  "keel-app|--bin keel|perf_open_100k_folder|"
  "keel-app|--bin keel|perf_first_frame|gpu"
  "keel-app|--bin keel|media_grid_perf|gpu"
)

rows=()
for t in "${tests[@]}"; do
  IFS='|' read -r crate target name gpu <<<"$t"
  [[ -n "$only" && "$name" != *"$only"* ]] && continue
  if [[ -n "$gpu" && -n "${KEEL_PERF_NO_GPU:-}" ]]; then
    rows+=("$crate|$name|skipped||needs a GPU")
    continue
  fi
  echo "== $crate $name" >&2
  log="$scratch/$name.log"
  start=$(date +%s)
  # shellcheck disable=SC2086
  cargo test --release -j "$jobs" -p "$crate" $target "$name" -- --ignored --nocapture \
    --test-threads 1 >"$log" 2>&1
  status=$?
  secs=$(($(date +%s) - start))
  result=ok
  grep -q "test result: ok. 0 passed" "$log" && result="not found"
  [[ $status -ne 0 ]] && result=FAILED
  # What the test printed (cargo's own lines left out).
  out="$(grep -vE '^(running |test |test result|\s*(Finished|Running|Compiling|Blocking|Doc-tests|warning)|$)' "$log" |
    grep -vE '^(successes|failures):' | tail -n 4 | tr '\n' ' ' | sed 's/  */ /g')"
  [[ $result == FAILED ]] && out="$out (log: $(grep -m1 -E 'panicked|error' "$log"))"
  rows+=("$crate|$name|$result|${secs}s|$out")
done

printf '\n| crate | measurement | result | wall | output |\n|---|---|---|---|---|\n'
for r in "${rows[@]}"; do
  IFS='|' read -r crate name result wall out <<<"$r"
  printf '| %s | %s | %s | %s | %s |\n' "$crate" "$name" "$result" "$wall" "$out"
done
du -sh "$root"/crates/keel-web/dist 2>/dev/null | awk '{print "\nweb client bundle (crates/keel-web/dist, build with scripts/build-web.sh): " $1}'
