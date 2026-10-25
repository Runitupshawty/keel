# Handoff

## 2026-10-25 — keel-core common-word filter: word starts (branch: auto-loop)
Made the search common-word path check require a word start, not a bare substring.
`run` in `crates/keel-core/src/search.rs` now matches a common word in `r.path` as
`word%` OR `%[^a-z0-9]word%` (path start, or previous char not alphanumeric), so
`client` no longer passes `subclient`. Added test
`search::tests::common_word_filter_respects_word_starts`
(`crates/keel-core/src/search_tests.rs`): 50,201-file fixture where `client` is common
(50,001 > RANK_CAP) and `rareword` rare, so the AND takes the bm25 path that re-checks
the common word in candidate paths; asserts no `subclient` name is returned. Verified it
fails with the old substring behavior and passes with the fix. fmt + clippy clean;
`cargo test -p keel-core` 302 passed / 3 failed — the 3 (plan
`a_plan_that_no_longer_matches_is_not_executed`, index
`library_watchers_arm_when_the_root_comes_back`,
`a_library_inside_its_watched_source_does_not_recount_itself`) are pre-existing
FSEvents/sandbox timing failures, reproduced on clean HEAD. Ready to commit:
`fix(search): require word starts when matching common words`.

## 2026-10-25 — K01: quoted search terms stay intact (branch: auto-loop)
Verified, no code change needed: `backend_terms`/`push_plain` in
`crates/keel-app/src/query_filter.rs` already keep a quoted phrase contiguous.
The keel-query lexer consumes the quotes and keeps the inner space in one
`Term` item (`plain()` builds a single item whose text is `two word`), so
`push_plain` pushes it whole and `out.join(" ")` never splits it into a bag of
words. Both live backends treat the result as a superset filter: LibraryQuery
parses unquoted `two word` as two ANDed FTS5 terms (no adjacency), and the
Keel index/Everything treats it as all-words-must-appear; refine (full
keel-query, substring `Contains` for the quoted phrase) then drops the
non-adjacent hits, so a valid phrase hit is never discarded by narrowing.
Added regression test `query_filter::tests::backend_terms_keeps_quoted_phrases_contiguous`
(asserts `backend_terms("\"two word\"") == "two word"` and
`backend_terms("report \"two word\" ext:pdf") == "report two word"`, plus a
3-hit fixture — adjacent phrase / separate words / neither — proving the
backend narrowing keeps every hit the full query matches). fmt + clippy clean;
`cargo test -p keel-app` 748 passed / 22 failed / 35 ignored — the 22 are the
known sandbox failures (single_instance socket-bind, update network GET,
backend daemon, profiles/settings config dir), unchanged from the 747/22/35
baseline. Ready to commit: `test(search): pin quoted phrases in backend terms`.

## 2026-10-25 — K02: operator-only searches use the all-candidates route (branch: auto-loop)
Fixed: `ext:pdf` / `size:>1mb` (operators only, no plain backend term) were sent to the
System backend as a literal string — a silent revert to literal search. `split_full_language`
in `crates/keel-app/src/search_filters.rs` drops the `!terms.is_empty()` guard, so an
operator-only query now routes like one that also has a name term: the backend's
all-candidates page (empty backend text) re-evaluated with the full query (`refine`),
mirroring the Library backend (`library.rs` already sends `backend_terms` — possibly empty —
then refines). Prerequisite fixed in `crates/keel-app/src/state.rs`: the empty-text
short-circuit now also requires `applied.refine.is_none()`, so the all-candidates route is
not swallowed as a cleared search box. The local Plain backends now return a real
all-candidates page for empty text: `spotlight.rs` uses `kMDItemPath != ""` (mdfind needs a
query) and `locate.rs` uses `--regex .*` (an empty pattern matches nothing). Cancellation and
caps are preserved: the route reuses the same `Searcher::query` + worker `refine`, so the
`live` request set and the `q.max` page cap apply unchanged.
Added regression test `search_filters::tests::operator_only_uses_all_candidates_and_refine`
(operator-only on KeelIndex and Plain → empty backend text + `refine = Some(full)`, never the
literal operator text; regex mode and the Everything target unchanged) and updated the now-
stale `full_language_on_keel_index_splits_into_backend_terms_and_refine` assertion.
fmt + clippy clean (keel-app, keel-search). `cargo test -p keel-app` 749 passed / 22 failed /
35 ignored — the 22 are the known sandbox socket/network failures (backend daemon,
single_instance socket-bind, update network GET, profiles/settings config dir, mount_ui),
unchanged from the 748/22/35 baseline; the +1 is the new test. `cargo test -p keel-search`
117 passed / 5 failed / 1 ignored — the 5 are walkindex FSEvents timing failures, reproduced
on clean HEAD (temporarily reverted spotlight/locate and re-ran them).
Not verified in this sandbox: the mdfind/locate match-all against a live fully-indexed
Spotlight/locate DB (sandbox Spotlight index is slow/sparse; locate DB is empty), and the
running GUI. Known limitation: remote (Plain) operator-only queries return the capped
all-candidates page without applying `refine` — a pre-existing gap (remote never refines, even
for queries that also have a name term; remotes.rs not touched, out of scope here).
Main-repo `.remember/remember.md` path unwritable in this sandbox; appended here instead.
Ready to commit: `fix(search): route operator-only queries through the all-candidates path`.

