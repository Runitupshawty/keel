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
