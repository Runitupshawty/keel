# Daemon, CLI, MCP and mounts

Reference for `keel-daemon`, the `keel` subcommands, the MCP server and mounts. Every operation, with schemas and examples, is in [api.md](api.md).

## Daemon, CLI and MCP

Everything the library can do is also a typed operation (54 of them: search, reading and previewing files, tags, favorites, recents, sources, jobs, duplicates, redundancy, protection, volumes, hashing, integrity and media jobs, copy, move, delete and rename plans, devices and their settings, library sync, shares, Spacedrop and mounts), reachable three ways: JSON-RPC from `keel-daemon`, `keel` subcommands, and an MCP server. The window uses the same operations when it is attached to the daemon. The reference with schemas and examples is [docs/api.md](api.md).

**The preview-first rule.** A command that would change anything only returns a preview with a plan id and an input hash. `execute` applies exactly that plan; it refuses a wrong hash, a plan older than 10 minutes, and a plan whose sources changed in the meantime. Revoking a share (`shares.revoke`), noting an opened file (`recents.note`) and noting that you are working (`activity.note`, which pauses idle jobs for 5 s, sidecar jobs for 1 s) are the only things done directly; removing a source previews too, with what its index store holds (size, tags, favorites).

**Start the daemon.** `keel daemon start` (or run `keel-daemon`) opens the profile's library in the background, resumes its jobs and listens on a per-user local socket (a named pipe on Windows) that only your user can reach; `keel daemon status` and `keel daemon stop` do what they say, and there is one daemon per profile. `keel-daemon --ws 127.0.0.1:7420` also serves a WebSocket that requires a bearer token from a file in the config folder; `keel daemon rotate-token` replaces it (clients sign in again, sessions with the old token are closed). Devices follow the window's Settings → Devices switch (`[devices] enabled` in the profile's `config.toml`); the daemon's device serves your library's sources to the devices you granted them, as the window does. It reads the inbox, always-accept list and relays (`[devices] inbox`, `auto_accept`, `relay`) when it starts; `devices.settings_set` changes the name, inbox and always-accept list while it runs (an attached window does so from Settings → Devices), relays need a restart. The daemon keeps every source current like the window: local folders are watched live, the others are asked what changed every `[library] remote_poll_secs` (read when it starts) and walked as described under [Library](library.md#library), and hashing follows each walk. The read operations never open Keel's configuration folder, and on Windows they open network (UNC) paths only inside your library sources. Without a daemon the subcommands open the library themselves, which works only while no Keel window holds it: start the daemon (or turn on Settings → Library → Run the library in a background daemon) and the window attaches to it instead, so the window, the subcommands and `keel mcp` all work at once. A daemon cannot start while a window has the library open in-process.

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

`--json` prints machine-readable output; `--profile NAME` selects a profile. Exit codes: 0 ok, 1 the operation failed, 2 usage. `keel tag`, `keel sources add|remove|index`, `keel mount` and `keel unmount` show their preview and apply it, since typing the command is the confirmation; `keel plan` stops at the preview. `keel mcp`, `keel execute`, `keel daemon` and `keel search` are always the subcommand, even in a folder with a subfolder of that name; write `keel ./mcp` to open such a folder.

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

Limits: a window that opened the library in-process keeps it until it closes or switches (Settings → Library → Run the library in a background daemon hands it over at once).

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
