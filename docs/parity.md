# Feature parity with Spacedrive and Atlas

Date: 2026-10-11. Keel: 0.18.0 (commit `171be99`). The design spec ([section 1](superpowers/specs/2026-10-08-keel-design.md#1-purpose)) names two references for feature parity: Atlas for how the window feels, and Spacedrive for the library across devices. This page lists every feature a user can reach in either app, says whether Keel has it, and ranks what Keel still lacks. The next cycle's plan is [Phase 13](superpowers/plans/2026-10-11-keel-phase13.md); earlier cycles: [Phase 12](superpowers/plans/2026-10-11-keel-phase12.md), [Phase 11](superpowers/plans/2026-10-11-keel-phase11.md), [Phase 10](superpowers/plans/2026-10-11-keel-phase10.md).

## How to read this

**Neither reference is a shipping product today.** Each table row says how far the reference has really got:

- **Atlas** has not been released. The free Windows alpha is planned for 2026-10-23, and until then only preorders are sold. The source is private until beta, and no docs, changelog or download exist. Every Atlas row comes from its two web pages and is a claim that cannot be checked yet.
- **Spacedrive** has two lines:
  - **v1** (0.x, a Tauri app). Its last release, 0.4.3, is still marked Latest on GitHub, and its notes say development is paused.
  - **v2**, a ground-up Rust rewrite. It has two alpha releases (2.0.0-alpha.1 for macOS and Linux, and alpha.2, which added Windows) and nightly builds of the CLI and daemon only. A 2.0 beta is planned for 2026-11-01.
  - Separately, spacedrive.com announces a paid, Mac-first native app under FSL-1.1-ALv2. It shows a mockup and says "demo coming soon".

Status words used in the Spacedrive table:

| Word | Meaning |
| --- | --- |
| v1 | Shipped in a 0.x release (the line is now paused) |
| alpha | In the v2 alpha release notes |
| landed | Marked done on `main` in `PROJECT_STATUS.md`, but not yet in a release |
| in progress | Being built, per `PROJECT_STATUS.md` |
| described | Written in the docs or README in the present tense, with no status given |
| planned | Stated as future |
| mockup | Only in the website's illustration |

Keel column: **Has** (where it is documented), **Part** (what is missing), **Lacks**, or **No (decision)** when the spec rules it out. Source links are listed at the end under short names such as [SD-README].

## Spacedrive

### Views and window

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| Grid, list, columns and media views | v1 | [SD-B23] | **Has**: Details, Grid, Columns, Media ([features.md](features.md), [files.md](files.md#columns-view-and-drop-zone), [library.md](library.md#media-view)) |
| Size view (folders by size, kept per tab) | alpha (alpha.2) | [SD-REL] | **Lacks**: listings have no folder sizes; a selected folder counts as 0 B ([QA 2026-10-10](qa/2026-10-10-walkthrough.md)) |
| Tabs | v1 (0.2) | [SD-B02] | **Has** |
| Dual pane | planned (2023 list) | [SD-B23] | **Has**: two panes, one with Ctrl+Shift+D |
| Quick Preview of images, video, audio and PDF, with pinch to zoom | v1 | [SD-B23], [SD-REL] | **Part**: preview panel (F3) and full-window media viewer; no audio preview |
| Inspector (identity, location, availability, provenance, actions) | described | [SD-DIR] | **Yes** (unreleased): the preview panel's Inspector tab (Ctrl+I) with source, volume, content id, first seen, last hashed, copies, history and tags ([files.md](files.md#inspector)) |
| Overview statistics | v1 (0.4.0) | [SD-REL] | **Has**: library Overview ([library.md](library.md#library)) |
| Drag and drop in and out of the OS | v1 (0.4.3; drag-out not on Linux) | [SD-REL] | **Has** (unreleased): drag-out on Windows, macOS and Linux on X11 or XWayland, not from a native Wayland window ([files.md](files.md#dragging-files-to-other-apps)) |
| Keybinds (duplicate tab, zoom, sidebar toggle) | v1 | [SD-REL] | **Has**: configurable keys, command palette ([files.md](files.md#keyboard-shortcuts)) |
| 11+ interface languages | v1 (0.2) | [SD-B02] | **Lacks**: English only |
| Places, pinned folders, Spaces (custom sidebar layouts) | alpha (Spaces); described (Places) | [SD-REL], [SD-DIR] | **Part**: fixed Quick access, library Favorites and saved views; no user-arranged sidebar |
| Collections or albums | alpha (Collections); 2023 announcement (albums) | [SD-REL], [SD-B23] | **Part**: tags and saved views; no hand-ordered collection |
| Mobile explorer | alpha (alpha.2); v1 mobile alpha | [SD-REL] | **Part**: the installable web app (PWA) with a phone layout ([devices.md](devices.md#phones)); no native app |

### Search and understanding

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| Search across every source, offline ones too | described | [SD-README] | **Has**: library search ([library.md](library.md#library)) |
| Filter UI with date range and kind | v1 (0.4.3) | [SD-REL] | **Part**: the same filters as query syntax (`ext:`, `dm:`, `size:`, `kind:`, `tag:`), but no filter chips |
| Text inside documents, OCR | alpha (alpha.1 notes claim OCR); sidecars doc says only thumbnails exist | [SD-REL], [SD-SIDECARS] | **Has** (unreleased) for the text inside documents (text, PDF, Office, OpenDocument; [library.md](library.md#searching)); **Lacks** OCR of images and scans |
| On-device AI image labels | v1 (0.2, YOLO model) | [SD-B02] | **Lacks** |
| File System Intelligence (folder summaries, user context) | planned ("flagship direction") | [SD-DIR] | **Lacks** (not planned) |
| Transcripts, faces, embeddings | planned | [SD-SIDECARS], [SD-B23] | **Lacks** |
| Capture time and place in search | in progress | [SD-STATUS] | **Has**: `taken:`, `camera:`, `has:gps`, `w:`, `duration:` |
| Index of email, people, places | planned ("later") | [SD-WEB] | **Lacks** |

### Tags and organization

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| Nested, colored tags that follow the content to every copy | described; v1 tags | [SD-TAGS], [SD-B23] | **Has** ([library.md](library.md#library)) |
| Tag assign mode (number keys) | v1 (0.4.0) | [SD-REL] | **Has** (unreleased): Ctrl+Shift+A, then 1–9 (Shift: 10–18) toggle tags on the selection ([library.md](library.md#library)) |
| Favorites, recents | mockup; described | [SD-WEB] | **Has** |
| Saved views | described | [SD-LIB] | **Has** |

### Library, index and protection

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| Sources: folder, drive, NAS, other node, cloud bucket | described | [SD-README] | **Has**: local, SFTP, cloud, `node://` ([library.md](library.md#library)) |
| Source adapters (Gmail, Obsidian, GitHub) | mockup | [SD-WEB] | **Lacks** (spec Phase 9 stretch) |
| Live index kept current by watchers | described | [SD-README] | **Has**: local sources watched live; remote and cloud polled or change feeds |
| Stable identity across moves, BLAKE3 content ids, duplicates confirmed by full hash | described | [SD-README] | **Has** |
| Portable source store, kept on the source itself if wanted ("provider exit") | described | [SD-README], [SD-DIR] | **Part**: one store per source that can move between libraries; always under the data folder, never on the source |
| Point-in-time freezes, browsing an old state | described | [SD-README] | **Part**: an offline source shows its last indexed state; no dated freezes |
| Multiple libraries | described | [SD-LIB] | **Has** |
| Library backup and restore | landed (2026-10-07) | [SD-STATUS], [SD-LIB] | **Has**: a `.keelbackup` file, restored as a new library ([library.md](library.md#backup-and-restore)) |
| Locked or unmounted volumes, drive inventory | landed | [SD-STATUS], [SD-README] | **Has**: drive inventory with Archived, Lost, Retired ([library.md](library.md#protection)) |
| Redundancy by failure domain, backup state, integrity checks | described | [SD-README] | **Has** ([library.md](library.md#protection)) |
| "Last copy" warnings; validate, then preview, then execute | described | [SD-README] | **Has** |

### Operations and jobs

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| Copy, move, delete, rename with previews | described | [SD-OPS] | **Has** |
| Batch rename | described (`files.rename_batch`) | [SD-OPS] | **Has**: Ctrl+F2 with a live preview |
| Undo | described (`files.undo`) | [SD-OPS] | **Part**: undo bulk rename only; deletes can be restored from the Recycle Bin or Trash by hand |
| Folder merge, organize, flatten | described; merge landed | [SD-OPS], [SD-STATUS] | **Has** (unreleased): previewed plans with undo ([files.md](files.md#folder-operations)) |
| Create and extract archives | described | [SD-OPS] | **Has**, plus archives open as folders ([files.md](files.md#archives)) |
| Set attributes, create links | described | [SD-OPS] | **Has** (unreleased): hidden, read-only, Unix mode; symbolic links, junctions, hard links ([files.md](files.md#folder-operations)) |
| Trash list and empty | described | [SD-OPS] | **Has** ([files.md](files.md#recycle-bin-and-trash)) |
| Durable jobs: progress, cancel, restart recovery, logs | described; ETA in v1 (0.4.0), live copy speed in alpha.2 | [SD-README], [SD-REL] | **Has**: resume after restart ([library.md](library.md#library)); time left is not documented |
| File sync relationships: copy, mirror, two-way, archive, on-demand cache | described (no status) | [SD-SYNC] | **Lacks** |

### Devices and sharing

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| Pairing by short code, local discovery, relay fallback (iroh) | alpha | [SD-REL], [SD-DEV] | **Has**, plus a QR code ([devices.md](devices.md#devices-and-spacedrop)) |
| Proxy pairing (pair through an already paired device) | alpha (alpha.2) | [SD-REL] | **Lacks** |
| Browse and operate another device's granted sources; revocable shares | described | [SD-README] | **Has**: `node://`, read or read-write grants |
| Spacedrop | v1 (0.2) | [SD-B02] | **Has**: resumable and verified |
| Library sync between devices (index replication, library join) | alpha; landed in part | [SD-REL], [SD-STATUS] | **Part**: tags and favorites sync; another device's files are reached as `node://` sources, not by copying its index |
| Device status (LAN, relay, offline) with storage | mockup | [SD-WEB] | **Has** |
| Mounts on macOS, Windows and Linux, with range reads | described | [SD-README] | **Part**: the Linux release has them; on Windows and macOS you build them from source ([daemon.md](daemon.md#mounts)) |

### Cloud

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| S3, Google Drive, Dropbox as sources | alpha | [SD-REL] | **Has**, plus WebDAV ([README](../README.md#cloud-accounts-bring-your-own-client-id)) |
| OneDrive | alpha | [SD-REL] | **No (decision)**: the spec says never OneDrive |
| "12+ services" through OpenDAL | alpha | [SD-REL] | **Part**: S3, Drive, Dropbox, WebDAV and SFTP |
| Optional encrypted cloud backup, a paid cloud service | planned; 2023 announcement | [SD-DIR], [SD-B23] | **No (decision)**: no accounts or hosted service |
| Cloud jobs (OCR, transcription) with spending limits | planned | [SD-DIR] | **Lacks** |

### Media

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| Thumbnail sidecars | landed | [SD-SIDECARS] | **Has** |
| Video thumbstrips and proxies | not built ("thumbnails are the only sidecar so far") | [SD-SIDECARS] | **Has**: 20-frame strips with hover scrubbing |
| EXIF enrichment | in progress | [SD-STATUS] | **Has** |
| 129,000 photos at 60 fps | claimed for the paid app | [SD-WEB] | **Has**: measured on 129,000 tiles ([performance.md](performance.md)) |

### API, clients and platforms

| Feature | Spacedrive | Source | Keel |
| --- | --- | --- | --- |
| CLI and daemon | nightly | [SD-REL], [SD-CLI] | **Has**: `keel`, `keel-daemon` ([daemon.md](daemon.md)) |
| Call any registered operation by name (`sd op`) | described | [SD-CLI] | **Has** (unreleased): `keel op <name> --params <json>`, preview first, `--execute` to apply ([api.md](api.md#keel-op-any-operation-by-name)) |
| MCP server | in progress (PR open) | [SD-STATUS] | **Has**, preview first |
| Agent skill | landed | [SD-STATUS] | **Has** (unreleased): [agent-skill.md](agent-skill.md), installable from `skills/keel` |
| Generated TypeScript and Swift clients | described | [SD-README] | **Part**: JSON-RPC documented ([api.md](api.md)); no generated SDK |
| Self-update and a nightly channel | nightly | [SD-REL] | **Part**: opt-in update check and Windows self-install; no nightly channel |
| Headless server with a web interface; Docker | described; v1 Docker | [SD-README], [SD-B23] | **Part**: `keel-daemon --web`; no container image |
| Extensions (WASM, SDK) | in progress | [SD-STATUS] | **Lacks** (spec 2.10 Extensions) |
| macOS, Windows, Linux desktop builds | v1 all three; v2 nightlies for macOS arm64 and Linux x64 only | [SD-README], [SD-REL] | **Has**: all three in every release |
| No telemetry, no account | v2 described; v1 had an opt-out | [SD-README], [SD-REL] | **Has**: never had any |

## Atlas

Every row is a claim from [AT] unless marked [AT-W11]. Nothing could be run.

| Feature | Atlas claim | Source | Keel |
| --- | --- | --- | --- |
| Fuzzy finder for files and folders | "Fast Fuzzy Finder" | [AT], [AT-W11] | **Has**: Ctrl+P over folders, the current folder's files and the library's files ([files.md](files.md#finding-folders-and-files-ctrlp)) |
| Miller columns | yes | [AT] | **Has**: Columns view ([files.md](files.md#columns-view-and-drop-zone)) |
| Tabs and split panes | "flexible tabbed and split-pane" | [AT] | **Has**: two panes with tabs; no tiling beyond two |
| Drop zone | yes | [AT] | **Has** ([files.md](files.md#columns-view-and-drop-zone)) |
| Drag and drop | yes | [AT] | **Has** (Atlas is Windows-only, where Keel's drag-out works) |
| Cloud integration | yes | [AT] | **Has** |
| Custom folder and file icons | "change folder and file icons" | [AT] | **Part** (unreleased): a color and an emoji or icon-theme icon per folder in a library source; file icons come from the icon theme |
| Filter by typing | yes | [AT] | **Has** |
| Multiple profiles and sessions | yes | [AT] | **Part**: profiles, each with a restored session; no named sessions within a profile |
| Rich color themes | yes | [AT] | **Part**: dark and light, each overridable in a TOML file ([features.md](features.md)) |
| Smooth animations | yes | [AT] | **Has**, with Reduce motion |
| Searchable context menus | yes | [AT] | **Part**: every context action is in the searchable command palette; the native Windows shell menu is missing (README roadmap) |
| Cloud drives: SFTP | yes | [AT] | **Has** ([remotes.md](remotes.md)) |
| Cloud drives: FTP | yes | [AT] | **Has**: FTP and FTPS remotes ([remotes.md](remotes.md#ftp-and-ftps)) |
| Cloud drives: Google Drive | yes | [AT] | **Has** |
| Cloud drives: OneDrive | yes | [AT] | **No (decision)** |
| Cloud drives "without syncing everything" | yes | [AT] | **Has** inside Keel; as an OS drive only in the Linux release |
| Document preview: PDF, CSV, Word, PowerPoint | yes | [AT] | **Has**, plus xlsx, OpenDocument and code |
| File tools: convert, compress and edit PDFs, videos and images | yes | [AT] | **Part**: compress means archives only; no conversion or editing |
| Icon theme manager (VS Code themes) | yes | [AT] | **Has** ([files.md](files.md#icon-themes)) |
| Media playback and galleries | yes | [AT] | **Has**: media view and viewer with playback |
| A 5,000-file folder opens in 100 ms | benchmark claim | [AT] | **Has**: 100,000 files in 0.24 s warm ([performance.md](performance.md)) |
| 128 MiB copy in 39 ms (CopyFile2) | benchmark claim | [AT] | **Part**: copies use CopyFileExW; not measured |
| Small files: 2,000 x 4 KiB in 525 ms with 4 workers (3,800 files/s) | benchmark claim | [AT] | **Has** (unreleased): a pool of up to eight workers copies 2,000 x 4 KiB in 0.62-0.74 s (2,700-3,250 files/s) and 3,000 one-byte files in 0.87-1.02 s; through a library plan 2.9 s ([performance.md](performance.md)) |
| Startup in 300 ms | claim | [AT] | **Part**: first frame in 0.62 s in the test harness; not compared like for like |
| Local-first, no telemetry | yes | [AT] | **Has** |
| Keyboard-friendly workflow | yes | [AT-W11] | **Has**, including screen readers ([README](../README.md#accessibility)) |
| Platforms | Windows 10/11; Linux in development; macOS on the roadmap | [AT] | **Has**: Windows, macOS and Linux |
| Price and license | free alpha; paid licenses from $9.79; open source at beta | [AT] | Free, MIT or Apache-2.0, open source now |

## What Keel has that neither reference offers

- An embedded terminal that follows the pane, with PowerShell, cmd and WSL shells; WSL distributions' files in the sidebar, and `keel` inside WSL opening the Windows window.
- Archives as folders: zip, 7z, tar and RAR, nested too, and zips editable in place.
- Everything, Spotlight and `locate` as search backends, plus its own NTFS index.
- Browsing and restoring the Recycle Bin or Trash.
- `~/.ssh/config` with jump hosts, WebDAV, cloud quotas and Copy link.
- Share → Keel from a phone's share sheet into Spacedrop.
- Accessibility: screen readers through AccessKit, contrast tested against WCAG.
- Privacy-scrubbed crash reports that never leave the machine, and an opt-in update check.
- A fuzz target for every parser of outside bytes.
- Shipped today on three platforms. Atlas has no release, and Spacedrive's current line has alphas, plus nightlies of the CLI and daemon only.

## voidtools Everything and WinDirStat

Added 2026-10-12 at the owner's request: every feature of voidtools Everything (1.4.1 and the 1.5 alpha) and WinDirStat (up to 2.9.3) is planned. The full checklist, verified against each project's own pages, and Keel's state for every item with file and line, are in [the audit](superpowers/plans/2026-10-12-everything-windirstat-audit.md). In short, at 0.22.0:

| Area | Keel today | Plan |
| --- | --- | --- |
| Everything: index | **Part**: NTFS file table and USN journal, names only (no size, dates or attributes in the index), library sources for other volumes | [Phase 15](superpowers/plans/2026-10-12-keel-phase15.md) Task 70: full-metadata index, folder sizes, fast sort, ReFS/FAT/exFAT/network/folder indexes, an index service |
| Everything: search language | **Part**: AND, quotes, globs, `regex:`, `folder:`, `in:` in the pane; `ext:`, `size:`, `dm:`, `kind:`, `content:`, `tag:` and media terms in the library; no OR, NOT, grouping, modifiers or most functions | Phase 15 Tasks 71 to 74: one parser with every operator, all 49 modifiers and every search function, constants, macros, the journal |
| Everything: search window | **Part**: search tabs, 500-result cap, fixed columns, library chips and saved views (no UI to create them) | [Phase 16](superpowers/plans/2026-10-12-keel-phase16.md) Task 76: toggles, no cap, column chooser, filters, bookmarks, history, omissions, configurable keys, Quick Search window, tray, localization |
| Everything: file lists | **Lacks** | Phase 16 Task 77 |
| Everything: HTTP and ETP servers, network indexes | **Part**: `keel-daemon --web` searches the library only | Phase 16 Task 78 |
| Everything: `es.exe`, command line, SDK, URL protocol | **Part**: `keel search` (library only), `--search` | Phase 16 Task 79 |
| WinDirStat: scanning | **Part**: Calculate sizes and library totals, apparent size only, serial walk, hard links counted per path | [Phase 17](superpowers/plans/2026-10-12-keel-phase17.md) Task 81 |
| WinDirStat: tree, file types, treemap, sunburst, flame graph | **Part**: one-level Sizes view with share bars; no tree, file types list or treemap | Phase 17 Tasks 82 and 83 |
| WinDirStat: largest files, duplicates, search, storage analytics, permissions | **Part**: library duplicate finder, search tabs | Phase 17 Task 84 |
| WinDirStat: cleanups, compression, recovery | **Part**: open, delete to Recycle Bin, terminal, properties | Phase 17 Task 85 |
| WinDirStat: export and command line | **Part**: `keel du --json` | Phase 17 Task 86 |

The owner also asked for a macOS Finder-style interface: [Phase 14](superpowers/plans/2026-10-12-keel-phase14.md), planned first so the surfaces of Phases 15 to 17 are built in it.

Two rows above were found out of date by the audit and are corrected in Phase 16: the Windows shell menu exists (Atlas table, "Searchable context menus"), and keyboard shortcuts are not configurable beyond the global hotkey (Spacedrive table, "Keybinds").

## Ranked gaps

The ranking weighs three things: the requirements in spec section 1 (**R**), daily-driver value, the spec's measure of success being a week without Explorer (**D**), and friends installing and using Keel (**F**). Effort is S, M or L.

| # | Gap | Reference | Why | Effort |
| --- | --- | --- | --- | --- |
| 1 | Native Windows shell context menu (installed handlers, Send to, Open with), searchable | Atlas; README roadmap | D: the main thing still sending a user back to Explorer | M |
| 2 | Undo for file operations (Ctrl+Z) | Spacedrive `files.undo` | D: open since the 0.1.0 known gaps | M |
| 3 | Previews of HEIC and AVIF photos, camera RAW, audio and fonts | Spacedrive Quick Preview; spec 2.3 | R: previews; iPhone photos are HEIC. Spec 2.3 lists HEIC, audio and fonts, none built | M |
| 4 | Folder sizes and a Sizes view | Spacedrive Size view | D: there is no way to see what fills a disk | M |
| 5 | Color themes beyond dark and light (built-ins plus VS Code color themes) | Atlas | R: section 1 asks for color themes | S-M |
| 6 | FTP and FTPS remotes (**done**, Phase 11: [remotes.md](remotes.md#ftp-and-ftps); browsing and copies, not mounts) | Atlas | R: section 1 asks for "cloud and FTP mounts" | S |
| 7 | Faster copying of many small files (a parallel queue) | Atlas | **Done** (unreleased): about 3,000 files/s, was 860, against a claimed 3,800 | S-M |
| 8 | **Done** (unreleased; not yet checked on a machine with WSL, see [Checking](wsl.md#checking)): WSL: distributions' files in the sidebar; `keel` from inside WSL opens the window ([wsl.md](wsl.md)) | spec section 1 | R: CLI and WSL | S |
| 9 | Mounts in the Windows and macOS release builds | Spacedrive | R: cloud as folders for every program. Needs the owner (licensing, see the plan) | M |
| 10 | Signed builds and package channels (winget, Scoop, Homebrew, Flatpak or AppImage) | both are commercial | F. Needs the owner (certificates, account submissions) | M |
| 11 | Text inside documents in library search (OCR later) (**done** (unreleased): [library.md](library.md#searching); text, PDF, Office and OpenDocument files; no OCR) | Spacedrive; spec 2.10 Search | D | M-L |
| 12 | Fuzzy finder over files, not only folders (**done**: Ctrl+P finds files, [files.md](files.md#finding-folders-and-files-ctrlp)) | Atlas | D | S |
| 13 | Folder sync or mirror between sources | Spacedrive File Sync | R: the owner's other machines | L |
| 14 | Library backup and restore | Spacedrive | D: a library that was hashed and tagged is worth keeping. **Done** (unreleased, [library.md](library.md#backup-and-restore)) | S |
| 15 | Drag-out on macOS and Linux (**done** (unreleased): [files.md](files.md#dragging-files-to-other-apps); Linux through X11 or XWayland) | Spacedrive v1; README roadmap | F | M |
| 16 | File tools: convert and resize images, merge and split PDFs, transcode video | Atlas | D | M |
| 17 | An icon or color per folder (**done**, unreleased: [files.md](files.md#folder-colors-and-icons); folders in library sources) | Atlas | D | S-M |
| 18 | Filter chips for library search (**done**: [Searching](library.md#searching), desktop and web client) | Spacedrive v1 | F: query syntax is a barrier for friends | S-M |
| 19 | Tag assign mode with number keys (**done**, unreleased: Ctrl+Shift+A, [library.md](library.md#library)) | Spacedrive v1 | D | S |
| 20 | Interface translations | Spacedrive v1 | F | M-L |
| 21 | Inspector panel (copies, provenance, history in one place) | Spacedrive | D. **Done** (unreleased, [files.md](files.md#inspector)) | S-M |
| 22 | Dated freezes and browsing a past state | Spacedrive | — | L |
| 23 | Index replication between devices, proxy pairing | Spacedrive alpha | — | L |
| 24 | Extensions and source adapters (Gmail, Obsidian, GitHub) | Spacedrive (in progress, mockup) | spec Phase 9 stretch | L |
| 25 | Agent skill, `keel op <name>`, generated SDK (**done**, unreleased: [agent-skill.md](agent-skill.md) and [`keel op`](api.md#keel-op-any-operation-by-name); no generated SDK) | Spacedrive | — | S |
| 26 | Folder merge, organize, flatten, set attributes, create links | Spacedrive operations | **Done** (unreleased, [files.md](files.md#folder-operations)) | S each |
| 27 | Docker image for `keel-daemon`; native phone apps | Spacedrive | the PWA covers phones | S; L |
| 28 | AI labels, folder summaries, transcription | Spacedrive (v1 labels; the rest planned) | out of scope for now | L |
| 29 | Finder-style interface (unified toolbar, sidebar sections, list with disclosure triangles, Gallery, Quick Look, Get Info, tag colors) | owner request | D, F | L (Phase 14) |
| 30 | Everything parity: full-metadata index and the whole search language | voidtools Everything | D: the owner's daily search tool | L (Phase 15) |
| 31 | Everything parity: search window, file lists, servers, `es`, SDK | voidtools Everything | D | L (Phase 16) |
| 32 | WinDirStat parity: scans, treemap, analysis tabs, cleanups, recovery | WinDirStat | D: "what fills this disk" | L (Phase 17) |

Ruled out by the spec: OneDrive, hosted accounts and any cloud service of Keel's own.

## What could not be checked

- **docs.spacedrive.com** answered with a permanent redirect to itself (a loop), so its pages could not be read. The same pages were read as source from the repository's `docs/` folder instead.
- **Spacedrive's paid native app** exists only as a website mockup. Its features, platforms and price are not stated beyond "one-time purchase".
- **OCR**: Spacedrive's alpha.1 notes list OCR, while its sidecar doc says thumbnails are the only sidecar kind. Here it is treated as not shipped.
- **Spacedrive docs without status**: tagging, file sync, devices, library and the cloud adapter model are written in the present tense with no status. The table marks them "described", not shipped.
- **Atlas** has no build to run, and its GitHub link points to `#`. All of its features, the plugin system and the benchmark numbers are unverified. A search found no other Atlas pages; results were about AtlasOS, which is unrelated. Re-run this comparison after the alpha (2026-10-23).

## Sources

| Short name | URL |
| --- | --- |
| [SD-README] | https://github.com/spacedriveapp/spacedrive |
| [SD-STATUS] | https://github.com/spacedriveapp/spacedrive/blob/main/PROJECT_STATUS.md (updated 2026-10-09) |
| [SD-REL] | https://github.com/spacedriveapp/spacedrive/releases |
| [SD-WEB] | https://spacedrive.com |
| [SD-B02] | https://spacedrive.com/blog/alpha-zero-two-release |
| [SD-B23] | https://spacedrive.com/blog/october-alpha-release |
| [SD-DIR] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/core/product-direction.mdx |
| [SD-OPS] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/core/ops.mdx |
| [SD-SYNC] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/core/file-sync.mdx |
| [SD-SIDECARS] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/core/virtual-sidecars.mdx |
| [SD-TAGS] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/core/tagging.mdx |
| [SD-LIB] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/core/library.mdx |
| [SD-DEV] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/core/devices.mdx |
| [SD-CLI] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/core/cli.mdx |
| [SD-HIST] | https://github.com/spacedriveapp/spacedrive/blob/main/docs/overview/history.mdx |
| [AT] | https://atlasfm.modhyt.org |
| [AT-W11] | https://atlasfm.modhyt.org/windows-11-file-manager |
