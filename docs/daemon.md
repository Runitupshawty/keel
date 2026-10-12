# Daemon, CLI, MCP and mounts

Reference for `keel-daemon`, the `keel` subcommands, the MCP server and mounts. Every operation, with schemas and examples, is in [api.md](api.md).

## Daemon, CLI and MCP

Everything the library can do is also a typed operation (62 of them: search (also inside documents), searching every file on the computer, reading and previewing files, tags, favorites, recents, folder colors and icons, sources, jobs, duplicates, redundancy, protection, volumes, hashing, integrity, media and text jobs, copy, move, delete and rename plans, library backup and restore, devices and their settings, library sync, shares, Spacedrop and mounts), reachable three ways: JSON-RPC from `keel-daemon`, `keel` subcommands, and an MCP server. The window uses the same operations when it is attached to the daemon. The reference with schemas and examples is [docs/api.md](api.md).

**The preview-first rule.** A command that would change anything only returns a preview with a plan id and an input hash. `execute` applies exactly that plan; it refuses a wrong hash, a plan older than 10 minutes, and a plan whose sources changed in the meantime. Revoking a share (`shares.revoke`), noting an opened file (`recents.note`) and noting that you are working (`activity.note`, which pauses idle jobs, the text job among them, for 5 s, sidecar jobs for 1 s) are the only things done directly; removing a source previews too, with what its index store holds (size, tags, favorites).

