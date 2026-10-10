# Changelog

All notable changes to Keel are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Library search media filters: `camera:`, `taken:`, `w:` / `h:`, `duration:`, `has:gps` and `kind:photo`, from the sidecar media rows. Words that match no name or path also match camera and keywords, ranked below name hits.

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