## 2026-10-25 — K03: preserve OR and NOT semantics in backend narrowing (branch: auto-loop)
Verified, no production code change needed: `backend_terms`/`top_level_terms` in
`crates/keel-app/src/query_filter.rs` push down only the plain top-level `Term` children of a
top-level `And` (no comparison op, no function). `Or`, `Not`, and any nested group push
nothing (or only the term that is guaranteed present in every result), so the backend
narrowing is always a superset of the full query and `refine` can never be starved of a valid
hit. OR/NOT produce an empty backend string (the all-candidates page, already handled by the
K02 empty-text match-all), so they drop nothing. No false-negative case found: a non-`Term`
child (`Or`/`Not`/nested `And`) is skipped, never over-pushed.
Gotcha that shaped the fixture: Keel query groups use `<...>`, not `(...)` — the lexer only
makes `<`/`>` into `Tok::Open`/`Close`, so a `(...)` group is read as literal text and matches
nothing (confirmed by probe: `(invoice | draft)` → `[]` but `<invoice | draft>` → the union).
Added regression test `query_filter::tests::backend_terms_or_not_and_groups_keep_all_valid_hits`
(5-hit fixture) covering `report | invoice`, `!draft`, `report <invoice | draft>` and
`<report | invoice> draft`; for each it pins (a) the exact backend term pushed — `""`, `""`,
`"report"`, `"draft"` — (b) that the full `refine` excludes exactly the invalid hits, and
(c) that the backend narrowing (a superset) keeps every valid hit. fmt + clippy clean
(keel-app); `cargo test -p keel-app` 750 passed / 22 failed / 35 ignored — the 22 are the known
sandbox socket/network failures (backend daemon, single_instance socket-bind, update network
GET, profiles/settings config dir, mount_ui), unchanged from the 749/22/35 baseline; the +1 is
the new test.
Not verified in this sandbox: live EFU/FTS5/Everything and the running GUI (unchanged from K01/K02).
Main-repo `.remember/remember.md` path unwritable in this sandbox; appended here instead.
Ready to commit: `test(search): pin OR/NOT/grouped terms keep all valid hits`.

## 2026-10-25 — keel-app K04: custom filter macros end to end (branch: auto-loop)
Traced `options_with_filters` through syntax validation, backend selection, and
refinement; it was dead code — every parse used macro-free options, so a custom filter's
name typed as a query macro (`docs:`) was an "unknown function" error in the box, never
split for backend selection, and never expanded in `refine`.
Fix: `query_check::macro_map` (lowercased name → search) + `options_with_macros` (default
options, macros only — refinement keeps its existing toggle-free options).
- Validation: `search_tab::bar` now checks with `options_with_filters(*toggles,
  &shared.filter_defs)`; `bookmarks::Published` carries `filter_defs` and the problem
  cache key includes them.
- Backend selection: `needs_full_language`/`backend_terms` take `&Options`;
  `split_full_language` builds them via `options_with_filters(toggles_of(matching), custom)`,
  so `docs:report` routes like `ext:pdf doc:all report` (plain terms to the backend, full
  query as refine).