**Start the daemon.** `keel daemon start` (or run `keel-daemon`) opens the profile's library in the background, resumes its jobs and listens on a per-user local socket (a named pipe on Windows) that only your user can reach; `keel daemon status` and `keel daemon stop` do what they say, and there is one daemon per profile. `keel-daemon --ws 127.0.0.1:7420` also serves a WebSocket that requires a bearer token from a file in the config folder; `keel daemon rotate-token` replaces it (clients sign in again, sessions with the old token are closed). Devices follow the window's Settings → Devices switch (`[devices] enabled` in the profile's `config.toml`); the daemon's device serves your library's sources to the devices you granted them, as the window does. It reads the inbox, always-accept list and relays (`[devices] inbox`, `auto_accept`, `relay`) when it starts; `devices.settings_set` changes the name, inbox and always-accept list while it runs (an attached window does so from Settings → Devices), relays need a restart. The daemon keeps every source current like the window: local folders are watched live, the others are asked what changed every `[library] remote_poll_secs` (read when it starts) and walked as described under [Library](library.md#library), and hashing and the text job (which reads the text inside documents, [Searching](library.md#searching)) follow each walk. The read operations never open Keel's configuration folder, and on Windows they open network (UNC) paths only inside your library sources. Without a daemon the subcommands open the library themselves, which works only while no Keel window holds it: start the daemon (or turn on Settings → Library → Run the library in a background daemon) and the window attaches to it instead, so the window, the subcommands and `keel mcp` all work at once. A subcommand without a daemon ends when its own work does: a job it started that is still running (the text job after `keel sources index`, hashing) stops with it, is named on stderr, and resumes when Keel or the daemon next opens the library. A daemon cannot start while a window has the library open in-process.

**Search all files over HTTP.** `keel-daemon --web` also answers searches of every file on the computer, not only the library: the web client's **All files** page (on a phone: Search, then All files) and, for tools written for Everything's HTTP server, `http://127.0.0.1:7421/everything?search=...` (or `/?search=...`) with Everything's parameters (`search`/`s`/`q`, `offset`/`o`, `count`/`c`, `json`/`j`, `case`/`i`, `diacritics`/`m`, `wholeword`/`w`, `path`/`p`, `regex`/`r`, `prefix`, `suffix`, `ignore_punctuation`, `ignore_whitespace`, `sort` = `name`, `path`, `date_modified` or `size`, `ascending`, `path_column`, `size_column`, `date_modified_column`). `json=1` answers in Everything's JSON, anything else is a plain HTML page with a search form. The search is the window's own search pane backend (Everything when it runs, else Keel's index on Windows; Spotlight or locate elsewhere), so it finds what the pane finds. Every request must sign in: the daemon token as `Authorization: Bearer <token>` (or as the password of Basic auth, with any user name), or an HTTP login for tools that only know Basic auth, which `keel-daemon --set-http-login USER` stores in the OS keychain (the password, at least 10 characters, is read from standard input; `--clear-http-login` removes it). An address that sends five wrong logins in a row is locked out (answered 429 with `Retry-After`, without checking anything it sends) for 5 seconds, doubling with each further failure up to 15 minutes; a request another site's page makes (`Sec-Fetch-Site: cross-site`) is refused. A page shows at most 1000 results (32 when `count` is not given); the total counts the first 10000 the backend finds. Result names link to their files only while `[web] allow_downloads = true` is in the profile's `config.toml` (off by default, read on every request); the download goes through the same checks as `file.get`, so Keel's configuration folder stays out of reach, and a download signed in with the HTTP login (not the token) may only name a file a recent search listed and never one in Keel's configuration or data folder. `--web` binds to loopback unless you add `--ws-allow-remote`. It has no TLS, so keel-daemon refuses a non-loopback address unless the profile's `config.toml` has `[web]` `allow_plaintext = true` (you accept plain http on a private network such as a tailnet); then it still prints a warning at start, the HTML page says "Not encrypted", and Basic auth (the HTTP login, or the token as a Basic password) is refused there: send `Authorization: Bearer <token>`. To use the HTTP login from a tool that only knows Basic auth, keep `--web` on loopback and put a TLS proxy in front. Differences from Everything: the backend matches names, so with Keel's own index or locate a word with `path=1` still has to be in the name unless it holds a `\` or `/` (a regular expression with `path=1` is matched against the whole path, after the backend has returned its first 10000 names); diacritics are folded for Latin letters only; `ignore_punctuation` and `ignore_whitespace` take plain words, not search functions; folder sizes are left empty. The same search is the `system.search` operation ([api.md](api.md)).

**Its log.** A daemon started by Keel writes to `daemon.log` in the configuration folder; `RUST_LOG` sets the level (default `info`). Whatever `RUST_LOG` says, the FTP library's own log is off (at trace level it records every command sent, the password included) and the S3 signing library logs warnings only.

**One-liners.**

```sh
keel search "invoice ext:pdf" --max 20
keel du ~/Photos
keel du --scan ~/Projects /mnt/data --top 10 --save projects.keeldu
keel du --scan /mnt/data --report data.html --save-types-to types.csv --quiet --strict
keel tag add receipts ~/Docs/invoice-2026.pdf
keel plan move ~/Downloads/old.iso --to /mnt/archive | keel execute
keel plan delete ~/old-photos --json
keel devices
keel shares
keel sources add ~/Photos --label photos
keel library backup /media/backup
keel library restore /media/backup/main-2026-10-11.keelbackup --as old-main
keel daemon status
keel op list
keel op library.stats
keel op tags.set --params '{"paths": ["/home/me/Docs/invoice-2026.pdf"], "tags": ["receipts"]}'
```

`--json` prints machine-readable output; `--profile NAME` selects a profile. Exit codes: 0 ok, 1 the operation failed, 2 usage (and 3 for `keel du --strict` when the scan is partial). `keel tag`, `keel sources add|remove|index`, `keel library backup|restore`, `keel mount` and `keel unmount` show their preview and apply it, since typing the command is the confirmation; `keel plan` stops at the preview. `keel op <name> --params '<json>'` runs any operation of the [API](api.md#keel-op-any-operation-by-name): one that changes something stops at its preview unless `--execute` is given. `keel du <path>` lists a folder's children by size, biggest first, with the folder's total last: from the library index inside a source (`library.folder_sizes`, the 1000 biggest), else by walking the folder (a child folder that cannot be read shows `?`); `keel du --scan <paths…>` runs the disk usage scanner on several folders or drives at once instead, with size on disk and hard links counted once, and writes CSV and JSON exports and an HTML report for unattended runs ([Disk usage scans](files.md#disk-usage-scans)). `keel mcp`, `keel execute`, `keel daemon` and `keel search` are always the subcommand, even in a folder with a subfolder of that name; write `keel ./mcp` to open such a folder.

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

**Backup and restore.** `keel library backup <file or folder>` writes the library to a `.keelbackup` file (in a folder, as `<library>-<date>.keelbackup`; never over an existing file) and `keel library restore <file> [--as <name>]` restores one as a new library, never over an existing one (both run within their request, so the daemon and `keel` wait up to 6 hours for them instead of 2 minutes) ([Backup and restore](library.md#backup-and-restore)). With a daemon running both run in the daemon, which holds the databases; the file is read or written on the daemon's machine at the path given (local paths only). Without one, `restore` writes straight into the data folder without opening a library, so it works on a machine where Keel has never run.

Limits: a window that opened the library in-process keeps it until it closes or switches (Settings → Library → Run the library in a background daemon hands it over at once).

## ETP/FTP server for Everything clients

keel-daemon can answer Everything's ETP clients (Everything's **Connect to ETP server**, and any FTP client): an FTP server with Everything's `SITE EVERYTHING` search extension, searching this computer with the window's search backend (Everything, Keel's index, Spotlight or locate) and serving only the folders you list. It is off by default, read-only, and stricter than Everything's own server:

- it binds to loopback (`127.0.0.1:21` on Windows, `127.0.0.1:2121` elsewhere) unless `[etp] bind` or `--etp ADDR` says otherwise;
- it always needs a user and password (Everything's accepts anyone when they are empty), kept in the OS keychain: `keel-daemon --set-etp-login USER` reads the password from the first line of standard input (at least 10 characters), `--clear-etp-login` removes it. Three failed sign-ins close the connection, each after a pause;
- nothing outside the listed folders can be named, found or read: each folder is a top folder under its own name (SUBST-style: `/Media/...`; a root named `C:` for `C:\` gives Everything's own layout), `..` never climbs above `/`, and a link inside a folder that points out of it is refused. Search results outside the folders are never sent;
- no command changes anything (`STOR`, `DELE`, `MKD`, `RNFR`... are refused), and files are sent (`RETR`) only while `[etp] allow_downloads = true`; browsing (`LIST`, `MLSD`, `SIZE`, `MDTM`) and searching work either way. The switch is read for every download, so turning it off applies at once;
- passive mode only (`PASV`, `EPSV`), and a data connection is accepted only from the control connection's address;
- explicit FTPS (`AUTH TLS`, `PBSZ 0`, `PROT P`) with your certificate: set `tls_cert` and `tls_key` (PEM files; a certificate from your own CA, `mkcert` or `openssl` works, Keel does not make one). With TLS configured, sign-in and transfers need it (`require_tls`, on by default; Everything's own client cannot use TLS, so turn it off for Everything clients on a trusted network);
- a network address without required TLS is refused unless `allow_unencrypted = true` accepts that the user name, password, searches and files cross the network readable; the daemon then logs a warning at start.

In the profile's `config.toml`:

```toml
[etp]
enabled = true                 # start with the daemon (or run keel-daemon --etp)
bind = "127.0.0.1:2121"
welcome = "Keel files"         # several lines allowed
allow_downloads = false
log = true                     # every command (passwords hidden) at info level
# tls_cert = "etp-cert.pem"    # relative to the profile folder
# tls_key = "etp-key.pem"
# require_tls = true
# allow_unencrypted = false

