---
name: keel
description: Use the Keel file manager's library from a shell or MCP to search, list, inspect, tag, favorite, copy, move, rename and delete the user's files across folders, drives, SSH hosts and cloud accounts. Every change is previewed and shown to the user before it is applied. Use when the user asks to find, organize, tag, clean up, copy, move or delete files that Keel indexes, or mentions Keel, `keel op` or `keel mcp`.
---

# Keel

Keel is a file manager with a library: an index of the user's folders, drives, SSH hosts
and cloud accounts that answers even while a drive is unplugged. Everything the library
can do is a named operation with a JSON schema (`search`, `list`, `stat`, `tags.set`,
`plan`, `execute`, ...). You reach the operations with `keel op` in a shell, or with the
MCP server `keel mcp`. Both go through `keel-daemon` when it runs, else they open the
library themselves.

## When to use

- The user asks to find files ("where are my 2025 tax PDFs?"), list a folder, or see
  sizes, duplicates or how many copies of a file exist.
- The user asks to tag, untag or favorite files, or to copy, move, rename or delete them,
  and the files are in Keel's library (or on a path Keel can reach).
- The user mentions Keel, `keel op`, `keel mcp` or a `library://` path.

Do not use it for files the user did not ask about, and do not use it to read Keel's own
configuration folder (the read operations refuse it anyway).

## The two-step protocol: preview, then execute

Almost nothing in Keel changes without a preview. These operations act at once, with no
preview: **ask the user first** and call them only on a clear yes.

- `shares.revoke`: takes a paired device's access to a folder away.
- `library.sync`: pulls tag, favorite, folder style and content-id changes from the
  user's other devices and applies them to the library.
- `recents.note` and `activity.note`: bookkeeping only (a file was opened, the user is
  working); they change no file. Call them only when the user asks.

1. **Preview.** Call the operation. A read (`search`, `list`, `stat`, `tags.list`,
   `library.stats`, `duplicates`, ...) answers at once. An operation that changes
   something (`tags.add`, `tags.set`, `favorites.set`, `sources.add`, and `plan` for
   copy, move, delete and rename) changes nothing: it returns a **preview** with a
   `summary`, the `changes`, the `warnings`, a `plan_id` and an `input_hash`.
2. **Show the preview to the user.** Give the summary, the changes (or their count and
   the first few), and every warning, in your own words where that helps. Ask whether to
   go ahead. Stop here unless the user says yes.
3. **Execute that preview.** Call `execute` with the preview's `plan_id` and
   `input_hash`. Keel applies exactly what was previewed and refuses a plan that expired
   (10 minutes), was already applied, or whose files changed since the preview. File
   operations run as a job: follow it with `jobs.info` until its `status` is `done`
   (`keel op execute` waits for it).

## `keel op` (a shell)

```sh
keel op list                                  # every operation, whether it previews, its schema
keel op search --params '{"query": "invoice ext:pdf", "max": 20}'
keel op stat --params '{"path": "D:\\Docs\\invoice.pdf"}'
keel op tags.set --params '{"paths": ["D:\\Docs\\invoice.pdf"], "tags": ["receipts"]}'
keel op plan --params '{"op": "move", "paths": ["D:\\Downloads\\old.iso"], "to": "E:\\Archive"}'
keel op execute --params '{"plan_id": "<from the preview>", "input_hash": "<from the preview>"}'
```

- Parameters are one JSON object, `--params '<json>'`, or `--params-file <file>` (easier
  than quoting JSON in PowerShell or cmd). `keel op list` shows each operation's schema
  and an example. Paths are absolute (`D:\...`, `/home/...`) or Keel URIs
  (`library://<source id>/<path>`, `sftp://<host id>/<path>`).
- When stdout is not a terminal the output is JSON: the result, or the preview with
  `plan_id` and `input_hash`.
- `keel op <name> ... --execute` previews and applies in one run. It prints the preview,
  but the user sees it only after it was applied. **Do not use `--execute`** unless the
  user has already seen and approved a preview of exactly this call (same operation, same
  parameters) and asked you to run it. The normal path is a preview, the user's yes, then
  `keel op execute` with that preview's `plan_id` and `input_hash`.
