# Keel — Windows file manager in Rust

Date: 2026-10-08. Owner: James King. Status: draft for review.

## 1. Purpose

Replace day-to-day use of Windows Explorer on JAMES-DESKTOP with a faster, previews-everything
file manager that also reaches every other machine James owns. Reference for feature parity and
feel: Atlas (https://atlasfm.modhyt.org). Keel is personal tooling, private repo, no licensing or
telemetry concerns.

Success = James opens Keel instead of Explorer for a normal week and does not go back.

### What James said (requirements)

- Windows, Rust, fully native UI (no web view).
- Preview: code, images, video, Word, CSV, PDF, "etc."
- Open `.rar` and `.zip` as folders.
- Everything-class instant search (same as the Everything shortcut he uses today).
- Fuzzy-find folders.
- Profiles; color themes and icon themes.
- Cloud and FTP mounts shown as folders.
- CLI and WSL support.
- Open any file on: Mac mini, Mac Studio, XPS laptop, laptopserver, iPhone.

### Decisions made in brainstorming

| Topic | Decision |
|---|---|
| UI toolkit | egui (eframe), wgpu backend |
| Build order | MVP first, then phases |
| Search | Everything SDK over IPC now; own MFT/USN indexer behind the same trait later |
| Remotes | SFTP everywhere (all four machines run SSH) |
| iPhone | via Mac mini's iCloud Drive over SFTP; no direct phone transport |
| Cloud | Google Drive, Dropbox, S3-compatible (Backblaze B2); iCloud via Mac. Never OneDrive. |
| Archives | zip/7z/tar/gz pure Rust; rar via `unrar` crate (libunrar bundled, read-only) |
| Previews | pdfium-render, docx -> styled text, ffmpeg thumbnails + egui-video, pure Rust for the rest |
| Terminal | embedded pane (portable-pty + vt parser); PowerShell, cmd, WSL distros |
| Layout | sidebar + dual pane + preview + bottom terminal; Miller columns later as a view mode |
| Name / repo | Keel, `D:\Work\home\filemgr`, private GitHub, does not hijack Win+E |

## 2. Architecture

Single Cargo workspace. One binary, library crates underneath so each piece builds and tests
alone.

```
keel/
  Cargo.toml              workspace
  crates/
    keel-vfs/             Provider trait + local, archive, sftp, cloud providers
    keel-search/          Searcher trait + everything (IPC) + fuzzy (nucleo)
    keel-preview/         Previewer trait + one module per format
    keel-term/            pty session + vt grid (no UI)
    keel-app/             egui UI: panes, tabs, sidebar, preview, terminal, settings
  assets/                 icon themes, color themes, bundled DLLs (pdfium, unrar)
  docs/superpowers/       specs and plans
```

### 2.1 VFS (`keel-vfs`)

Everything the UI shows is a `Provider`. The UI never touches `std::fs` directly.

```rust
pub trait Provider: Send + Sync {
    fn scheme(&self) -> &str;                         // "file", "zip", "sftp", "gdrive", ...
    fn list(&self, path: &VPath) -> Result<Vec<Entry>>;
    fn read(&self, path: &VPath) -> Result<Box<dyn Read + Send>>;
    fn write(&self, path: &VPath) -> Result<Box<dyn Write + Send>>;   // Unsupported for rar/7z
    fn stat(&self, path: &VPath) -> Result<Entry>;
    fn mkdir / remove / rename(...)                   // Unsupported where read-only
    fn local_copy(&self, path: &VPath) -> Result<PathBuf>; // materialise to temp for "open with"
    fn capabilities(&self) -> Caps;                   // read, write, rename, watch
}
```

`VPath` = `scheme://authority/path` with an optional nested chain so `zip` inside `sftp` works:
`sftp://laptopserver/backups/x.zip!/inner/file.txt`. A `Router` resolves a `VPath` to the right
provider and handles the `!/` archive nesting by materialising the archive via `local_copy` once
and caching it under `%LOCALAPPDATA%\Keel\cache`.

Providers:

- `local`: std::fs + `notify` watcher. Copy uses `CopyFileExW` (progress callback), move uses
  `MoveFileExW`, delete sends to Recycle Bin via `IFileOperation`. Long paths (`\\?\`) on.
- `archive`: `zip` crate (read/write), `sevenz-rust` (read), `tar`+`flate2` (read), `unrar` (read).
  Listed as folders; "extract here" and "add to zip" as explicit actions.
- `sftp`: `russh` + `russh-sftp`, key auth from `~/.ssh`, one connection per host, reconnect on
  drop. Hosts configured in profile (see 2.6). Also used for iCloud Drive on the Mac mini
  (`~/Library/Mobile Documents/com~apple~CloudDocs`) which is where the iPhone's files appear.
- `gdrive`, `dropbox`, `s3`: `reqwest` + OAuth (loopback redirect) / static keys. Listing cached,
  downloads streamed, uploads on write. Credentials in Windows Credential Manager via `keyring`
  (Bitwarden is the source of truth; Keel stores a copy, per James's credential rule).

### 2.2 Search (`keel-search`)

```rust
pub trait Searcher { fn query(&self, q: &Query) -> Result<Vec<Hit>>; }
```

- `EverythingSearcher`: loads `Everything64.dll` (bundled), uses `Everything_SetSearchW`,
  `Everything_QueryW`, result enumeration with size/date columns. Same syntax as the Everything
  window James already uses. Fails soft if Everything.exe is not running (status bar says so).
- `FuzzySearcher`: `nucleo` over a folder-path list fed by Everything (`folder:` filter) for the
  Ctrl+P "jump to folder" popup, and over the current listing for filter-by-typing.
- Later: `NtfsSearcher` (USN journal + MFT) drops in behind the same trait.

### 2.3 Preview (`keel-preview`)

```rust
pub trait Previewer { fn accepts(&self, entry: &Entry) -> bool;
                      fn render(&self, bytes: Source, ctx: &PreviewCtx) -> Result<Preview>; }
```

Previews run on a worker thread; the UI shows a spinner, never blocks. Results are cached by
(path, mtime, size). Formats:

| Kind | Engine |
|---|---|
| Code / text / md / json / toml / log | `syntect` highlighting, `pulldown-cmark` for md |
| Images (png jpg gif webp bmp ico svg heic) | `image`, `resvg`, `libheif-rs` (heic optional) |
| Video / audio | `ffmpeg` (already installed) for thumbnail + metadata; `egui-video` for playback |
| PDF | `pdfium-render`, page thumbnails + zoom, text search |
| Word `.docx` | `docx-rs` -> paragraphs/headings/tables rendered as rich text; `.doc` shows metadata only |
| Excel `.xlsx` / CSV / TSV | `calamine` / `csv` into a virtual table (`egui_extras::TableBuilder`) |
| PowerPoint `.pptx` | slide text + embedded images |
| Archives | first-level listing with sizes |
| Fonts | sample sentence rendered |
| Anything else | hex dump + metadata |

### 2.4 Terminal (`keel-term`)

`portable-pty` spawns `pwsh.exe`/`powershell.exe`/`cmd.exe`/`wsl.exe -d <distro>`; `vt100`
crate maintains the grid; the UI paints it in a monospace egui panel. Cwd follows the active pane
(toggle). WSL path translation via `wslpath` so `D:\Work` becomes `/mnt/d/Work`. Note: WSL is not
installed on this machine today; the pane shows the distro picker empty until `wsl --install`.

### 2.5 App (`keel-app`)

eframe window. State lives in one `AppState` struct; panes hold a `VPath` + listing + selection.

- Sidebar: Quick access, drives, remotes, cloud, archives-open-now, tags (later).
- Two file panes (toggle to one), each with tabs. Views: details, list, grid with thumbnails.
  Virtual scrolling; 100k-entry folders stay 60 fps.
- Preview panel on the right, collapsible, follows selection.
- Bottom terminal, collapsible.
- Command palette (Ctrl+Shift+P) lists every action and every context-menu item, searchable.
- Ctrl+P: fuzzy jump to folder. Ctrl+F: Everything search in a results tab.
- Drop zone: a strip to stash selections across navigation.
- Drag and drop in-app and from/to Explorer (OLE via `winit` file-drop events for in; `IDataObject`
  for out, phase 2).
- Open with: Windows shell associations (`ShellExecuteW`); remote/cloud files materialise first.

### 2.6 Profiles and themes

`%APPDATA%\Keel\profiles\<name>\config.toml` holds: remotes, cloud accounts (refs into Credential
Manager), layout, theme, icon theme, key bindings, terminal shells. Profiles switch from the
command palette. Default profile `james`.

Color themes: TOML files mapping to `egui::Visuals`; ship dark/light plus a few. Icon themes: VS
Code icon-theme JSON + SVG folders dropped into `assets/icons/<theme>`, rendered with `resvg`.

### 2.7 Error handling

- Every provider error is `anyhow::Error` with the VPath attached; UI shows a toast and the pane
  keeps its last good listing.
- Remote disconnects: pane shows "reconnecting" and retries with backoff; never loses the tab.
- Long operations (copy/move/extract/upload) run on a job queue with progress, cancel, and
  conflict prompts (skip / overwrite / rename).
- Destructive ops: delete goes to Recycle Bin locally; remote delete asks once per batch.
- Crashes: `panic = "unwind"` + a top-level catch writes `%LOCALAPPDATA%\Keel\crash.log` and
  restores open tabs on next start.

### 2.8 Testing

- Unit tests per crate: VPath parsing, archive listing fixtures (zip/7z/rar/tar in `tests/fixtures`),
  preview `accepts` tables, vt100 grid.
- Integration: a `local` provider test against a temp tree; sftp test against laptopserver
  (`#[ignore]` unless `KEEL_SFTP_TEST=1`).
- UI: `egui_kittest` snapshot of the main window at 1280x800.
- Perf check: open a 50k-file folder under 150 ms (generated fixture), tracked in CI output.

## 3. Phases

**Phase 1 — MVP (usable)**: workspace, local provider, dual pane + tabs + sidebar + details/grid,
Everything search tab, Ctrl+P fuzzy folder jump, filter-by-typing, previews for code/image/pdf/
csv/docx/video-thumbnail, copy/move/delete/rename with progress, open-with, dark/light theme,
one icon theme, crash recovery, installer-less exe.

**Phase 2 — Archives + terminal**: zip/7z/tar/rar as folders (nested too), extract/add, embedded
terminal with PowerShell/cmd/WSL, cwd follow.

**Phase 3 — Remotes**: sftp provider, hosts for mac-mini (iCloud Drive bookmark), mac-studio, xps,
laptopserver; remote preview + open-with via materialise; job queue for transfers.

**Phase 4 — Cloud**: Google Drive, Dropbox, S3/B2 providers with OAuth.

**Phase 5 — Polish**: profiles UI, icon-theme manager (download VS Code themes), Miller columns,
drop zone, Explorer drag-out, animations, global hotkey, own NTFS indexer.

## 4. Prerequisites on JAMES-DESKTOP

Not installed today: `rustup`/cargo, MSVC Build Tools, `gh`, WSL. Phase 1 plan starts with
installing these via winget (rustup, Microsoft.VisualStudio.2022.BuildTools with C++ workload,
GitHub.cli). Everything.exe already runs as a service. ffmpeg 9.0.1 present.

## 5. Out of scope

Replacing Explorer as the default handler; OneDrive; Linux/macOS builds; direct iPhone
transport; tagging and bulk rename (candidates after Phase 5); any telemetry.