[[etp.roots]]
path = 'D:\Media'
name = "Media"                 # default: the folder's name, or C: for a drive

[[etp.roots]]
path = 'C:\Users\you\Documents'
```

Then `keel-daemon --etp` (or with `enabled = true`, just `keel-daemon`); `--etp 127.0.0.1:2121` overrides `bind`. At most 32 connections at once, 8 from one remote address. The search behaves as `system.search`: Everything's match switches (`CASE`, `WHOLE_WORD`, `PATH`, `DIACRITICS`, `PREFIX`, `SUFFIX`, `IGNORE_PUNCTUATION`, `IGNORE_WHITESPACE`, `REGEX`), `FILTER_SEARCH` and its switches, `SORT` (name, path, size and date modified; extension, attributes and the others sort by name, date created by date modified), `OFFSET`, `COUNT` (at most 1000 per query) and the `PATH`, `SIZE`, `DATE_MODIFIED`, `DATE_CREATED` and `ATTRIBUTES` columns. At most 10,000 hits are read from the backend per search, before the folders are applied (with Everything as the backend the folders go into its search, so they are all its hits); `RESULT_COUNT` counts those. `PATH` matching sees the folder's real path. On Linux and macOS, names holding `\` are left out of results and listings (Everything's paths use it as the separator).

### Searching an ETP server

`keel search QUERY --etp [USER@]HOST[:PORT]` searches an ETP server, Everything's or Keel's, instead of the library, and prints `ftp://` paths (`--json` for JSON). Without a server it uses the one in the profile's `config.toml`:

```toml
[search]
etp = "you@nas.example:2121"
etp_tls = true                 # AUTH TLS, never falling back to plain FTP
# etp_pin = "AB:CD:..."        # SHA-256 fingerprint of a certificate the OS does not trust
# etp_remote = "nas"           # the FTP remote (id or label) the results open through
```

The password comes from `KEEL_ETP_PASSWORD`, or from the keychain of the FTP or FTPS remote with the same host and port (or `etp_remote`), which also lends its user name and TLS setting and opens the results (`ftp://<remote id>/...`). An FTPS server's certificate is checked against the OS trust store or the fingerprint pinned for that host (`ftps_known_hosts`, as for FTPS remotes, or `etp_pin`); an untrusted one is refused with its fingerprint. A plain FTP server without the `EVERYTHING` feature is refused. `keel_search::etp::EtpSearcher` is the same backend for code. The search window cannot use it yet.

## Mounts

keel-daemon can serve a library source, or a folder in it, as a drive letter or mount folder that any program can open:

```
keel daemon start
keel mount Photos K: --subtree 2026     # Windows: a drive letter, or a folder that does not exist yet
keel mount Photos ~/mnt/photos          # Linux, macOS: an empty folder (made when missing)
keel mounts
keel unmount K:
```

The source is given by id or label (`keel sources`). Mounts belong to the daemon: they last until `keel unmount` or until the daemon stops (which unmounts them). The JSON-RPC and MCP operations are `mounts.list`, `mounts.add` and `mounts.remove` ([docs/api.md](api.md)).

| | Windows | Linux | macOS |
| --- | --- | --- | --- |
| Backend | WinFsp | FUSE (through `fusermount3` or `fusermount`; no libfuse needed) | macFUSE |
| In the release build | no: build from source (`--features winfsp`, GPL-3.0) | yes (tarball and .deb) | no: build it yourself |
| Driver to install | [WinFsp](https://winfsp.dev) | `fuse3` (`sudo apt install fuse3`; the .deb recommends it) | [macFUSE](https://macfuse.github.io) |
| Build `keel-daemon` from source with | `--features winfsp` (needs LLVM/libclang for bindgen: `scripts/libclang.ps1`) | `--features fuse` | `--features fuse` (needs macFUSE and `pkg-config` at build time) |
| Target | `K:` or a new folder | empty folder (made when missing) | empty folder (made when missing) |

The Linux release build of `keel-daemon` includes the FUSE backend; install `fuse3` to mount. The Windows and macOS builds include no backend: winfsp-rs is GPL-3.0, so the shipped daemon stays MIT/Apache (build `keel-daemon` from source with `--features winfsp`, which needs the WinFsp driver to mount), and macFUSE cannot be installed on the build machines. Without the driver or the backend, `keel mount` fails with error -32008 and says what to install or build.

What a mount does:

- **Listings** come from the source while it is online and from the library index while it is offline, so an unplugged drive or an unreachable server still shows its folders (files cannot be opened or changed until it is back).
- **Reads** are on demand: a program reading part of a file reads that range through Keel's VFS (local, SFTP, cloud), nothing is downloaded up front.
- **Writes** go to a `.keel-partial-…` staging file next to the target (for remote sources, a local spool uploaded on close) and replace the file atomically when the program closes it (before its `close` returns), so other programs never see a half-written file and an aborted or interrupted write leaves the old file (or none) in place. While a file is being written only the program writing it sees the new content: listings show the saved file, opening it elsewhere fails with a sharing violation (`EBUSY` on Linux and macOS) until it is closed, and a file being created does not show yet. If publishing fails, the program's close reports an error and the data is kept as `<name> (unsaved <date>).<ext>` (next to the file, or in `mount-spool` under the data folder for remote sources), a name Keel never cleans up; the daemon log says where. Unmounting drops writes still open (the files stay as they were).
- **Renames** of a file being written (or of a folder holding one, or a move to another folder) take effect at once: the write is published under the new name when the program closes the file. A file being created is not on the source until then, so renaming it only moves its write (over an existing file, that file is replaced when the new one is published). A file being written is never replaced by a rename.
- **Deletes** go to the trash for local sources, like Keel's own delete; on SFTP and S3 they are permanent (the `mounts.add` preview says so).
- **Attributes**: files and folders show the source's modified time (also as the access and change time); entries without one (S3 folders) take the time from the library index. While a file is being written, the system sees its written length and the time of its last write. Files of a read-only source show as read-only (`r--`, the Windows read-only attribute) and changes fail with "read-only file system" (`EROFS`). `df` and the drive's properties show the free and total space of the source's volume: the local disk, the SFTP server's filesystem (servers with the `statvfs@openssh.com` extension, as OpenSSH has), the cloud account's quota. Where Keel cannot tell, Linux and macOS show 0 and Windows a large placeholder (Explorer refuses to copy onto a drive with no free space).

Limits: file times, attributes and permissions are the source's and cannot be changed through the mount; a mount folder inside the folder it shows is refused; staging files and (on Windows) names Windows cannot show (`aux.txt`, `a:b`, names differing only in case on a case-sensitive source) are hidden; Windows refuses to rename a folder holding a file being written on a local source, as it does for any open file; on Windows only the current user, SYSTEM and Administrators can open the drive. SFTP and cloud sources mount the same way, through the profile's remotes and cloud accounts that keel-daemon registers.

## Updating with a daemon running

The install scripts stop every daemon of the install before the files change and start the default profile's again afterwards, with no arguments. A daemon you run with arguments (`--web`, a profile) from a scheduled task or a service should be restarted by that task. On Windows the restarted daemon inherits the installer's standard output, so a script that pipes `install.ps1` waits until that daemon exits: redirect the output to a file instead.
