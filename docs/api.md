# Keel API: JSON-RPC, CLI and MCP

Everything Keel's library can do from outside the window goes through one list of typed
operations (`keel_api::OPS`, crate `keel-api`). The same list serves three front ends:

| Front end | How to reach it | Transport |
| --- | --- | --- |
| JSON-RPC 2.0 | `keel-daemon` (per user, per profile) | local socket / named pipe; optional WebSocket |
| CLI | `keel search …`, `keel plan …`, …, and `keel op <name>` for any operation | the daemon when it runs, else the library in-process |
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
A copy or move that stopped (the daemon closed, a crash) resumes when the library opens
again, where it stopped: its log gets `resumed at file N of M`, files it placed are not
written again, the file it was writing is continued on local and SFTP targets, and a
placed file changed since makes the job fail (`… changed since the preview …`) rather
than be overwritten. `jobs.info` keeps its shape.

The folder operations (`plan.merge`, `plan.flatten`, `plan.organize`) read the folders
when they preview and list every move (`move`, with `to`), every folder they make
(`make_folder`) and every folder they remove when it is left empty (`remove_folder`), and
an `exists` warning for each target name that is taken. Two items of one plan never land
on one name (letter case aside): with `skip` or `rename_new` the later ones move to the
next free name (`same (2).txt`, in `to`), and with `overwrite` the preview is refused,
one line per item that would replace another. `execute` runs them as file plans; when the folder no longer gives the same moves (a
file added, removed or renamed), it answers `PLAN_CHANGED` with the new preview. For
example, `plan.organize` with `{"path": "D:\\Inbox", "by": "type"}` previews

```json
{"operation": "plan", "summary": "Organize D:\\Inbox by type: 2 files into 2 folders",
 "changes": [{"action": "make_folder", "path": "D:\\Inbox\\Documents", "files": 0, "bytes": 0},
   {"action": "make_folder", "path": "D:\\Inbox\\Images", "files": 0, "bytes": 0},
   {"action": "move", "path": "D:\\Inbox\\a.pdf", "to": "D:\\Inbox\\Documents\\a.pdf", "files": 1, "bytes": 5120},
   {"action": "move", "path": "D:\\Inbox\\b.jpg", "to": "D:\\Inbox\\Images\\b.jpg", "files": 1, "bytes": 81920}],
 "warnings": [{"kind": "not_indexed", "path": "D:\\Inbox", "message": "…"}], "…": "…"}
```

and `plan.set_attributes` with `{"paths": ["D:\\Inbox\\a.pdf"], "readonly": true}`
previews `"summary": "Set the attributes of 1 item: read-only on"` with one `set_attributes` change;
`plan.link` with `{"target": "D:\\Photos", "link": "D:\\Desk\\Photos - link", "kind":
"junction"}` previews one `link` change from the target to the link. Attributes and links
work on this computer's files only, and none of these takes a path inside an archive or
Keel's configuration folder.

