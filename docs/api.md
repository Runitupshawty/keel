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
(`library://<source id>/<path>`, `sftp://host/path`). Relative paths are refused: the
daemon's working folder is not the caller's.

## Preview first

Nothing changes without a preview. A **mutating operation called directly only previews**:
it validates its input and returns a `PlanPreview`:

```json
{
  "plan_id": "5247b6c49c4b619d0012fe5fe04ea6db",
  "input_hash": "9c9fc3349e6b2e93b6a9cdebf373ecf2b2821f2992b2e541e8742788031166bf",
  "operation": "plan",
  "summary": "Copy 1 item(s), 1 file(s), 6 bytes to D:\\dst",
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

Only `sources.remove` and `shares.revoke` (taking something away) act directly; they
return what they removed.

## Operations

| Operation | Kind | What it does |
| --- | --- | --- |
| `version` | read | Keel version, API revision, open library, pid of the host |
| `sources.list` | read | Library sources with status (online / indexing / offline / error) |
| `sources.add` | preview | Add a folder, drive or share as a source |
| `sources.remove` | direct | Forget a source (files untouched; `delete_store` drops its index) |
| `sources.index` | preview | Index a source (a job) |
| `list` | read | List a folder: `library://` paths from the index (offline too), others live |
| `stat` | read | One entry, plus its library record, tags and favorite state |
| `read` | read | A byte range of a file (`path`, `offset`, `len` up to 4 MiB), base64; library paths read the real file |
| `preview.render` | read | The desktop previewers' output: `kind` `text` (code, Markdown, tables as TSV, documents, hex) or `image` (a PNG of at most `max_px` 64..2048, default 1024: images, a PDF `page`, a video frame), or `none` with a `message`; `content_id` when indexed (a cache key) |
| `media.thumb` | read | A photo or video thumbnail (`size` `thumb256` / `thumb1024`, WebP) from the sidecar store, keyed by content id when indexed; made on first request, served from earlier sidecars when the source is offline |
| `file.get` | read | A one-time download link: `{url: "/file/<token>", name, size, expires_at}` on the `--web` address, valid once for 60 s |
| `search` | read | Library search: words, `"phrases"`, `kind:`, `ext:`, `size:`, `dm:`, `source:`, `tag:` |
| `tags.list` | read | All tags, or the tags on one path |
| `tags.add` / `tags.remove` | preview | Tag / untag indexed paths (`tags.add` creates a missing tag) |
| `tags.set` | preview | Exactly these tags on the paths (Favorites kept) |
| `favorites.list` | read | Favorites |
| `favorites.set` | preview | Add to (or with `on: false` remove from) Favorites |
| `recents` | read | Recently opened files |
| `jobs.list` / `jobs.info` | read | Jobs; one job with its log |
| `jobs.cancel` | preview | Cancel a job |
| `duplicates` | read | Same-content groups, most wasted bytes first |
| `redundancy` | read | How many copies of a file's content exist, and in which sources |
| `plan` | preview | Preview copy / move / delete / rename (`op`, `paths`, `to`, `new_name`, `on_conflict`) |
| `execute` | direct | Apply a preview (`plan_id`, `input_hash`) |
| `devices.list` | read | This device and paired devices (LAN / relay / offline) |
| `devices.pair_code` | preview | One-time pairing code (10 minutes) |
| `devices.pair_with` | preview | Pair with a device's code (grants nothing) |
| `devices.forget` | preview | Forget a device and its grants |
| `shares.list` | read | Grants to paired devices |
| `shares.grant` | preview | Give a device read or read-write access to a source or subtree |
| `shares.revoke` | direct | Revoke a grant at once |

Devices and shares need keel-net: `[net] enabled = true` in the profile's
`config.toml` (`<config dir>/profiles/<profile>/config.toml`), and keel-daemon running
(it owns the profile's one node); otherwise they fail with `NET_DISABLED`. Serving
sources to peers arrives with the remote-source work (Task 36): until then a grant is
recorded but the node offers no sources.

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
| -32004 | the sources changed since the preview; `data` is the new preview |
| -32005 | devices are off (keel-net disabled) |
| -32006 | the request timed out (the daemon answers within 120 s) |
| -32007 | `--web`: a message before `auth`, or a wrong token (the connection closes) |

## keel-daemon (JSON-RPC)

```
keel-daemon [--profile NAME] [--ws 127.0.0.1:PORT] [--web [127.0.0.1:PORT]] [--ws-allow-remote]
keel-daemon --status            # exit 0 when one runs for the profile, 1 when not
keel daemon start|stop|status   # the same from keel (start runs it in the background)
```

The daemon opens the profile's library (`[library] name` in the profile's config.toml,
default `james`, under `KEEL_DATA_DIR` or the platform data folder), resumes its jobs and
serves until Ctrl-C, SIGTERM or `daemon.shutdown`. It survives clients disconnecting; a
second daemon for the same profile exits with "keel-daemon is already running for
profile …". While the Keel window has the library open, neither the daemon nor in-process
CLI calls can open it (one process holds a library).

