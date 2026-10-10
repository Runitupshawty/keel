# Keel API: JSON-RPC, CLI and MCP

Everything Keel's library can do from outside the window goes through one list of typed
operations (`keel_api::OPS`, crate `keel-api`). The same list serves three front ends:

| Front end | How to reach it | Transport |
| --- | --- | --- |
| JSON-RPC 2.0 | `keel-daemon` (per user, per profile) | local socket / named pipe; optional WebSocket |
| CLI | `keel search …`, `keel plan …`, … | the daemon when it runs, else the library in-process |
| MCP | `keel mcp` | stdio (Model Context Protocol 2025-06-18) |

Every operation has a stable name, JSON schemas for its parameters and result (generated
from the Rust types with `schemars`), and an example. Paths are strings: an absolute local
path (`D:\Photos\a.jpg`, `/home/me/a.jpg`, `D:\x.zip!/inside.txt`) or a VPath URI
(`library://<source id>/<path>`, `sftp://<host id>/<path>`, `cloud://<account id>/<path>`). Relative paths are refused: the
daemon's working folder is not the caller's. The read operations (`list`, `stat`, `read`,
`preview.render`, `media.thumb`, `file.get`) refuse anything inside Keel's configuration
folder (the daemon token, keys; links, `..` and case are resolved first, library paths
included) and, on Windows, UNC and device paths (`\\server\share`, `\\?\UNC\...`,
`\\.\...`) outside the library's sources, since opening one sends the user's
credentials to that server.

## Preview first

Nothing changes without a preview. A **mutating operation called directly only previews**:
it validates its input and returns a `PlanPreview`:

```json
{
  "plan_id": "5247b6c49c4b619d0012fe5fe04ea6db",
  "input_hash": "9c9fc3349e6b2e93b6a9cdebf373ecf2b2821f2992b2e541e8742788031166bf",
  "operation": "plan",
  "summary": "Copy 1 item(s) (D:\\src\\invoice.txt), 1 file(s), 6 bytes to D:\\dst",
  "changes": [{"action": "copy", "path": "D:\\src\\invoice.txt", "to": "D:\\dst\\invoice.txt", "files": 1, "bytes": 6}],
  "warnings": [{"kind": "not_indexed", "path": "D:\\src\\invoice.txt", "message": "…"}],
  "expires_at": 1791589924
}
```

