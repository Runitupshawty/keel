# QA walkthrough: Phase 12 (0.21.0 candidate)

Date: 2026-10-11. Candidate: `rel21` after the review fixes. Machines: the owner's Windows desktop (debug and release builds), the owner's Mac mini and Linux box for drag-out. Every run used a throwaway profile (`KEEL_CONFIG_DIR`, `KEEL_DATA_DIR`, `KEEL_NET_SECRET=memory`).

## Automated flows

`cargo test -p keel-app` runs the kittest walkthrough flows (`crates/keel-app/src/walkthrough_tests.rs`), now including the Phase 12 flows: search chips build the query (Kind, Tag, a hand-typed term lighting its chip, Tab + Space); folder color and icon from the context menu, the dot and the emoji in the row, the Ctrl+P result, Clear; Ctrl+I shows the inspector, a Copies row opens the copy, a folder shows its totals; tag assign mode (Ctrl+Shift+A, digit 1 tags two files, the bar's count, untag, Esc, leaving on navigation); Organize by type with its preview, Enter runs it and Ctrl+Z puts the tree back. Result at the reviewed tree: keel-app 402 + 12 passed; the other crates all green (keel-core 221, keel-api 67, keel-vfs 174 + 97, keel-net 69, keel-daemon 35, keel-web 22).

## Live checks

| Flow | Where | Result |
| --- | --- | --- |
| Drag two files onto a Finder window and onto a TextEdit document; Command held for a move | Mac mini | copies land, originals kept; the move is logged only after the source is gone |
| Drag onto a GTK drop target under Xvfb: copy, Shift for move, Escape to cancel, drop on empty desktop, drop back onto Keel | Linux box | URIs arrive escaped; Keel deletes nothing; cancel sends only a leave |
| Folder color and icon on three fixture sources, Keel restarted three times; a styled folder renamed while Keel was closed and re-indexed | desktop | styles persist and follow the rename |
| Three library searches built with chips only, in the window and in the web client | desktop (web client through keel-daemon in the browser pane) | Document → 6 files; + taxes tag → `notes.md`; + This week → `notes.md` |
| `keel op library.stats`, a `tags.set` preview (nothing tagged), `tags.set --execute`, `op plan` copy with `--execute` (job waited for) | desktop, through keel-daemon | pass |
| `library.inspect` on a two-source fixture: copies across sources, protection count, tag and move in history, folder totals, a path outside the library | desktop, `keel mcp` through keel-daemon | pass; a stale "last hashed" after a move was found and fixed |
| Merge two folders with Skip (the clashing file stays), flatten `x/y`, organize by year, a file added after the preview refuses `execute`, a junction and a hard link, a refused symlink (Developer Mode off) | desktop, CLI | pass |
| Review probes: flatten with three same-named files under Overwrite, merge undo with a shared subfolder name, a held Enter on the preview, 100,000 empty twins in the inspector | desktop | each found a defect, fixed before this release (see the CHANGELOG's Fixed and Security bullets) |

## Left for the owner

- Drag-out in a native Wayland session (the Linux box has no compositor); the workaround is documented.
- Symlink creation with Developer Mode on (junctions and hard links were checked).
- The Ctrl+P rendering of folder styles by eye (covered by the walkthrough flow; keystrokes from a background process did not reach the window).