**Local socket.** Named `keel-daemon-<hash of user, profile and data folder>`: a named
pipe on Windows whose DACL admits only the current user (clients refuse impersonation and
check the server runs as the same user), a socket in a 0700 folder
(`$XDG_RUNTIME_DIR/keel-<uid>/`) on Unix with peer-uid checks on both ends. Framing: one
JSON-RPC message per line (UTF-8, `\n`), at most 16 MiB; batches allowed. Once a request
has started it must arrive within 30 s; idle connections may stay open.

```
→ {"jsonrpc":"2.0","id":1,"method":"search","params":{"query":"invoice ext:pdf","max":5}}
← {"jsonrpc":"2.0","id":1,"result":[{"path":"D:\\Docs\\invoice-2026.pdf","name":"invoice-2026.pdf","size":48213,...}]}
→ {"jsonrpc":"2.0","id":2,"method":"tags.add","params":{"tag":"receipts","paths":["D:\\Docs\\invoice-2026.pdf"]}}
← {"jsonrpc":"2.0","id":2,"result":{"plan_id":"…","input_hash":"…","operation":"tags.add","summary":"Tag 1 item(s) with receipts",...}}
→ {"jsonrpc":"2.0","id":3,"method":"execute","params":{"plan_id":"…","input_hash":"…"}}
← {"jsonrpc":"2.0","id":3,"result":{"plan_id":"…","operation":"tags.add","result":{"records":1}}}
```

Besides the operations: `subscribe` (then notifications `job.progress`
`{id, status, progress}`, `library.changed` `{method}` and `net.event` arrive on that
connection), `unsubscribe`, and `daemon.shutdown`.

**WebSocket (optional).** `--ws 127.0.0.1:7420` serves the same JSON-RPC, one message per
text frame. Every connection must send `Authorization: Bearer <token>`, the token in
`<config dir>/daemon.token` (created on first use, owner-only on Unix). Browsers cannot
set that header, so web pages cannot connect. Non-loopback addresses need
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

Browsers cannot send an `Authorization` header on a WebSocket, so on `/rpc` the **first
message must be `auth`** with the daemon token:

```
→ {"jsonrpc":"2.0","id":0,"method":"auth","params":{"token":"<daemon.token>"}}
← {"jsonrpc":"2.0","id":0,"result":{"ok":true}}
```

Anything else first, a wrong token, or no `auth` within 10 s gets error -32007 and the
connection closes (a native client may still send the `Authorization: Bearer` header
instead). The token never travels in a URL: `/rpc` with a query string is refused (400).
A WebSocket whose `Origin` is not this host is refused (403); on a loopback bind the
`Host` header must be `localhost`, `127.0.0.1` or `[::1]` (403 otherwise, against DNS
rebinding). Only `GET` is served. Every answer carries `Cache-Control: no-store`,
`Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`
and `Content-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval';
connect-src 'self' ws://<host> wss://<host>; ...`. Non-loopback addresses need
`--ws-allow-remote`, exactly as `--ws`; there is no TLS (a tailnet, or a TLS proxy).

## CLI

```
keel search <query> [--max N] [--json]
keel tag add|remove <tag> <paths…>
keel plan copy|move <src…> --to <dir> [--on-conflict skip|overwrite|rename] [--json]
keel plan delete <paths…> [--json]
keel execute [<plan id> --hash <hash>] [--no-wait]   # or: keel plan … | keel execute
keel sources [add <path> [--label L] [--no-index] | remove <id> [--delete-store] | index <id>]
keel devices | keel shares
keel daemon start|stop|status
keel mcp
```

All take `--profile NAME` and `--json`. Exit codes: 0 ok, 1 the operation failed, 2
usage. `keel plan` prints the preview, the plan id and hash; `keel execute` reads them
from its arguments or from piped `keel plan` output (text or `--json`) and waits for the
job. Other mutating subcommands (`tag`, `sources add|index`) print the preview and
confirm it themselves: typing the command is the confirmation. Plans are kept in the
library folder (`api-plans.json`), so `keel plan` and a later `keel execute` work without
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
otherwise it opens the library itself for as long as the session lasts.

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

Give the full path to `keel.exe` when it is not on `PATH`. Start `keel daemon start` first
when the Keel window is closed and several agents share the library, or keep the window
closed while an agent works without one (one process opens a library at a time). Agents
see every mutating tool return a preview; tell them to show it and to call `execute` only
after you confirm.
