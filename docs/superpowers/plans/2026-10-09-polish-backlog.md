# Polish backlog (from the Task 5 and Task 6 reviews, 2026-10-09)

Items the reviews confirmed but that were not folded into a task. None loses data. Work them as a
"Task 10: polish" pass after Task 8, one commit per bullet group, tests where the bullet names a scenario.

## keel-app
- Path box: resolve relative input against the pane's folder; `X:` → `X:\`; a file path opens its parent and selects the file.
- Thumbnails: request `THUMB_PX * pixels_per_point`; exempt video/pdf thumbnails from the 64 MiB `MAX_PREVIEW_BYTES` cap; `worker::request` returns false on disconnect; drop queued requests for tiles no longer visible.
- Grid inline rename: put the text box on a child Ui so later tiles do not shift.
- `Theme::load` reads the config file on the UI thread: load once at startup, cache both themes.
- `platform.rs`: reap child processes on a helper thread (unix zombies); try a list of terminals on Linux and split `$TERMINAL` args; escape `;` for `wt -d` on Windows; Linux "Open with" picker from `xdg-mime query`; Properties dialog (in-app on all OSes, Explorer verb on Windows via a keel-vfs helper).
- Sidebar: `drive_of` must match on path components; filter pseudo/read-only mounts on macOS/Linux.
- Keys: allow AltGr (ctrl+alt) text to start the filter; macOS hidden-files shortcut Cmd+Shift+. instead of Cmd+H.
- `drives()` in-flight flag and timeout/error state; a listing that never returns must stop the spinner.
- Refresh hidden tabs on activation and pane 1 when dual mode is re-enabled; watch the parent folder so a rename of the current folder is noticed.
- `move_tab`: shift the active index arithmetically (two tabs on the same folder are normal).
- Jobs: `plan_conflicts` uses the long-path form; report skipped counts in the job result; `Msg::Planned` must not replace an open dialog; optional one-transfer-per-volume queue.
- Clipboard: write on a worker thread with a toast on failure and OpenClipboard retries; macOS compare canonicalized paths; Linux cut flag (`x-special/gnome-copied-files`) is an open deviation; round-trip test must save/restore the real clipboard or be `#[ignore]`.
- In-pane drag tooltip says exactly "Move" or "Copy", computed with the drop rule.
- Listing: queue a relist when a listing for the same folder is already in flight (mark dirty).
- 100k-entry folders: measure the per-keystroke and per-refresh cost after the Task 6 fixes (target < 16 ms).

## keel-app (from the Task 7 review, 2026-10-09)
- Preview worker: abandon a render stuck for ~15 s, show `Error("timed out")`, start a fresh thread (cap abandoned threads).
- Search: one newest-wins search worker instead of a thread per query (stale queries pile up on the Everything lock).
- Preview cache: byte budget (~256 MB) in addition to the 64-entry count.
- PDF: render page width to the panel width and add a width bucket to `PreviewKey` so widening the panel re-renders sharp.
- Table preview: cap displayed columns at 64 with a "first N columns" note.
- Code preview: `selectable_labels = false` (light-theme selection highlight under light text is unreadable).
- Status bar: truncate the left group under ~800 px width.
- Palette shortcut labels: "Cmd" on macOS.
- `move_tab`: track the active tab by index (search tab + folder tab with the same dir).
- Open location into a folder the other pane already shows: clear the filter and reveal the cursor immediately.

## keel-vfs
- `ops_unix.rs` `rename_noreplace`: allow case-only renames on case-insensitive filesystems (`same_file::is_same_file`).
- `reflink-copy` fast path on macOS/Linux (skipped in Task 2).
- Links: `.lnk` shortcut files are ordinary files; confirm copy/move does not refuse them (only symlinks/junctions are refused).

## keel-search
- `$HOME` walk treats `regex` as a literal substring (needs the `regex` crate); Spotlight and locate backends are untested on real hardware.

## keel-preview
- Windows-1252 text decoding (`encoding_rs`); PowerShell highlighting needs the onig syntect engine (left out on purpose).

## Repo
- History scrub: commits 816dd36 and 4a24e42 contain `crates/keel-app/tests/snapshots/main.png` (local user path + drive labels). Rewrite with `git filter-repo --path crates/keel-app/tests/snapshots/main.png --invert-paths` once all worktrees are merged, then force-push `main` (branch protection: allow_force_pushes is false, enforce_admins false).
- Everything SDK license text may be missing from the SDK zip; verify `target/deps/licenses/everything/` after `fetch-deps` and add the MIT notice by hand if empty.
