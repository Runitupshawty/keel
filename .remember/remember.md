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