File-plan summaries name the first three paths (`Delete 5 item(s) (D:\a, D:\b, D:\c, 2
more), …`). `shares.revoke` (taking access away) acts directly; it returns what it
revoked. `library.sync` acts directly too: it applies the tag, favorite, folder style and
content-id changes it pulls. `recents.note` (a file was opened) and `activity.note` (the
user is working) also act directly: they change no file.

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
| `media.thumb` | read | A photo or video thumbnail (`size` `thumb256` / `thumb1024`, WebP) from the sidecar store, keyed by content id when indexed (`content_id` in the answer); made on first request, served from earlier sidecars when the source is offline. `make: false` answers only from a sidecar that exists (NOT_FOUND otherwise); `content_id` (64 hex digits) answers from a sidecar made earlier for that content, before `path` (which may then be left out) is looked at. An attached window asks with `make: true`, at most four at a time, visible tiles first |
| `file.get` | read | A one-time download link: `{url: "/file/<token>", name, size, expires_at}` on the `--web` address, valid once for 60 s |
| `search` | read | Library search: words, `"phrases"`, `kind:`, `ext:`, `size:`, `dm:`, `source:`, `tag:`, and the text inside documents: bare words match it too, `content:` only it, `-content:` not it, `name:` names only, `contents:off` (or `contents: false`) names only. A hit found by its text has `snippet` (`text`, at most 160 characters on one line, and `marks`, the byte ranges of the matched words), for the first 100 hits; name hits rank first. `max` defaults to 100 (at most 10000) |
| `system.search` | read | Every file on the computer, not only the library: the platform's search (the window's search pane backend: Everything when it runs, else Keel's index on Windows; Spotlight or locate elsewhere). `query`: words (all must match), `"phrases"`, wildcards over the whole name (`*.pdf`), or with `regex` one regular expression; other syntax (`ext:`, `!`, `\|`) goes to the backend as it is. Switches (all off by default): `match_case`, `whole_word`, `match_path` (a word with `\` or `/` always matches paths), `match_diacritics` (off: accents of Latin letters are ignored), `regex`, `prefix`, `suffix` (a word matches at the start or end of a word of the name), `ignore_punctuation`, `ignore_whitespace` (plain words only). `sort`: `name` (default), `path`, `date_modified`, `size`; `ascending` (default true); `offset`; `count` (default 32, at most 1000, 0 for the total alone). Answers `{backend, total, capped, offset, hits: [{path, name, parent, is_dir, size, modified}]}`; `total` and the order cover the first 10000 hits the backend returns (`capped` when it returned that many). A bad regular expression or a query over 4096 bytes is -32602 |
| `files.names` | read | The full paths of indexed files (no folders) in the online sources, each source's newest first: `{paths, truncated}`, for a fuzzy file finder (the desktop app's Ctrl+P when attached). At most `max` (default and largest 100000); `truncated` says there were more |
| `tags.list` | read | All tags, or the tags on one path |
| `tags.tagged` | read | Every tagged path with its tag ids (Favorites is id 1) |
| `views.list` | read | Saved views (the window's library bookmarks), by folder then position: `id`, `name`, `query` (a `search` query, or the platform search's text when `backend` is `system`), `layout`, `backend` (`library` or `system`), `sort` (`<column>` or `<column>:desc`, up to three comma-separated keys; `""` default), `columns` (names; empty: default), `matching` (the match toggles that are on: `case`, `whole_words`, `path`, `diacritics`, `regex`, `prefix`, `suffix`, `ignore_punctuation`, `ignore_whitespace`), `filter` (a filter name; `""` Everything), `folder` (`/`-separated; `""` top level), `position`, `shortcut` (`""` none) |
| `views.create` | preview | Save a view: `name` and `query` required, every other `views.list` field optional (`layout` defaults to `details`, `backend` to `library`, `position` to last in its folder). A `library` view's query must parse as a `search` query; a bad sort, column name, folder or backend, or a `shortcut` another view has, is refused at the preview. Answers the saved view |
| `views.update` | preview | Change view `id`: omitted fields stay as they are (`""` clears `shortcut`, `folder`, `filter` or `sort`). Checked as `views.create`; answers the view |
| `views.delete` | preview | Delete view `id` (the files it finds are never touched); answers the deleted view |
| `tags.add` / `tags.remove` | preview | Tag / untag indexed paths (`tags.add` creates a missing tag, with `color` when given; with no paths it only creates the tag) |
| `tags.set` | preview | Exactly these tags on the paths (Favorites kept) |
| `favorites.list` | read | Favorites |
| `favorites.set` | preview | Add to (or with `on: false` remove from) Favorites |
| `recents` | read | Recently opened files |
| `recents.note` | direct | Note that an indexed file was opened (it moves to the top of `recents`) |
| `jobs.list` / `jobs.info` | read | Jobs; one job with its log |
| `jobs.cancel` | preview | Cancel a job. A copy or move uploading to a cloud account stops its request on the wire (within the current 8 MiB chunk); an unfinished S3 multipart upload is aborted, Drive and Dropbox sessions expire on their own |
| `duplicates` | read | Same-content groups, most wasted bytes first |
| `redundancy` | read | How many copies of a file's content exist, and in which sources; each location has its volume's `state`, `backup` mark and whether it is only a device's `claimed` copy (never counted) |
| `redundancy.folder` | read | `redundancy` for every indexed file in a folder (the first 5000): `[{path, copies}]` |
| `library.folder_sizes` | read | A folder's indexed total and each child's size, biggest first: `{path, bytes, files, children: [{name, path, is_dir, bytes, files}], truncated}`. At most `max` children (default 1000; `truncated` says there were more); `dirs_only: true` lists only the child folders. `bytes` / `files` are absent for a folder whose total is not known yet (its source's first walk has not finished) and for a link. Apparent sizes; links are not counted. A path in no source, or not indexed, is NOT_FOUND |
| `du.scan` | read | Start a disk usage scan on the host in the background: `paths` (absolute folders or drives) and/or `all_drives`, with the filters `exclude_names` (globs), `exclude_paths`, `exclude_regex`, `min_size`, `older_than_secs`, `newer_than_secs`, `exclude_hidden`, `exclude_system`, and `follow_symlinks`, `follow_junctions`, `follow_mount_points`, `count_hard_links_each`. Answers `{id}` at once. The read rules apply to the roots (no UNC or device paths outside sources on Windows) and Keel's configuration folder is always left out. The host keeps the last 4 scans; a fifth drops the oldest finished one (FAILED while 4 are running) |
| `du.result` | read | A scan's `state` (`running`, `done`, `failed`), `progress` (`files`, `folders`, `bytes`, `elapsed_ms`, `current`), and when done its `summary` as `keel du --scan --json` prints it (`top` items of each root, default 20; `size_on_disk` ranks by bytes on disk), or its `error`. `wait_ms` (at most 10000) waits for a running scan. An unknown or dropped `id` is NOT_FOUND |
| `du.export` | read | A finished scan as text, `{format, content}`: `list` `tree` (every item; JSON is a scan file `keel du --load-from` reads), `types` (one row per extension), `largest` (`top` files, default 100) as `format` `csv` or `json` (default; `bom` for CSV), or `report` (the HTML report; `top` folders and types, default 20). At most 64 MB; FAILED while the scan runs |
| `library.inspect` | read | Everything the library knows about one `path` (a real or a `library://` path), for an inspector: `{entry, library?}`. `entry` is the file or folder as `stat` shows it, from the file system (from the index when the source is offline). `library` is absent when the path is in no source or not indexed yet; else `source`, `source_label`, `record`, `volume` (as `volumes.list` shows it), `content_id` (64 hex digits; absent before hashing, for a folder, after drift), `first_seen` and `last_hashed` (unix seconds; absent when not recorded), `copies` (a file's protection count as `redundancy` answers it, with `locations` the OTHER files holding its content, device claims last) and `more_copies`, `tags` (Favorites as `favorite`), `history` (the operation log entries that touched it, newest first, followed back through moves and renames: `{id, ts, kind, ok?, from?, to?, tag?}` with `kind` `copy`, `move`, `rename`, `delete`, `tag`, `untag`, or `merge`, `flatten` or `organize` for a folder operation that moved it; `from`: it came from there, `to`: a copy of it was made there; a file that left one of its paths before it came there is another file and is not listed) and `more_history`, and for a folder `folder` (`bytes`, `files`, `folders`, `biggest` child). Copies and history at most `max` each (default 50, at most 1000). A path that cannot be reached and is not indexed is NOT_FOUND |
| `library.folder_style` | read | A folder's color and icon, `[{path, color?, icon?}]`: with `path`, that folder (one entry, without `color` and `icon` when it has none); without, every folder in the library that has a style. A path in no library source is NOT_FOUND ("add the folder's drive to the library to style it") |
| `library.set_folder_style` | preview | Set an indexed folder's `color` (`#rrggbb`) and `icon` (one emoji, or `theme:<name>`: the icon theme's folder icon for folders of that name, shown by the desktop app when its theme has one); an omitted field stays as it is and `""` clears it. Answers `{path, color?, icon?}`. A file, a malformed color, more than one emoji, or a folder in no library source is refused at the preview. Changes only the library (never the folder); syncs to other devices with library sync |
| `library.comment` | read | The Get Info comment on an indexed file or folder, `{path, comment?}` (no `comment`: none). A path in no library source is NOT_FOUND |
| `library.set_comment` | preview | Set the Get Info comment (`comment`, at most 64 KiB; `""` clears it) on an indexed file or folder. Answers `{path, comment?}`. Changes only the library (never the file); comments follow moves and renames the index records, stay on this device (not synced) and are in library backups |
| `library.stats` | read | Counts over every source: `sources`, `offline_sources`, `records`, `files`, `bytes`, `unique_content`, `running_jobs`, and `per_source` (each source's `id`, `label`, `files`, `folders`, `bytes`, `hashed_files`, `last_walk`, `offline`; example below); `hashing`: the remote hashing policy (`remote`, `cloud`, `max_remote_bytes`, as `hashing.set` last set them); `last_backup`: when `library.backup` last wrote a backup (unix seconds, absent before the first); `text`: the text inside documents (`on`: Index file contents, `documents` indexed, `skipped` (no reader or over the size cap), `failed`, `chars`), and each source's own `text` (`on`: read now) |
| `library.backup` | preview | Write a backup of the library to `file` on the host: a new `.keelbackup` file, or a folder to write `<library>-<YYYY-MM-DD>.keelbackup` in (an absolute local path; an existing file is refused). Answers `{file, bytes, created, sources}`. Holds `library.db` and every source's store as `VACUUM INTO` snapshots; no thumbnails or search index ([Backup and restore](library.md#backup-and-restore)) |
| `library.restore` | preview | Restore the backup `file` on the host as a new library, named `name` or the backed-up library's; a library of that name is never replaced (refused at the preview). The preview lists the library and each source, walked again or offline; answers `{name, dir, sources: [{id, label, root, online}]}`. The host's open library stays open: switch to the new one in the window. A file that is not a backup, is damaged or comes from a newer Keel is refused with that message |
| `protection.summary` | read | The protection card: `single_copy`, `single_domain`, `unbacked`, `drifted`, `unchecked` (not hashed yet, in none of the other counts), `offline_volumes` |
| `volumes.list` | read | The drive inventory: each volume's `id`, `label`, `kind`, `failure_domain` (`domain_set` when set by hand), `state`, `last_seen`, `backup`, `used` / `total` |
| `volumes.set` | preview | Set a volume's `state` (`archived` / `lost` / `retired`; `online` makes it automatic again), `backup` mark or `failure_domain` (`""`: the detected one) |
| `integrity.check` | preview | Re-hash `sample_pct` % (default 1) of the hashed files of one `source` or all, as a job; with `due_days`, only when the last check of every source is that old (the first call starts the clock; `job` absent when nothing is due) |
| `hashing.set` | preview | Content hashing after walks `on` / off (`idle_only`: pause while the user works); on returns the hash `job`, off cancels a running one. Optional `remote` (hash SFTP sources, default true), `cloud` (hash cloud sources, default false: downloads can cost egress fees) and `max_remote_bytes` (remote files bigger than this are not hashed, default 1073741824) are kept with the library (`library.stats` `hashing` reports them); omitted ones stay as they are. Turning `remote` or `cloud` off stops a running job's downloads from those sources at once. The preview states all three |
| `media.index` | preview | Thumbnails and metadata for a source's photos and videos, as an idle-priority job |
| `contents.set` | preview | Index file contents: `on` for the library, or with `id` for one source (`default: true`: back to its kind's default, local folders and drives on; shares (a drive mapped to one included), SFTP, cloud and devices off). On reads the text at idle priority (a remote source downloads every document once); off deletes the text read so far. Answers `{}`; `library.stats` `text` and each source's `text.on` show the result |
| `activity.note` | direct | The user is working: idle-only hashing, integrity and text jobs pause for the next 5 s, sidecar jobs for 1 s (an attached window sends it on input, at most every 4 s) |
| `plan` | preview | Preview copy / move / delete / rename (`op`, `paths`, `to`, `new_name`, `on_conflict`). Paths inside a zip or jar on the host (`D:\x.zip!/dir/a.txt`) can be deleted, renamed and copied or moved into: the preview warns `rewrites_archive` with the archive's size (each execution rewrites it once, entries that stay copied byte for byte) and refuses 7z, tar and RAR archives, archives inside archives or on other providers, and moves out of an archive |
| `plan.merge` | preview | Merge folders: everything in the folders `paths` (but `into`, which may be one of them) moves into `into` with `on_conflict` (default `skip`), then the folders left empty are removed. Links stay where they are |
| `plan.flatten` | preview | Every file in the subfolders of `path`, at any depth, moves up into it with `on_conflict`, then the subfolders left empty are removed |
| `plan.organize` | preview | The files directly in `path` move into subfolders by `by`: `type` (Images, Videos, Audio, Documents, Archives, Other), `year` (`2024`) or `month` (`2024-03`) of the modified date, local time; missing subfolders are made |
| `plan.set_attributes` | preview | On each of `paths` (local, not recursive): `hidden` (Windows, macOS), `readonly`, `mode` (Unix, octal string such as `"755"`); omitted ones stay |
| `plan.link` | preview | A new `link` (a full path that must not exist) to `target`, of `kind` `symlink`, `junction` (Windows, a folder) or `hardlink` (a file, same drive). A symbolic link on Windows without Developer Mode or elevation is refused at the preview with that reason |
| `execute` | direct | Apply a preview (`plan_id`, `input_hash`); `job` names the job when the operation runs as one (file plans, `sources.index`, `spacedrop.send`) |
| `library.sync` | direct | Pull tag, favorite, folder style and content-id changes now from `peer` (a paired device id), or from every device library sync is on with; answers `[{peer, label, applied, error?}]` (`applied`: changes newer than this library's, applied; `error`: the device is offline, or its switch for this device is off). Refused for a `peer` sync is off with here. Reads from the other device and writes only the library's tags and favorites |
| `devices.list` | read | This device and paired devices (LAN / relay / offline); each device's `sync` (library sync is on with it here) and `last_sync` (unix seconds of the last completed pull from it, absent before the first) |
| `devices.pair_code` | preview | One-time pairing code (10 minutes); the short code is found through internet discovery or, on the same network, mDNS |
| `devices.pair_with` | preview | Pair with a device's code (grants nothing) |
| `devices.forget` | preview | Forget a device and its grants |
| `devices.settings` | read | This device's `label`, Spacedrop `inbox`, `auto_accept` device ids, `relay` (as the node started) and `sync` (the device ids library sync is on with) |
| `devices.settings_set` | preview | Change any of `label` (at most 256 bytes, no control or direction characters, checked in the preview), `inbox` (an absolute folder, never in Keel's configuration or data folder but its inbox, nor a folder holding either), `auto_accept`, `sync` (every paired device listed syncs the library, every other one does not) and `relay` on the running host: the label, inbox, always-accept list and sync devices apply at once; relays only when the host starts again (`restart: true` in the answer, and a `restart` warning in the preview). Nothing is written to `config.toml` |
| `shares.list` | read | Grants to paired devices |
| `shares.grant` | preview | Give a device read or read-write access to a source or subtree (a local folder, an SFTP host or a cloud account: the device's writes go through that source's provider); a read-write preview warns `read_write`, and `deletes_permanent` where deletes there are for good (SFTP, S3) |
| `shares.revoke` | direct | Revoke a grant at once |
| `mounts.list` | read | Sources keel-daemon serves as drives or mount folders |
| `mounts.add` | preview | Mount a source or a subtree (`source`, `subtree`, `target`: `K:` or a folder) |
| `mounts.remove` | preview | Unmount (`target`); writes still in progress there are discarded |
| `spacedrop.send` | preview | Send files or folders on the host's machine to a paired device (`peer`, `paths`) as a job; the preview lists every file with its size (first 500) and warns when the device is offline. Only paths in a library source, the Spacedrop inbox or a claimed share upload are sent; Keel's configuration and data folders (and folders holding them) are refused, for the paths given and every file reached; links inside folders are skipped. `execute` sends exactly the previewed files: a folder that changed since the preview gives -32004 with the new preview |
| `spacedrop.inbox` | read | The host's Spacedrop inbox: offers waiting for an answer (`pending`: device, id, file count, bytes, first names) and what arrived (`entries`, newest first; download with `file.get`) |
| `spacedrop.answer` | preview | Accept or decline a waiting offer (`peer`, `id`, `accept`) |

`library.stats` answers, for one source walked at a Unix time and half hashed:

```json
{"sources":1,"offline_sources":0,"records":1302,"files":1200,"bytes":5368709120,
 "unique_content":580,"running_jobs":0,
 "per_source":[{"id":"3f2a","label":"Photos","files":1200,"folders":101,"bytes":5368709120,
   "hashed_files":600,"last_walk":1790000000,"offline":false,
   "text":{"on":true,"documents":42,"skipped":3,"failed":1,"chars":81234}}],
 "hashing":{"remote":true,"cloud":false,"max_remote_bytes":1073741824},
 "text":{"on":true,"documents":42,"skipped":3,"failed":1,"chars":81234}}
```

`folders` counts the folders below the root, `hashed_files` the files with a content hash
(only those are checked for copies), and `last_walk` (absent before the first walk) is the
last completed full walk. The counts come from each store's counters and indexes, not a
scan (about 2 ms for 100,000 records). `hashing` is kept in the library (`library.db`), not
in a window's settings: a window reads it when it opens the library (and writes it to its
`settings.toml`), and sends it only when the user changes it in Settings, so a client's
`hashing.set` is never reverted by a window opening.

Devices and shares need keel-net: the window's Settings → Devices switch, written as
`[devices] enabled = true` with `explicit = true` in the profile's `config.toml`
(`<config dir>/profiles/<profile>/config.toml`); `[net] enabled = true` is read when
`[devices]` has no explicit value. Otherwise they fail with `NET_DISABLED`. Spacedrops
sent to a host (the daemon, or a CLI session holding the node) land in `[devices] inbox`
(default `<data dir>/inbox`); offers from the device ids in `[devices] auto_accept` are
accepted at once, the others wait in `spacedrop.inbox` for `spacedrop.answer` and are
declined when the sender stops waiting. `[devices] relay = false` starts the node without
public relays. `[devices] sync` lists the device ids library sync is on with (none by
default) and `[devices] sync_secs` how often each of them is pulled (60 by default, 10 to
3600). These are read when the host starts; `devices.settings_set` changes the
label, inbox, always-accept list and sync devices of a running host (an attached window writes its
Settings → Devices through it, the inbox only when one is set there, so drops land in the
same folder with or without a window), relays and `sync_secs` need a restart. Library
sync answers a device's pulls only while sync with it is on here, and a host that does not
serve a library (no `LibraryHandler`) neither pulls nor answers. When a pull changes the
library, the daemon sends `library.changed` `{method: "library.sync", kind:
"library.sync"}` (and `net.event` `library_synced` with the `peer` and how many changes it
`applied`), so attached windows and web clients read tags and favorites again. A
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
the others asked what changed every `[library] remote_poll_secs`, default 120, clamped
to 30..=3600, read when the daemon starts: Drive and Dropbox through their change feeds, SFTP by its folders'
times, S3 and WebDAV by walks; a completed walk schedules hashing) once no index job
walks it.

SFTP remotes in `[[remotes]]` take `use_ssh_config` (default `true`, so remotes saved
before it existed read the config too): `host` may then be an alias from
`~/.ssh/config`, and an empty `user`, an empty key-file path and `port = 22` take the
config's values, including `ProxyJump` routes
([what is read](remotes.md#openssh-configuration-and-jump-hosts)). The daemon resolves
the config on every connection like the window; with no window to confirm a prompt, an
unknown host key on any hop fails the connection. `ProxyCommand` is refused, never run.
Paths stay `sftp://<host id>/...`; no operation changed.

FTP and FTPS remotes are `[[remotes]]` entries with `protocol = "ftp"` or `"ftps"` (an
entry without `protocol` is SFTP, so earlier configurations load unchanged); their paths
are `ftp://<host id>/...`, and `list`, `stat`, `read`, `preview.render` and `plan` /
`execute` (copy, move, delete) take them like SFTP paths. The user
`anonymous` signs in without a password; any other user's password comes from the OS
keychain (`KEEL_NET_SECRET=memory`: from memory, for tests), where the window's
**Settings → Remotes** put it. An FTPS server whose certificate does not verify is refused
by the daemon and the CLI (nobody can be asked) unless its fingerprint was trusted in the
window before ([certificates](remotes.md#certificates)). `sources.add` refuses
an `ftp://` root ("library sources are not supported for FTP remotes"), so FTP folders
are never indexed, hashed or mounted:

```toml
[[remotes]]
id = "camera"
label = "Camera"
protocol = "ftps"
host = "camera.example.invalid"
port = 21
user = "anonymous"
auth = "PasswordInKeyring"
bookmarks = []
```

Mounts live in keel-daemon (a mount made by an in-process CLI call would vanish when the
command exits) and need a daemon built with a mount backend (the Linux release build has
one; from source, `--features winfsp` on Windows (GPL-3.0, in no release build) and
`--features fuse` on Linux and macOS; see the README's Mounts section) and its driver installed;
otherwise `mounts.add` fails with `MOUNTS_UNAVAILABLE` (-32008). The `mounts.add` preview checks the
target is free and the source or subtree can be listed, and warns when the source is
offline (`source_offline`: the mount then lists it from the index and cannot read or change files)
or deletes are permanent there (`deletes_permanent`: SFTP, S3). The `mounts.remove`
preview warns when files are still being written through the mount (`discards_writes`).
A target inside the folder the mount shows is refused. On Linux and macOS a target folder
that does not exist yet (its parent must) is made when the mount runs, after the preview
was confirmed, and removed again if mounting fails. While a file is written through a
mount, only its writer sees the new content (other opens are busy, listings show the saved
file); it is published when the writer closes it, and a failed publish keeps the data as
`<name> (unsaved <date>).<ext>`. A file being written can be renamed or moved, as can the
folder holding it: the write is published under the new name. Files show the source's
modified time and, on a read-only source, the read-only bit (writes fail with `EROFS`);
the drive's free and total space are the source volume's where it can be told (local
folders, SFTP servers with `statvfs@openssh.com`, cloud accounts with a quota). Mounts are
unmounted when the daemon stops (writes still open are dropped).

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
| -32006 | the request timed out (the daemon answers within 120 s; `library.backup` and `library.restore`, and their `execute`, within 6 hours) |
| -32007 | `--web`: a message before `auth`, a wrong token or no `auth` within 10 s; `--ws`/`--web`: the token was rotated (the connection closes) |
| -32008 | mounts unavailable: not served by this host (no daemon) or no mount backend built in |

## keel-daemon (JSON-RPC)

```
keel-daemon [--profile NAME] [--ws 127.0.0.1:PORT] [--web [127.0.0.1:PORT]] [--ws-allow-remote]
            [--web-host NAME]...  # NAME: letters, digits, . _ -
keel-daemon --status            # exit 0 when one runs for the profile, 1 when not
keel-daemon --set-http-login USER < password   # --web's Basic login for Everything tools
keel-daemon --clear-http-login
keel daemon start|stop|status   # the same from keel (start runs it in the background)
keel daemon rotate-token        # a new token: clients sign in again
```

The daemon opens the profile's library (`[library] name` in the profile's config.toml,
default `main`, under `KEEL_DATA_DIR` or the platform data folder), resumes its jobs and
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
`{id, status, progress}`, `library.changed` `{method, kind}`, `net.event` and, when the
daemon stops, `daemon.stopping` `{pid}` arrive on that connection), `unsubscribe`,
`daemon.shutdown`, and `share.claim` `{id}` (the web client's share target, below).
`library.changed` follows a call by any client that changed something: `method` is the
call (`execute`, `shares.revoke` or `recents.note`) and `kind` what changed (the executed
plan's operation, such as `tags.add`, `volumes.set` or `integrity.check`, else the
method). An `execute` that started no job and returned an empty result (an integrity
check with `due_days` that was not due yet) changed nothing and sends none. A client
refreshes what that kind can change; for job starters (`sources.index`, `hashing.set`,
`integrity.check`, `media.index`) the job's `job.progress` to `done` is the moment to.
A sidecar job also sends `library.changed` `{method: "job", kind: "media.index", job,
done}` every 2 s while it runs and when it ends (`done: true`). An attached window does not
wait for it: it asks `media.thumb` with `make: true`. After every protection recount
(at the end of a walk, hashing or integrity job, after a volume change, and 5 s after the
last change a watcher or an executed operation applied) the daemon sends `library.changed`
`{method: "protection", kind: "protection.recount"}`: `protection.summary`, `volumes.list`
and `redundancy.folder` may read differently. A library sync pull that changed the library (another
device's tags or favorites arrived, on the daemon's own schedule or through
`library.sync`) sends `library.changed` `{method: "library.sync", kind: "library.sync"}`:
tags, favorites and the tags on listed files may read differently.

The daemon also runs the scheduled integrity check of every source (`[library]
integrity_days`, default 7, 0 turns it off, and `integrity_pct`, default 1, in the
profile's config.toml, read again each time), so clients need not ask for it.

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
| `/everything`, `/` with Everything's parameters | Search all files for tools written for Everything's HTTP server (below) |
| `/everything/file?path=<file>` | a file found there, while `[web] allow_downloads = true` (`Content-Disposition: attachment`) |

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

**Everything-compatible search (`/everything`).** `GET /everything?<parameters>`, or
`GET /?<parameters>` when they include `search`, `s`, `q`, `json`, `j`, `offset`, `o`,
`count`, `c` or `sort` (`/` without them is the web client, so `/?share=...` still is), runs
`system.search` with Everything's HTTP parameters:

| Parameter | `system.search` |
| --- | --- |
| `search`, `s`, `q` | `query` |
| `offset`, `o` / `count`, `c` | `offset` / `count` (digits only; `count` at most 1000) |
| `json`, `j` | answer in JSON (else HTML) |
| `case`, `i` / `diacritics`, `m` / `wholeword`, `w` / `path`, `p` / `regex`, `r` | `match_case` / `match_diacritics` / `whole_word` / `match_path` / `regex` |
| `prefix`, `suffix`, `ignore_punctuation`, `ignore_whitespace` | the same |
| `sort` (`name`, `path`, `date_modified`, `size`; others: `name`), `ascending` | `sort`, `ascending` |
| `path_column`, `size_column`, `date_modified_column` | the JSON fields `path`, `size`, `date_modified` |

Switches take `1` or `0` (`true`/`false`, `on`/`off`, `yes`/`no`; a bare key is `1`); a
bad number or switch is 400. Unknown parameters are ignored; a repeated one counts once,
the last. JSON, as Everything's: `{"totalResults": N, "results": [{"type": "file" |
"folder", "name": "...", "path": "<folder>", "size": "<bytes>", "date_modified":
"<FILETIME>"}]}`, with `path`, `size` and `date_modified` only when their `_column` is 1;
size and date are strings, the date in 100 ns steps since 1601 (a folder's size and an
unknown date are `""`). An error is `{"error": "..."}` with 400 (bad parameters, a bad
regular expression) or 503 (the backend failed). Without `json` the answer is an HTML page
(no script; its CSP is `default-src 'none'; style-src 'unsafe-inline'; form-action 'self'`)
with a search form, the results, sort links on the column heads and Previous / Next.

Every request needs `Authorization: Bearer <daemon.token>`, Basic auth with the token as
the password (any user name), or Basic auth with the HTTP login from `keel-daemon
--set-http-login USER` (kept in the OS keychain, per profile); otherwise 401 with
`WWW-Authenticate: Basic realm="Keel"`. A wrong one waits 1 s before its 401. A request
with `Sec-Fetch-Site: cross-site` or `same-site` is refused (403). `/everything/file` is
403 unless `[web] allow_downloads = true` is in the profile's `config.toml` (read on each
request) and then sends the file through `file.get`'s checks (404 for what it refuses,
such as Keel's configuration folder). On a non-loopback bind there is no TLS: keel-daemon
warns at start and the HTML page shows "Not encrypted".

## CLI

```
keel search <query> [--max N] [--json]
keel du <path> [--top N] [--json]
keel du --scan <paths…> | --all-drives | --load-from <file> [--top N] [--save <file>] [--save-to <file.csv|.json>]
        [--save-types-to <file>] [--save-largest-to <file>] [--report <file.html> [--mail]] [--bom] [--size-on-disk]
        [--exclude GLOB…] [--exclude-path PATH…] [--exclude-regex RE] [--min-size SIZE] [--older-than AGE] [--newer-than AGE]
        [--no-hidden] [--no-system] [--follow-links | --follow-symlinks --follow-junctions --follow-mount-points]
        [--count-hardlinks-each] [--strict] [--quiet] [--json]
keel tag add|remove <tag> <paths…>
keel plan copy|move <src…> --to <dir> [--on-conflict skip|overwrite|rename] [--json]
keel plan delete <paths…> [--json]
keel plan merge <folders…> --into <dir> [--on-conflict skip|overwrite|rename] [--json]
keel plan flatten <folder> [--on-conflict …] [--json]
keel plan organize <folder> --by type|year|month [--on-conflict …] [--json]
keel plan attrs <paths…> [--hidden true|false] [--readonly true|false] [--mode 755] [--json]
keel plan link <target> <link> --kind symlink|junction|hardlink [--json]
keel execute [<plan id> --hash <hash>] [--no-wait]   # or: keel plan … | keel execute
keel sources [add <path> [--label L] [--no-index] | remove <id> [--delete-store] | index <id>]
keel library backup <file or folder>
keel library restore <file> [--as NAME]
keel devices | keel shares
keel mount <source id or label> <K:|folder> [--subtree PATH]
keel unmount <K:|folder>
keel mounts
keel daemon start|stop|status|rotate-token
keel mcp [--allow-execute]
keel filelist create <folder> -o <file> [filters] [--format F] [--no-header] [--bom]
keel filelist export <list> -o <file> [--query Q] [--format F] [--no-header] [--bom]
keel op [list]
keel op <name> [--params <json> | --params-file <file>] [--execute]
```

All take `--profile NAME` and `--json`; `--json` prints exactly one JSON document per
invocation (`sources add` returns the source with its index job under `job`; a failure
is `{"error": {"code", "message", "data"?}}`). `keel sources --json` lists each source as
`sources.list` does (`id`, `label`, `root`, `kind`, `status`, `indexed_at`, `last_seen`,
`detail`, `generation`) with its `library.stats` counts added: `files`, `folders`,
`bytes`, `hashed_files` and `last_walk`. `last_walk` duplicates `indexed_at` (the end of
the last completed walk, under the name `library.stats` uses), but is kept while the
source is offline, where `indexed_at` is absent. `keel du` answers as `library.folder_sizes`
does (the 1000 biggest children); a local folder in no source is walked instead (each
child's total and the folder's, in the same shape; a child folder that cannot be read has
no `bytes` and the folder's total counts what could be read); `--top N` keeps the N
biggest children (`truncated` true when there were more). `keel du --scan` (and
`--all-drives`, `--load`) uses the disk usage scanner instead, without the library
([Disk usage scans](files.md#disk-usage-scans)); its JSON is `{complete, elapsed_ms,
bytes, allocated, files, folders, items, unreadable, excluded, roots}`, each root
`{path, bytes, allocated, files, folders, items, newest?, free?, unknown?, notes?,
children, truncated}` and each child `{name, path?, kind ("dir", "file", "link"), bytes,
allocated, files, folders, items, newest?, notes?}` (`allocated`: bytes on disk;
`newest`: the newest change below, unix seconds; `free` and `unknown` only for drive
roots), with `written: [{what, path}]` (the files it wrote) and `errors` when one could
not be written ([Exports and reports](files.md#exports-and-reports)). Exit codes: 0 ok,
1 the operation failed, 2 usage, and for `keel du --strict` 3: written, but the scan is
partial. Typed at a terminal, a lone argument that names an existing folder opens that
folder in the window even when it is a subcommand name (`keel devices` where `devices` is
a folder), except `mcp`, `execute`, `daemon` and `search`, which are always the
subcommand; without a terminal (an agent starting `keel mcp`, a script) a name is never
taken as a folder. `keel ./name` always means the folder. `keel plan` prints the preview, the plan id and hash; `keel execute` reads them
from its arguments or from piped `keel plan` output (text or `--json`) and waits for the
job. Other mutating subcommands (`tag`, `sources add|remove|index`, `library backup|restore`, `mount`, `unmount`) print the preview and
confirm it themselves: typing the command is the confirmation. Plans are kept in the
library folder (`api-plans.json`, owner-only; on Windows a file owned by the user, the
token's default owner or Administrators, with a protected DACL naming only them and
SYSTEM; the hash covers everything a plan runs and is
checked again at `execute`, so a plan altered there is refused), so `keel plan` and a later `keel execute` work without
a daemon too. Release builds on Windows are GUI programs that attach to the calling
console, which does not wait for them: pipe the output (`keel … | more`, PowerShell
`keel … | Out-Host`) to see it in order. Redirected output (`--json` into a file or a
program, `keel mcp` under an agent) is unaffected.

### `keel op`: any operation by name

`keel op <name>` calls any operation in the [list above](#operations) with its parameters
as a JSON object, given as `--params '<json>'` or in a file with `--params-file <file>`
(default `{}`; paths absolute, as everywhere in the API). `keel op list` (or `keel op`
alone) lists every operation with its summary and whether it previews; in JSON each entry
also has `mutating`, the parameters' JSON schema (`params`) and an `example`. `list` is
also an operation (a folder listing): `keel op list --params '{"path": "…"}'` calls it.

- A read (and the few operations that act directly, such as `shares.revoke`) prints its
  result.
- An operation that previews (every other mutating one, and `plan`) prints the preview
  (summary, changes, warnings, plan id and input hash) and exits 0; nothing changes.
  Apply exactly that preview with
  `keel op execute --params '{"plan_id": "…", "input_hash": "…"}'` (or
  `keel execute <plan id> --hash <hash>`), which waits for a job as `keel execute` does.
- `--execute` previews, prints the preview, then executes that plan (its `plan_id` and
  `input_hash`) in the same run, waiting for its job. Typing it is the confirmation, as
  with `keel tag`. It is refused (usage) on an operation that does not preview.

The output is JSON whenever stdout is not a terminal (or with `--json`): the result, the
preview, or with `--execute` one document `{"preview": …, "executed": …}` (`executed` is
the finished job, or `execute`'s answer); an execute that fails prints
`{"error": …, "preview": …}`. Exit codes are those of the other subcommands: 2 for an
unknown name, parameters that are not a JSON object, or `--execute` without a preview; 1
when the operation refuses its parameters (the error names the field:
``missing field `tags` ``, ``unknown field `tag` ``) or fails. Like the other subcommands it
goes through keel-daemon when one runs for the profile, else opens the library
in-process. [The agent skill](agent-skill.md) teaches an agent this protocol.

```
$ keel op tags.set --params-file tag.json
Set the tags of 1 item(s) to [receipts]
  tags.set D:\Scans\r1.pdf  [receipts]
Warnings:
  ! tag "receipts" does not exist yet and will be created
plan: 13c5dd406fb8e46347589df153de3cc3
hash: 33a24e08724cf73a2cf5f81bb3e2663808e6330cf26227e8a5315e5815f5d2b4
Apply with: keel execute 13c5dd406fb8e46347589df153de3cc3 --hash 33a24e08724cf73a2cf5f81bb3e2663808e6330cf26227e8a5315e5815f5d2b4
```

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

Agents that run shell commands rather than MCP tools can load [the agent
skill](agent-skill.md) instead: it teaches `keel op` with the same preview-then-execute
rules. Install it by copying the repository's `skills/keel` folder into the agent's skills
folder (Claude Code: `~/.claude/skills/keel/SKILL.md`, or `.claude/skills/keel/` in a
project).

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