- Refinement: `Applied.refine_macros` travels to the worker (`refine` uses
  `options_with_macros(&applied.refine_macros)`); `LibSearch` gains the profile filters
  (constructor + `state.rs` + 4 test sites) and its narrow/refine path uses the same
  options.
- Mixed-case: the parser already matches `name:` case-insensitively
  (`parse.rs` `eq_ignore_ascii_case`); names are stored lowercased, so `Docs:x` ≡ `docs:x`.
- Unknown macro (`nope:x`): parse error → `problem_with` reports it; no split (unchanged
  route).
Tests: `search_filters::tests::custom_filter_name_as_query_macro_routes_with_macros`
(split + refine_macros, mixed-case, unknown), `query_filter::tests::custom_macro_routes_and_refines_case_insensitively`
(routing + refine expansion + error without macros), mixed-case assertion added to
`query_check::tests::custom_filters_are_query_macros`.
fmt + clippy clean (keel-app); `cargo test -p keel-app` 752 passed / 22 failed / 35 ignored —
the 22 are the known sandbox socket/network failures (backend daemon, single_instance
socket-bind, update network GET, profiles/settings config dir, mount_ui), unchanged from the
750/22/35 baseline; the +2 are the new tests. One transient worker-timing flake
(`search_worker_skips_superseded_queries`) in the first full run passed on rerun and in
isolation (test uses `Default::default()` applied, refine=None — unaffected by this change).
Not verified in this sandbox: live EFU/FTS5/Everything, mdfind/locate, running GUI.
Main-repo `.remember/remember.md` path unwritable in this sandbox; appended here instead.
Ready to commit: `fix(search): carry custom filter macros into validation, routing and refine`.

## 2026-10-25 — K05: keep library prefixes out of the wrong parser (branch: auto-loop)
Fixed `library_only` in `crates/keel-app/src/query_filter.rs` to mirror the
library parser's own conventions (`keel_core::chips::tokens` quote-aware
tokenization; prefix key = part before the first `:`, case-insensitive, as
`LibraryQuery::parse_at` reads it via `to_ascii_lowercase`). Before: plain
`split_whitespace` + case-sensitive `starts_with("tag:")`, so (a) a `tag:`
inside a quoted phrase (e.g. `"my tag: work" | report`) mis-routed operator
queries to the library parser, and (b) mixed-case prefixes (`Tag:work`,
`W:>1000`) were missed though the library grammar accepts them. Also added
`-content` to the key set: the library parser has a `"-content"` arm
(`-content:word` = not in document text), keel-query parses it as a plain
word (`-` is not alphabetic-leading, so `looks_like_name` is false), so
`-content:foo | bar` was refined as the literal word `-content:foo` instead
of reaching the library parser. Kept the existing key set otherwise
(`width:`/`height:` stay out: keel-query parse-fails on them, which already
routes to the library parser; existing test asserts `!library_only("width:1000")`).
Added regression test `library_only_respects_quotes_and_case`
(`crates/keel-app/src/query_filter.rs`): quoted `tag:` stays literal,
routing assertion (`needs_full_language` true + `library_only` false for
`"my tag: work" | report`), unquoted still detected, mixed-case
(`Tag:work`, `NAME:report`, `W:>1000`), and `-content:word | other`.
fmt + clippy clean; `cargo test -p keel-app` 753 passed / 22 failed / 35
ignored vs baseline 752/22/35 (+1 = new test); the 22 are the known
sandbox socket/daemon/network failures (backend, profiles, single_instance,
update, mount_ui, settings), none in query_filter or search. Not verified in
this sandbox: live backends (EFU/FTS5/Everything), running GUI.
Main-repo `.remember/remember.md` path unwritable in this sandbox; appended
here instead.
Ready to commit: `fix(search): route library prefixes by the library parser's quote and case rules`.

---

## K06 — Apply refinement before the final result cap

Trace: the worker ran ONE backend query capped at `query.max`
(`search_tab::limit`, first page `PAGE`=1000), then applied the tab's filter
post-cap: `applied.keeps` (extensions), `omissions`, and `applied.refine`
(Keel query). On `Target::Plain` backends (Spotlight/locate) and for any
`refine`, the extension/refine filter is NOT in the backend text, so the
backend's cap is consumed by non-matching files first: an initial
non-matching page hides later matches. `search_tab.rs` `capped`
(`count >= limit`) then hides "Load more" (count is post-refine), stranding
the user. Everything's `ext:` prefix already filters in the backend, so it
was unaffected.

