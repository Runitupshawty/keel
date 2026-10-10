# Keel features

Everything Keel does, with links to the reference for each part. New to Keel? Start with the [getting-started guide](getting-started.md).

## Feature list

- **Dual pane and tabs**: two panes (Ctrl+Shift+D for one), any number of tabs per pane (drag to reorder), back/forward history, breadcrumb or editable path (Ctrl+L), details and grid views with thumbnails, sidebar with home folders and drives.
- **Search**: Ctrl+F opens a search tab. Windows uses [Everything](https://www.voidtools.com/) when it is running and otherwise Keel's own index (see [Search without Everything](files.md#search-without-everything)), macOS uses Spotlight (`mdfind`), Linux uses `plocate`/`locate` when installed, otherwise a folder walk. Ctrl+Enter opens a result's folder with the file selected.
- **Fuzzy folder jump**: Ctrl+P matches against every folder the search index knows (or a walk of your home folder).
- **Command palette**: Ctrl+Shift+P lists every action by name with its shortcut.
- **Filter by typing**: start typing in a list to filter it; Esc clears.
- **Previews**: F3 shows or hides the preview panel (there is no Ctrl+Shift+V shortcut). Code with syntax highlighting, text, Markdown, images (PNG, JPEG, GIF, WebP, BMP, ICO, SVG), PDF pages (needs the pdfium library next to the binary: included in the release archives, or run `scripts/fetch-deps`), CSV/TSV and spreadsheets (xlsx, xls, xlsb), Word documents (docx, with headings, lists, tables and page breaks), PowerPoint slide text (pptx), OpenDocument text, spreadsheets and presentations (odt, ods, odp), video thumbnails (optional: needs `ffmpeg` on `PATH`), hex for anything else.
- **File operations**: copy, move, rename (F2; Ctrl+F2 renames many at once), new folder/file, and delete to the OS trash (never a permanent delete), with progress, cancel, and skip/overwrite/rename prompts on name clashes. Drag and drop between panes and from other apps.
- **System clipboard**: Ctrl+C / Ctrl+X / Ctrl+V exchange files with Explorer and with Linux file managers, cut included (on Linux through X11, or XWayland in a Wayland session: GNOME Files, Nemo, Caja, Thunar and PCManFM read `x-special/gnome-copied-files`, Dolphin `application/x-kde-cutselection`, and a cut made in those apps pastes in Keel as a move), and with Finder (copy; Finder has no cut on the clipboard, so a cut made in Keel is a move only when pasted in Keel).
- **Recycle Bin / Trash**: the sidebar's Recycle Bin (Windows) or Trash (macOS, Linux) entry opens your bin as a folder showing when each item was deleted and, on Windows and Linux, where it came from. Right-click to Restore (macOS: Restore to…), Delete permanently or Empty; both deletes ask first. See [Recycle Bin and Trash](files.md#recycle-bin-and-trash).
- **Archives as folders**: zip, 7z, tar (gz, bz2, xz, zst) and rar open like directories, nested ones too; entries of a zip can be deleted, renamed, moved and added. See [Archives](files.md#archives).
- **Embedded terminal**: a shell pane under the file panes that follows the active folder. See [Terminal](files.md#terminal).
- **SFTP remotes**: browse, preview and copy to and from any SSH host. See [Remotes over SSH](remotes.md#remotes-over-ssh).
- **Cloud accounts** (Google Drive, Dropbox, S3-compatible buckets, WebDAV) as folders; Drive and Dropbox sign in with your own OAuth client id, S3 and WebDAV with keys or a password; all secrets stay in the OS keychain; storage quota (Drive, Dropbox) and Copy link. See [Cloud accounts](../README.md#cloud-accounts-bring-your-own-client-id).
- **Columns view**: Miller columns per pane, plus a **drop zone** strip that carries files across navigation. See [Columns view and drop zone](files.md#columns-view-and-drop-zone).
- **Profiles**: separate settings and sessions you can switch at runtime. See [Profiles](files.md#profiles).
- **Icon themes**: install VS Code icon themes from the Marketplace. See [Icon themes](files.md#icon-themes).
- **Command line, single instance and a global hotkey**: `keel <folder>` opens in the running window; Ctrl+Shift+Alt+K brings Keel forward. See [Command line](files.md#command-line).
- **Drag out** of Keel onto other apps (Windows).
- **Own search index** on Windows, so search works without Everything. See [Search without Everything](files.md#search-without-everything).
- **Library**: an index of every file across your folders, drives, SSH hosts and cloud accounts that keeps working when a drive is unplugged. Cross-source search, tags, favorites, saved views, a duplicate finder, and a preview before every copy, move or delete. See [Library](library.md#library).
- **Media view**: a photo and video grid that scrolls 129,000 items smoothly, video scrubbing on hover, date headers, and a full-window viewer with zoom, an info panel and video playback with sound. See [Media view](library.md#media-view).
- **Protection**: how many copies of each file exist and on how many physical disks or accounts, backup state, integrity checks and a drive inventory, with "last copy" warnings before a delete. See [Protection](library.md#protection).
- **Devices and Spacedrop**: pair your own machines with a short code or QR code, browse their shared folders as `node://` sources, send files with resumable, verified Spacedrop, and keep tags and favorites in sync between them. See [Devices and Spacedrop](devices.md#devices-and-spacedrop).
- **Daemon, CLI and MCP**: `keel-daemon` serves the library over JSON-RPC, `keel search`, `keel plan` and friends work from a terminal, and `keel mcp` lets Claude Code, Codex and other agents use the library with a preview before every change. The window attaches to a running daemon, so all of them share one library while it is open. See [Daemon, CLI and MCP](daemon.md#daemon-cli-and-mcp).
- **Open, open with, reveal** in the system file manager, open a terminal in the current folder.
- **Themes**: dark and light (a TOML file in `<config>/themes/dark.toml` or `light.toml` overrides the built-in colours). One built-in file icon theme.
- **Settings** (Ctrl+,): theme, hidden files, dual pane, preview panel, maximum preview size. Open tabs are restored on the next start; a local folder that was deleted falls back to your home folder, while a tab on an unreachable network share stays open and shows the error. A `config.toml` or `session.json` that cannot be read is kept as `config.toml.bad` / `session.json.bad`.
- **Crash safety**: a bug inside the UI is written to `crash.log`, the panes are reset and Keel keeps running.

Settings live in `%APPDATA%\Keel` (Windows), `~/Library/Application Support/Keel` (macOS) or `~/.config/keel` (Linux); session and `crash.log` in `%LOCALAPPDATA%\Keel`, `~/Library/Caches/Keel` or `~/.cache/keel`. Set `KEEL_CONFIG_DIR` to keep settings, session and `crash.log` in one folder instead (the window size and position that eframe saves in `app.ron` stay in eframe's own folder).