- Exit codes: 0 ok, 1 the operation refused or failed (the JSON is `{"error": {...}}`),
  2 a usage mistake (unknown name, parameters that are not a JSON object, `--execute` on
  an operation without a preview).
- `--profile NAME` uses another Keel profile (another library).

## `keel mcp` (an MCP client)

`keel mcp` is an MCP server over stdio with one tool per operation (`.` written `_`:
`tags_set`, `jobs_info`). Register it once, for example
`claude mcp add keel --scope user -- keel mcp`. Mutating tools return the same preview.
`execute` applies only plans previewed in the same session, and Keel asks the user to
confirm each plan itself when the client supports elicitation. A client without
elicitation cannot execute unless the server was started with `--allow-execute`, and then
`execute` must repeat the preview's `summary` exactly. Never ask the user to add
`--allow-execute` just to get past a confirmation.

## Safety rules

1. **Never apply a change the user has not seen.** Show the preview (summary, changes,
   warnings) and wait for a yes before `execute`. Never use `--execute` to skip that.
2. **Never delete for good without the user's own words.** A delete goes to the Recycle
   Bin or Trash where there is one. A preview warning of kind `permanent` (an SFTP host,
   a cloud account or another place without a trash) means the files cannot be brought
   back: say so plainly, and execute only when the user explicitly said to delete those
   files permanently. A `deletes_permanent` warning (on `shares.grant` with read-write
   access, or `mounts.add`) means deletes made through that share or mount are for good
   too: say so before asking. A `last_copy` warning on a delete or move means Keel knows
   no other copy of that content: say so before asking.
3. **Mention every warning** (`last_copy`, `single_domain`, `offline_source`,
   `not_indexed`, `exists`, `content_unverified`, `creates_tag`, ...), and do not
   execute a plan whose warnings the user has not seen.
4. **Do only what was asked.** Do not widen a selection, add `on_conflict: "overwrite"`,
   or delete "similar" files the user did not name.
5. **One preview, one execute.** If execute says the plan changed, show the new preview
   (it is in the error's `data`) and ask again; never loop retries.

## Reading errors

Errors are `{"error": {"code", "message", "data"?}}`. The `message` says what went wrong
in words; read it before retrying.

| Code | Meaning | What to do |
| --- | --- | --- |
| -32601 | no such operation | check the name with `keel op list` |
| -32602 | bad parameters; the message names the field (``missing field `tags` ``, ``unknown field `tag` ``) | fix that field against the schema in `keel op list` |
| -32001 | not found (path, source, tag, job, device) | check the path with `stat` or `search`; it may not be indexed |
| -32002 | the plan expired, was already applied, or never existed | preview again and show the new preview |
| -32003 | the input hash is not the previewed one | use the `plan_id` and `input_hash` of the same preview |
| -32004 | the files changed since the preview; `data` is the new preview | show the new preview and ask again |
| -32005 | devices are off in Keel's settings | tell the user; do not change settings yourself |
| -32006 | timed out | the operation may still finish: check `jobs.list` before retrying |
| -32007 | not signed in to keel-daemon's web or WebSocket server (no or a wrong token, or it was rotated) | tell the user; never look for the token yourself |
| -32008 | mounts need keel-daemon | suggest `keel daemon start` |
| -32000 | the operation failed (I/O, a locked library) | report the message; "already open" means a Keel window holds the library: suggest `keel daemon start` so both can share it |

## Examples

Find and tag:

```sh
keel op search --params '{"query": "invoice ext:pdf dm:2026-10"}'
keel op tags.add --params '{"tag": "receipts", "paths": ["D:\\Scans\\r1.pdf", "D:\\Scans\\r2.pdf"]}'
# show the preview, get a yes, then:
keel op execute --params '{"plan_id": "…", "input_hash": "…"}'
```

Move to an archive drive:

```sh
keel op plan --params '{"op": "move", "paths": ["D:\\Videos\\2019"], "to": "E:\\Archive", "on_conflict": "skip"}'
# show the summary, the changes and any warnings (exists, last_copy), get a yes, then:
keel op execute --params '{"plan_id": "…", "input_hash": "…"}'
```

The full list of operations, their schemas and the error codes is in Keel's
[API reference](https://github.com/Runitupshawty/keel/blob/main/docs/api.md).
