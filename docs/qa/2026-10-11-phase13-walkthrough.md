# QA walkthrough: Phase 13 (0.22.0 candidate)

Date: 2026-10-11. Candidate: `rel22` after the review fixes. Machine: the owner's Windows desktop (debug and release builds), throwaway profile (`KEEL_CONFIG_DIR`, `KEEL_DATA_DIR`, `KEEL_NET_SECRET=memory`).

## Automated flows

`cargo test -p keel-app` runs the kittest walkthrough flows, now including `search_finds_words_inside_documents` (index a fixture folder, search a word that is only inside a document, the hit shows with its snippet, the Contents chip off hides it; it waits on the library's own job list). At the fixed tree: keel-app 406 + 13 (CLI), keel-core 257, keel-preview 11 + 7 + 1 + 39, keel-api 72 (including the end-to-end run of the four crafted probe documents through the real text reader), keel-daemon 33 + 2.

## Live checks

| Flow | Result |
| --- | --- |
| 200 generated documents of 11 kinds through keel-daemon (release build): the text job after the walk | all read in 1.1 s (about 180 documents/s); `library.stats` 200 documents, 0 skipped, 0 failed |
| 18 malformed generated .docx files | logged as 18 failures, no crash |
| `keel search --json` for harbour, ledger, quartzite; `contents:off`; typed FTS operators | 29 hits each with snippets across all formats; 0 with contents off; operators return 0 without error |
| Web client search page: harbour, `content:ledger ext:docx`, quartzite; Contents chip off | highlighted snippets; 3 hits for the ext filter; nothing with the chip off |
| Review probes: a docx with 20,000 nested tables, an xlsb with a cell at XFD1048576, a docx whose central directory lies about a part's size, an xlsx error cell holding a secret marker, 1,200 "invoice" .txt files starving 5 .md hits under `ext:md`, a source on a mapped network drive | each found a defect; all fixed before this release and covered by tests (see the CHANGELOG's Fixed and Security bullets) |
| A v0.21.0 store migrated to version 12 with its tag and folder style kept; 0.21.0 refuses the migrated store | pass |

## Left for the owner

- OCR of images and scanned PDFs (a later phase).
- The legacy `.xls` preview still loads a whole workbook into memory (text indexing skips `.xls`); listed under Known limitations.