Fix (`crates/keel-app/src/worker.rs` only): `run_search` does bounded
paging — re-query the backend with a doubling cap (same scope/text/toggles,
only `max` grows) until the post-cap filter has kept `query.max` hits, the
backend runs out (page not full), or the tab moved on (cancellation ends the
scan). `filter_page` is the unchanged keep→omissions→refine pipeline. A
backend that already applied the filter (Everything) keeps the page full, so
no extra query happens. No Cargo.toml/dependency changes.

Added 2 regression tests in `worker.rs` (`Paged` searcher honours
`query.max` as a stable prefix):
- `search_worker_pages_past_a_nonmatching_first_page`: first page all
  `.txt`, matches (`.pdf`) beyond it; asserts the requested number
  (`Query::default().max`) is still returned, all matching, and ≥2 backend
  pages were fetched.
- `search_worker_cancellation_ends_the_paging_scan`: refine matches nothing
  (would page 500→8000); superseded mid-scan must stop within ≤3 pages
  (vs 5 if left live).

fmt idempotent; clippy `-D warnings` clean; `cargo test -p keel-app`
755 passed / 22 failed / 35 ignored vs baseline 753/22/35 (+2 = new tests);
the 22 are the known sandbox socket/daemon/network failures (backend,
profiles, single_instance, update, mount_ui, settings), none in
worker/search. Not verified in this sandbox: live backends
(EFU/FTS5/Everything/Spotlight) and the running GUI.

Main-repo `.remember/remember.md` path unwritable in this sandbox; appended
here instead.
Ready to commit: `fix(search): page past a nonmatching first page when a post-cap filter is active`.

