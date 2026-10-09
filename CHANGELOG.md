# Changelog

All notable changes to Keel are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/).

## [0.5.0] - Unreleased

### Security

- Icon themes: SVG icons from the VS Code Marketplace are checked when a theme is installed and again when it loads. An icon is refused (and the number skipped is reported) when it has `<image>`, `<script>` or `<foreignObject>`, an entity declaration, an `href` that is not `#id` or `data:`, CSS `@import`, a `url()` to anything but `#id`, or CSS escapes. Before, an icon could make the SVG renderer read a local file or a `\\server\share` path, leaking the Windows NTLM hash and freezing the window. A theme may hold at most 10,000 icons and 64 MB of them; the license shown is cut at 64 KB.
- Single instance: only the same user can hand Keel a folder or search. On Windows the pipe's access list grants only the current user, the client refuses impersonation and checks that the pipe is served by a process of the same user (and lets only that process take the foreground). On macOS and Linux the socket lives in a folder only the user can enter (`$XDG_RUNTIME_DIR/keel-<uid>` or `<temp>/keel-<uid>`, mode 0700), and both ends check the peer's user id. Socket names hash the user name, so non-ASCII user names no longer share one name. Relative paths sent by another process are refused.
- Single instance: a second `keel` no longer hangs, with no window, when the running instance is busy or another client connected and sent nothing. Connecting and the answer share a 3 s limit; the running instance reads each client on its own thread, for at most 2 s and 64 KiB.

## [0.3.0] - 2026-10-09

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

[0.2.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.2.0
[0.1.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.1.0
