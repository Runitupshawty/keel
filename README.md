# Keel

Keel is an open-source, cross-platform file manager written in Rust (egui + wgpu): two panes, tabs, fast search and rich previews, with no telemetry. Windows 10/11 is the daily driver; macOS and Linux builds come from the same code.

![Dual pane with tabs and the sidebar](docs/screenshots/2026-10-09-task5-dual-pane.png)

![PDF preview next to the file list](docs/screenshots/2026-10-09-task7-pdf-preview.png)

## What works (v0.2.0)

- **Dual pane and tabs**: two panes (Ctrl+Shift+D for one), any number of tabs per pane (drag to reorder), back/forward history, breadcrumb or editable path (Ctrl+L), details and grid views with thumbnails, sidebar with home folders and drives.
- **Search**: Ctrl+F opens a search tab. Windows uses [Everything](https://www.voidtools.com/) when it is running (optional: without it Keel works, only the search tab is unavailable and Ctrl+P walks your home folder), macOS uses Spotlight (`mdfind`), Linux uses `plocate`/`locate` when installed, otherwise a folder walk. Ctrl+Enter opens a result's folder with the file selected.
- **Fuzzy folder jump**: Ctrl+P matches against every folder Everything knows (or a walk of your home folder).
- **Command palette**: Ctrl+Shift+P lists every action by name with its shortcut.
- **Filter by typing**: start typing in a list to filter it; Esc clears.
- **Previews**: F3 shows or hides the preview panel (there is no Ctrl+Shift+V shortcut). Code with syntax highlighting, text, Markdown, images (PNG, JPEG, GIF, WebP, BMP, ICO, SVG), PDF pages (needs the pdfium library next to the binary: included in the release archives, or run `scripts/fetch-deps`), CSV/TSV and spreadsheets (xlsx, xls, xlsb, ods), Word documents (docx), video thumbnails (optional: needs `ffmpeg` on `PATH`), hex for anything else.
- **File operations**: copy, move, rename (F2), new folder/file, and delete to the OS trash (never a permanent delete), with progress, cancel, and skip/overwrite/rename prompts on name clashes. Drag and drop between panes and from other apps.
- **System clipboard**: Ctrl+C / Ctrl+X / Ctrl+V exchange files with Explorer (including cut) and Finder / file managers on Linux (copy only for now).
- **Archives as folders**: zip, 7z, tar (gz, bz2, xz, zst) and rar open like directories, nested ones too. See [Archives](#archives).
- **Embedded terminal**: a shell pane under the file panes that follows the active folder. See [Terminal](#terminal).
- **SFTP remotes**: browse, preview and copy to and from any SSH host. See [Remotes over SSH](#remotes-over-ssh).
- **Open, open with, reveal** in the system file manager, open a terminal in the current folder.
- **Themes**: dark and light (a TOML file in `<config>/themes/dark.toml` or `light.toml` overrides the built-in colours). One built-in file icon theme.
- **Settings** (Ctrl+,): theme, hidden files, dual pane, preview panel, maximum preview size. Open tabs are restored on the next start; a local folder that was deleted falls back to your home folder, while a tab on an unreachable network share stays open and shows the error. A `config.toml` or `session.json` that cannot be read is kept as `config.toml.bad` / `session.json.bad`.
- **Crash safety**: a bug inside the UI is written to `crash.log`, the panes are reset and Keel keeps running.

Settings live in `%APPDATA%\Keel` (Windows), `~/Library/Application Support/Keel` (macOS) or `~/.config/keel` (Linux); session and `crash.log` in `%LOCALAPPDATA%\Keel`, `~/Library/Caches/Keel` or `~/.cache/keel`. Set `KEEL_CONFIG_DIR` to keep settings, session and `crash.log` in one folder instead (the window size and position that eframe saves in `app.ron` stay in eframe's own folder).

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

## Roadmap

Phases 1 to 3 are released (the usable core, archives and terminal, SFTP remotes). Next:

4. Cloud storage: Google Drive, Dropbox, S3/B2.
5. Polish: profiles, icon-theme manager, Miller columns, drag-out to other apps, global hotkey, own indexer.
6. Library core: an index of every file across sources, content hashes, duplicate finder, tags and saved views.
7. Media and protection: fast photo grid, video scrubbing, EXIF, redundancy and backup state per file.
8. Devices: pairing and file transfer between your machines, a headless daemon and CLI.
9. Clients and extensions: adapters (mail attachments, notes, repositories), web and mobile clients.

Known limitations of v0.2.0 are listed in [CHANGELOG.md](CHANGELOG.md).

## Contributing

Bug reports and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Run `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --workspace` before opening a pull request.

## License

Licensed under either the MIT License or the Apache License 2.0, at your option. Third-party components: see [THIRD_PARTY.md](THIRD_PARTY.md).