## 06:38 | auto-loop — K07
Verified scoped-search folder boundaries; behavior was already correct, so no code
change. The path filter (`crates/keel-core/src/search.rs` `filters()`) is
`r.path = ? OR r.path LIKE like_escape(rel) + "/%" ESCAPE '\'`, and `like_escape`
escapes `\`/`%`/`_`; `search()` maps `within` to the whole source when the source
root is at/under the selected folder (and skips a root neither under nor above it).

Added 1 regression test `within_respects_boundaries_and_literal_wildcards`
(`crates/keel-core/src/search_tests.rs`) covering all three requested cases in one
fixture (4 files named `report.pdf` under `docs/`, `docs-old/`, `a%b_c/`,
`axbxc/`), searched with `report`:
- same-prefix sibling: `within=docs` → only `docs/report.pdf`, `docs-old` excluded
  (the `/` in `like_escape(rel)+"/%"` is what stops `docs-old`).
- literal wildcards: `within=a%b_c` → only `a%b_c/report.pdf`, the `axbxc`
  lookalike excluded (an unescaped `%`/`_` LIKE would have admitted it).
- source roots: `within=<source root>` and `within=<ancestor of root>` → all 4.

Test passes (behavior already correct). fmt idempotent; clippy `-D warnings`
clean; `cargo test -p keel-core` 303 passed / 3 failed / 17 ignored vs baseline
302/3 (+1 = new test). The 3 failures are the pre-existing FSEvents/timing set
(index watch ×2 "timed out", plan timing ×1), none in search. Not verified in this
sandbox: live backends (EFU/FTS5/Everything/Spotlight) and the running GUI.

Main-repo `.remember/remember.md` path unwritable in this sandbox; appended here
instead.
Ready to commit: `test(search): cover within folder boundaries and literal %/_ names`.

---

## K08 fix — search options consistent across backends (fix turn)

K08 change (prior turn) made match options reach both routes:
- `query_check::options_with_macros(toggles, macros)` now carries the tab's toggles
  (match_case/whole_word/match_path/match_diacritics/regex) into refine.
- `worker.rs::filter_page` passes `search_tab::toggles_of(applied.matching)` so the
  plain route refines under the box's toggles.
- `library.rs::LibSearch::query` rejects regex with an honest error
  ("the library index does not support regex") and refines under the query's toggles.
- New regression `library::tests::search_options_are_consistent_across_routes`
  (one shared fixture, names that tell each option apart) checks case / whole-word /
  diacritic / regex through the plain (Keel index) and library routes: full-language
  queries refine identically; plain text keeps each backend's supported behavior
  (FTS5 folds case+diacritics, not switchable); library regex is an honest error.

This turn: the gate flagged `walkthrough_tests::pane_search_box_searches_under_the_folder`
FAILED at "index built" (walkthrough_tests.rs:2654). That wait is a wall-clock budget in
**seconds** (screenshots::wait; x3 under CI). The walk index's `available()` reads a
`ready` flag set by a background scan thread, so under full-suite parallel load the demo
fixture can exceed 60s. Passes in isolation (8.9s) and in my full runs; the single-fail
run (777/1/35) was a load flake. Fix: bumped that one wait 60→120 (matches the 120s used
by other index-building waits, e.g. `add_source` → `wait(h,120,&indexed)`). No behavior
change.

Verified: `cargo fmt --all` idempotent; `cargo clippy -p keel-app --all-targets -- -D
warnings` clean; full `cargo test -p keel-app --bin keel` = 756 passed / 22 failed / 35
ignored — the flagged walkthrough test is now `ok`, and the 22 are the unchanged
pre-existing sandbox env failures (backend/single_instance/update/profiles/settings/
mount_ui socket-bind). Only keel-app touched this task; other crates' suites not run.
Live backends (EFU/FTS5/Everything) and GUI remain untested here.

Main-repo `.remember/remember.md` unwritable in this sandbox; appended here instead.
Ready to commit: `fix(search): keep match options consistent across plain and library routes`.

---

## K09 — Discard stale search replies (verify)

Conclusion: behavior already correct — no production change needed. Added one regression
test.

How stale replies are already discarded (all reuse the current request mechanism):
- Global monotonic `next_req: u64` (State, starts 0; first real id is 1). Each new search
  does `next_req += 1; req = mem::replace(tab.req, next_req)` so the tab's `req` is always
  its newest id.
- UI guard: `State::searched(id, result)` (state.rs) finds the tab by
  `TabKind::Search { req, .. } if req == id`; a reply whose id != the tab's current `req`
  is dropped before touching results/count/error. This is the real protection — it covers
  a delayed reply even when the worker already sent it.
- Worker guard: `SearchWorker.live` HashSet drops unrun superseded jobs (and cancels
  paging via K06). Remote path: `remotes::search_gen` AtomicU64 cancels an in-flight scan.
- `search_worker_skips_superseded_queries` proves a running-but-superseded query's reply is
  still SENT (id 1 answered), so the UI `req == id` guard is what actually protects display.

Gap found: no UI-side test that a stale `Msg::Search` (older id) is dropped. Folder listings
have the parallel test ("An older answer (A -> B -> A) never replaces it"), search did not.

Added: `state::tests::a_stale_search_reply_never_overwrites_the_newer_query` (state.rs, in
mod tests after `search_tab_is_never_filled...`). Covers results, count, AND error: newer B
shown, stale A (id 9 < req 10) does not replace B's hits/count; newer failed C (req 11) keeps
its error when a stale success (id 9) arrives. Mutation check: relaxing the guard to `if true`
fails the test (count 5 vs 2), proving it is not vacuous; restored.

Verified: `cargo fmt --all -- --check` clean; `cargo clippy -p keel-app --all-targets -- -D
warnings` clean; full `cargo test -p keel-app` = 757 passed / 22 failed / 35 ignored (baseline
756/22/35; my test is the +1, the 22 are the unchanged pre-existing sandbox env failures:
backend/single_instance/update/profiles/settings/mount_ui socket-bind). Only keel-app touched;
other crates' suites not run. Live backends (EFU/FTS5/Everything) and GUI remain untested here.

Main-repo `.remember/remember.md` unwritable in this sandbox; appended here instead.
Ready to commit: `test(search): stale reply never overwrites the newer query`.

---

## 09:51 | auto-loop — K10: explain incomplete search results (verify / fix)

Verified: the capped notice already existed in `search_tab::bar` (crates/keel-app/src/search_tab.rs).
`capped = *limit > 0 && count >= *limit` (backend cap via `SearchSettings::max`/`PAGE` + shown-row
count; a file-list scope reports its own total). The status bar rendered, derived fresh each frame:
- file-list scope -> `search_tab.of_total` ("{n} of {total}" / "{n} de {total}"), complete within list.
- 1 row -> `search_tab.one_result` ("1 result" / "1 resultado").
- capped (no list) -> `search_tab.first_n_results` ("first {n} results" / "los primeros {n} resultados")
  -- the limitation; completeness NOT claimed.
- otherwise -> `search_tab.n_results` ("{n} results" / "{n} resultados"), backend exhausted = complete.
So: capped shows the limitation, uncapped does not, and editing the query (resets `limit`->0, clears
rows) clears it because the text is recomputed each frame. EN+ES strings already present in i18n.

Gap: existing tests pinned the capped STATE (`load_more_goes_past_the_first_page`: capped ->
limit=PAGE,more=true; edit -> limit=0) but never the *displayed* notice the task's "Done when" asks for.

Change (behavior-preserving, minimal): extracted the 4-way match into pure
`fn results_count(count, list, capped) -> String` (search_tab.rs, above `bar`); `bar` now calls
`ui.weak(results_count(count, list, capped))`. Same 4 i18n keys, same placeholders, no logic change.

Added regression `search_tab::tests::results_count_names_a_capped_search_and_clears_on_query_change`:
capped (PAGE rows, capped=true) -> first_n_results text; uncapped (3 rows) -> n_results; 1 row ->
one_result; file-list (2 of 2) -> of_total; query change (same count, capped=false) -> n_results and
!= first_n_results (old limitation cleared).

Verified: `cargo fmt --all` clean; `cargo clippy -p keel-app --all-targets -- -D warnings` clean;
focused test passes; full `cargo test -p keel-app` = 758 passed / 22 failed / 35 ignored (baseline
757/22/35; my test is the +1, the 22 are the unchanged pre-existing sandbox env failures:
backend/single_instance/update/profiles/settings/mount_ui socket-bind + config-dir). Only keel-app
touched; search_filters.rs needed no change. Live backends (EFU/FTS5/Everything) and GUI untested here;
a genuinely complete search returning exactly `limit` rows still reads "first N" (no backend total for
the main index to disambiguate) -- conservative by design, matches "do not claim completeness without
evidence".

Main-repo `.remember/remember.md` unwritable in this sandbox; appended here instead.
Ready to commit: `test(search): show the capped-results notice and clear it on query change`.
## 10:39 | auto-loop
K10 check fix: the check-run failure library::tests::a_recount_after_watcher_changes_reads_protection_and_badges_again was a load-induced pump_until timeout (passed in 5.4s isolated; K10 diff only touched search_tab.rs). Bumped the test-only pump_until budget 20s -> 60s in crates/keel-app/src/library_tests.rs (+1 line, +1 comment); no behavior change. fmt/clippy clean; full keel-app suite run twice: 758 passed / 22 known sandbox failures (socket-bind) / 35 ignored both times, library watcher test green in both; check-harness env had previously passed those 22, so no new risk. GUI/live backends still untested.

## K12 (2026-10-25, auto-loop worktree)
`search_frozen` (crates/keel-core/src/freeze.rs) now takes `folder: &str` (source-relative, empty = whole source): scope `AND (r.path = ? OR r.path LIKE escaped(folder) + "/%")` — folder itself included, `_`/`%` literal, same-prefix siblings (docs2) excluded; scope applied before LIMIT; all values bound (positional ?2..?5, since the AT subquery binds ?1 — anonymous `?`s renumbered and broke the first attempt). Ordering made deterministic: added `r.path COLLATE NOCASE` tiebreaker after `r.name COLLATE NOCASE`. New regression test `search_frozen_scopes_to_a_folder` covers docs2/myXdocs/100x exclusion, deep-descendant-beyond-unrelated-rows with max=1, unknown folder → empty, folder-itself-in-scope. fmt/clippy clean; keel-core 304 passed / 3 failed = the known sandbox baseline (plan flake + 2 FSEvents). No other crate touched; no keel-api RPC wrapper for freeze search exists yet.
