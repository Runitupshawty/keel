# QA walkthrough, 2026-10-10 (after 0.15.0)

The desktop window was driven through the egui_kittest harness, flow by flow, on the screenshot fixture (`crates/keel-app/src/walkthrough_tests.rs`, run with `scripts/walkthrough.sh`; see [CONTRIBUTING.md](../../CONTRIBUTING.md#qa-walkthrough)). Each flow asserts what the user sees and renders a 1280x800 PNG to `target/walkthrough/`; every PNG was looked at. The tasks of [getting-started.md](../getting-started.md) were the script.

**Flows**: 36, on Windows. 28 run with the normal tests; 8 are `#[ignore]`d (library, devices, media view, PDF and video previews, the Recycle Bin, window sizes) and run through the script.

## Found and fixed

| # | What a user saw | Fixed in |
| --- | --- | --- |
| 1 | Properties of a folder: "Contains 2 files, 1 folders". | `598575e` fix(app): Properties counts "1 folder" |
| 2 | Status bar of a folder with one entry: "1 items". | `01bff93` fix(app): the status bar says "1 item" |
| 3 | F2, then typing a new name, appended it: "todo.txtchores". The caret sat after the name. | `561eed8` fix(app): F2 selects the name's stem |
| 4 | Ctrl+P without a search backend walked the real home folder, also from the tests, which pass a no-op backend and a fixture home. | `edb2986` fix(app): Ctrl+P's fallback walk starts at the app's home folder |
| 5 | Library Overview: "1 sources, 0 offline"; the duplicates line and finder: "1 groups"; Settings → Library: "main (1 sources)". | `af344fb` fix(app): the library Overview counts "1 source" and "1 group" |
| 6 | The Recycle Bin tab's path bar read `trash:///`. | `0710d50` fix(app): the Recycle Bin's path bar names it |
| 7 | Path bar: at 800x600 it ran under the view buttons ("umentsDetails"); its first visible part was cut in half ("emgr-wt"); with the preview open the current folder itself was cut ("ocuments"); hovering it drew a scroll bar over the path. | `bf33b10` fix(app): the path bar shows whole parts and never runs under the buttons |
| 8 | Opening the Recycle Bin in a folder tab (the sidebar does) kept the folder's narrow Ext column for Original location: "Original…", paths cut to a few letters. | `2d8355e` fix(app): a folder tab opened on the Recycle Bin gets its wide columns |
| 9 | Video preview: "0:01" over "1.520000", the raw duration again instead of the picture's size and codec. | `cc683ca` fix(preview): a video's line under its duration says its size and codec |
| 10 | Pair… → Show code wrapped the short code at a hyphen, so it read as two codes. | `9ccaefd` fix(app): the pairing code stays on one line |
| 11 | Right-clicking `site.zip` offered Add to "site.zip": a job that can only fail ("cannot add an archive to itself"). | `59276bf` fix(app): a zip's menu no longer offers to add it to itself |
| 12 | The palette said "Search with Everything" on Windows while Keel's own index answered (Everything not running). | `9dbe2cb` fix(app): the palette's search entry is "Search files" on Windows too |
| 13 | With a modal open that is not one of Keel's own dialogs (Settings → Remotes → Add host…, the cloud wizard, an icon license), typed letters opened the file list's filter behind it, which took the focus from the field being typed in, and Del asked to trash the selected file. | `f04f9c4` fix(app): keys never reach the file list behind a modal |

Each fix has a regression test next to the code it changes; the walkthrough flows check them again.

## The guide

Every task of the guide was driven, and none contradicted the app. Two fixes changed what the guide describes, and the guide now says so (`747317c`): the path bar's **…** menu and F2's selected name. The keys table was pressed key by key; Ctrl+\` (it starts a real shell) and the global hotkey (a system-wide grab) were checked only to be bound, not pressed.

## Found and left

- **Columns cut off in a narrow pane.** In an 800x600 window the Details Modified column ends at the pane's edge ("2026-"); a search tab with the library's Copies and Tags columns shows "Co…" and no Tags in a half-width pane. Details uses fixed initial widths (egui_extras) and the columns can be resized; fitting them to the pane is a layout change of its own.
- **Selection actions in the palette that do not apply.** With a `.txt` selected the palette lists Extract here, Extract to folder and Open location; picking one says why it cannot run. Kept: the guide says the selection's actions come first, and the list is one fixed set.
- **An icon a theme cannot draw.** An SVG without a size in an installed icon theme shows egui's red warning sign instead of falling back to the built-in icon. Installing checks the icons for safety, not that egui can draw them; a follow-up.
- **Settings keeps its widest size.** After the Library page the window stays that wide on General (egui windows remember their size). Cosmetic.
- **Theme names in lower case** ("dark", "light") in Settings and the status bar: they are the theme files' names, and custom themes show theirs.
- **Status bar on the Overview tab**: "0 items · 0 B". A selected folder counts as 0 B (listings have no folder sizes).
- **While a folder loads** the previous listing stays under the new path (no flicker, by design).
- **The image preview's header** says the file's size and date but not the picture's size, where text says "Plain Text · 3 lines". A small addition, not a defect.
- **A flaky test**: `backend::tests::attached_media_tiles_come_from_the_daemons_sidecars` failed once in a full run and passed again; not investigated here.

## Harness notes

- **The Recycle Bin flow** (Windows) lists, restores and purges a file of its own and filters the bin's view to that file, so nothing else in it is shown or touched. In the harness the shell's listing sometimes stalls after a restore and a second delete in the same process: the shell answers through a thread's message queue, which the harness (unlike the app's event loop) does not pump. The same delete, list and restore sequence on plain threads lists in about a second every time. When it stalls, the flow puts its file back and reports itself skipped; the script runs it in a process of its own.
- Clicks arrive one frame after the pointer, as with a real mouse; a press in the frame the pointer jumps drags the window under the old position (that looked like Settings jumping across the screen; it was the harness).
- Not driven, because they leave Keel: Show in system file manager, Open terminal here and Toggle terminal, an app from Open with, Enter on a file (the default app), Extract to… and Browse… (the system pickers), cloud accounts (they need an account).
- `docs/screenshots/` were not made again; they still show the path bar from before #7.
