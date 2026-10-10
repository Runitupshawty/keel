# Changelog

All notable changes to Keel are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Mounts: keel-daemon serves a library source, or a folder in it, as a drive letter or mount folder (`keel mount <source> <K:|folder> [--subtree P]`, `keel unmount`, `keel mounts`; API `mounts.list`, previewed `mounts.add` and `mounts.remove`). Listings come from the index while the source is offline, reads are on-demand range reads, and writes are staged and replace the file atomically when it closes. Mounts are unmounted when the daemon stops. The backends (`winfsp` on Windows, `fuse` on Linux and macOS) are off by default: build `keel-daemon` with the feature and install the driver; without one, `mounts.add` fails with error -32008. The `winfsp` feature links GPL-3.0 code (see THIRD_PARTY.md).
- Phones: the web client installs as an app (a PWA: `manifest.webmanifest` with 192 and 512 px icons, standalone display, theme colours, and a service worker that caches only the app shell, keyed by the build's wasm hash, never `/rpc`, `/file/` downloads, `/share` or other answers). Below 700 px it switches to a phone layout: one pane or a media grid (pinch to resize the tiles), a bottom bar with Browse, Search, Library and Devices, the preview as a full-screen sheet with pinch zoom, double-tap and swipes between files, long-press menus, pull to refresh and larger touch targets. The desktop layout is unchanged above that width.
- Share → Keel: the installed app is a Web Share Target. The daemon parks the shared files under `<data dir>/shares` (512 MiB, 100 files, 4 waiting at most; cross-site posts refused) until the signed-in client claims them with `share.claim`, once, within 5 minutes; unclaimed uploads are deleted. The client then sends them to the paired device the user picks with the new, previewed `spacedrop.send`.
- `spacedrop.send` (preview lists every file and size, execute starts the drop job), `spacedrop.inbox` (waiting offers and what arrived) and `spacedrop.answer` (previewed accept or decline) in keel-api, so the daemon receives Spacedrops into `[devices] inbox` (default `<data dir>/inbox`, auto-accept from `[devices] auto_accept`) and the web client shows an Inbox with download links. `execute` now names the job of any operation that starts one.
- The web client warns when the page came over plain http from a non-loopback host, and README has a Phones section (same LAN, a tailnet with `--web <tailnet IP>:7421 --ws-allow-remote --web-host <name>`, a TLS reverse proxy).

## [0.8.0] - 2026-10-09

Devices release: pair your own machines, browse and share folders between them, send files with Spacedrop, and drive the library from a daemon, a command line and an MCP server.

### Added

- Devices and pairing: pair two devices with a short code or a QR ticket. Codes last 10 minutes and work once, there is no account and no server beyond iroh's public relays, and each pair of devices keeps one connection.
- Remote sources: a paired device's sources open as `node://<device>/<source>/...` folders that list, preview and copy like any other, and can be added as indexed library sources.
- Grants: share a source or a subtree with a device as read or read-write; a revoke or downgrade takes effect immediately and cuts off running transfers.
- Spacedrop: send files and folders to a paired device in resumable 4 MiB chunks, verified with BLAKE3 over the whole file, staged in a `.keel-partial` folder and moved into the receiver's inbox folder only when complete. The receiver gets an accept prompt, with per-device auto-accept. Drops run as durable jobs that survive a dropped link or a restart.
- App: a Devices sidebar section (status dot, name, storage bar, Browse, Send files, Shares, Forget), a pairing dialog with the code and QR, a shares dialog, "Send with Spacedrop..." in the context menu, and Settings → Devices (enable, device name, inbox, relays, auto-accept list).
- `keel-api`, a typed operation registry with 29 operations and generated JSON schemas. Mutating operations are preview-first: `plan` returns a preview with a plan id and an input hash, and `execute` applies only that exact input, refusing a wrong hash, a changed source or a plan older than 10 minutes.
- `keel-daemon`, a headless host for the profile's library that serves the API as JSON-RPC on a per-user local socket, with an optional token-authenticated loopback WebSocket, notifications for job progress, library changes and device events, and a single daemon per profile.
- Command line subcommands: `keel search`, `keel tag`, `keel plan ... | keel execute`, `keel devices`, `keel shares`, `keel sources` and `keel daemon start|stop|status`, all with `--json`.
- `keel mcp`, an MCP server over stdio with one tool per operation, so Claude Code, Codex and other agents can use the library; every mutating tool returns a preview first. See [docs/api.md](docs/api.md).
- Library search media filters: `camera:`, `taken:`, `w:` / `h:`, `duration:`, `has:gps` and `kind:photo`, from the sidecar media rows. Words that match no name or path also match camera and keywords, ranked below name hits.
- Previews for PowerPoint (`.pptx`, text per slide) and OpenDocument (`.odt`, `.ods` with sheets by name, `.odp`) files. Word previews now keep bullet and numbered lists and page breaks. Previews stop at 200 slides or about 500 pages of paragraphs, read at most 64 MiB of decompressed content (a zip bomb gives an error), and report malformed or non-zip files as an error. `.ods` files are now shown as a document with one table per sheet instead of the spreadsheet grid.
- WebDAV cloud accounts (Nextcloud, ownCloud, Synology, Apache `mod_dav`): add one in Settings → Cloud with the collection URL, user name and password (kept in the OS keychain), test the connection first, then browse, upload, rename (MOVE), make folders (MKCOL) and delete like any other cloud account. No client id is needed; plain `http://` needs an explicit opt-in.
- Add to 7z, tar and tar.gz archives (not only zip): new entries are written to a staging file beside the archive and renamed over it, so cancel or an error leaves the original untouched. RAR stays read-only.
- One-line installers: `scripts/install.ps1` (Windows, per user, Start menu shortcut, optional Desktop shortcut and `PATH`, Settings, Apps entry, `-Uninstall`) and `scripts/install.sh` (macOS, Linux, `--uninstall`). Both verify the download against the release's `SHA256SUMS`.
- Releases now include `SHA256SUMS` and, for Linux, a `.deb` package.
- CI: `cargo deny` (licenses, advisories, bans, sources; `deny.toml`) is required via the `check` job, and pull requests get GitHub dependency review. The workspace crates are marked `publish = false`.
- Persistent name index for macOS and Linux (and the last resort on Windows): search works instantly without Spotlight or `locate`. It walks your home folder, keeps names, sizes and dates in SQLite under the cache folder, follows file changes live, and rebuilds weekly; the status bar shows "indexing N files…" while it builds. It also feeds the folder jump (Ctrl+P).
- Compress dialog: choose Zip, 7z, Tar or Tar.gz (the name's extension follows unless you edited it; the choice is remembered as `archive.default_format`). New **Add to "name"…** context-menu entry when you select one zip, 7z, tar or tar.gz plus items beside it, and dropping files on such an archive row does the same; both ask first and run as a job with progress and cancel.
- Open with… (Ctrl+Shift+O, context menu): Keel's own picker lists the recent apps for the file's extension, the apps the system knows for it (Linux: MIME associations; macOS: applications in /Applications and ~/Applications), Browse… for any executable or .app, and on Windows the system chooser. "Remember for .ext files" files the app in `open_with.recent` in config.toml. The context menu shows the last 5 apps for that extension in an "Open with" submenu. With several files selected, all of them open with the chosen app (one process per file, or one `open -a` on macOS).
- Bulk rename (Ctrl+F2, or "Bulk rename…" in the context menu and palette): a pattern with `{name}`, `{ext}`, `{n}` / `{n:3}` (counter with start and step), `{date}` and `{parent}`, find/replace (plain or regex, optionally case-insensitive) and a case transform, with a live old-to-new preview. Duplicate targets, names that collide with other files, invalid names, empty names and reserved Windows names are flagged and block Apply. Renames go through the folder's provider (local, SFTP, cloud), swaps and chains use temporary names, a partial failure lists the items that failed, and "Undo bulk rename" (toast button or palette) reverses it until the next operation.
- Browser client: `keel-daemon --web` serves the keel-web client (built into the daemon) on `http://127.0.0.1:7421/`: browse sources (offline ones from the index), search, preview text, images and PDF pages, tag, rename and delete through the same previews, follow jobs, see devices, and download files through one-time links. Sign in with the token from `daemon.token` (sent as the first WebSocket message, never in an address; "Remember on this device" is optional). Loopback only unless `--ws-allow-remote`, with `--web-host` for the names a remote bind answers to. See the README "Web client" section and [docs/api.md](docs/api.md).
- `keel daemon rotate-token`: a new daemon token; clients sign in again and sessions with the old token are closed.
- **Recycle Bin / Trash as a folder**: a "Recycle Bin" (Windows) or "Trash" (Linux) entry in the sidebar opens the current user's bin as a `trash://` tab with Name, Original location, Size and Deleted on columns. Restore puts items back where they were deleted from and reports a name clash instead of overwriting; Delete permanently and Empty ask first and say how many items go. Previews work for trashed files. Read-only otherwise (no paste, rename or new items). Not available on macOS yet.

### Changed

- Roadmap: Phases 6 to 8 are released and Phase 9 is in progress.
- Subcommands talk to the daemon when it runs for the profile and otherwise open the library in the same process.
- 7z support moved from the unmaintained `sevenz-rust` to `sevenz-rust2`.
- `syntect` is built with only what the previews use (bundled syntax and theme dumps, fancy-regex), which drops the unmaintained `yaml-rust` (RUSTSEC-2024-0320). The remaining `cargo deny` advisory ignores now name the crate that pulls each one and why it stays.

### Fixed

- Review fixes in `keel-net`: a device store is opened under an exclusive lock so a second process cannot change grants; pairing reveals nothing about either device before the other side proves it knows the code; names, labels and paths are validated on both sides; each peer has connection and request limits; stalled transfers are dropped after an idle timeout; and a request that fails because a connection closed under it is retried once.
- A move of a file or folder within one SFTP host now renames on the server (OpenSSH `posix-rename` when replacing, else the plain SFTP rename) instead of downloading and re-uploading it; folders move in one step. If the server refuses the rename (for example across devices) the move falls back to copy and delete. Conflict handling (skip, overwrite, keep both) is unchanged. This lifts the 0.6.0 known limitation about same-host SFTP moves; copies between hosts still stream through this PC.
- SFTP: `russh` 0.50 to 0.64 and `russh-sftp` 2.4 to 3.0, fixing unbounded memory allocation driven by a hostile server (RUSTSEC-2026-0154, RUSTSEC-2026-0153). Host key trust, key file, agent and password logins, keepalive and cancellation behave as before; a host certificate offered by a server is refused.
- Spreadsheet previews: `calamine` 0.26 to 0.36, which moves its XML parser to `quick-xml` 0.41 and fixes a quadratic-time attribute check and a namespace allocation DoS on crafted xlsx/ods files (RUSTSEC-2026-0194, RUSTSEC-2026-0195).
- 7z: the unmaintained `sevenz-rust` 0.6 is replaced by its maintained fork `sevenz-rust2` 0.23 (RUSTSEC-2026-0246; RUSTSEC-2026-0245 is a path traversal in its own extractor, which Keel never used). A test now proves that 7z, tar and zip entries named `../../evil.txt`, `/evil.txt` or `C:/evil.txt` refuse the whole extraction and nothing is written.
- Battery detection: `battery` 0.7 is replaced by its maintained fork `starship-battery` 0.12, which drops `nix` 0.19 (out-of-bounds write in `getgrouplist`, RUSTSEC-2021-0119; never called by Keel) and the unmaintained `mach` (RUSTSEC-2020-0168). The `power` feature is unchanged.
- Devices: a paired device's listing could claim any content id, so a decoy file hid the "last copy" warning for your only copy and showed as its duplicate. A device's ids are now kept apart as claims: listed in the copies hover, never counted as copies, never in Duplicates. A host no longer reports ids for files whose bytes drifted.
- Spacedrop: an accepted drop id let the sender push a different file list later without asking. An answer now holds for one device, drop id and file list; completed, cancelled or forgotten drops forget it.
- Spacedrop: unanswered offers held connection slots until answered, and accepting after the sender gave up left a staging folder behind. Offers are answered at once and the sender polls; a cancel withdraws the prompt; staging is created only for a sender still waiting, and stale staging is swept.
- Spacedrop: transfers are now in the op log on both sides (`net.drop-sent`, `net.drop-received` per file).
- Spacedrop: receiving many small files rewrote the drop's state after each one (quadratic); 2,000 files took about 38 s and now take about 3 s. Received files never replace an existing file, even when two drops land the same name at once; staging folders are hidden on Windows; an offer too big to send fails at once instead of retrying for ten minutes.
- API/MCP: `sources.remove` with `delete_store` deleted an index store (tags, favorites, hashes) in one direct call an agent could chain. It now previews like every mutating operation, stating the store's size, tags and favorites, and only `execute` removes; `keel sources remove` confirms it as before.
- API: the read operations (`read`, `stat`, `list`, `preview.render`, `media.thumb`, `file.get`) could open Keel's configuration folder (`daemon.token`) and UNC paths (sending the user's credentials to another server). They now refuse the configuration folder however the path is written, and UNC and device paths outside the library's sources.
- API: `read` on SFTP, cloud and device files re-read the file from the start on every call; it now asks the provider for the range. Other locations refuse an offset past 64 MiB.
- API: file-plan summaries (the string `keel mcp --allow-execute` clients repeat for approval) name the first three paths; `DaemonProvider` runs a plan only when its confirmation callback accepts it, and refuses a plan with warnings without one.
- CLI: `keel mcp` opened the window when the working folder had an `mcp` folder. `mcp`, `execute`, `daemon` and `search` are now always the subcommand, and without a terminal no name is taken as a folder.
- Config: `KEEL_NET_SECRET=memory` is honoured by `keel-daemon` and the CLI too; one switch turns devices on (`[devices] enabled`, the window's Settings switch; `[net] enabled` is the fallback); a configuration saved while Devices defaulted on is switched off once. The data folder is created owner-only.
- Windows: the owner-only check of `api-plans.json`, the token and the socket salt refused files made by an elevated process or an administrator account without UAC (owned by Administrators, or an account SDDL writes as an alias), so plans vanished and tokens were replaced at every start. Owners and access entries are now compared as SIDs.
- Spacedrop: when the reply to a drop's last piece was lost, the sender asked the user again, sent the last file a second time as `name (1).ext` and left staging behind. A completed drop is now remembered for an hour and answers the sender from that record.
- Spacedrop: one device could stack unlimited offers (each held in memory with up to 1 MiB of names, each a prompt). At most 4 offers per device and 16 in all wait now (more are refused as busy), and an offer its sender stopped polling is withdrawn with its prompt.
- Spacedrop: an invalid name or a staging failure on the receiver made the sender retry and re-prompt for ten minutes; it now fails at once with the reason. Names that are Windows short names (`KEEL-P~1`) are refused on Windows, and nothing is published through a folder that leads into a staging folder or out of the inbox.
- Spacedrop: the received-files marker is synced to disk, files found complete on resume are logged as sent, the op log is written out when the node closes (a failed batch is retried once), withdrawn prompts disappear at once, and the window's device event queue is bounded like the node's.
- Tests and caches: RAR extraction temp folders and SFTP, cloud and device downloads ignored `KEEL_DATA_DIR` and could land in the real `%LOCALAPPDATA%\Keel`. Every cache and temp root now comes from one resolver (`<KEEL_DATA_DIR>/cache`, else `<KEEL_CONFIG_DIR>/cache`, else a temp folder in tests, else the platform cache folder).
- `--web`: unauthenticated connections could each send 16 MiB messages and fill all 64 connection slots. Until `auth` a message may be at most 4 KiB, at most 16 connections may wait to sign in and at most 8 come from one remote address; a connection that does not sign in within 10 s gets error -32007 before it is closed (as documented). A remote bind checks the `Host` header against its IP and `--web-host` names.
- Devices: forgetting a device drops its pending offers and staging and removes its folders from the library; day-old device download folders are swept from the temp folder; devices are off until turned on in Settings (the identity is created then); received files and pairings are no longer lost from the event queue, and an idle window shows them at once; the offer prompt lists file names and cannot overflow its total; without a Downloads folder drops go to `<data dir>/inbox` instead of being declined silently.

### Security

- API plans: the hash covers everything a plan runs (for file operations also the changes and warnings) and is checked again at `execute`; `api-plans.json` is owner-only on every platform, so a plan altered on disk is refused.
- MCP: `execute` applies a plan only after the user confirmed it through the client (MCP elicitation, showing the summary, changes and warnings); a client that cannot ask is refused unless `keel mcp --allow-execute`, and then each call must repeat the preview's summary. Nothing but `ping` is answered before `initialize`; bad or oversized lines get an error and the session goes on.
- keel-daemon: the WebSocket handshake (`--ws`, and the request head on `--web`) has a 5 s deadline and at most 64 connections are served per listener; `daemon.token` is owner-only and a token file others can access is replaced; a request on the local socket must arrive within 30 s however slowly it trickles in; the socket name is derived from the user's SID or uid and a per-user random salt; `--profile` is validated.
- CLI: `--json` prints one JSON document per invocation; a lone argument naming an existing folder opens it even when it matches a subcommand (`keel ./search` always does); the in-process host keeps extracted archives under the data folder, brings keel-net online only for devices and shares, and registers the profile's SFTP hosts and cloud accounts.

### Known limitations

- The window does not yet attach to a running daemon; only one of them can hold the library at a time.
- Short-code pairing needs internet discovery; on an offline network use the full ticket.
- The daemon's router has no SFTP or cloud providers and does not watch folders live.
- Writes from a device land only in local sources.
- Tabs on `node://` sources show the raw id as their title.
- Video in the viewer shows stills, not playback.

## [0.7.0] - 2026-10-09

Media and protection release: a fast photo and video grid with a full-window viewer, and a Protection card that shows how many copies each file has and on how many physical disks or accounts.

### Added

- Media view (the pane toolbar's **Media** button; tile sizes S/M/L, Ctrl+wheel): a virtualized square-tile grid drawn from the sidecar thumbnails. Thumbnails are decoded on worker threads and uploaded at most 8 per frame; cached textures are capped at 384 MiB (and 4,000 textures); tiles that scroll away are dropped from the queue. In the perf test 129,000 items (5 % videos, real-size 256 px and 1024 px thumbnails, M and L tiles at 1.5x and 2x) scroll at a mean frame time of 0.5 to 1.5 ms. Videos show their strip and scrub across it on hover, and their thumbnail until the strip is made. EXIF orientation is applied. The **Dates** toggle groups tiles under capture-day headers.
- Viewer: Space or Enter opens the selected photo or video full-window (the 1024 px thumbnail at once, the full image decoded in the background at up to twice the screen size), Left/Right/Home/End navigate, mouse wheel, `+`/`-`, `0` (fit) and `1` (100 %) zoom and pan, `I` toggles the info panel (dimensions, capture time, camera, lens, GPS, duration, codec), `F` favorites, Esc or Space closes. Videos show still frames from the strip; Enter or Play opens the system player. A file deleted while open closes the viewer.
- With the library off, thumbnails live in their own cache (2 GiB budget) under the cache folder. With it on, the sidecar job ("Library: media thumbnails") runs for every local source after indexing and the grid reuses its thumbnails.
- Protection (library on): every fixed, removable, network and cloud volume is listed with its failure domain (physical disk serial, server, or cloud account), state (Online, Offline, Archived, Lost, Retired; the last three set by you in the drive inventory), capacity and last seen. `Redundancy` counts copies and failure domains: two copies on one disk or one account are one domain; hard links count once; copies on Archived volumes count (flagged offline); copies on Lost or Retired volumes and drifted files do not count. "Backed up" means a copy on a volume marked as backup in a second failure domain.
- The failure domain is editable in the drive inventory: type the same name on two volumes to make them one domain (two names of one server, a disk the detection splits), or another name to split them; clear the field to go back to the detected one.
- Overview → Protection card: files not checked yet (no content hash), files with one copy, files whose copies share one failure domain, files not backed up, files changed since their last check, offline volumes; each number explains itself on hover.
- Integrity job ("Library: checking integrity"): re-hashes a sample of files on a schedule and marks a file whose bytes changed while its size and times did not (`drift`); drift clears when the file really changes.
- Details view: a Copies column with the locations on hover.
- Delete and move previews warn `SingleDomain` when the remaining copies would all sit in one failure domain, `CopiesOffline` when they would all be on offline or archived drives, and `LastCopy` only counts copies that still exist.
- Cloud sources (S3, Drive, Dropbox) index through their paged listings and count as their own failure domain; a cut-off listing never deletes records.

### Changed

- Source stores migrate to schema v7 (`record.drift`); `library.db` to v6 (`volume.domain_set`, the failure domain set by hand).
- Media metadata (`media` table) is read by the grid and the viewer.
- Sidecar failures are recorded per kind in `meta.json` (`failed`); `error` is the metadata's own error.
- Hashing set to "pause on battery" no longer pauses on input, but the media and integrity jobs always do.

### Fixed

- Deleting the only copy of a file through a junction, symlink, subst drive or bind mount warned nothing: sources are compared by their resolved roots, a walk refuses a root another source already indexes, and a surviving record of the same file at the same resolved path is no longer taken for a hard link.
- Deleting a file marked as drifted warned nothing; it is now unverified (or the last copy). Duplicates no longer list drifted files as identical copies.
- The Protection card read "0 files with one copy only" when nothing was hashed; it now leads with the files not checked yet and marks the other counts as covering checked files only.
- Linux failure domains: LVM and LUKS volumes resolve to their disks, btrfs subvolumes to the mounted device, and NFS, SMB, sshfs and WebDAV mounts are named by their server and share (stable across remounts) instead of one domain per mount.
- One failed sidecar (a strip that timed out) no longer blocks a file's other thumbnails; a timeout is retried instead of recorded; a failed thumbnail keeps the file's real metadata.
- The texture cache is bounded by memory (strips decoded to the tile height), not only by count.
- Date headers no longer reset after the app sat idle for two seconds.
- Media and integrity jobs pause on input under every hashing policy.
- A source whose root is a junction or symlink indexes the folder it leads to (it indexed nothing on Windows).
- Media work asked for while a sidecar job runs is queued instead of dropped; a just-made sidecar cannot be evicted before it is shown; big TIFFs are not read whole for EXIF; the viewer's full decode no longer stalls the UI with a texture bigger than the screen needs; copies badges and integrity checkpoints do less work per file.

### Known limitations

- Video playback in the viewer shows still frames; press Enter to open the system player.
- Date headers sort by capture time only when metadata exists; otherwise the modified time is used.
- Protection counters catch up at the next index, hash or integrity run (live watcher events do not recount).
- A disk cloned with its volume serial reads as the same disk: its files count as hard links of the original, and a source on the clone whose root has the original's file id is refused as the same folder. Give the clone a new serial.
- A Windows disk without a serial (a VHDX Dev Drive, some RAID controllers) is named by its disk number, which can change; set its failure domain by hand. A btrfs filesystem spanning several disks is named by its mounted device only.
- A source aliased in a way neither the resolved paths nor the root's file id reveal (a share of a local folder reached through the network path while the folder is also a source, when the server reports other file ids) still counts its files twice.
- TIFFs over 64 MiB are shown without their EXIF orientation.
- Remote thumbnails follow the "remote thumbnails" setting; the viewer always downloads the file you open.
- A video whose strip times out is retried each session.

## [0.6.0] - 2026-10-09

Library release: Keel now keeps an index of every file across your sources, works offline from it, finds duplicates, and previews every copy, move and delete before it runs.

### Added

- `keel-core`, a UI-free library crate (typed requests and responses, so a later daemon can wrap it). Each source has its own SQLite store (bundled SQLite, FTS5) under `<data dir>/library/<name>/`; set `KEEL_DATA_DIR` to relocate it.
- Streaming indexer with stable record identity: a renamed or moved file keeps its record, and a new file never takes over a renamed file's record. Unreadable files are kept with their error. A different or emptied root, or a cut-off cloud listing, never deletes records.
- Live watching (`notify`) with a full reconcile every 6 hours and after lost events; remote and cloud sources are re-walked on a poll interval (15 minutes by default).
- Durable jobs: progress is persisted and pending jobs resume after a restart. A step checkpoints before its side effects, so a resumed job checks whether the step already ran.
- Safe operations: copy, move, delete and rename go through validate, preview, execute. The preview is projected from the index (it works for offline sources) with warnings for the last copy, a permanent delete on SFTP or S3, content that has not been verified, and an offline source. Execute re-validates and refuses a plan that no longer matches its preview. Every operation is written to a per-library log.
- Content ids: a sampled BLAKE3 hash first, a full hash only on a collision; files up to 192 KiB are hashed whole. Hashing runs at idle priority and pauses on activity or battery.
- Duplicate finder (window and Overview card) with how much space is reclaimable.
- Library search with Everything-style syntax (`ext:`, `size:>1mb`, `dm:2026-10`, `tag:`, `in:`, quoted phrases), ranked, in under 50 ms on 2 million rows.
- Tags (with colors), favorites, recents (from the operation log) and saved views.
- App: a Library section in the sidebar (Overview, Favorites, Recents, Sources with status, Tags, Views), an Add source wizard, an Overview dashboard, and `library://` tabs that browse the last indexed state when a source is offline.
- Search backend selector now includes Library. Tag chips on rows, a tag picker (Ctrl+Shift+T) and Ctrl+D to favorite.
- Preview dialog before copy, move and delete from library views.
- Settings → Library: enable or disable, hashing policy (idle only, pause on battery, off), rescan interval and rebuild.
- Sidecar store and media metadata, the foundation for the media view: an idle-priority job makes thumbnails and metadata sidecars for every image and video of a source and fills a `media` table (dimensions, orientation, capture time, duration, camera, GPS, keywords) under a size budget. Not yet shown in the app or searchable.
- An offline source says why in its tooltip (unreachable, a different folder at its root, or an empty root); for the last two its context menu offers "Adopt new root", which indexes whatever is there now.

### Changed

- New tab stays on Ctrl+T; the tag picker is Ctrl+Shift+T.
- The cloud provider forwards `list_complete` and `remove_kind`, so a cut-off cloud listing is recognised as incomplete.
- Roadmap: Phase 6 is released; phases 7 to 9 are planned.

### Fixed

- Library stores open safely in parallel; one process owns a library at a time; ended jobs are pruned and drop their state.
- Case-insensitive names keep their real casing, including non-ASCII.
- An operation on an unreachable source fails instead of being skipped.
- Removing a source stops its walk, hashing and watchers.
- Shorter write locks during walks, so the UI stays responsive while a big source indexes.
- Delete previews warn about permanent and unverified deletes; locations in logs are redacted.
- A folder copy or move whose target appeared while it ran resumes as a merge instead of reporting done; operations run in batches with one durable checkpoint each, and a refused plan is compared by actions, paths and warnings, not counts.
- Content ids stay only while size, modification and change time (nanoseconds) are unchanged, and are BLAKE3 of the bytes (existing content ids are recomputed once). Hard links count once in duplicates and copies.
- A drive letter or mount point now holding a different (or empty) folder keeps the source offline instead of replacing its index.
- Hashing pauses for 5 seconds after input, skips network shares unless the source allows it, starts after each completed walk (not when hashing is off or paused), and stops promptly when asked.
- Filter-only library searches (`ext:`, `size:`, `dm:`) take about 1 ms on 2 million rows instead of up to 400 ms; `dm:` dates use local time.
- Tag and record ids are never reused, so a deleted tag or file never passes its tags to a new one; stores from another library merge their tags by name.
- Every file system provider states whether its listing is complete and what a delete does (no silent defaults).

### Known limitations

- Hashing skips remote and cloud sources (SFTP, S3, Drive, Dropbox); only sources on a local path are hashed, so duplicates are found among those.
- Two sources on one physical disk count as two locations for the "last copy" warning. Failure domains come in a later release.
- The Overview shows library totals but no per-source counts.
- Remote and cloud sources are found changed by re-walking on a poll interval, not by live events.
- A folder copy resumed after a crash re-runs as a merge into the half-written target.
- The library can be turned off in Settings → Library; library tabs, tags and the Library search backend are then unavailable.

## [0.5.0] - 2026-10-09

Polish release: profiles, icon themes, Miller columns, a drop zone, a command line with single instance and a global hotkey, drag-out to other Windows apps, and Keel's own search index on Windows.

### Added

- Columns view (per pane): column 0 is the tab's folder, a selected folder opens in the next column and a selected file is previewed in the last. Left/Right move between columns, Enter opens a folder, Up and Backspace step back; column widths are draggable and saved in settings.
- Drop zone: a strip above the status bar (Ctrl+Shift+Z) that holds a list of paths across navigation. Drop rows on it or press Ctrl+Shift+S to stash the selection; Paste here, Move here and Clear act on it. The stash is saved with the session (50,000 items at most).
- Profiles: Settings → Profiles creates (a copy of the current settings), renames, deletes (to the OS trash) and switches profiles; the command palette offers "Switch profile: <name>". Each profile has its own `config.toml` and session (tabs, views, column chains, drop zone).
- Icon themes: Settings → Icons lists the built-in icons and installed themes, previews them, switches at runtime and removes them. "Install from VS Code Marketplace…" downloads an icon theme extension by id (`publisher.name`), shows its license and installs only after "I accept".
- Command line: `keel [FOLDER] [--new-window] [--profile NAME] [--search QUERY]`, plus `--version` and `--help`.
- Single instance per user and profile: a later `keel` hands its folder or search to the running window and exits (setting "Reuse the running window", on by default; `--new-window` opts out).
- Global hotkey (default Ctrl+Shift+Alt+K, Settings → General, empty = off) brings Keel to the front from any app, including a minimized window.
- Drag-out (Windows): dragging rows out of the Keel window onto Explorer or another app starts a native file drag (copy by default).
- Own search index (Windows): NTFS MFT and USN journal indexer used when Everything is not running; substring, glob and `regex:` queries, `folder:` and `in:<path>` filters. "Index all drives (administrator)" in Settings → General builds the full index through a one-off elevated helper (`keel --index-service`, hidden); the index files stay owned by the user. The status bar names the active search backend.
- Animations (pane split, preview panel, drop zone, toasts, selection highlight) with a "Reduce motion" setting that makes them instant.
- Optional "One at a time per drive" transfer queue (Settings → General).
- Properties dialog on every OS (plus Explorer's sheet on Windows) and an Open with picker on Linux.
- Path box: relative paths, `~` and `~/x` (home folder), `X:dir` (means `X:\dir`) and file paths (opens the folder with the file selected).
- Copies use reflinks (APFS, Btrfs, XFS clones) before the chunked copy on macOS and Linux.

### Changed

- The folder-walk search (Linux, and Windows home-folder walk) matches regex queries with a real regex instead of plain text.
- Text previews decode non-UTF-8 files as Windows-1252 instead of showing U+FFFD.
- Thumbnails render at physical pixels (sharp on high-DPI screens); queued thumbnails that scroll away are cancelled; PDF and video thumbnails skip the 64 MiB preview cap.
- Name-sorted listings are sorted on the listing worker: refreshing a 100,000-entry folder went from 48 ms to about 1 ms (release build).
- AltGr text starts the filter; the hidden-files shortcut on macOS is Cmd+Shift+. and labels say Cmd there.
- The sidebar hides pseudo and read-only mounts on macOS and Linux. Drive and folder listings time out instead of spinning forever.
- Jobs report skipped items. Clipboard writes run on a worker with retries and a toast on failure.
- Preview renders that hang are abandoned after 15 s; the preview cache has a 256 MB budget; PDFs re-render sharp when the panel is widened; tables show at most 64 columns.
- `THIRD_PARTY.md` lists the new dependencies.

### Fixed

- Hidden tabs and pane 1 refresh when shown; renaming the open folder is noticed; `move_tab` keeps the right tab active.
- Case-only renames work on case-insensitive filesystems on macOS and Linux.
- The one-transfer-per-drive queue is first-in first-out and keys drives by their real volume (subst drives, junctions and mount points included), so no waiter starves.
- Columns view: each folder is listed once per change however many columns and tabs show it; a hung listing stops with an error instead of being asked again; every visible column is watched; context menus act on their own column.
- Drop zone: Move here removes only the items that actually moved (a cancelled conflict, Skip or a failed job keeps them); a stash mixing archive entries and plain files pastes correctly; a hung stat no longer freezes the missing-item check.
- Switching profiles applies the target profile's views, column chains and drop zone instead of writing the old ones into its session.
- Global hotkey: needs a modifier besides Shift, and the default no longer uses Ctrl+Alt alone (that is AltGr on many keyboard layouts). No "hotkey taken" toast from a process that is not the single instance.
- Drag-out validates the data object and never starts a drag whose mouse button is already up; the main window is found by its own window class, not the first titled window.

### Security

- Icon themes: SVG icons from the VS Code Marketplace are checked when a theme is installed and again when it loads. An icon is refused (and the number skipped is reported) when it has `<image>`, `<script>` or `<foreignObject>`, an entity declaration, an `href` that is not `#id` or `data:`, CSS `@import`, a `url()` to anything but `#id`, or CSS escapes. Before, an icon could make the SVG renderer read a local file or a `\\server\share` path, leaking the Windows NTLM hash and freezing the window. A theme may hold at most 10,000 icons and 64 MB of them; the license shown is cut at 64 KB.
- Single instance: only the same user can hand Keel a folder or search. On Windows the pipe's access list grants only the current user, the client refuses impersonation and checks that the pipe is served by a process of the same user (and lets only that process take the foreground). On macOS and Linux the socket lives in a folder only the user can enter (`$XDG_RUNTIME_DIR/keel-<uid>` or `<temp>/keel-<uid>`, mode 0700), and both ends check the peer's user id. Socket names hash the user name, so non-ASCII user names no longer share one name. Relative paths sent by another process are refused.
- Single instance: a second `keel` no longer hangs, with no window, when the running instance is busy or another client connected and sent nothing. Connecting and the answer share a 3 s limit; the running instance reads each client on its own thread, for at most 2 s and 64 KiB.

### Known limitations

- Drag-out to other apps works on Windows only; on macOS and Linux dragging out of the window does nothing yet.
- The single-instance code for macOS and Linux (socket in a private folder, peer user check) and the global hotkey on those systems compile and pass CI, but have not been run on real hardware. The Spotlight and `locate` search backends are still unverified there too.
- The own search index is Windows-only and needs NTFS volumes. Without a saved full index and without administrator rights it indexes the home folder (recursively) plus one level of each drive's root; run "Index all drives (administrator)" to cover everything.
- Everything, when running, is still preferred over Keel's own index; the "Index all drives" button is greyed out then.
- A stat on a hung network, SFTP or cloud path runs on its own thread that is given up on after a timeout; while a host stays dead, abandoned threads can accumulate until the calls return.
- Switching profile at runtime does rebind the single-instance name to the new profile, so a later `keel --profile <name>` reaches the right window. If another Keel already holds that profile's name, the switch is refused with a toast.
- Icon themes: only SVG and PNG icons are used, and an SVG that references anything outside itself is refused, so a theme can show fewer icons than in VS Code (the number skipped is reported on install). Font-based icon themes are not supported.
- The Linux "cut" flag for the file clipboard is still not set (other apps see a copy).
- Builds are not code-signed or notarized.
- Earlier known limitations still apply unless listed here.

## [0.3.0] - 2026-10-09

### Added
- Cloud accounts as folders: Google Drive, Dropbox and any S3-compatible bucket (Backblaze B2, AWS, MinIO). Sign in with your own OAuth client id (PKCE loopback flow); tokens and S3 keys live only in the OS keychain.
- Settings → Cloud: add/edit/remove accounts, per-account client id override, "Sign in again".
- Sidebar "Cloud" section with status dots; copy, move and delete between local, SFTP and cloud through the normal paste flow; per-service delete semantics (Drive trash, Dropbox recoverable delete, S3 permanent) with matching confirm dialogs.
- Google Docs-native files and Drive shortcuts are skipped in listings; rate limits are retried with backoff; uploads to Drive/Dropbox are single requests capped at 256 MB / 150 MB.

### Known limitations

- Cloud accounts show no storage quota or free space anywhere yet.
- There is no "Copy link" (share link) for cloud files; the sidebar menu leaves it out.
- Cancelling a Google Drive or Dropbox upload ends its retries, but a request already on the wire finishes first.

## [0.2.0] - 2026-10-09

Archives as folders, an embedded terminal and SFTP remotes, on Windows, macOS and Linux.

### Added

- Archives as folders: `.zip`, `.jar`, `.7z`, `.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`, `.tar.xz`, `.tar.zst` and `.rar` open like directories (Enter or double-click), including archives inside archives. Breadcrumb, back/forward, Up, preview and thumbnails work inside them; encrypted entries are marked and refuse preview.
- Extract here, Extract to folder, Extract to… (folder picker), Add to `<name>.zip` and Compress to zip… as jobs with progress, cancel and the usual skip / overwrite / rename prompts. Copying out of an archive extracts on paste. Archives that are open show in the sidebar under "Open archives".
- Embedded terminal pane: Ctrl+` (Cmd+` on macOS) opens, focuses or hides it; F6 or Shift+Esc return to the file panes. Shells per OS: PowerShell, Windows PowerShell and cmd plus one entry per installed WSL distribution on Windows, `$SHELL`, zsh and bash on macOS, `$SHELL`, bash and sh on Linux. "Follow pane" changes the shell's directory when the active pane navigates; "Open terminal here" focuses the pane in the current folder.
- SFTP remotes: Settings → Remotes (add, edit, remove), stored in `config.toml` under `[[remotes]]`. Authentication by SSH agent, key file (optional passphrase) or password; passwords and passphrases are kept in the OS keychain only. First connect asks whether to trust the host-key fingerprint; keys are checked against `~/.ssh/known_hosts` and a changed key is a hard error.
- Remotes in the sidebar with status dots (grey, yellow, green, red), bookmarks, and a context menu (connect, disconnect, edit, open terminal here, copy address). Connections reconnect with backoff, every request has a timeout, and a remote tab is never lost.
- Copy and move between local and remote folders and between two remotes, with progress and cancel. Deleting on a remote asks for confirmation first (there is no trash). Ctrl+F in a remote tab searches names below the current folder. Preview and open-with fetch remote files through a size-bounded local cache.

### Changed

- RAR support reads through libunrar's C API (`unrar_sys`) instead of the `unrar` wrapper crate.
- Transfers stage into `.keel-partial-<pid>-<n>` files (at most 255 bytes) and sweep day-old leftovers from destination folders.
- Moving local files to a remote deletes them permanently after the copy is verified, not to the Recycle Bin.
- `THIRD_PARTY.md` lists the archive, terminal and SSH dependencies.

### Fixed

Findings from the adversarial reviews of the three features:

- Terminal: the `cd` line sent by "Follow pane" and "Open terminal here" is inert for every shell, including folder names with quotes, backslashes or the typographic quotes PowerShell treats as quotes; WSL paths are translated in Rust. It is sent only when the shell looks idle at a local prompt. Plain Esc reaches the shell (vim, less, fzf); only F6 and Shift+Esc leave the pane. AltGr, Alt+letter, modified arrow keys and bracketed paste (ESC stripped) behave correctly; Ctrl+` on a focused terminal hides it and keeps the shell running.
- Archives: zip-slip (`..`, absolute and drive-letter names) and Windows reserved names (`con`, `COM1`, `CONIN$`, trailing dots or spaces, `<>"|?*`, control characters) are rejected on extract. RAR symlink, junction, hard-link and file-reference entries are left out of listings and never extracted. Entry bodies must match their declared size (short or overlong is an error); extraction checks free space and the copied size. Solid 7z archives read correctly. Extracting 3,000 files from a zip went from about 45 s to 1.4 to 2.4 s. A decoder thread that ends early is an error rather than a silent end of file. The materialise cache never evicts an entry in use. Add to zip keeps the archive comment and, on Unix, the file mode.
- SFTP: listings have no overall deadline and a slow listing is not reported as a lost connection. Uploads go to a staging file and are renamed only when finished; an abandoned upload removes its staging file. Replacing a file keeps its mode. A host with known keys only negotiates those key types, so a server offering another type fails instead of prompting. `@cert-authority` and `@revoked` lines in `known_hosts` no longer count as trusted keys. Non-UTF-8 remote names are refused with a clear error. Without a prompt listener an unknown host key fails at once.
- Transfers take the local fast path only when every path is plain local, and a move from a provider that cannot delete is refused up front.
- Windows: deleting on a drive without a Recycle Bin (network, removable, RAM disk) is refused with a message instead of being attempted.

### Known limitations

- Archives are read-only except Add to zip / Compress to zip: nothing can be deleted, renamed or created inside an archive, and RAR, 7z and tar are never written. Extracting into a remote folder is not supported yet.
- A move within one SFTP host streams through this PC instead of renaming on the server; remote-to-remote copies also stream through this PC.
- The macOS and Linux terminal and SFTP code paths are compile-checked and run in CI, but have not been run on real hardware yet. WSL shells were not tested live.
- "Follow pane" depends on seeing a plain prompt; custom prompts such as oh-my-posh that hide the user, host or path can stop the `cd` from being sent.
- Remote thumbnails are off by default (a setting turns them on, each one is a download). `~/.ssh/config` is not read, so there is no jump-host or ProxyCommand support.
- The 0.1.0 gaps below still apply unless listed above.

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

[0.6.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.6.0
[0.5.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.5.0
[0.2.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.2.0
[0.1.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.1.0
