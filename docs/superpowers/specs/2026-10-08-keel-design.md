# Keel — cross-platform file manager in Rust

Date: 2026-10-08. Owner: James King. Status: draft for review.

## 1. Purpose

Replace day-to-day use of Windows Explorer on the desktop PC with a faster, previews-everything
file manager that also reaches every other machine James owns, and that runs the same on Windows,
macOS and Linux (James's Macs and the file server, friends on anything). Reference for feature parity and
feel: Atlas (https://atlasfm.modhyt.org). Keel is open source: public GitHub repo so James's friends
can use it, fork it, open issues and PRs. No telemetry.

Success = James opens Keel instead of Explorer for a normal week and does not go back.

### What James said (requirements)

- Rust, fully native UI (no web view). Runs on Windows, macOS and Linux (added 2026-10-08).
- Preview: code, images, video, Word, CSV, PDF, "etc."
- Open `.rar` and `.zip` as folders.
- Everything-class instant search (same as the Everything shortcut he uses today).
- Fuzzy-find folders.
- Profiles; color themes and icon themes.
- Cloud and FTP mounts shown as folders.
- CLI and WSL support.
- Open any file on: Mac mini, Mac Studio, XPS laptop, the file server, iPhone.
- Added 2026-10-09: everything Spacedrive (https://spacedrive.com, github.com/spacedriveapp/spacedrive) does: one
  library across devices, drives, NAS and clouds; content identity + dedupe; tags, favorites, recents, overview
  dashboard; paired devices over an encrypted P2P link; durable jobs; media views; CLI/API/MCP. See 2.10 and Phases 6-9.

### Decisions made in brainstorming

| Topic | Decision |
|---|---|
| UI toolkit | egui (eframe), wgpu backend |
| Platforms | Windows 10/11 x64 is the daily driver and ships first; macOS (arm64 + x64) and Linux (x64, X11 + Wayland) are first-class: same features except the OS-specific rows in 2.9, CI builds and tests all three |
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
`sftp://the file server/backups/x.zip!/inner/file.txt`. A `Router` resolves a `VPath` to the right
provider and handles the `!/` archive nesting by materialising the archive via `local_copy` once
and caching it under `%LOCALAPPDATA%\Keel\cache`.

Providers:

- `local`: std::fs + `notify` watcher. Copy uses `CopyFileExW` (progress callback), move uses
  `MoveFileExW`, delete sends to Recycle Bin via `IFileOperation`. Long paths (`\\?\`) on.
- `archive`: `zip` crate (read/write), `sevenz-rust` (read), `tar`+`flate2` (read), `unrar` (read).
  Listed as folders; "extract here" and "add to zip" as explicit actions.
- `sftp`: `russh` + `russh-sftp`, key auth from `~/.ssh`, one connection per host, reconnect on
  drop. Hosts configured in profile (see 2.6); with `use_ssh_config` (default on) the host
  may be a `~/.ssh/config` alias (HostName, User, Port, IdentityFile, IdentitiesOnly,
  ProxyJump, ServerAliveInterval; values set in the profile win). ProxyJump hops chain
  over `direct-tcpip` channels, each with its own host-key check; Match is skipped,
  ProxyCommand refused (never executed), UserKnownHostsFile ignored.
  Also used for iCloud Drive on the Mac mini
  (`~/Library/Mobile Documents/com~apple~CloudDocs`) which is where the iPhone's files appear.
- `gdrive`, `dropbox`, `s3`: `reqwest` + OAuth (loopback redirect) / static keys. Listing cached,
  downloads streamed, uploads on write. Credentials in the OS keychain (Credential Manager / Keychain / Secret Service) via `keyring`
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

`<config dir>/profiles/<name>/config.toml` (per-OS path in 2.9) holds: remotes, cloud accounts (refs into Credential
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
- Destructive ops: delete goes to the OS trash (Recycle Bin / macOS Trash / freedesktop Trash) locally; remote delete asks once per batch.
- Crashes: `panic = "unwind"` + a top-level catch writes `<cache dir>/crash.log` (see 2.9) and
  restores open tabs on next start.

### 2.9 Platforms

One codebase, three targets. Every OS-specific piece sits behind a `cfg` module with one shared
interface; nothing OS-specific leaks into `keel-app`.

| Concern | Windows | macOS | Linux |
|---|---|---|---|
| Search backend | Everything SDK over IPC (`Everything64.dll`) | `mdfind` (Spotlight) via `std::process` | `plocate`/`locate` if present, else `fd`-style walk with `ignore` crate; own indexer in Phase 5 covers all three |
| Delete | Recycle Bin via `trash` crate | Trash via `trash` crate | freedesktop Trash via `trash` crate |
| Copy with progress | `CopyFileExW` fast path | chunked `std::io::copy` with progress (`fs::copy` fallback, clonefile via `reflink-copy` when available) | same as macOS (`reflink-copy` on btrfs/xfs) |
| Clipboard files | `CF_HDROP` | NSPasteboard `public.file-url` | `text/uri-list` (GNOME/KDE) via `arboard` + custom MIME |
| Open / open-with / properties | `ShellExecuteW`, `openas`, `properties` verbs | `open -a`, `open -R`, Get Info via `osascript` | `xdg-open`, `gio open`, "Properties" = in-app dialog |
| Drives / volumes | `GetLogicalDrives` + volume info | `/Volumes/*` + `statfs` | `/proc/mounts` + `statvfs` (`sysinfo::Disks` wraps all three) |
| Long paths | `\\?\` prefix, `longPathAware` manifest | n/a | n/a |
| Terminal shells | PowerShell, cmd, WSL distros | zsh, bash | user `$SHELL` |
| Config dir | `%APPDATA%\Keel` | `~/Library/Application Support/Keel` | `~/.config/keel` (via `directories`) |
| Cache / logs | `%LOCALAPPDATA%\Keel` | `~/Library/Caches/Keel` | `~/.cache/keel` |
| Bundled binaries | `Everything64.dll`, `pdfium.dll` | `libpdfium.dylib` | `libpdfium.so` (pdfium-binaries ships all) |
| ffmpeg | PATH or winget | PATH or brew | PATH or distro package |
| Packaging | zip of exe + dlls (Phase 1), MSIX later | `.app` via `cargo-bundle` (Phase 5), zip of binary in Phase 1 | tar.gz of binary (Phase 1), AppImage later |
| Icon / manifest | `embed-resource` (`cfg(windows)` only) | `Info.plist` via cargo-bundle | `.desktop` file in release tarball |
| Keyboard | Ctrl | Cmd (egui `Modifiers::command` maps both) | Ctrl |

CI matrix: `windows-latest`, `macos-latest`, `ubuntu-latest`; every PR must pass all three.
Ubuntu runner installs `libgtk-3-dev libxkbcommon-dev libwayland-dev` for eframe. Tests that need
an OS-specific service (Everything, Spotlight) are `#[ignore]` unless the service is present.

### 2.10 Library layer (Spacedrive-class, Phases 6-9)

Phases 1-5 make Keel a great *browser*. The library layer makes it a *filesystem of record*: it knows
every file James owns, on every machine, even when the drive is unplugged. It is additive: the VFS
browser keeps working with the library off.

**Core (`keel-core`, in-process first; `keel-daemon` later)**

- **Library**: one SQLite database per library (`rusqlite`, bundled, WAL, FTS5). Default library
  `main`. Multiple libraries supported; never merged automatically.
- **Sources**: a folder, a whole drive, a NAS share, a cloud bucket, a paired device's source, or an
  adapter (Gmail attachments, Obsidian vault, GitHub repos). Each source has its own portable store
  (`source.db`) that can travel with the data ("provider exit": leave a cloud and keep the
  organization). A *generation* refreshes while the source is online; offline sources stay browsable
  from the last generation (frozen snapshots).
- **Indexer jobs**: walk + watch (`notify`), record name/size/mtime/kind, stable record identity
  across moves (inode / file-id + rename tracking), BLAKE3 content ids computed lazily (sampled hash
  first for duplicate candidates, full hash to confirm). Unreadable files are kept with their error.
- **Identity + dedupe**: records with the same content id are linked; "last copy" warnings before
  delete; duplicate finder view.
- **Protection**: per-record redundancy (how many independent physical copies, by drive / pool /
  failure domain), backup state, integrity (hash drift), capacity; drive inventory with states
  online / offline / archived / lost / retired.
- **Safe operations**: every mutating op is `validate -> preview -> execute`; previews are projected
  from the index without touching files (works for offline drives); execution re-validates and
  reports divergence. The Phase 1 job queue grows into durable jobs: persisted progress, cancel,
  restart recovery, logs.
- **Search**: one query across every source (FTS5 over names, paths, extracted text, tags,
  metadata), ranked, with filters (kind, size, date, device, tag). Everything/Spotlight stay as the
  instant local backends; the library search covers what they cannot (offline, remote, cloud).
- **Tags, favorites, recents, albums/views**: tags with color, nested; favorites; recents from
  operation history; saved views (query + layout) shown in the sidebar.
- **Media**: thumbnails and proxies written as persistent sidecars (`<cache>/thumbs/<cas-id>`),
  video thumbstrips + scrubbing, EXIF/XMP metadata (`kamadak-exif`), HEIC via `libheif` optional,
  a photo grid that scrolls 100k items at 60 fps (virtualised, decode off the hot path).
- **History**: operation log per library; point-in-time browsing of a source.

**Network (`keel-net`)**

- **Devices**: pair nodes with a QR/code (`iroh` + QUIC, end-to-end encrypted, direct when possible,
  relay fallback). Sidebar shows LAN / Relay / Offline per device with storage used/total.
- **Remote sources**: browse and operate a paired device's authorized sources as `node://<device>/...`
  (a VFS provider); content streams on demand with byte-range reads; metadata projections cached.
- **Spacedrop**: send files/folders to a paired device with progress, resumable.
- **Shares**: grant a source or subtree to a person, device or agent; scoped, visible, revocable by the
  host; never merges libraries.
- **Mounts** (stretch): expose any source or subtree as a drive letter / mount point via WinFsp,
  macFUSE, FUSE with on-demand range reads.

**Cloud (`keel-cloud`)**: Phase 4's providers move onto `opendal` so one backend list covers S3 (incl.
Glacier tier), B2, Google Drive, Dropbox, WebDAV, SFTP; cloud sources index like local ones and
stream on demand.

**API and automation (`keel-api`)**: compile-time registered, typed operations (same list for UI,
CLI `keel`, JSON-RPC over local socket / WebSocket, an MCP server, and agent skills); stable ids;
structured output; capabilities are explicit and revocable; agents must `validate -> preview` and
then commit the exact previewed input.

**Extensions**: source adapters, storage backends, file recognition (extension, MIME, magic bytes,
UTType on macOS), typed metadata fields, preview renderers and sidecars, registered jobs / actions /
menus / settings / views; OS handler bridges. Explicit install with declared access. Rust crates
behind a stable trait set first; dynamic loading later.

**Overview dashboard**: total / used storage across devices, files indexed, unique content ids,
devices + states, jobs running, protection summary.

**Clients**: desktop (egui) first; headless `keel-daemon` + CLI; web UI and iOS/Android are Phase 9
stretch goals (the daemon API makes them possible; the egui app does not run on phones).

**Privacy**: local-first, no account, no telemetry; any transfer to another device or cloud is shown
before it happens (recipient, what leaves).

### 2.8 Testing

- Unit tests per crate: VPath parsing, archive listing fixtures (zip/7z/rar/tar in `tests/fixtures`),
  preview `accepts` tables, vt100 grid.
- Integration: a `local` provider test against a temp tree; sftp test against the file server
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
the file server; remote preview + open-with via materialise; job queue for transfers.

**Phase 4 — Cloud**: Google Drive, Dropbox, S3/B2 providers with OAuth.

**Phase 5 — Polish**: profiles UI, icon-theme manager (download VS Code themes), Miller columns,
drop zone, Explorer drag-out, animations, global hotkey, own NTFS indexer.

**Phase 6 — Library core**: `keel-core` with library + source stores (SQLite/FTS5), indexer + watcher
jobs, stable identity, BLAKE3 content ids + duplicate finder, tags / favorites / recents / saved views,
library search across sources, Overview dashboard, durable jobs with restart recovery, `validate ->
preview -> execute` for every op, operation history, offline (frozen) sources.

**Phase 7 — Media + protection**: thumbnail/proxy sidecars, 60 fps photo grid, video thumbstrips +
scrubbing, EXIF/XMP, HEIC; protection model (redundancy by failure domain, backup state, integrity,
drive inventory, last-copy warnings); cloud sources on `opendal`.

**Phase 8 — Devices**: `keel-net` on `iroh`: pairing, LAN/relay/offline, remote sources as
`node://`, Spacedrop, scoped shares; `keel-daemon` + CLI + JSON-RPC + MCP server; mounts (stretch).

**Phase 9 — Clients + extensions (stretch)**: extension traits + adapters (Gmail, Obsidian, GitHub),
web UI on the daemon, iOS/Android clients.

## 4. Public repo rules

- Nothing machine-specific in the repo: no hostnames, IPs, usernames, SSH keys, OAuth client
  secrets. Profiles live in `%APPDATA%\Keel`, never in-tree. James's own hosts go in his profile.
- OAuth: Google/Dropbox client IDs are public by design (PKCE loopback flow, no client secret).
  Users create their own app credentials or use Keel's; documented in `docs/cloud-setup.md`.
- `README.md` (what, screenshots, install, build), `CONTRIBUTING.md` (cargo fmt + clippy clean,
  one PR per change, tests for providers), `LICENSE-MIT`, `LICENSE-APACHE`, `THIRD_PARTY.md`.
- GitHub Actions: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on
  windows-latest for every PR; tagged release builds upload `keel.exe` + zip to Releases.
- Issue templates: bug, feature request. Discussions on for suggestions.
- Branch protection on `main`: PRs required, CI green. James and friends review each other.

## 5. Prerequisites on the desktop PC

Not installed today: `rustup`/cargo, MSVC Build Tools, `gh`, WSL. Phase 1 plan starts with
installing these via winget (rustup, Microsoft.VisualStudio.2022.BuildTools with C++ workload,
GitHub.cli). Everything.exe already runs as a service. ffmpeg 9.0.1 present.

## 6. Out of scope

Replacing Explorer as the default handler; OneDrive; direct iPhone transport (Phase 9 may add a
phone client); bulk rename (candidate after Phase 5); any telemetry; accounts or subscriptions.
