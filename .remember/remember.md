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