`execute` with that `plan_id` **and** `input_hash` applies exactly the previewed input.
It refuses (typed errors below) when the hash is not the previewed one, when the plan
expired (10 minutes) or was already executed, and, for file operations, when the sources
changed since the preview (the error's `data` is the fresh preview to confirm instead).
File operations run as jobs: `execute` returns `{"job": N}`; follow it with `jobs.info`.

File-plan summaries name the first three paths (`Delete 5 item(s) (D:\a, D:\b, D:\c, 2
more), …`). Only `shares.revoke` (taking access away) acts directly; it returns what it
revoked. `recents.note` (a file was opened) also acts directly: it changes no file.

## Operations

| Operation | Kind | What it does |
| --- | --- | --- |
| `version` | read | Keel version, API revision, open library, pid of the host |
| `sources.list` | read | Library sources with status (online / indexing / offline / error) |
| `sources.add` | preview | Add a folder, drive or share as a source |
| `sources.remove` | preview | Forget a source (files untouched; `delete_store` also deletes its index store): the preview states the store's size, tags and favorites (a `deletes_store` warning with `delete_store`) |
| `sources.index` | preview | Index a source (a job); `adopt: true` first accepts whatever folder is at its root now, for a source held offline because a different folder (or nothing) is there (an `adopts_root` warning) |
| `list` | read | List a folder: `library://` paths from the index (offline too), others live |
| `stat` | read | One entry, plus its library record, tags and favorite state |
| `read` | read | A byte range of a file (`path`, `offset`, `len` up to 4 MiB), base64; library paths read the real file. SFTP, cloud and device files are read from the offset (a ranged request); elsewhere (inside archives) an offset past 64 MiB is refused |
| `preview.render` | read | The desktop previewers' output: `kind` `text` (code, Markdown, tables as TSV, documents, hex) or `image` (a PNG of at most `max_px` 64..2048, default 1024: images, a PDF `page`, a video frame), or `none` with a `message`; `content_id` when indexed (a cache key) |
| `media.thumb` | read | A photo or video thumbnail (`size` `thumb256` / `thumb1024`, WebP) from the sidecar store, keyed by content id when indexed; made on first request, served from earlier sidecars when the source is offline |
| `file.get` | read | A one-time download link: `{url: "/file/<token>", name, size, expires_at}` on the `--web` address, valid once for 60 s |
| `search` | read | Library search: words, `"phrases"`, `kind:`, `ext:`, `size:`, `dm:`, `source:`, `tag:` |
| `tags.list` | read | All tags, or the tags on one path |
| `tags.tagged` | read | Every tagged path with its tag ids (Favorites is id 1) |
| `views.list` | read | Saved views: `id`, `name`, `query` (a `search` query), `layout` |
| `tags.add` / `tags.remove` | preview | Tag / untag indexed paths (`tags.add` creates a missing tag, with `color` when given) |
| `tags.set` | preview | Exactly these tags on the paths (Favorites kept) |
| `favorites.list` | read | Favorites |
| `favorites.set` | preview | Add to (or with `on: false` remove from) Favorites |
| `recents` | read | Recently opened files |
| `recents.note` | direct | Note that an indexed file was opened (it moves to the top of `recents`) |
| `jobs.list` / `jobs.info` | read | Jobs; one job with its log |
| `jobs.cancel` | preview | Cancel a job |
| `duplicates` | read | Same-content groups, most wasted bytes first |
| `redundancy` | read | How many copies of a file's content exist, and in which sources; each location has its volume's `state`, `backup` mark and whether it is only a device's `claimed` copy (never counted) |
| `redundancy.folder` | read | `redundancy` for every indexed file in a folder (the first 5000): `[{path, copies}]` |
| `library.stats` | read | Counts over every source: `sources`, `offline_sources`, `records`, `files`, `bytes`, `unique_content`, `running_jobs` |
| `protection.summary` | read | The protection card: `single_copy`, `single_domain`, `unbacked`, `drifted`, `unchecked` (not hashed yet, in none of the other counts), `offline_volumes` |
| `volumes.list` | read | The drive inventory: each volume's `id`, `label`, `kind`, `failure_domain` (`domain_set` when set by hand), `state`, `last_seen`, `backup`, `used` / `total` |
| `volumes.set` | preview | Set a volume's `state` (`archived` / `lost` / `retired`; `online` makes it automatic again), `backup` mark or `failure_domain` (`""`: the detected one) |
| `integrity.check` | preview | Re-hash `sample_pct` % (default 1) of the hashed files of one `source` or all, as a job; with `due_days`, only when the last check of every source is that old (the first call starts the clock; `job` absent when nothing is due) |
| `hashing.set` | preview | Content hashing after walks `on` / off (`idle_only`: pause while the user works); on returns the hash `job`, off cancels a running one |
| `media.index` | preview | Thumbnails and metadata for a source's photos and videos, as an idle-priority job |
| `plan` | preview | Preview copy / move / delete / rename (`op`, `paths`, `to`, `new_name`, `on_conflict`) |
| `execute` | direct | Apply a preview (`plan_id`, `input_hash`); `job` names the job when the operation runs as one (file plans, `sources.index`, `spacedrop.send`) |
| `devices.list` | read | This device and paired devices (LAN / relay / offline) |
| `devices.pair_code` | preview | One-time pairing code (10 minutes) |
| `devices.pair_with` | preview | Pair with a device's code (grants nothing) |
| `devices.forget` | preview | Forget a device and its grants |
| `shares.list` | read | Grants to paired devices |
| `shares.grant` | preview | Give a device read or read-write access to a source or subtree |
| `shares.revoke` | direct | Revoke a grant at once |
| `mounts.list` | read | Sources keel-daemon serves as drives or mount folders |
| `mounts.add` | preview | Mount a source or a subtree (`source`, `subtree`, `target`: `K:` or a folder) |
| `mounts.remove` | preview | Unmount (`target`); writes still in progress there are discarded |
| `spacedrop.send` | preview | Send files or folders on the host's machine to a paired device (`peer`, `paths`) as a job; the preview lists every file with its size (first 500) and warns when the device is offline. Only paths in a library source, the Spacedrop inbox or a claimed share upload are sent; Keel's configuration and data folders (and folders holding them) are refused, for the paths given and every file reached; links inside folders are skipped. `execute` sends exactly the previewed files: a folder that changed since the preview gives -32004 with the new preview |
| `spacedrop.inbox` | read | The host's Spacedrop inbox: offers waiting for an answer (`pending`: device, id, file count, bytes, first names) and what arrived (`entries`, newest first; download with `file.get`) |
| `spacedrop.answer` | preview | Accept or decline a waiting offer (`peer`, `id`, `accept`) |

Devices and shares need keel-net: the window's Settings → Devices switch, written as
`[devices] enabled = true` with `explicit = true` in the profile's `config.toml`
(`<config dir>/profiles/<profile>/config.toml`); `[net] enabled = true` is read when
`[devices]` has no explicit value. Otherwise they fail with `NET_DISABLED`. Spacedrops
sent to a host (the daemon, or a CLI session holding the node) land in `[devices] inbox`
(default `<data dir>/inbox`); offers from the device ids in `[devices] auto_accept` are
accepted at once, the others wait in `spacedrop.inbox` for `spacedrop.answer` and are
declined when the sender stops waiting. A
configuration saved while Devices defaulted on (`enabled = true` without `explicit`) is
treated as off, and the window switches it off once. `KEEL_NET_SECRET=memory` keeps the
device identity in memory instead of the OS keychain (tests). keel-daemon owns the profile's one node while it runs; without it, a CLI
or MCP session brings the node online itself, only for its first device or share call.

Both hosts (the daemon and the in-process CLI) register the profile's SFTP hosts
(`[[remotes]]`) and cloud accounts (`[[clouds]]`) from that `config.toml`, with their
secrets from the OS keychain, so `sftp://<host id>/…` and `cloud://<account id>/…`
paths work and Share / Cloud sources can be listed and indexed. A host key that is not
trusted yet is refused (there is no one to ask): connect once from the Keel window to
trust it. Files extracted from archives go to `<data dir>/archives` (never the app's own
cache). Other caches and temp folders (RAR extraction, SFTP, cloud and device downloads)
live under `<KEEL_DATA_DIR>/cache` when that is set (`keel_vfs::cache_dir`). The data
folder is created owner-only when the host creates it. A host's node serves the
library's sources to the devices granted them (as the window does), and `node://<device
id>/` paths (`list`, `stat`, `read`) browse what paired devices share with this one.
keel-daemon also keeps every source current: each one is watched (local folders live,
the others re-walked periodically; a completed walk schedules hashing) once no index job
walks it.

Mounts live in keel-daemon (a mount made by an in-process CLI call would vanish when the
command exits) and need a daemon built with a mount backend (`--features winfsp` on
Windows, `--features fuse` on Linux and macOS; see the README's Mounts section);
otherwise `mounts.add` fails with `MOUNTS_UNAVAILABLE` (-32008). The `mounts.add` preview checks the
target is free and the source or subtree can be listed, and warns when the source is
offline (`source_offline`: the mount then lists it from the index and cannot read or change files)
or deletes are permanent there (`deletes_permanent`: SFTP, S3). The `mounts.remove`
preview warns when files are still being written through the mount (`discards_writes`).
A target inside the folder the mount shows is refused. While a file is written through a
mount, only its writer sees the new content (other opens are busy, listings show the saved
file); it is published when the writer closes it, and a failed publish keeps the data as
`<name> (unsaved <date>).<ext>`. Mounts are unmounted when the daemon stops (writes still
open are dropped).

The full schemas: `keel mcp` → `tools/list`, or `keel_api::OPS[i].params()` /
`.result()` in Rust.

### Errors

`{"code", "message", "data"?}`, JSON-RPC codes:

| Code | Meaning |
| --- | --- |
| -32700 / -32600 / -32601 / -32602 / -32603 | parse error / invalid request (also over 16 MiB) / no such method / bad params / internal |
| -32000 | the operation failed (I/O, locked library, refused request) |
| -32001 | not found (path, source, tag, job, peer) |
| -32002 | no such plan, already executed, or expired |
| -32003 | `input_hash` is not the previewed input's |
| -32004 | the sources (or the files a drop sends) changed since the preview; `data` is the new preview |
| -32005 | devices are off (keel-net disabled) |
| -32006 | the request timed out (the daemon answers within 120 s) |
| -32007 | `--web`: a message before `auth`, a wrong token or no `auth` within 10 s; `--ws`/`--web`: the token was rotated (the connection closes) |
| -32008 | mounts unavailable: not served by this host (no daemon) or no mount backend built in |

## keel-daemon (JSON-RPC)

```
keel-daemon [--profile NAME] [--ws 127.0.0.1:PORT] [--web [127.0.0.1:PORT]] [--ws-allow-remote]
            [--web-host NAME]...  # NAME: letters, digits, . _ -
keel-daemon --status            # exit 0 when one runs for the profile, 1 when not
keel daemon start|stop|status   # the same from keel (start runs it in the background)
keel daemon rotate-token        # a new token: clients sign in again
```

The daemon opens the profile's library (`[library] name` in the profile's config.toml,
default `james`, under `KEEL_DATA_DIR` or the platform data folder), resumes its jobs and
serves until Ctrl-C, SIGTERM or `daemon.shutdown`. It survives clients disconnecting; a
second daemon for the same profile exits with "keel-daemon is already running for
profile …". One process holds a library: while a Keel window has it open in-process,
neither the daemon nor in-process CLI calls can open it. A window that finds the
profile's daemon running when it opens the library attaches to it instead (a client
like any other, using these operations and `subscribe`), and with Settings → Library →
"Run the library in a background daemon" it starts the daemon itself; then the window,
the CLI and MCP all work at once, and closing the window leaves the daemon running.

**Local socket.** Named `keel-daemon-<hash of the user's SID or uid, a per-user random
salt, the profile and the data folder>` (the salt is `<config dir>/socket.salt`, created
owner-only on first use, so other local users cannot predict the name and take it
first): a named pipe on Windows whose DACL admits only the current user (clients refuse impersonation and
check the server runs as the same user), a socket in a 0700 folder
(`$XDG_RUNTIME_DIR/keel-<uid>/`) on Unix with peer-uid checks on both ends. Framing: one
JSON-RPC message per line (UTF-8, `\n`), at most 16 MiB; batches allowed. Once a request
has started all of it must arrive within 30 s (however slowly it trickles in); idle
connections may stay open.

```
→ {"jsonrpc":"2.0","id":1,"method":"search","params":{"query":"invoice ext:pdf","max":5}}
← {"jsonrpc":"2.0","id":1,"result":[{"path":"D:\\Docs\\invoice-2026.pdf","name":"invoice-2026.pdf","size":48213,...}]}
→ {"jsonrpc":"2.0","id":2,"method":"tags.add","params":{"tag":"receipts","paths":["D:\\Docs\\invoice-2026.pdf"]}}
← {"jsonrpc":"2.0","id":2,"result":{"plan_id":"…","input_hash":"…","operation":"tags.add","summary":"Tag 1 item(s) with receipts",...}}
→ {"jsonrpc":"2.0","id":3,"method":"execute","params":{"plan_id":"…","input_hash":"…"}}
← {"jsonrpc":"2.0","id":3,"result":{"plan_id":"…","operation":"tags.add","result":{"records":1}}}
```

Besides the operations: `subscribe` (then notifications `job.progress`
`{id, status, progress}`, `library.changed` `{method}`, `net.event` and, when the daemon
stops, `daemon.stopping` `{pid}` arrive on that connection), `unsubscribe`,
`daemon.shutdown`, and `share.claim` `{id}` (the web client's share target, below).

**WebSocket (optional).** `--ws 127.0.0.1:7420` serves the same JSON-RPC, one message per
text frame. Every connection must send `Authorization: Bearer <token>`, the token in
`<config dir>/daemon.token` (created on first use, owner-only: a protected DACL for the
user alone on Windows, mode 0600 on Unix). A token file anybody else may read or change
is replaced with a new token at start, since it may have been read or planted. The daemon
reads the file for each connection: `keel daemon rotate-token` writes a new token, new
connections need it, and connections signed in with the old one get -32007 and are
closed (within a few seconds). Browsers
cannot set that header, so web pages cannot connect. The handshake (token check
included) must be over within 5 s, and at most 64 connections are served at once; more
are closed at once. Non-loopback addresses need
`--ws-allow-remote`; there is no TLS, so put a remote bind behind a TLS proxy or a
private network.

**Web client (`--web`, optional).** `--web` (default `127.0.0.1:7421`) serves the
browser client (crate keel-web, embedded at build time; README "Web client") on one
listener:

| Path | What |
| --- | --- |
| `/`, `/<file>` | the client bundle (a page saying how to build it when keel-daemon was built without one) |
| `/rpc` | JSON-RPC over a WebSocket, one message per text frame |
| `/file/<token>` | the download a `file.get` link names, once, within 60 s (`Content-Disposition: attachment`) |
| `/manifest.webmanifest`, `/sw.js`, `/icon-192.png`, `/icon-512.png` | the installable app (PWA): manifest with the share target, and a service worker that caches the app shell only (never `/rpc`, `/file/`, `/share` or other answers), fetches it from the daemon first and uses the cache only offline; its cache is named after a hash of every shell file of the build; served even by a daemon built without the client |
| `POST /share` | the Web Share Target ("Share → Keel" on a phone), `multipart/form-data` |

Browsers cannot send an `Authorization` header on a WebSocket, so on `/rpc` the **first
message must be `auth`** with the daemon token:

```
→ {"jsonrpc":"2.0","id":0,"method":"auth","params":{"token":"<daemon.token>"}}
← {"jsonrpc":"2.0","id":0,"result":{"ok":true}}
```

Anything else first, a wrong token, or no `auth` within 10 s gets error -32007 and the
connection closes (a native client may still send the `Authorization: Bearer` header
instead). Until `auth` succeeds a message (and frame) may be at most 4 KiB; a bigger one
closes the connection. The token never travels in a URL: `/rpc` with a query string is refused (400).
A WebSocket whose `Origin` is not this host is refused (403); on a loopback bind the
`Host` header must be `localhost`, `127.0.0.1` or `[::1]`, on a remote bind the bound
IP, and on either a name given with `--web-host` (a tailnet name, or the name a TLS
reverse proxy passes on; 403 otherwise, against DNS rebinding). Only `GET` is served, and `POST /share`. Every answer carries `Cache-Control: no-store`,
`Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`
and `Content-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval';
connect-src 'self' ws://<host> wss://<host>; ...; manifest-src 'self'; worker-src 'self'; ...`. As on `--ws`, the request head
(the WebSocket handshake on `/rpc`) must be in within 5 s, at most 64 connections are
served at once (more are closed at once), and the `auth` rule above follows the
handshake. Of the 64, at most 16 may be connections that have not signed in (or are
fetching a page), and at most 8 may come from one remote address (loopback is not
limited this way), so held connections cannot lock the user out. Non-loopback addresses need `--ws-allow-remote`, exactly as `--ws`; there is
no TLS (a tailnet, or a TLS proxy).

**Share target (`POST /share`).** The browser makes this request itself when the user
shares to the installed app, so it cannot carry the token. The daemon therefore only parks
the files: it refuses a request whose `Origin` is another site's or whose
`Sec-Fetch-Site` is `cross-site` / `same-site` (403), one without `Content-Length` (411),
over 512 MiB (413, before reading the body), not `multipart/form-data` (415), or while 4
shares already wait (429). Otherwise it writes the files (at most 100; names reduced to a
plain, portable file name: bidi controls dropped, Windows device names prefixed with `_`,
at most 240 bytes) to `<data dir>/shares/<id>/` and answers `303 See Other` to
`/?share=<id>&files=<n>&bytes=<total>` (the id is 24 hex digits; the counts are only for
the client's question). The body must keep coming: under 64 KiB/s over any 30 s it is cut
off (400), and an upload that makes no progress for 5 minutes is dropped. Nothing else
happens until the client asks the user "Open N shared files?" and, on yes, a client signed
in over `/rpc` calls `share.claim` `{id}`, which works once and only within 5 minutes; it
returns `{id, dir, files: [{name, path, size}]}`, and the client sends those paths with
`spacedrop.send` (previewed) to the device the user picks. An upload not claimed in time
is deleted; a claimed one after 24 hours (its drop reads the files), also when it was
claimed before a restart; unclaimed uploads left by an earlier run are deleted at start.

## CLI

```
keel search <query> [--max N] [--json]
keel tag add|remove <tag> <paths…>
keel plan copy|move <src…> --to <dir> [--on-conflict skip|overwrite|rename] [--json]
keel plan delete <paths…> [--json]
keel execute [<plan id> --hash <hash>] [--no-wait]   # or: keel plan … | keel execute
keel sources [add <path> [--label L] [--no-index] | remove <id> [--delete-store] | index <id>]
keel devices | keel shares
keel mount <source id or label> <K:|folder> [--subtree PATH]
keel unmount <K:|folder>
keel mounts
keel daemon start|stop|status|rotate-token
keel mcp [--allow-execute]
```

All take `--profile NAME` and `--json`; `--json` prints exactly one JSON document per
invocation (`sources add` returns the source with its index job under `job`; a failure
is `{"error": {"code", "message", "data"?}}`). Exit codes: 0 ok, 1 the operation failed,
2 usage. Typed at a terminal, a lone argument that names an existing folder opens that
folder in the window even when it is a subcommand name (`keel devices` where `devices` is
a folder), except `mcp`, `execute`, `daemon` and `search`, which are always the
subcommand; without a terminal (an agent starting `keel mcp`, a script) a name is never
taken as a folder. `keel ./name` always means the folder. `keel plan` prints the preview, the plan id and hash; `keel execute` reads them
from its arguments or from piped `keel plan` output (text or `--json`) and waits for the
job. Other mutating subcommands (`tag`, `sources add|remove|index`, `mount`, `unmount`) print the preview and
confirm it themselves: typing the command is the confirmation. Plans are kept in the
library folder (`api-plans.json`, owner-only; on Windows a file owned by the user, the
token's default owner or Administrators, with a protected DACL naming only them and
SYSTEM; the hash covers everything a plan runs and is
checked again at `execute`, so a plan altered there is refused), so `keel plan` and a later `keel execute` work without
a daemon too. Release builds on Windows are GUI programs that attach to the calling
console, which does not wait for them: pipe the output (`keel … | more`, PowerShell
`keel … | Out-Host`) to see it in order. Redirected output (`--json` into a file or a
program, `keel mcp` under an agent) is unaffected.

## MCP (`keel mcp`)

`keel mcp` speaks MCP 2025-06-18 on stdin/stdout: `initialize`, `ping`, `tools/list`
(one tool per operation, `.` written `_`: `sources_add`, `tags_list`, …, with the
operation's JSON schema as `inputSchema`) and `tools/call`. Mutating tools say "Returns a
preview; call execute with the plan id to apply" and do exactly that; read-only tools
carry `readOnlyHint`. Results come back as JSON text (and `structuredContent` for
objects); failures as `isError: true`. It uses the running daemon when there is one,
otherwise it opens the library itself for as long as the session lasts. Nothing but
`initialize` and `ping` is answered before `initialize`; a line that is not JSON (or not
UTF-8) gets a parse error and one over 16 MiB an invalid-request error, and the session
goes on.

**`execute` needs a person.** A model can chain a preview and `execute` in one turn, so
the preview alone is not a confirmation. `execute` applies only plans previewed in the
same session, and only after the user confirmed:

- When the client declared the `elicitation` capability (MCP 2025-06-18), Keel asks the
  user itself (`elicitation/create`) with the plan's summary, its changes (the first 50)
  and its warnings, and applies the plan only when the user accepts. Declining or
  cancelling leaves the plan unapplied (it can be confirmed later until it expires).
- When the client cannot ask, `execute` is refused: show the user the preview and apply
  it with `keel execute <plan_id> --hash <input_hash>`. Starting the server with
  `keel mcp --allow-execute` lets such a client execute, but every call must pass the
  preview's `summary` string exactly (`"summary": "Delete 1 item(s) (D:\\old.iso), …"`;
  file-plan summaries name the first three paths), so the client's own tool-approval
  prompt shows what runs. Use it only with a client that
  prompts before each tool call.

```
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"me","version":"1"}}}
← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"keel",...},"instructions":"…"}}
→ {"jsonrpc":"2.0","method":"notifications/initialized"}
→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"plan","arguments":{"op":"delete","paths":["D:\\Downloads\\old.iso"]}}}
← {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{ \"plan_id\": …, \"warnings\": [ … last_copy … ] }"}],"structuredContent":{…},"isError":false}}
```

### Pointing agents at it

Claude Code (user scope, every project):

```
claude mcp add keel --scope user -- keel mcp
claude mcp add keel-work --scope user -- keel mcp --profile work
```

or in a project's `.mcp.json`:

```json
{ "mcpServers": { "keel": { "command": "keel", "args": ["mcp"] } } }
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.keel]
command = "keel"
args = ["mcp"]
```

Give the full path to `keel.exe` when it is not on `PATH`. Run the daemon (`keel daemon
start`, or the window's "Run the library in a background daemon" setting) so the window
and several agents share the library; without one, keep the window closed while an agent
works (one process opens a library at a time). Agents
see every mutating tool return a preview. With a client that supports elicitation, each
`execute` then opens a confirmation showing what the plan does. A client without
elicitation (check its MCP docs) cannot execute unless you add `--allow-execute`
(`claude mcp add keel --scope user -- keel mcp --allow-execute`, or `"args": ["mcp",
"--allow-execute"]`), and then only by repeating the preview's summary.
