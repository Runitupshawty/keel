# Changelog

All notable changes to Keel are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-10-09

First release: the Phase 1 core on Windows, macOS and Linux. The Windows build links the C runtime statically (no Visual C++ redistributable needed); the Linux build targets glibc 2.35 (Ubuntu 22.04, Debian 12 and newer).

### Added

- Dual pane with tabs per pane (reorder by drag), back/forward history, breadcrumb and editable path box, details and grid views with thumbnails, sidebar with home folders and drives.
- Search tab (Ctrl+F): Everything on Windows, Spotlight on macOS, `plocate`/`locate` or a folder walk on Linux; "open location" with the file selected.
- Fuzzy folder jump (Ctrl+P) and command palette (Ctrl+Shift+P).
- Filter by typing in any list.
- Preview panel (F3): code and text with syntax highlighting, Markdown, images including SVG, PDF pages (pdfium, bundled in the release archives), CSV/TSV and spreadsheets, docx, video thumbnails via ffmpeg (optional), hex fallback. A missing pdfium or ffmpeg is named in the panel.
- Copy, move, rename, new folder/file, delete to the OS trash; progress, cancel, conflict prompts (skip / overwrite / rename); drag and drop between panes and from other apps.
- File clipboard shared with Explorer (copy and cut) and with Finder / Linux file managers (copy).
- Open, open with, reveal in the system file manager, open a terminal here.
- Dark and light themes; one file icon theme.
- Settings window (Ctrl+,) saved to `<config>/profiles/default/config.toml`; open tabs restored from `<cache>/session.json` without blocking startup. A deleted local folder falls back to the home folder; a tab on an unreachable share stays. A file that cannot be read is kept as `.bad` and reported. `KEEL_CONFIG_DIR` moves settings, session and crash log to one folder.
- Crash log (`<cache>/crash.log`, capped at 1 MiB) and a panic guard: a bug inside a frame resets the panes and shows a dialog instead of killing the app.
- App icon, Windows manifest (long paths, per-monitor DPI v2), Linux `.desktop` file; release archives for win64, macOS arm64/x64 and Linux x64 with the pdfium (and Everything) libraries and their license notices.

### Known gaps

- No Properties dialog yet, no "Open with" picker on Linux, no undo, no drag-out to other apps, no native Windows shell context menu.
- A cut made in Keel is honoured inside Keel only on macOS and Linux (other apps see a copy).
- Hidden tabs are not refreshed when you switch to them; renaming the folder a pane is showing is not noticed.
- Spotlight and `locate` search backends are untested on real hardware; on Linux the folder-walk search treats regex queries as plain text.
- Text previews assume UTF-8 (no Windows-1252 decoding); previews stop at 64 MB.
- Path box: relative paths and `X:` without a backslash are not resolved yet.
- On macOS/Linux the sidebar lists pseudo and read-only mounts; apps launched from Keel stay as zombie processes until Keel exits.
- Builds are not code-signed or notarized.

[0.1.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.1.0
