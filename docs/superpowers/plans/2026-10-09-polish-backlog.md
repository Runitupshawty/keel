# Polish backlog (from the Task 5 and Task 6 reviews, 2026-10-09)

Items the reviews confirmed but that were not folded into a task. None loses data. Worked in Task 21
(2026-10-09): each bullet is marked done (commit), already fixed (by which task) or deferred (why).

## keel-app
- [x] done (59e3f9c) — Path box: resolve relative input against the pane's folder; `X:` → `X:\`; a file path opens its parent and selects the file.
- [x] done (59e3f9c) — Thumbnails: request `THUMB_PX * pixels_per_point`; exempt video/pdf thumbnails from the 64 MiB `MAX_PREVIEW_BYTES` cap; `worker::request` returns false on disconnect; drop queued requests for tiles no longer visible.
- [x] done (59e3f9c) — Grid inline rename: put the text box on a child Ui so later tiles do not shift.
- [x] done (59e3f9c) — `Theme::load` reads the config file on the UI thread: load once at startup, cache both themes.
- [x] done (59e3f9c): reaping, Linux Open with picker, Properties (in-app + Explorer sheet via `keel_vfs::shell`); terminal list / `$TERMINAL` / `wt ;` already fixed by Task 12 (7fdc2fa: "Open terminal here" opens the in-app terminal pane, no external terminal is launched any more) — `platform.rs`: reap child processes on a helper thread (unix zombies); try a list of terminals on Linux and split `$TERMINAL` args; escape `;` for `wt -d` on Windows; Linux "Open with" picker from `xdg-mime query`; Properties dialog (in-app on all OSes, Explorer verb on Windows via a keel-vfs helper).
- [x] done (59e3f9c) — Sidebar: `drive_of` must match on path components; filter pseudo/read-only mounts on macOS/Linux.
- [x] done (59e3f9c) — Keys: allow AltGr (ctrl+alt) text to start the filter; macOS hidden-files shortcut Cmd+Shift+. instead of Cmd+H.
- [x] done (59e3f9c) — `drives()` in-flight flag and timeout/error state; a listing that never returns must stop the spinner.
- [x] done (59e3f9c) — Refresh hidden tabs on activation and pane 1 when dual mode is re-enabled; watch the parent folder so a rename of the current folder is noticed.
- [x] done (59e3f9c) — `move_tab`: shift the active index arithmetically (two tabs on the same folder are normal).
- [x] done (59e3f9c): long-path conflicts, skipped counts, optional per-drive queue (Settings → Transfers); `Msg::Planned` not replacing a dialog already fixed by the Task 7 review fixes (ada3b4d) — Jobs: `plan_conflicts` uses the long-path form; report skipped counts in the job result; `Msg::Planned` must not replace an open dialog; optional one-transfer-per-volume queue.
- [x] done (59e3f9c): worker writes + toast + OpenClipboard retries, macOS canonical compare, round-trip test restores the user's file clipboard (it was already `#[ignore]`, fa96742); [ ] deferred: Linux cut flag (`x-special/gnome-copied-files`) needs a custom MIME type arboard lacks and a Linux desktop to verify — Clipboard: write on a worker thread with a toast on failure and OpenClipboard retries; macOS compare canonicalized paths; Linux cut flag (`x-special/gnome-copied-files`) is an open deviation; round-trip test must save/restore the real clipboard or be `#[ignore]`.
- [x] done (59e3f9c) — In-pane drag tooltip says exactly "Move" or "Copy", computed with the drop rule.
- [x] done (59e3f9c) — Listing: queue a relist when a listing for the same folder is already in flight (mark dirty).
- [x] done (59e3f9c): `perf_100k` (release, JAMES-DESKTOP): refresh 0.96 ms, typing 0.71 ms, Backspace 0.96 ms per keystroke (target < 16 ms); worker-side lowercase + name sort 60 ms. Before the fix: refresh 48 ms, Backspace 35 ms — 100k-entry folders: measure the per-keystroke and per-refresh cost after the Task 6 fixes (target < 16 ms).

## keel-app (from the Task 7 review, 2026-10-09)
- [x] done (59e3f9c) — Preview worker: abandon a render stuck for ~15 s, show `Error("timed out")`, start a fresh thread (cap abandoned threads).
- [x] done (59e3f9c) — Search: one newest-wins search worker instead of a thread per query (stale queries pile up on the Everything lock).
- [x] done (59e3f9c) — Preview cache: byte budget (~256 MB) in addition to the 64-entry count.
- [x] done (59e3f9c) — PDF: render page width to the panel width and add a width bucket to `PreviewKey` so widening the panel re-renders sharp.
- [x] done (59e3f9c) — Table preview: cap displayed columns at 64 with a "first N columns" note.
- [x] done (59e3f9c) — Code preview: `selectable_labels = false` (light-theme selection highlight under light text is unreadable).
- [x] done (59e3f9c) — Status bar: truncate the left group under ~800 px width.
- [x] done (59e3f9c) — Palette shortcut labels: "Cmd" on macOS.
- [x] done (59e3f9c) — `move_tab`: track the active tab by index (search tab + folder tab with the same dir).
- [x] done (59e3f9c) — Open location into a folder the other pane already shows: clear the filter and reveal the cursor immediately.

## keel-vfs
- [x] done (7a731ab) — `ops_unix.rs` `rename_noreplace`: allow case-only renames on case-insensitive filesystems (`same_file::is_same_file`).
- [x] done (7a731ab; runs on macOS/Linux CI, not verifiable on Windows) — `reflink-copy` fast path on macOS/Linux (skipped in Task 2).
- [x] done (7a731ab): test confirms `.lnk` files copy and move — Links: `.lnk` shortcut files are ordinary files; confirm copy/move does not refuse them (only symlinks/junctions are refused).

## keel-search
- [x] done (0d0d9a1): regex crate in the walk; [ ] deferred: Spotlight and locate backends still need real macOS/Linux hardware — `$HOME` walk treats `regex` as a literal substring (needs the `regex` crate); Spotlight and locate backends are untested on real hardware.

## keel-preview
- [x] done (56d4394); PowerShell highlighting stays out by design (the onig engine is a C build) — Windows-1252 text decoding (`encoding_rs`); PowerShell highlighting needs the onig syntect engine (left out on purpose).

## Repo
- History scrub: commits 816dd36 and 4a24e42 contain `crates/keel-app/tests/snapshots/main.png` (local user path + drive labels). Rewrite with `git filter-repo --path crates/keel-app/tests/snapshots/main.png --invert-paths` once all worktrees are merged, then force-push `main` (branch protection: allow_force_pushes is false, enforce_admins false).
- Everything SDK license text may be missing from the SDK zip; verify `target/deps/licenses/everything/` after `fetch-deps` and add the MIT notice by hand if empty.
