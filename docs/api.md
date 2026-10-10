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
`config.toml` (`<config dir>/profiles/<profile>/config.toml`); otherwise they fail with
`NET_DISABLED`. keel-daemon owns the profile's one node while it runs; without it, a CLI
or MCP session brings the node online itself, only for its first device or share call.

Both hosts (the daemon and the in-process CLI) register the profile's SFTP hosts
(`[[remotes]]`) and cloud accounts (`[[clouds]]`) from that `config.toml`, with their
secrets from the OS keychain, so `sftp://<host id>/…` and `cloud://<account id>/…`
paths work and Share / Cloud sources can be listed and indexed. A host key that is not
trusted yet is refused (there is no one to ask): connect once from the Keel window to
trust it. Files extracted from archives go to `<data dir>/archives` (never the app's own
cache). Serving
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

## keel-daemon (JSON-RPC)

```
keel-daemon [--profile NAME] [--ws 127.0.0.1:PORT [--ws-allow-remote]]   # NAME: letters, digits, . _ -
keel-daemon --status            # exit 0 when one runs for the profile, 1 when not
keel daemon start|stop|status   # the same from keel (start runs it in the background)
```

The daemon opens the profile's library (`[library] name` in the profile's config.toml,
default `james`, under `KEEL_DATA_DIR` or the platform data folder), resumes its jobs and
serves until Ctrl-C, SIGTERM or `daemon.shutdown`. It survives clients disconnecting; a
second daemon for the same profile exits with "keel-daemon is already running for
profile …". While the Keel window has the library open, neither the daemon nor in-process
CLI calls can open it (one process holds a library).

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
`{id, status, progress}`, `library.changed` `{method}` and `net.event` arrive on that
connection), `unsubscribe`, and `daemon.shutdown`.

**WebSocket (optional).** `--ws 127.0.0.1:7420` serves the same JSON-RPC, one message per
text frame. Every connection must send `Authorization: Bearer <token>`, the token in
`<config dir>/daemon.token` (created on first use, owner-only: a protected DACL for the
user alone on Windows, mode 0600 on Unix). A token file anybody else may read or change
is replaced with a new token at start, since it may have been read or planted. Browsers
cannot set that header, so web pages cannot connect. The handshake (token check
included) must be over within 5 s, and at most 64 connections are served at once; more
are closed at once. Non-loopback addresses need
`--ws-allow-remote`; there is no TLS, so put a remote bind behind a TLS proxy or a
private network.

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
keel mcp [--allow-execute]
```

All take `--profile NAME` and `--json`; `--json` prints exactly one JSON document per
invocation (`sources add` returns the source with its index job under `job`; a failure
is `{"error": {"code", "message", "data"?}}`). Exit codes: 0 ok, 1 the operation failed,
2 usage. A lone argument that names an existing folder opens that folder in the window
even when it is a subcommand name (`keel search` where `search` is a folder); write
`keel ./search` for the folder, and run a subcommand with any further argument or from
another folder. `keel plan` prints the preview, the plan id and hash; `keel execute` reads them
from its arguments or from piped `keel plan` output (text or `--json`) and waits for the
job. Other mutating subcommands (`tag`, `sources add|index`) print the preview and
confirm it themselves: typing the command is the confirmation. Plans are kept in the
library folder (`api-plans.json`, owner-only; the hash covers everything a plan runs and is
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
  preview's `summary` string exactly (`"summary": "Delete 1 item(s), …"`), so the
  client's own tool-approval prompt shows what runs. Use it only with a client that
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

Give the full path to `keel.exe` when it is not on `PATH`. Start `keel daemon start` first
when the Keel window is closed and several agents share the library, or keep the window
closed while an agent works without one (one process opens a library at a time). Agents
see every mutating tool return a preview. With a client that supports elicitation, each
`execute` then opens a confirmation showing what the plan does. A client without
elicitation (check its MCP docs) cannot execute unless you add `--allow-execute`
(`claude mcp add keel --scope user -- keel mcp --allow-execute`, or `"args": ["mcp",
"--allow-execute"]`), and then only by repeating the preview's summary.
