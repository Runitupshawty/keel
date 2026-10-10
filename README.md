# Keel

Keel is an open-source, cross-platform file manager written in Rust (egui + wgpu): two panes, tabs, fast search and rich previews, with no telemetry. Windows 10/11 is the daily driver; macOS and Linux builds come from the same code.

![Dual pane with tabs and the sidebar](docs/screenshots/2026-10-09-task5-dual-pane.png)

![PDF preview next to the file list](docs/screenshots/2026-10-09-task7-pdf-preview.png)

## What works (v0.8.0)

- **Dual pane and tabs**: two panes (Ctrl+Shift+D for one), any number of tabs per pane (drag to reorder), back/forward history, breadcrumb or editable path (Ctrl+L), details and grid views with thumbnails, sidebar with home folders and drives.
- **Search**: Ctrl+F opens a search tab. Windows uses [Everything](https://www.voidtools.com/) when it is running and otherwise Keel's own index (see [Search without Everything](#search-without-everything)), macOS uses Spotlight (`mdfind`), Linux uses `plocate`/`locate` when installed, otherwise a folder walk. Ctrl+Enter opens a result's folder with the file selected.
- **Fuzzy folder jump**: Ctrl+P matches against every folder the search index knows (or a walk of your home folder).
- **Command palette**: Ctrl+Shift+P lists every action by name with its shortcut.
- **Filter by typing**: start typing in a list to filter it; Esc clears.
- **Previews**: F3 shows or hides the preview panel (there is no Ctrl+Shift+V shortcut). Code with syntax highlighting, text, Markdown, images (PNG, JPEG, GIF, WebP, BMP, ICO, SVG), PDF pages (needs the pdfium library next to the binary: included in the release archives, or run `scripts/fetch-deps`), CSV/TSV and spreadsheets (xlsx, xls, xlsb, ods), Word documents (docx), video thumbnails (optional: needs `ffmpeg` on `PATH`), hex for anything else.
- **File operations**: copy, move, rename (F2), new folder/file, and delete to the OS trash (never a permanent delete), with progress, cancel, and skip/overwrite/rename prompts on name clashes. Drag and drop between panes and from other apps.
- **System clipboard**: Ctrl+C / Ctrl+X / Ctrl+V exchange files with Explorer (including cut) and Finder / file managers on Linux (copy only for now).
- **Archives as folders**: zip, 7z, tar (gz, bz2, xz, zst) and rar open like directories, nested ones too. See [Archives](#archives).
- **Embedded terminal**: a shell pane under the file panes that follows the active folder. See [Terminal](#terminal).
- **SFTP remotes**: browse, preview and copy to and from any SSH host. See [Remotes over SSH](#remotes-over-ssh).
- **Cloud accounts** (Google Drive, Dropbox, S3-compatible buckets) as folders; sign in with your own OAuth client id; tokens stay in the OS keychain. See [Cloud accounts](#cloud-accounts-bring-your-own-client-id).
- **Columns view**: Miller columns per pane, plus a **drop zone** strip that carries files across navigation. See [Columns view and drop zone](#columns-view-and-drop-zone).
- **Profiles**: separate settings and sessions you can switch at runtime. See [Profiles](#profiles).
- **Icon themes**: install VS Code icon themes from the Marketplace. See [Icon themes](#icon-themes).
- **Command line, single instance and a global hotkey**: `keel <folder>` opens in the running window; Ctrl+Shift+Alt+K brings Keel forward. See [Command line](#command-line).
- **Drag out** of Keel onto other apps (Windows).
- **Own search index** on Windows, so search works without Everything. See [Search without Everything](#search-without-everything).
- **Library**: an index of every file across your folders, drives, SSH hosts and cloud accounts that keeps working when a drive is unplugged. Cross-source search, tags, favorites, saved views, a duplicate finder, and a preview before every copy, move or delete. See [Library](#library).
- **Media view** (0.7.0): a square-tile grid for photos and videos from persistent thumbnail sidecars, with EXIF orientation applied, video thumbstrips you scrub with the mouse, and a full-window viewer (Space or Enter, arrows to move, `I` for the metadata panel). Sidecars are made by an idle-priority job, never on the UI thread, and kept within a size budget.
- **Protection** (0.7.0): the Overview shows how safe your data is: copies per file counted by failure domain (two copies on one disk or one cloud account count once), backup state, integrity checks that flag files changed since they were last verified, an editable drive inventory, and "last copy" warnings in delete and move previews. Cloud accounts are indexed like local folders.
- **Devices and Spacedrop**: pair your own machines with a short code or QR code, browse their shared folders as `node://` sources and send files with resumable, verified Spacedrop. See [Devices and Spacedrop](#devices-and-spacedrop).
- **Daemon, CLI and MCP**: `keel-daemon` serves the library over JSON-RPC, `keel search`, `keel plan` and friends work from a terminal, and `keel mcp` lets Claude Code, Codex and other agents use the library with a preview before every change. See [Daemon, CLI and MCP](#daemon-cli-and-mcp).
- **Open, open with, reveal** in the system file manager, open a terminal in the current folder.
- **Themes**: dark and light (a TOML file in `<config>/themes/dark.toml` or `light.toml` overrides the built-in colours). One built-in file icon theme.
- **Settings** (Ctrl+,): theme, hidden files, dual pane, preview panel, maximum preview size. Open tabs are restored on the next start; a local folder that was deleted falls back to your home folder, while a tab on an unreachable network share stays open and shows the error. A `config.toml` or `session.json` that cannot be read is kept as `config.toml.bad` / `session.json.bad`.
- **Crash safety**: a bug inside the UI is written to `crash.log`, the panes are reset and Keel keeps running.

Settings live in `%APPDATA%\Keel` (Windows), `~/Library/Application Support/Keel` (macOS) or `~/.config/keel` (Linux); session and `crash.log` in `%LOCALAPPDATA%\Keel`, `~/Library/Caches/Keel` or `~/.cache/keel`. Set `KEEL_CONFIG_DIR` to keep settings, session and `crash.log` in one folder instead (the window size and position that eframe saves in `app.ron` stay in eframe's own folder).

## Library

The library is an index of the files in your **sources** (a local folder or drive, an SSH host, a cloud account). It records name, size, dates and kind, follows files across renames, and keeps the last indexed state of each source so you can still browse, search and plan operations on a drive that is unplugged. It is additive: Keel works as before with the library off (Settings → Library).

**Add a source.** In the sidebar's Library section choose **Add source…** and pick a local folder, a configured remote or a cloud account. Indexing starts at once and runs in the background; the source shows a status dot. Local sources are watched live and fully re-checked every 6 hours; remote and cloud sources are re-walked on a poll interval (15 minutes by default).

**Sidebar.** Overview (counts, storage, sources, running jobs, duplicates), Favorites (Ctrl+D toggles), Recents, Sources, Tags and saved Views. Sources open as `library://` tabs that work offline. Ctrl+Shift+T opens the tag picker; tags show as chips on rows. Search can use the Library as its backend.

**Where data lives.** Each source has its own SQLite store (with full-text search) in `<data dir>/library/<name>/`, where `<data dir>` is `%LOCALAPPDATA%\Keel` (Windows), `~/Library/Application Support/Keel` (macOS) or `~/.local/share/keel` (Linux). Set `KEEL_DATA_DIR` to use another folder. The library only changes your files through an operation you confirmed.

**Content hashing.** To find duplicates Keel computes BLAKE3 content ids lazily: a sample of the file first, and a full hash only when samples collide; files of 192 KiB or less are hashed whole. Hashing runs at idle priority. Settings → Library → Hashing chooses *idle only* (default), *pause on battery* (also runs while you work, but not on battery) or *off*. Only sources on a local path are hashed.

**Preview before you act.** Copy, move, delete and rename started from a library view first show a preview built from the index: what will change, plus warnings for the last copy of a file, a permanent delete on SFTP or S3, content that has not been hashed (unverified), and an offline source. Execution checks the plan again and stops if anything changed since the preview.

**Duplicate finder.** Open it from the Overview or the command palette ("Find duplicates"). It lists groups of files with identical content across sources and the space you could reclaim; removing copies goes through the same preview.

**Search syntax.** Several terms must all match; results are ranked.

| Term | Meaning |
| --- | --- |
| `report` | name or path contains the word |
| `"annual report"` | exact phrase |
| `ext:pdf` | extension |
| `size:>1mb`, `size:<10kb` | size comparison |
| `dm:2026-10` | modified in that month (ranges and `today` work too) |
| `kind:image` | file kind (`file:`, `folder:`) |
| `tag:taxes` | has the tag |
| `in:photos` | under a path or source |

Limits: hashing skips remote and cloud sources; two sources on one disk count as two locations (failure domains come later); the Overview has no per-source counts; remote and cloud sources are polled rather than watched; a folder copy resumed after a crash re-runs as a merge.

## Devices and Spacedrop

Keel can talk directly to your other computers. There is no account and no server of ours: devices connect peer to peer over an encrypted link ([iroh](https://www.iroh.computer/)), using iroh's public relay servers only when a direct path is not possible. Turn it on or off in Settings → Devices, where you also set this device's name, the Spacedrop inbox folder (default `Downloads/Keel Drops`) and which devices may send without asking. Keel's identity key lives in the OS keychain.

**Pair two devices.**

1. On one device open the sidebar's **Devices** section and choose **Pair a device… → Show code**. Keel shows a short code and a QR code (the QR carries the full ticket).
2. On the other device choose **Pair a device… → Enter code** and type the short code, or paste the ticket.
3. Both sides now list each other under Devices with a status dot (direct, relay or offline), the device's name and a storage bar.

A code works for 10 minutes and for one pairing; showing a new code replaces the old one. Treat it like a password until it is used. Pairing grants nothing: a freshly paired device can see only its name until you share something. Each pair of devices keeps one connection, whichever side opened it.

**Share folders (grants).** **Shares…** on a device row (or the Devices menu) lists what you give that device. Add a grant for a whole source or for one folder inside it, as **Read** or **Read-write**. A grant covers the folder and everything under it, and **Revoke** takes effect at once: running transfers from that device are cut off and later requests are refused. Sources that come from another device are never re-shared.

**Browse a remote source.** **Browse** on a device opens a tab at `node://<device>/<source>/...` (the tab title shows the raw id for now). It behaves like any other folder: listing, preview, copy and drag between panes. Add a device's source as a library source to index it and search it like local files. Writes to a device land only in its local sources, are staged and published atomically, and are checked with BLAKE3.

**Spacedrop.** Drag files or folders onto a device in the sidebar, or use **Send with Spacedrop…** in a file's context menu. The receiver sees an accept prompt (Accept, Decline, or "always accept from this device"). Files travel in resumable 4 MiB pieces and show up as a job in the jobs panel; if the link drops or either app restarts, the transfer continues where it stopped. Pieces are staged in a `.keel-partial-<id>` folder inside the inbox, each file is verified against a BLAKE3 hash of the whole file, and only then moved into the inbox (a name clash becomes `name (1).ext`, never an overwrite). Cancel from the jobs panel.

**Security model, in plain words.**

- Only devices you paired can connect; everyone else is rejected before any request is read.
- Pairing reveals nothing about either device until the other side proves it knows the code, and the code works once.
- Every request is checked against your current grants, so a revoked or narrowed share applies even on an open connection. Paths that try to escape the shared folder (`..`, symlinks and junctions, Windows device names) are refused.
- Each peer is limited in connections and in concurrent requests, and a transfer that stalls is dropped after an idle timeout.
- Only one process may own a device store at a time, so grants cannot be changed behind the owner's back.
- Spacedrop needs your accept (or a standing auto-accept for that device), and the sender cannot choose where files land.
- Anyone holding a still-valid code can pair, so show it only to the person in front of you.

Limits: the short code is found through internet discovery, so on a network with no internet use the full ticket (the QR code does); device writes go only to local sources.

## Daemon, CLI and MCP

Everything the library can do is also a typed operation (29 of them: search, tags, favorites, sources, jobs, duplicates, redundancy, copy, move, delete and rename plans, devices and shares), reachable three ways: JSON-RPC from `keel-daemon`, `keel` subcommands, and an MCP server. The reference with schemas and examples is [docs/api.md](docs/api.md).

**The preview-first rule.** A command that would change anything only returns a preview with a plan id and an input hash. `execute` applies exactly that plan; it refuses a wrong hash, a plan older than 10 minutes, and a plan whose sources changed in the meantime. Taking something away (`sources.remove`, `shares.revoke`) is the only thing done directly.

**Start the daemon.** `keel daemon start` (or run `keel-daemon`) opens the profile's library in the background, resumes its jobs and listens on a per-user local socket (a named pipe on Windows) that only your user can reach; `keel daemon status` and `keel daemon stop` do what they say, and there is one daemon per profile. `keel-daemon --ws 127.0.0.1:7420` also serves a WebSocket that requires a bearer token from a file in the config folder. Devices need `[net] enabled = true` in the profile's `config.toml`. Without a daemon the subcommands open the library themselves, which works only while the Keel window is closed.

**One-liners.**

```sh
keel search "invoice ext:pdf" --max 20
keel tag add receipts ~/Docs/invoice-2026.pdf
keel plan move ~/Downloads/old.iso --to /mnt/archive | keel execute
keel plan delete ~/old-photos --json
keel devices
keel shares
keel sources add ~/Photos --label photos
keel daemon status
```

`--json` prints machine-readable output; `--profile NAME` selects a profile. Exit codes: 0 ok, 1 the operation failed, 2 usage. `keel tag` and `keel sources add|index` show their preview and apply it, since typing the command is the confirmation; `keel plan` stops at the preview.

**MCP for agents.** `keel mcp` is an MCP server on stdio with one tool per operation. For Claude Code:

```sh
claude mcp add keel --scope user -- keel mcp
```

or in a project's `.mcp.json`:

```json
{ "mcpServers": { "keel": { "command": "keel", "args": ["mcp"] } } }
```

For Codex, in `~/.codex/config.toml`:

```toml
[mcp_servers.keel]
command = "keel"
args = ["mcp"]
```

Use the full path to `keel` when it is not on `PATH`. Every mutating tool returns a preview and says to call `execute` with the plan id; tell your agent to show you the preview and run `execute` only after you agree. Read-only tools are marked as such.

Limits: the window does not attach to a running daemon yet (only one of them can hold the library), and the daemon has no SFTP or cloud providers and no live file watching.

## Columns view and drop zone

Switch a pane to **Columns** from the view buttons in the pane header. Column 0 is the tab's folder; selecting a folder lists it in the next column, and selecting a file shows its preview in the last one. Left/Right move between columns, Enter opens a folder, Up or Backspace step back one column, and the column dividers can be dragged (widths are saved). All file actions work on the column you are in.

The **drop zone** is a strip above the status bar (Ctrl+Shift+Z shows or hides it; it also appears while you drag rows). Drop rows on it, or press Ctrl+Shift+S to stash the selection, then navigate anywhere and use **Paste here** (copy) or **Move here** into the active pane's folder; **Clear** empties it. Items that no longer exist are greyed and skipped, and moved items leave the strip only once they have really moved. The stash is saved with the session. "Reduce motion" in Settings → General turns off the animations.

## Profiles

A profile is a separate settings file and session (tabs, views, drop zone). Settings → Profiles creates one (a copy of the current settings), renames, deletes (to the OS trash) and switches; the command palette has "Switch profile: <name>". `keel --profile <name>` starts in a profile. Files: `<config>/profiles/<name>/config.toml`; the session is in the cache folder (`profiles/<name>/session.json`, or `session.json` for `default`).

## Icon themes

Settings → Icons lists the built-in icons and any installed theme, previews them and switches at runtime. To install one, type the extension id from the Visual Studio Marketplace (`publisher.name`, for example `PKief.material-icon-theme`) and press **Install from VS Code Marketplace…**. Keel downloads the package, shows its license and installs only after you press **I accept**. Themes are unpacked into your own config folder; Keel does not bundle or redistribute them.

Only SVG and PNG icons are used. An SVG that references anything outside itself (an `<image>`, `<script>` or `<foreignObject>`, an external `href` or `url()`, CSS `@import`, entities) is refused, both at install time and when the theme loads, and the number skipped is reported. This stops a theme from making Keel read local files or network shares. A theme holds at most 10,000 icons and 64 MB.

## Search without Everything

On Windows, when Everything is not running, Keel uses its own index: it reads the NTFS file table and keeps it current from the USN journal, and stores it in the cache folder. Queries are substring, glob (`*.pdf`) or `regex:`, with `folder:` and `in:<path>` filters.

Reading the whole file table needs administrator rights. Until you grant them Keel indexes your home folder (recursively) plus one level of each drive's root. Settings → General → **Index all drives (administrator)** asks Windows for elevation once, runs a helper (`keel --index-service`) that builds the full index, and hands the files back to your user; later starts need no elevation. The status bar names the active backend and shows its state. If Everything is running it is used instead and the button is greyed out. macOS and Linux keep Spotlight, `locate` and the folder walk.

## Command line

```
keel [FOLDER] [--new-window] [--profile NAME] [--search QUERY]
```

| Flag | Meaning |
| --- | --- |
| `FOLDER` | Open this folder (a file opens its folder with the file selected). Relative paths are resolved against where you ran `keel` |
| `--new-window` | Start a separate window even if Keel is already running |
| `--profile NAME` | Use the profile `NAME` ([Profiles](#profiles)) |
| `--search QUERY` | Open a search tab with this query (in FOLDER when given) |
| `--version`, `--help` | Print and exit |

By default there is one Keel per user and profile: a second `keel` hands its folder or search to the running window over a private pipe or socket that only your user can reach, brings it forward and exits. Turn this off with "Reuse the running window" in Settings → General. Settings → General also has the **global hotkey** (default Ctrl+Shift+Alt+K, empty to disable) that brings Keel forward from any app; it needs Ctrl, Alt or the Windows/Command key besides Shift, and Ctrl+Alt alone is avoided because it is AltGr on many layouts.

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| Ctrl+, | Settings |
| Ctrl+Shift+T | Tag picker (library) |
| Ctrl+D | Toggle favorite (library) |
| Ctrl+Shift+Z | Show or hide the drop zone |
| Ctrl+Shift+S | Stash the selection in the drop zone |
| Ctrl+Shift+Alt+K | Global hotkey: bring Keel to the front (configurable) |

The full list, with your bindings, is in the command palette (Ctrl+Shift+P). Cmd replaces Ctrl on macOS.

## Archives

Press Enter or double-click a `.zip`, `.jar`, `.7z`, `.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`, `.tar.xz`, `.tar.zst` or `.rar` file to open it as a folder (an archive inside an archive opens too). Up leaves the archive; preview works on the files inside. Right-click for **Extract here**, **Extract to folder**, **Extract to…**, **Add to "name.zip"** and **Compress to zip…**. Extraction asks before overwriting and refuses entries that would land outside the target folder.

Archives are read-only: you can extract from any of them and add to a zip, but not delete, rename or create anything inside one. RAR is read through libunrar and never written. Password-protected entries are marked with a lock and cannot be previewed.

## Terminal

A terminal pane sits under the file panes. It starts in the active pane's folder and, with "Follow pane" on, changes directory when you navigate. Shells: PowerShell, Windows PowerShell, cmd and each installed WSL distribution on Windows; `$SHELL`, zsh or bash on macOS; `$SHELL`, bash or sh on Linux.

| Key | Action |
| --- | --- |
| ``Ctrl+` `` (backtick; Cmd on macOS) | Open, focus or hide the terminal. Hiding keeps the shell running; the x button ends it |
| `F6` or `Shift+Esc` | Leave the terminal and return to the file panes |
| `Esc` | Goes to the shell (vim, less, fzf) |
| Context menu, "Open terminal here" | Focus the terminal in the current folder |

"Follow pane" sends a `cd` only when the shell looks idle at a plain prompt; heavily customised prompts (oh-my-posh and similar) can prevent it.

## Remotes over SSH

Any host that runs an SSH server with SFTP enabled can be added under **Settings → Remotes** and then appears in the sidebar with a status dot (grey: disconnected, yellow: connecting, green: connected, red: failed). You can copy and move between local folders and remotes, and between two remotes (the data passes through your PC), preview files, and search names in a remote folder with Ctrl+F. Deleting on a remote asks first and is permanent: there is no trash.

### Add your Mac or NAS

1. On the Mac or NAS turn SSH on (macOS: System Settings → General → Sharing → Remote Login; NAS: its SSH or SFTP service). Check that `ssh user@host` works from a terminal.
2. In Keel open **Settings** (`Ctrl+,`) → **Remotes** → **Add**. Enter a label, the host name or IP address, the port (22 by default) and your user name.
3. Pick the authentication: **SSH agent** or **Key file** (for example one from `~/.ssh`, with its passphrase if it has one) are preferred; **Password** also works. Passwords and passphrases are stored in the OS keychain and never in `config.toml`.
4. Optionally set the initial folder and bookmarks. On a Mac, iCloud Drive is at `~/Library/Mobile Documents/com~apple~CloudDocs`.
5. Click the host in the sidebar. The first time, Keel shows the server's host-key fingerprint. Compare it with the one printed by `ssh-keyscan -t ed25519 host | ssh-keygen -lf -` (run it from a machine you trust) and choose Trust only if they match. Keel then records the key in `~/.ssh/known_hosts`.
6. Browse. Bookmarks are listed under the host; copy and paste or drag files between a remote pane and a local pane.

If a known host presents a different key, Keel refuses to connect and says so. Remove the old line from `~/.ssh/known_hosts` only after you know why the key changed.

### Security notes

Passwords and key passphrases live in the operating system's keychain (Windows Credential Manager, macOS Keychain, Secret Service on Linux) and are never written to `config.toml`, logs or toasts. Host keys are checked against your `~/.ssh/known_hosts`; an unknown key needs your explicit approval and a changed key is a hard error. Uploads are written to a temporary file next to the target and renamed only when complete, so a dropped connection does not leave a half-written file under the real name.

## Install

Download the archive for your system from [Releases](https://github.com/Runitupshawty/keel/releases). The builds are not signed yet.

**Windows** (`keel-<version>-win64.zip`): extract anywhere and run `keel.exe`; keep the DLLs next to it. No Visual C++ redistributable is needed (the runtime is linked statically). SmartScreen may warn about an unknown publisher (More info, Run anyway). Optional: install and start Everything for search.

**macOS** (`keel-<version>-macos-arm64.tar.gz` for Apple silicon, `-macos-x64` for Intel): extract, then clear the download quarantine and run it:

```sh
tar xzf keel-*-macos-*.tar.gz && cd keel-*-macos-*
xattr -dr com.apple.quarantine .
./keel
```

**Linux** (`keel-<version>-linux-x64.tar.gz`, x86-64, X11 or Wayland; glibc 2.35 or newer, e.g. Ubuntu 22.04, Debian 12):

```sh
tar xzf keel-*-linux-x64.tar.gz
mkdir -p ~/.local/opt ~/.local/bin ~/.local/share/applications ~/.local/share/icons/hicolor/256x256/apps
mv keel-*-linux-x64 ~/.local/opt/keel
ln -sf ~/.local/opt/keel/keel ~/.local/bin/keel
cp ~/.local/opt/keel/keel.desktop ~/.local/share/applications/
cp ~/.local/opt/keel/keel.png ~/.local/share/icons/hicolor/256x256/apps/
```

Optional everywhere: `ffmpeg` on `PATH` for video thumbnails. The release archives already contain pdfium for PDF previews.

## Build from source

Install stable Rust with [rustup](https://rustup.rs). CI builds and tests on Windows, macOS and Linux.

**Windows**: install the Visual Studio Build Tools (C++ workload), then:

```powershell
pwsh scripts/fetch-deps.ps1
cargo build --release
```

**macOS**:

```sh
scripts/fetch-deps.sh
cargo build --release
```

**Linux**:

```sh
sudo apt-get install libgtk-3-dev libxkbcommon-dev libwayland-dev libasound2-dev
scripts/fetch-deps.sh
cargo build --release
```

`fetch-deps` downloads the runtime libraries (pdfium, and the Everything SDK DLL on Windows) into `target/deps/`; the build copies them next to the binary. Run the tests with `cargo test --workspace`. The app icon is generated from `assets/keel.svg` with `cargo run -p keel-app --example gen_icon`.

## Cloud accounts: bring your own client id

Google Drive and Dropbox sign-in uses OAuth with PKCE and a loopback redirect (`http://127.0.0.1:<port>/`). Keel ships no OAuth client ids: `assets/cloud-clients.toml` holds placeholders. Register your own free app (a Google Cloud "Desktop app" OAuth client with the Drive API enabled, or a Dropbox scoped app with redirect URI `http://127.0.0.1`) and enter its client id per account, or replace the placeholders in that file before building; the file explains each step. Tokens and S3 keys are kept in the OS keychain, never in `config.toml`.

Limits and requirements:

- Account ids are lowercase (`[a-z0-9_-]`), because Windows Credential Manager ignores case.
- Drive and Dropbox take a file in one request, so uploads are held in memory and capped: 256 MB for Google Drive and 150 MB for Dropbox, until resumable uploads exist. S3 uploads stream.
- Google Docs, Sheets and other Drive-native files, and Drive shortcuts, have no bytes to download; they are not listed, so copying a folder skips them.
- A folder shows at most 50,000 entries.
- HTTPS uses rustls with the pure-Rust graviola crypto and the operating system's certificate store. Graviola needs an x86-64 CPU with AES, AVX2, ADX and BMI2 (most made since about 2014) or a 64-bit ARM CPU with AES, PMULL and SHA-2 (Apple silicon, Raspberry Pi 5); on other CPUs adding or opening a cloud account fails with "cloud accounts need a CPU with ...".
- If a sign-in is revoked, the account stops making requests and asks to sign in again.
- Keep `reqsign_core=warn` in any debug log filter (`keel_vfs::cloud::LOG_FILTER_HINT`): S3 keys are redacted, but the signing library logs credential providers at debug level.
- Cloud support is `keel-vfs`'s default `cloud` feature; build without it to leave out opendal, reqwest and rustls.

## Roadmap

Phases 1 to 8 are released (the usable core, archives and terminal, SFTP remotes, cloud storage, polish, the library, media and protection, devices with the daemon, CLI and MCP). In progress:

9. Clients and extensions: adapters (mail attachments, notes, repositories), web and mobile clients.

Known limitations of each release are listed in [CHANGELOG.md](CHANGELOG.md).

## Contributing

Bug reports and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Run `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --workspace` before opening a pull request.

## License

Licensed under either the MIT License or the Apache License 2.0, at your option. Third-party components: see [THIRD_PARTY.md](THIRD_PARTY.md).
