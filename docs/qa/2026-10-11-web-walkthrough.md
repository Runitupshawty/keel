# QA walkthrough of the web client, 2026-10-11 (after 0.17.0)

The web client (`keel-daemon --web`) was driven in Chromium through Playwright, flow by flow, on a fixture library in a throwaway profile (`tests/web-e2e`, run with `bash tests/web-e2e/run.sh`; see the README's Contributing section). The client is an egui canvas, so the flows act through the app's own actions and read its state through the `window.__keel` hook of the browser-test bundle (cargo feature `e2e`, never in the release bundle), and touch the canvas for real where the gesture is the point: taps on the bottom bar and on rows, a long-press, pull to refresh, a swipe, a double tap and a two-finger pinch in the preview sheet. Every flow saves a screenshot to `tests/web-e2e/out/`; every screenshot was looked at.

**Flows**: two files, 40 screenshots, about 22 s on Windows (plus about 2 s to start the daemon and index the fixture).

- `desktop.mjs` (1280x800, light): the shell and its manifest; a paste into the token box; a wrong token, then the right one with **Remember on this device**, a new load that signs in by itself, **Sign out** and a load that asks again; both sources of the sidebar; folders down two levels and up again; an `ext:` search, a search that finds nothing, a `tag:` search; previews of a text file, a PNG, a PDF and a file inside a zip; a tag added, Favorite set and the tag taken off (each previewed, then executed: library metadata only); delete and rename previews, cancelled; a copy onto an existing name, previewed and cancelled; a copy into a fixture folder, previewed and executed (the only file operation executed: the file arrives, nothing else in the fixture changes), with the job in the jobs list; Devices (devices on, an identity in memory, nothing paired; no pairing) and the empty Spacedrop inbox; a file posted to `/share`, claimed after signing in, with nothing paired to send to; the service worker.
- `phone.mjs`: a phone at 390x844 with touch and the dark theme (the four bottom-bar tabs tapped, a tapped file in the preview sheet, the sheet closed by its Back button and by the browser's Back, a swipe to the next file, double tap to zoom in and out, a long-press menu, pull to refresh, the media grid, a search, an image in the sheet pinched to zoom in); a phone at 320x568; a tablet at 820x1180; the daemon stopped and started again; the light theme.

On every page the token must not appear in the address, the console, the page text or the hook's state, and the console must stay free of errors.

## Found and fixed

| # | What a user saw | Fixed in |
| --- | --- | --- |
| 1 | Light theme: the token box was a black bar on the light sign-in form. | `14f77c3` fix(web): the sign-in box is light in the light theme |
| 2 | A mistyped token: "The daemon refused the token (the first message must be auth with the daemon token).", as if the client had sent the wrong thing. | `734ded2` fix(daemon): a wrong token is refused as "not the token in daemon.token" |
| 3 | A search with no match left the Search tab blank, the same as a search still running. | `90f703c` fix(web): a search that finds nothing says so |
| 4 | The desktop preview panel shrank from 360 to about 140 points while nothing was selected ("Select a file to preview it." cut off at the window's edge); a selected file's Delete and Rename buttons were then cut off too. | `d0b5d30` fix(web): the preview panel keeps its width |
| 5 | Tablet in portrait (820 wide): two panes of about 120 points each, paths cut to a few letters, a file's size and date drawn over its name. | `f3d4a7f` fix(web): narrow panes cut names short instead of drawing them under sizes and dates |
| 6 | Devices with nothing paired showed only "Keel (this device)": no device id, no word that pairing happens in the window. | `febb058` fix(web): Devices shows this device's id and says when nothing is paired |
| 7 | With the daemon stopped, only the small dot turned to "offline, retrying"; the last listings and preview stayed as if current and clicks did nothing. | `c763187` fix(web): a strip says when the daemon cannot be reached |
| 8 | After signing in, the phone's first screen (Browse) and the desktop panes were blank under the path field; on the phone the sources are on another tab. | `e8ee10b` fix(web): an empty pane says where the sources are |
| 9 | Every PDF previewed in the web client said "PDF previews need pdfium next to keel.exe", even with pdfium next to keel-daemon: the daemon never loaded it. | `858a39b` fix(daemon): PDF previews work in the web client |
| 10 | The same message named keel.exe from keel-daemon, which needs its own copy. | `2593e58` fix(preview): the missing-pdfium message names the program that needs it |
| 11 | The phone's Back (button, gesture, or the browser's) with the preview sheet open left Keel altogether instead of closing the sheet. | `b7ef82c` fix(web): the phone's Back closes the preview sheet instead of leaving the app |
| 12 | On a 320-point phone the bottom bar's labels broke mid-word over two lines: "Brow/se", "Searc/h", "Librar/y", "Devic/es". | `70fe10f` fix(web): the phone's bottom bar labels stay whole on a 320-point screen |
| 13 | After any refresh (a finished job, another client's change, pull to refresh) the second pane became the active one, so the next source picked in the sidebar opened there. | `6ae2134` fix(web): reading the panes again keeps the active pane |
| 14 | The jobs list showed raw kinds: "#5 op", "#4 hash", "#3 index". | `9df209c` fix(web): the jobs list names each job |
| 15 | Swiping left or right in the phone's preview sheet never moved to the next or previous file (docs/devices.md promises it): the swipe was measured from the press origin after egui had dropped it. | `b8bafd7` fix(web): swiping in the phone's preview sheet shows the next or previous file |

Each fix has a step in the browser test that failed before it (the hook reports what the step needs: the notes drawn in the last frame, the preview panel's width, how many panes show, whether a bottom-bar label fits, a preview's message, the selected file). #2 and #10 also have a Rust test; #14 a unit test of the names.

## Asked for and added

The script asked for flows the web client could not do: take a tag off, set a favorite, and copy (with a conflict, and executed, for the jobs list). The preview panel adding a tag but never removing one, and two panes with no way to copy between them, read as gaps rather than design, so `e6ff32d` feat(web) adds ★/☆ Favorite and a button per tag that previews taking it off, and **Copy to the other pane…** in the desktop layout (the daemon's preview warns "… exists already (Skip)" for a name that exists). docs/devices.md says so (`c4c1b98`).

## The docs

- docs/devices.md listed "tag, rename and delete" for the web client: it now lists untag, favorites and the copy too, this device's id, and the offline strip.
- docs/devices.md, Phones: the long-press menu of a folder has Open instead of Preview and Download; the phone's Back closes the sheet (both added).
- "Preview text, images and PDF pages" was not true of the web client until #9; the text stays.
- README, Contributing: the browser test's two flow files, `SPEC=` and the pdfium check are described.

## Found and left

- **No breadcrumb or back history in the web client.** Folders are reached by double-click, the path field and its ⬆ (Up) button; there is no Back/Forward between folders and no breadcrumb. The flows used open and Up. A feature, not a defect; docs do not promise either.
- **Details, grid and media views on the desktop layout.** The desktop layout has only the list; the phone has List and Grid. Not promised by the docs.
- **The path field shows the raw `library://<id>/` of a source** (32 hex digits), cut off on a phone; the desktop hover shows the source's name, a phone has no hover. A display name in the field would have to round-trip through editing; left for a breadcrumb.
- **Search results with the same name are indistinguishable.** `alpha-notes.txt` and `copies/alpha-notes.txt` show as two identical rows ("alpha-notes.txt Fixture"); the desktop hover shows the path, the phone shows it only in the sheet. A hit selected from a pane is not highlighted in the results (the pane has a `library://` path, the hit the real path).
- **Copy has no conflict choice.** The web client copies with the daemon's default (skip names that exist); there is no overwrite or rename option as in the window.
- **No pairing in the web client.** Devices lists this device and paired ones; pairing stays in the window, which the new empty-state line now says.
- **The phone list centres its rows** ("alpha-notes.txt 33 B" in the middle of a full-width row) and the folder glyph (📁 from egui's emoji font) reads as a small slanted outline at list size. Cosmetic.
- **The device id on the phone is clipped at the edge** without an ellipsis inside the scrolling Devices tab (the desktop sidebar shows "…"). Cosmetic.
- **A pinch zooms the image from its top-left corner**, not around the fingers, so the part under them slides away. Cosmetic; the viewer scrolls to pan.
- **Test environment, not the client:** headless Chromium's ResizeObserver reports `devicePixelContentBoxSize` without the emulated device scale, so eframe drew a 390-point phone at 195 points (everything twice as large) with `deviceScaleFactor: 2`. The phone flows run at scale 1 (egui points are CSS pixels either way); real phone browsers report the right size.
- **Pinch on the media grid** (the tile size) was not driven (the hook does not report the tile size); its arithmetic is unit-tested in `crates/keel-web/src/gesture.rs`, and the same pinch input zooms the sheet in the browser test.
