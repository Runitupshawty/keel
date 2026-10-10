# Changelog

All notable changes to Keel are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Large uploads to Google Drive, Dropbox and S3 go in 8 MiB chunks through each service's upload sessions: Drive resumable uploads, Dropbox upload sessions (`upload_session/start`, `append_v2`, `finish`) and S3 multipart uploads. A file of at most 8 MiB still goes in one request. Each chunk is retried on its own (rate limits, server errors, a lost connection, an expired sign-in), and a Drive or Dropbox upload whose connection dropped mid-chunk carries on from what the service already holds instead of starting over. Copying a large file to the cloud now moves the Jobs panel's byte count while it uploads, and at most one chunk of the file is held in memory.
- Remote and cloud library sources follow changes between walks. Every 2 minutes Keel asks each one what changed and applies it to the index the way a local folder's changes are applied: new, changed, renamed, moved and deleted files and folders show up in library views and search without walking the source, a new folder is indexed with everything in it, and the protection counters are recounted once per batch of changes. Google Drive sources read Drive's change feed (`changes.list`, with removed and trashed files), Dropbox sources theirs (`list_folder/continue`). Each source keeps its position in the feed with its index, taken just before each walk so nothing that changes during the walk is missed. A source is walked again at once when the service no longer accepts that position (an old Drive page token, a Dropbox `reset`) or reports a change it cannot place, and every 6 hours like a local source.
- SFTP sources are checked cheaply every 2 minutes: every indexed folder's modified time is read, and only the folders whose time moved are listed again (a POSIX server moves a folder's time when an entry is added, removed or renamed in it). New, deleted and renamed files and new folders appear without a walk; a full walk still runs every 6 hours.
- S3 sources whose bucket (or prefix) holds at most 1,000 objects are checked with one list request every 2 minutes and walked only when the object count, total size or newest modification time changed. Bigger buckets and WebDAV accounts are walked every 5 minutes, or at the check interval when that is longer (they were walked every 15 minutes).
- Settings → Library → **Check remote sources every** (`[library] remote_poll_secs` in the profile's `config.toml`, 120 seconds by default, 30 seconds to an hour) sets how often remote and cloud sources are asked what changed. It replaces **Rescan remote sources** (`rescan_minutes`, which is no longer read). keel-daemon reads it when it starts.
- keel-vfs: `Provider::changes` (a change feed that stops when its cancel flag is set: `ChangeFeed`, `ChangeCursor`, `ChangedPath`, `ChangeKind`, and `FeedError::Unsupported` or `CursorRejected`) and `Provider::folder_times_track_entries`. keel-core: `Library::set_remote_poll`, `WatchConfig::walk` and `WatchConfig::for_source`, `WALK_INTERVAL`; `POLL_INTERVAL` is now 2 minutes.
- Copy and move jobs record their progress durably as they go (every 256 files or 4 MiB, in one row of the library's job table plus the files of the running batch, never one write per file): which files were placed, at which name, with their size and modified time, and how far the file in progress got in its `.keel-partial` copy. A copy or move resumed after Keel or keel-daemon closed, or after a crash, continues from there and its log says "resumed at file N of M".
- Device writes reach shared sources that are not local: a read-write grant on an SFTP host or a cloud account lets the device write, make folders, rename and delete there, through this machine's connection to that server, with the same grant and path checks (inside the shared folder, no links, no `..`) as a local folder. A file streams to the server (never held whole), is checked with BLAKE3 and placed by the server's own upload only once complete; pieces of a file resume from a staging file beside the target where the server can write in place (SFTP). The `shares.grant` preview of a read-write share warns `deletes_permanent` when deletes there are for good (SFTP, S3).
- keel-vfs: `Provider::write_at` (write a file in place from an offset, for resumable transfers; local folders, SFTP and the test memory provider) and `ops::transfer_resumable` with its `Journal`.
- Editing inside zip and jar archives: Delete, Rename (F2), Cut and Paste, drag and drop, New folder and New file work in a folder inside a `.zip` or `.jar` on this computer, and files and folders from anywhere (a local folder, an SFTP host, a cloud account, another archive) can be pasted or dropped into one. Each operation rewrites the archive once: the new archive is written beside it (`<name>.keel-partial-<pid>-<n>`), synced to disk and renamed over the old one, so a cancel, an error or a crash leaves the original as it was. Entries that stay are copied byte for byte, never decompressed: stored entries stay stored, password-protected entries are never decrypted (and keep working with their password), extra fields, comments and zip64 records are kept, and archives with more than 65,535 entries or over 4 GiB are written with zip64 records. A copy or move inside one archive copies the compressed bytes; new files are deflated. Deleted entries go for good (the confirmation says so) and their folder stays, even when it ends up empty. The job shows the progress of the rewrite by bytes, and name clashes ask Skip / Overwrite / Keep both as for other folders.
- Library operations on archive entries: `plan` previews deletes, renames and copies or moves into a zip with a `rewrites_archive` warning naming the archive and its size ("rewrites the 1.2 GB archive …"), counts the files and bytes under a folder inside the archive, and refuses writes into archives that cannot be changed and moves out of an archive in the preview. A delete of several entries of one zip is one rewrite.
- keel-vfs: `archive::zipedit` (the rewriter, `editable`, `remove_entries`, `rename_entry`, `mkdir_entry`), `ops::remove_entries`, and `archive::editable`, which names why an archive is read-only.
- Extracting into a folder on an SFTP host, or a cloud account (Google Drive, Dropbox, S3, WebDAV): paste or drag entries copied inside an archive into such a folder, use **Extract here** and **Extract to folder** on an archive that lives there, or the new **Extract to the other pane** (context menu and command palette) with the target folder open in the other pane. Each entry is streamed from the archive through the provider in one pass, without extracting the archive to disk first: it is written as a staging file beside its target, its size checked against the archive's and then renamed into place, so a cancel or an error leaves no partial file. Name clashes ask Skip / Overwrite / Keep both, folders merge, and the job shows progress by entries and bytes. **Extract to…** still opens the folder picker for folders on this computer.
- keel-vfs: `ops::extract_to` extracts into a folder on any provider; `memory::MemoryProvider` (feature `test-util`) can now be written to.

### Fixed

- The .deb package depends on `libxkbcommon-x11-0` and the Linux install script says when it is missing: on an X11 session the window needs it and exited at once without it ("Library libxkbcommon-x11.so could not be loaded"); Wayland sessions were not affected. Found on the first run of a release build on real Ubuntu hardware, which also showed the window, the drives and the listings working there.
- Files of any size can be uploaded to Google Drive and Dropbox, which lifts the 0.3.0 known limitation that uploads were single requests capped at 256 MB (Drive) and 150 MB (Dropbox) and held in memory. An S3 upload that fails partway is aborted, so its parts are not left in the bucket (and billed) until a lifecycle rule removes them; a Drive or Dropbox session that is not finished expires on its own.
- Cancelling a job that uploads to Google Drive, Dropbox, S3 or WebDAV now stops the request already on the wire (the connection is closed) instead of letting it finish first, so an upload stops within its current chunk. Nothing more is sent afterwards except the abort of an unfinished S3 multipart upload, and the job's error says what was cancelled: the file, how much of it had been sent, and what became of the partial upload. This lifts the 0.3.0 known limitation that a cancelled upload's request in flight finished first.
- Remote and cloud sources no longer wait up to 15 minutes for a full re-walk to show changes: Google Drive and Dropbox follow their change feeds and SFTP sources their folders' times, which lifts the 0.6.0 known limitation that remote and cloud sources were found changed only by re-walking on a poll interval. S3 buckets of up to 1,000 objects are walked only when they changed; bigger buckets and WebDAV accounts are still walked, every 5 minutes.
- A folder copy or move resumed after a crash no longer runs again as a merge into the half-written target, which lifts the 0.6.0 known limitation. Files it had placed are skipped without being written again, the file it was writing is continued from its partial copy on local folders and SFTP hosts (started again on cloud targets, or when the partial copy no longer matches the record), a name picked by Rename new is kept instead of a third copy being made, and a placed file that changed in the meantime stops the job ("changed since the preview") instead of being overwritten. A move still deletes each source file only after its copy is complete and verified.
- Writes from a device into a shared source that is not local are no longer refused, which lifts the 0.8.0 known limitation that they landed only in local sources. A read-only or unreachable source refuses the write, as before.
- The 0.2.0 known limitation that nothing can be deleted, renamed or created inside an archive is lifted for zip and jar archives on this computer. 7z, tar (all compressions) and RAR archives, archives inside archives and archives on SFTP hosts or cloud accounts stay read-only; a write there is refused with a message naming the format (or saying that the archive is inside another one or not on this computer). Add to "name"… still writes 7z, tar and tar.gz archives as before.
- The 0.2.0 known limitation that extracting into a remote folder is not supported is lifted: archives extract into folders on SFTP hosts and cloud accounts.
- TIFF photos over 64 MiB get their EXIF orientation in thumbnails and media view tiles, lifting the 0.7.0 known limitation. The orientation of every TIFF is now read from the file's header and first directory (a few KiB, little- or big-endian, classic or BigTIFF) instead of the whole file; capture time, camera and the other EXIF fields of TIFFs over 64 MiB are still not read.
- **New file** on an SFTP host or a cloud account now creates the file. The empty upload was never committed, so the file was discarded.
- A Google Drive source no longer misses deletions and moves when a check stops partway (a request fails, or applying a change fails) and reads the same part of Drive's change feed again, or when two sources follow the same Drive account: Keel keeps the last 16 pages of the feed it read and answers each one with the same changes whenever it is read again. Before, a deleted file could stay in search and a moved one stay at its old path until the 6-hour walk.
- Closing the library, closing Keel or keel-daemon, or removing a Google Drive, Dropbox or S3 source no longer waits for change-feed requests already under way (minutes for the first listing of a large drive, or on a network that does not answer): Drive and Dropbox requests stop at once, an S3 check before its next retry.
- A Google Drive source no longer lists the whole drive before every walk; the drive is listed once per account while Keel runs (again only when Drive no longer accepts the saved position), so a walk that keeps failing no longer lists the drive every 2 minutes.
- A remote or cloud source whose host or service stops answering between walks is shown offline at the next check (the failed check walks it at once) instead of staying online until the 6-hour walk. A walk that fails is tried again at the next check, then after twice as long each time, up to 30 minutes.
- SFTP checks read at most 2,000 folders' times each, taking the folders in turn from where the last check stopped, so a source of tens of thousands of folders no longer keeps its server busy with one check after another. A change in a folder is no longer missed when the folder above it was listed again first.
- `[library] remote_poll_secs` outside 30 seconds to an hour (set by hand in `config.toml`) is clamped to that range in the app and keel-daemon instead of asking every source as often as every second.
- A copy or move into a zip that stopped after the archive was rewritten (Keel or keel-daemon closed or crashed before the job recorded it) no longer adds its files again as "name (2)" when it resumes, nor leaves moved sources behind: what the rewrite places is recorded before it runs, and a resumed move deletes what is left of its sources.
- An upload over 8 MiB that must not replace an existing file, to an S3-compatible store without conditional writes, fails at once with "conditional writes unsupported by this store" instead of being retried five times and failing with "cloud HTTP 501". Keel's own requests to cloud services (uploads, change feeds, quota and links) no longer retry a 501 (Not Implemented) answer.

### Known limitations

- S3 objects can be at most about 78 GiB (10,000 parts of 8 MiB); a bigger upload stops with an error naming the limit.
- The job's byte count can run up to one chunk (8 MiB) ahead of what the service has received, and a file of at most 8 MiB is counted before its one request is sent.
- A Drive or Dropbox upload session that is cancelled or fails is not deleted; the service drops it after about a week.
- On SFTP, a change inside an existing file (same name, nothing added or removed in its folder) is found by the 6-hour walk, as is an entry added in the same second the folder was last listed (SFTP folder times are whole seconds). Each check reads at most 2,000 folders' times, one after another: on a source with more folders each one is read every few checks (50,000 folders: every 25 checks, about 50 minutes at the default interval).
- A watched Google Drive source keeps every item of the drive in memory by id (about 100 bytes each) to place moves and renames.
- Dropbox sources are asked every check interval; Dropbox's long poll is not used.
- A Google Drive source more than 16 pages of the account's change feed behind another source on the same account (up to 16,000 changes) is walked again instead of reading those pages.
- An S3-compatible store that ignores `If-None-Match` replaces an existing file of the same name with an upload that should not (Keep both, **New file**); Keel cannot tell such a store from one that honours it (this was already so for uploads of at most 8 MiB).
- A new **Check remote sources every** value applies from the next start, like the rescan interval it replaces.
- After a crash (not a close or a cancel), the files a copy or move placed into a folder that existed before it, since its last record (at most 256 files or 4 MiB), are not known to be its own: they get the conflict choice again (Skip leaves them, Overwrite writes them again, Rename new adds a second copy).

## [0.12.0] - 2026-10-10

### Added

- SFTP remotes read the OpenSSH client configuration (`~/.ssh/config`). The Host field in Settings → Remotes can be an alias; `HostName`, `User`, `Port`, `IdentityFile`, `IdentitiesOnly` and `ServerAliveInterval` apply, with `Host` patterns (wildcards, several patterns, `!` exclusions), quoting, `keyword=value` and `Include`. A user name, port or key file set in Keel wins over the config. Each remote has a "Use ~/.ssh/config" switch (on by default, also for remotes saved earlier), and the editor shows what the host resolves to under the Host field, or the alias and config line of an error.
- SFTP jump hosts: `ProxyJump` routes of one or several hops are followed, each hop reached through a forwarding channel of the one before. Every hop has its own host-key check and first-connection prompt (the prompt now names the host and port whose key it is); jump hosts sign in with the agent or keys, never with a remote's password (a password remote's jump hosts try the SSH agent's keys first, then the key files).
- Content hashing reads SFTP sources: each file is streamed once through the source's connection (1 MiB reads, one remote file at a time, idle priority, checkpoints and resume as for local files) and gets the same BLAKE3 content id as a local copy of the same bytes. Duplicates, the copies badges, the Protection card and the last-copy warning now see copies between this PC and a file server; a copy on an SSH host counts in that host's failure domain (two sources on one host are one domain), so a file here and on the server is 2 copies in 2 domains. Settings → Library → Hashing gains **Remote hashing** (on by default), **Cloud hashing** (Google Drive, Dropbox, S3 and WebDAV; off by default, because every file is downloaded and downloads can cost egress fees) and **Remote size cap** (1024 MiB by default; bigger remote files stay unhashed and the hash job's log names them). "Hash now" follows them. Service checksums (ETag, MD5, Dropbox's content hash) are never used as content ids: they would never match a BLAKE3 id.
- `hashing.set` takes optional `remote`, `cloud` and `max_remote_bytes` (omitted: unchanged, kept with the library), and its preview states all three.
- Cloud storage quota: Google Drive and Dropbox accounts show how much of their storage is used ("12.3 GB of 15 GB used", or "used (no limit)") when you hover over the account in the sidebar and in Settings → Cloud. The quota is asked on a worker when it is shown and kept for 10 minutes per account; **Refresh** in Settings → Cloud asks again. Drive reads `about.storageQuota`, Dropbox `users/get_space_usage` (the individual or team allocation, or a team member's own limit). S3 and WebDAV accounts report none; a failed request shows "Quota unknown".
- **Copy link** in the context menu of cloud files and folders. Google Drive copies the file's own link without changing its sharing, and the toast says it opens only for people who already have access. Dropbox reuses an existing shared link, and only when there is none asks "Create a link anyone can open?" before making one. S3 asks first, then copies a presigned download link that works for 1 hour (files only). Local files, WebDAV accounts and S3 folders do not show it. The link goes to the clipboard like Copy path.
- keel-vfs: `Provider::quota` (`Quota { used, total }`) and `Provider::share_link` (a link, or a question to ask before making one); both default to none.
- Per-source counts: the library Overview has a **Per source** table (files, folders, size, share of files hashed, last walk, offline sources flagged; a name opens the source), `library.stats` answers `per_source` (each source's `id`, `label`, `files`, `folders`, `bytes`, `hashed_files`, `last_walk`, `offline`), `keel sources` prints each source's counts under it and adds them to every source in `--json`, and the web client shows them under its sources (**Counts**). They come from each store's counters and indexes, about 2 ms for 100,000 records. This lifts the 0.6.0 known limitation that the Overview showed library totals only.

### Fixed

- The 0.2.0 known limitation "`~/.ssh/config` is not read, so there is no jump-host or ProxyCommand support" is lifted for the config and jump hosts. `ProxyCommand` stays unsupported on purpose: it is refused with an error naming the alias and config line, because Keel never runs programs from the SSH configuration.
- Hashing no longer skips SFTP sources, which lifts the 0.6.0 known limitation that only sources on a local path were hashed; cloud sources are hashed when Cloud hashing is turned on. A remote host that is offline leaves its files unhashed without failing the job, and the next run hashes them; a remote file that cannot be read is skipped, counted as unreadable in the job's result and tried again on the next run.
- Tabs on `node://` sources are titled with the device's name at its root and "<device name> / <source label>" in a shared source, and the breadcrumb and the tab's hover show the names instead of the raw ids; only a device that is no longer paired (forgotten) still shows its id. The web client's source list and path box show the same names on hover. This lifts the 0.8.0 known limitation about raw ids in tab titles.
- Cloud accounts now show their storage quota (Google Drive, Dropbox) and cloud files have Copy link, lifting the 0.3.0 known limitations about the missing quota and share links.
- Protection counters follow changes made outside Keel: 5 s after the last change a watcher applied (a file created, modified, deleted, renamed or moved), and after operations Keel executes, the counters are recounted in the background, once for a whole burst (at most a minute after its first change, however long it goes on) and not while that source is being walked (the walk recounts when it ends). The Overview's Protection card and the Copies badges are read again afterwards, in the window and in windows attached to keel-daemon, which sends `library.changed` `{method: "protection", kind: "protection.recount"}` after every recount (the web client, which shows no protection, ignores it). A window's own source watchers now recount after each full walk too. A recount reads every store (about 0.12 s for 100,000 hashed files in a release build). This lifts the 0.7.0 known limitation that the counters caught up only at the next index, hash or integrity run.
- A video whose strip (the hover and viewer frames) times out is no longer run through `ffmpeg` again every session: the timeout is remembered in the file's sidecar `meta.json` and the strip is tried again after 7 days or once the file changes (its size, when the record is by content id: copies of one video share it). The viewer's **⋯** menu has **Retry strip** to run it again at once (it also retries a strip that failed). This lifts the 0.7.0 known limitation that a timed-out strip was retried each session.
- An SSH configuration that starts with a byte-order mark, or has a misspelt keyword (such as `Hots`), no longer makes its first block apply to every remote: the mark is skipped and an unknown keyword is an error naming the alias and line, as in OpenSSH (`IgnoreUnknown` is honoured). Before, every remote could be sent, with its password, to the first block's host.
- Resolving deeply nested `ProxyJump` routes no longer takes seconds to minutes (each later hop's own route was resolved and thrown away), and an `Include` or `IdentityFile` naming a huge file, a pipe or a device is refused instead of read without limit.
- Turning **Cloud hashing** or **Remote hashing** off now stops a running hash job's downloads at once, instead of after every remaining file of the source.
- A remote source holding a file over the size cap, or a file that cannot be read, no longer starts a hash job after every poll (every 15 minutes, forever): over-cap files are left out, and a failed read is retried after an hour, then 2, 4 and so on up to 24 hours, or as soon as the file changes.
- Removing a remote source while one of its files is hashed no longer cancels the whole hash job; the other sources are still hashed.
- Keel's data folder inside a watched source (a home-folder source on Windows or Linux) no longer recounts the protection counters every few seconds forever: a recount writes the library, and that write no longer counts as a change. Changes to ignored paths and files whose index entry did not change no longer recount either.
- Steady changes anywhere (a log written every few seconds) no longer hold back protection recounts for hours: they run at most a minute after the first change.
- Closing a library now waits for a protection recount in progress, so it can be opened again right away.
- **Copy link** acts on the item you right-clicked even with several selected, its question names the file, and asking for a second link while a question is open no longer replaces that question (a note says to answer it first).
- Paired devices' names no longer stay on `node://` tabs after Devices is turned off or the profile changes, and long device names and source labels are cut to 64 characters in tab titles and breadcrumbs (the hover shows them whole).
- A `hashing.set` from `keel` or another client is no longer undone the next time a window opens the library: the remote hashing settings are the library's, `library.stats` reports them (`hashing`), and a window adopts them and sends them only when you change them.
- Cloud quota and Copy link requests stop once their account is removed or replaced, instead of retrying in the background.
- The Retry strip hover and the strip timeout message no longer have a run of spaces in the middle.

### Known limitations

- `Match` blocks in `~/.ssh/config` are skipped (the editor notes it), `UserKnownHostsFile` is ignored (host keys stay in `~/.ssh/known_hosts`), the system-wide SSH configuration is not read, and `Include` wildcards work in file names only. A jump host that needs a password is not supported.
- Two remotes in Settings → Remotes that name the same server count as two failure domains; give their volumes one failure domain by hand in the drive inventory.
- Remote files are hashed whole and one at a time across all hosts, so a large server share takes as long as downloading it once.
- A remote file whose read failed waits out its retry delay only until Keel restarts (the delay is kept in memory).
- An executed operation whose steps are more than 5 s apart (a copy of several large files) recounts the protection counters between steps rather than once at its end.

## [0.11.1] - 2026-10-10

### Fixed

- The release packages (the Windows zip, the macOS and Linux tarballs and the .deb) now include `keel-daemon` next to `keel`, and the installers link or stop it like `keel`. Since 0.8.0 they shipped only `keel`, so from an installed build `keel daemon start`, Settings → Library → *Run the library in a background daemon*, mounts, the web client and phones failed with "keel-daemon was not found next to keel" unless Keel was built from source. `keel` also resolves its own path through the installer's symlink before looking for the daemon next to it.

## [0.11.0] - 2026-10-10

### Added

- Attached to keel-daemon, the media grid and viewer show the daemon's sidecars instead of decoding thumbnails in the window: `media.thumb` through the same bounded uploader, kept in a small cache under the window's cache folder by content id and size. A missing 256 px thumbnail starts the source's sidecar job (`media.index`, once per source while it runs) and is asked for again on the job's news; the daemon now sends `library.changed` `{method: "job", kind: "media.index", job, done}` every 2 s while a sidecar job runs and when it ends. A file the job could not make one for, or a file in no source, is decoded in the window; 1024 px thumbnails are made by the daemon on request. `media.thumb` takes `content_id` (answer from a sidecar made earlier for that content) and `make` (false: only an existing sidecar).
- `activity.note` (acts directly): idle-only hashing and integrity checks pause for 5 s and sidecar (thumbnail) jobs for 1 s. An attached window sends it on input (at most every 4 s), so the daemon's idle jobs pause while you work, as in-process.
- `devices.settings` and previewed `devices.settings_set`: the device name, Spacedrop inbox and always-accept list change on a running daemon (no restart); relays answer `restart: true`, and the daemon now reads `[devices] relay` when it starts. An attached window writes Settings → Devices through them as you change it (53 operations).
- Web client: a `library.changed` reads again only what its kind can change (the selected file for tags and favorites, jobs for job starters, sources and listings for added or removed sources, devices for pairing and shares, the inbox for Spacedrop answers; nothing for recents, volumes, mounts or sidecar-job news; everything for an unknown kind), and changes arriving within 250 ms are read once.
- Video playback in the media viewer, with sound: Space or Play plays and pauses, a click on the progress bar or a strip frame seeks, Up/Down set the volume, M mutes, L loops, and the elapsed and total time are shown. Frames are decoded by `ffmpeg` at the viewer's size (at most 1920 px) into a 3-frame queue (64 MiB at most) and the sound plays through the default output device (`cpal`), which is the clock the frames follow (late frames are dropped). Seeking, moving to another file or closing the viewer kills both `ffmpeg` processes and waits for them. Without `ffmpeg`, or for a file it cannot read, the viewer keeps its still frames and the system player (Enter or Open), with a note saying why.

### Fixed

- Attached, the media grid no longer stays blank while you work: thumbnails are asked with `media.thumb` `make: true`, so the daemon decodes a missing one on demand (visible tiles first, at most four at a time per window) instead of the window waiting for the source's sidecar job, which paused after every `activity.note`. The sidecar jobs the window starts on open only fill the store ahead, and the window no longer uses their news. Sidecar jobs now pause for 1 s after input (hashing and integrity checks still 5 s), so they run between an attached window's notes.
- Attached, a content id from the daemon is checked (64 hex digits) before it names a file in the window's thumbnail cache.
- Attached, Spacedrops land in the same folder whether or not a window is attached: the window sends its inbox to the daemon only when one is set in Settings → Devices; otherwise the daemon's own (`[devices] inbox`, default `<data dir>/inbox`) applies.
- Video playback: Play and seeks before a remote video is downloaded no longer download it again for each (the seeks wait for the one download and the last one wins), closing the viewer or moving on cancels a download still running, and the viewer shows "Downloading…" with the progress the provider reports.
- Video playback: a paused video no longer keeps the sound output running (the stream pauses with it); closing or seeking no longer waits on the UI thread for a killed `ffmpeg` stuck reading a network file (processes are reaped on a thread, and those that end by themselves at once); a crash of the sound thread no longer freezes the picture; videos with non-square pixels (anamorphic DVD and broadcast files) are no longer stretched; the picture no longer runs ahead of the sound by the device's buffer; the 64 MiB frame budget now counts the read buffer too.
- `devices.settings_set` refuses an inbox folder that holds Keel's configuration or data folder (not only one inside them), and checks the device name in its preview instead of failing at execute.
- The .deb package depends on `libasound2 | libasound2t64` (the sound output links it; without it the app did not start).

## [0.10.0] - 2026-10-10

### Added

- The window attaches to keel-daemon: when the profile's daemon runs, the window works through its API instead of opening the library itself, so the `keel` subcommands, `keel mcp` and other windows keep working while it is open (the 0.8.0 known limitation). Sources, `library://` tabs, search, tags, favorites, recents, the jobs panel (live progress through `subscribe`), previews and file operations (confirmed in the window's own preview dialog), duplicates, copies badges, protection, the drive inventory, hashing, integrity and media jobs and Devices (pairing, shares, Spacedrop, `node://` tabs) all go through the daemon; the Overview says "Connected to the daemon". Closing the window leaves the daemon running. Settings → Library → "Run the library in a background daemon" (off by default) starts `keel-daemon --profile <name>` when none runs. If the daemon stops, a banner offers Reconnect or Open in this window, and nothing is written until one is chosen.
- API operations for it: `tags.tagged`, `views.list`, `recents.note` (acts directly), `redundancy.folder`, `library.stats`, `protection.summary`, `volumes.list`, previewed `volumes.set`, `integrity.check`, `hashing.set` and `media.index`; `sources.index` takes `adopt`, `tags.add` a `color`, and redundancy locations carry their volume's state, backup mark and `claimed` flag (50 operations).
- keel-daemon keeps every source current (watched like the window does, hashing after each walk), its device serves the library's sources to granted devices and browses paired devices' `node://` paths, and subscribers get `daemon.stopping` when it stops. It is also a library crate, so clients' tests can run a daemon in-process.
- Desktop app: Mount… on a library source (sidebar context menu, and Mount this folder… on a folder in a `library://` tab). The dialog picks a free drive letter (K: to Z:) on Windows or a folder (default `~/Keel Mounts/<label>`) elsewhere, shows the subtree read-only, previews `mounts.add` through keel-daemon, asks to confirm, then executes. Mounted sources show a "mounted K:" badge and an Unmount entry. A stopped daemon or a daemon without a mount backend is a toast with the `keel daemon start` hint.
- CI: a `web-e2e` job (not part of `check`) runs the web client in Chromium under Playwright against a fixture keel-daemon: sign in, open a source, search, preview a text file, preview a delete and cancel it, service worker ready, no console errors. Run it locally with `tests/web-e2e/run.sh` (README, Contributing).

### Fixed

- An attached window no longer makes every client refresh everything once a minute: `library.changed` now names the change (`kind`: the executed operation, or `recents.note` / `shares.revoke`) and is not sent when nothing changed (an `execute` that started no job and returned nothing, such as an integrity check that was not due); the window refreshes only what a kind can change and ignores job starters and policy changes, whose jobs refresh it when they end. While attached, keel-daemon runs the scheduled integrity checks itself (`[library] integrity_days` and `integrity_pct` from the profile's config.toml) and the window no longer asks for them.
- Reconnect and Open in this window right after the daemon said it stops no longer fail while it is still closing the library: they wait up to 15 s for it to let go, keep the read-only banner until the library is open again (and keep it, with the error, if that fails), and are disabled while they run. Before, Open in this window left the Overview saying the library is off, and Reconnect started a daemon that exited on the held library.
- "Hash now" while attached no longer turns off the daemon's *idle only* hashing.
- A tag created outside the tag picker while attached no longer fails: `tags.add` with no paths only creates the tag.
- Opening a file is announced to other clients (Recents), and a drive inventory change by another client refreshes the protection card.
- The window keeps at most four idle connections to the daemon, and closes its event subscription when it lets go of the daemon (library off, switching, Reconnect).
- A keel-daemon started by the window is reaped when it ends (no zombie process on Linux and macOS), and when it exits before answering the window says so at once, with the end of `daemon.log`, instead of waiting 15 s.
- The daemon is started with `--profile=<name>`, and profile names may no longer start with `-` (such a profile could not start its daemon).
- Mount…: the mount list is not polled while the daemon is lost; with the library open in this window the dialog and the error point to Settings → Library → "Run the library in a background daemon" (`keel daemon start` cannot work while the window holds the library); on Linux and macOS a new mount folder is made by the daemon when the confirmed mount runs (removed again if mounting fails), not before the preview.
- The default library name is `main`. A profile without `[library] name` that has no `main` yet but one existing library from before opens that one (the window then saves its name).
- CI `web-e2e`: the browser test drives the app through a test hook (`window.__keel`, built only with keel-web's `e2e` feature and enabled by `?e2e=1`; the release bundle is checked not to contain it) instead of clicking guessed positions on the canvas, which could not find the delete preview. It also previews a rename and cancels it, and checks that nothing was executed and the fixture is unchanged. `tests/web-e2e` has a `package-lock.json` (`npm ci`).

## [0.9.0] - 2026-10-10

### Added

- Mounts: keel-daemon serves a library source, or a folder in it, as a drive letter or mount folder (`keel mount <source> <K:|folder> [--subtree P]`, `keel unmount`, `keel mounts`; API `mounts.list`, previewed `mounts.add` and `mounts.remove`). Listings come from the index while the source is offline, reads are on-demand range reads, and writes are staged and replace the file atomically when it closes. Mounts are unmounted when the daemon stops. The backends (`winfsp` on Windows, `fuse` on Linux and macOS) are off by default: build `keel-daemon` with the feature and install the driver; without one, `mounts.add` fails with error -32008. The `winfsp` feature links GPL-3.0 code (see THIRD_PARTY.md).
- Phones: the web client installs as an app (a PWA: `manifest.webmanifest` with 192 and 512 px icons, standalone display, theme colours, and a service worker that caches only the app shell, keyed by a hash of the build, never `/rpc`, `/file/` downloads, `/share` or other answers). Below 700 px it switches to a phone layout: one pane or a media grid (pinch to resize the tiles), a bottom bar with Browse, Search, Library and Devices, the preview as a full-screen sheet with pinch zoom, double-tap and swipes between files, long-press menus, pull to refresh and larger touch targets. The desktop layout is unchanged above that width.
- Share → Keel: the installed app is a Web Share Target. The daemon parks the shared files under `<data dir>/shares` (512 MiB, 100 files, 4 waiting at most; cross-site posts refused) until the signed-in client claims them with `share.claim`, once, within 5 minutes; unclaimed uploads are deleted. The client then sends them to the paired device the user picks with the new, previewed `spacedrop.send`.
- `spacedrop.send` (preview lists every file and size, execute starts the drop job), `spacedrop.inbox` (waiting offers and what arrived) and `spacedrop.answer` (previewed accept or decline) in keel-api, so the daemon receives Spacedrops into `[devices] inbox` (default `<data dir>/inbox`, auto-accept from `[devices] auto_accept`) and the web client shows an Inbox with download links. `execute` now names the job of any operation that starts one.
- The web client warns when the page came over plain http from a non-loopback host, and README has a Phones section (same LAN, a tailnet with `--web <tailnet IP>:7421 --ws-allow-remote --web-host <name>`, a TLS reverse proxy).

### Fixed

- Mounts: a file being written is private to its writer. Other programs see the saved file in listings and get a sharing violation (`EBUSY`) when they open it until it is closed, instead of reading the half-written copy; a file being created does not show yet.
- Mounts (FUSE): a closed file is published before `close` returns (on flush, and on WinFsp cleanup), and its pending write stays until the publish finishes, so a program reading it right after `cp` gets the new content and a second writer cannot start from the old one.
- Mounts: a save that cannot be published is kept as `<name> (unsaved <date>).<ext>`, which is never cleaned up (it used to stay under a staging name that was deleted after a day), and the error reaches the program.
- Mounts: unmounting no longer publishes half-written files closed during teardown; a mount folder inside the folder it shows is refused; a slow remote download for a write no longer blocks listings of the whole mount.
- `Provider::rename_replace` documents its atomic-replace contract and refuses by default instead of falling back to a plain rename.
- Share → Keel: slow uploads can no longer hold the waiting places (a body under 64 KiB/s over 30 s is cut off, a stalled upload is dropped after 5 minutes), claimed shares from an earlier run are cleaned up on time, and shared file names drop bidi controls and every Windows device name (`COM¹`, `CONIN$`, …) and stay under 240 bytes.
- `spacedrop.send` sends only from library sources, the Spacedrop inbox and claimed shares, never from Keel's configuration or data folder (checked for every file, not only the paths given), never follows links inside folders, and sends exactly the previewed files (a folder that changed since the preview gives -32004 with a new preview).
- Web client: a share link no longer claims by itself (the client asks "Open N shared files?" first); "Send to device…" no longer drops a share in progress; pull to refresh is phone-only; the service worker cache is keyed by every shell file and the shell is loaded from the daemon first, so an update is never served stale. CI builds the release bundle and checks the service worker's version stamp.
- README counts 39 operations (with Spacedrop); `keel-daemon --help` describes `--web-host` for loopback binds behind a proxy too.

## [0.8.0] - 2026-10-10

Devices release: pair your own machines, browse and share folders between them, send files with Spacedrop, and drive the library from a daemon, a command line and an MCP server.

### Added

- Devices and pairing: pair two devices with a short code or a QR ticket. Codes last 10 minutes and work once, there is no account and no server beyond iroh's public relays, and each pair of devices keeps one connection.
- Remote sources: a paired device's sources open as `node://<device>/<source>/...` folders that list, preview and copy like any other, and can be added as indexed library sources.
- Grants: share a source or a subtree with a device as read or read-write; a revoke or downgrade takes effect immediately and cuts off running transfers.
- Spacedrop: send files and folders to a paired device in resumable 4 MiB chunks, verified with BLAKE3 over the whole file, staged in a `.keel-partial` folder and moved into the receiver's inbox folder only when complete. The receiver gets an accept prompt, with per-device auto-accept. Drops run as durable jobs that survive a dropped link or a restart.
- App: a Devices sidebar section (status dot, name, storage bar, Browse, Send files, Shares, Forget), a pairing dialog with the code and QR, a shares dialog, "Send with Spacedrop..." in the context menu, and Settings → Devices (enable, device name, inbox, relays, auto-accept list).
- `keel-api`, a typed operation registry with 29 operations and generated JSON schemas. Mutating operations are preview-first: `plan` returns a preview with a plan id and an input hash, and `execute` applies only that exact input, refusing a wrong hash, a changed source or a plan older than 10 minutes.
- `keel-daemon`, a headless host for the profile's library that serves the API as JSON-RPC on a per-user local socket, with an optional token-authenticated loopback WebSocket, notifications for job progress, library changes and device events, and a single daemon per profile.
- Command line subcommands: `keel search`, `keel tag`, `keel plan ... | keel execute`, `keel devices`, `keel shares`, `keel sources` and `keel daemon start|stop|status`, all with `--json`.
- `keel mcp`, an MCP server over stdio with one tool per operation, so Claude Code, Codex and other agents can use the library; every mutating tool returns a preview first. See [docs/api.md](docs/api.md).
- Library search media filters: `camera:`, `taken:`, `w:` / `h:`, `duration:`, `has:gps` and `kind:photo`, from the sidecar media rows. Words that match no name or path also match camera and keywords, ranked below name hits.
- Previews for PowerPoint (`.pptx`, text per slide) and OpenDocument (`.odt`, `.ods` with sheets by name, `.odp`) files. Word previews now keep bullet and numbered lists and page breaks. Previews stop at 200 slides or about 500 pages of paragraphs, read at most 64 MiB of decompressed content (a zip bomb gives an error), and report malformed or non-zip files as an error. `.ods` files are now shown as a document with one table per sheet instead of the spreadsheet grid.
- WebDAV cloud accounts (Nextcloud, ownCloud, Synology, Apache `mod_dav`): add one in Settings → Cloud with the collection URL, user name and password (kept in the OS keychain), test the connection first, then browse, upload, rename (MOVE), make folders (MKCOL) and delete like any other cloud account. No client id is needed; plain `http://` needs an explicit opt-in.
- Add to 7z, tar and tar.gz archives (not only zip): new entries are written to a staging file beside the archive and renamed over it, so cancel or an error leaves the original untouched. RAR stays read-only.
- One-line installers: `scripts/install.ps1` (Windows, per user, Start menu shortcut, optional Desktop shortcut and `PATH`, Settings, Apps entry, `-Uninstall`) and `scripts/install.sh` (macOS, Linux, `--uninstall`). Both verify the download against the release's `SHA256SUMS`.
- Releases now include `SHA256SUMS` and, for Linux, a `.deb` package.
- CI: `cargo deny` (licenses, advisories, bans, sources; `deny.toml`) is required via the `check` job, and pull requests get GitHub dependency review. The workspace crates are marked `publish = false`.
- Persistent name index for macOS and Linux (and the last resort on Windows): search works instantly without Spotlight or `locate`. It walks your home folder, keeps names, sizes and dates in SQLite under the cache folder, follows file changes live, and rebuilds weekly; the status bar shows "indexing N files…" while it builds. It also feeds the folder jump (Ctrl+P).
- Compress dialog: choose Zip, 7z, Tar or Tar.gz (the name's extension follows unless you edited it; the choice is remembered as `archive.default_format`). New **Add to "name"…** context-menu entry when you select one zip, 7z, tar or tar.gz plus items beside it, and dropping files on such an archive row does the same; both ask first and run as a job with progress and cancel.
- Open with… (Ctrl+Shift+O, context menu): Keel's own picker lists the recent apps for the file's extension, the apps the system knows for it (Linux: MIME associations; macOS: applications in /Applications and ~/Applications), Browse… for any executable or .app, and on Windows the system chooser. "Remember for .ext files" files the app in `open_with.recent` in config.toml. The context menu shows the last 5 apps for that extension in an "Open with" submenu. With several files selected, all of them open with the chosen app (one process per file, or one `open -a` on macOS).
- Bulk rename (Ctrl+F2, or "Bulk rename…" in the context menu and palette): a pattern with `{name}`, `{ext}`, `{n}` / `{n:3}` (counter with start and step), `{date}` and `{parent}`, find/replace (plain or regex, optionally case-insensitive) and a case transform, with a live old-to-new preview. Duplicate targets, names that collide with other files, invalid names, empty names and reserved Windows names are flagged and block Apply. Renames go through the folder's provider (local, SFTP, cloud), swaps and chains use temporary names, a partial failure lists the items that failed, and "Undo bulk rename" (toast button or palette) reverses it until the next operation.
- Browser client: `keel-daemon --web` serves the keel-web client (built into the daemon) on `http://127.0.0.1:7421/`: browse sources (offline ones from the index), search, preview text, images and PDF pages, tag, rename and delete through the same previews, follow jobs, see devices, and download files through one-time links. Sign in with the token from `daemon.token` (sent as the first WebSocket message, never in an address; "Remember on this device" is optional). Loopback only unless `--ws-allow-remote`, with `--web-host` for the names a remote bind answers to. See the README "Web client" section and [docs/api.md](docs/api.md).
- `keel daemon rotate-token`: a new daemon token; clients sign in again and sessions with the old token are closed.
- **Recycle Bin / Trash as a folder**: a "Recycle Bin" (Windows) or "Trash" (Linux) entry in the sidebar opens the current user's bin as a `trash://` tab with Name, Original location, Size and Deleted on columns. Restore puts items back where they were deleted from and reports a name clash instead of overwriting; Delete permanently and Empty ask first and say how many items go. Previews work for trashed files. Read-only otherwise (no paste, rename or new items). Not available on macOS yet.

### Changed

- Roadmap: Phases 6 to 8 are released and Phase 9 is in progress.
- Subcommands talk to the daemon when it runs for the profile and otherwise open the library in the same process.
- 7z support moved from the unmaintained `sevenz-rust` to `sevenz-rust2`.
- `syntect` is built with only what the previews use (bundled syntax and theme dumps, fancy-regex), which drops the unmaintained `yaml-rust` (RUSTSEC-2024-0320). The remaining `cargo deny` advisory ignores now name the crate that pulls each one and why it stays.

### Fixed

- Review fixes in `keel-net`: a device store is opened under an exclusive lock so a second process cannot change grants; pairing reveals nothing about either device before the other side proves it knows the code; names, labels and paths are validated on both sides; each peer has connection and request limits; stalled transfers are dropped after an idle timeout; and a request that fails because a connection closed under it is retried once.
- A move of a file or folder within one SFTP host now renames on the server (OpenSSH `posix-rename` when replacing, else the plain SFTP rename) instead of downloading and re-uploading it; folders move in one step. If the server refuses the rename (for example across devices) the move falls back to copy and delete. Conflict handling (skip, overwrite, keep both) is unchanged. This lifts the 0.6.0 known limitation about same-host SFTP moves; copies between hosts still stream through this PC.
- SFTP: `russh` 0.50 to 0.64 and `russh-sftp` 2.4 to 3.0, fixing unbounded memory allocation driven by a hostile server (RUSTSEC-2026-0154, RUSTSEC-2026-0153). Host key trust, key file, agent and password logins, keepalive and cancellation behave as before; a host certificate offered by a server is refused.
- Spreadsheet previews: `calamine` 0.26 to 0.36, which moves its XML parser to `quick-xml` 0.41 and fixes a quadratic-time attribute check and a namespace allocation DoS on crafted xlsx/ods files (RUSTSEC-2026-0194, RUSTSEC-2026-0195).
- 7z: the unmaintained `sevenz-rust` 0.6 is replaced by its maintained fork `sevenz-rust2` 0.23 (RUSTSEC-2026-0246; RUSTSEC-2026-0245 is a path traversal in its own extractor, which Keel never used). A test now proves that 7z, tar and zip entries named `../../evil.txt`, `/evil.txt` or `C:/evil.txt` refuse the whole extraction and nothing is written.
- Battery detection: `battery` 0.7 is replaced by its maintained fork `starship-battery` 0.12, which drops `nix` 0.19 (out-of-bounds write in `getgrouplist`, RUSTSEC-2021-0119; never called by Keel) and the unmaintained `mach` (RUSTSEC-2020-0168). The `power` feature is unchanged.
- Devices: a paired device's listing could claim any content id, so a decoy file hid the "last copy" warning for your only copy and showed as its duplicate. A device's ids are now kept apart as claims: listed in the copies hover, never counted as copies, never in Duplicates. A host no longer reports ids for files whose bytes drifted.
- Spacedrop: an accepted drop id let the sender push a different file list later without asking. An answer now holds for one device, drop id and file list; completed, cancelled or forgotten drops forget it.
- Spacedrop: unanswered offers held connection slots until answered, and accepting after the sender gave up left a staging folder behind. Offers are answered at once and the sender polls; a cancel withdraws the prompt; staging is created only for a sender still waiting, and stale staging is swept.
- Spacedrop: transfers are now in the op log on both sides (`net.drop-sent`, `net.drop-received` per file).
- Spacedrop: receiving many small files rewrote the drop's state after each one (quadratic); 2,000 files took about 38 s and now take about 3 s. Received files never replace an existing file, even when two drops land the same name at once; staging folders are hidden on Windows; an offer too big to send fails at once instead of retrying for ten minutes.
- API/MCP: `sources.remove` with `delete_store` deleted an index store (tags, favorites, hashes) in one direct call an agent could chain. It now previews like every mutating operation, stating the store's size, tags and favorites, and only `execute` removes; `keel sources remove` confirms it as before.
- API: the read operations (`read`, `stat`, `list`, `preview.render`, `media.thumb`, `file.get`) could open Keel's configuration folder (`daemon.token`) and UNC paths (sending the user's credentials to another server). They now refuse the configuration folder however the path is written, and UNC and device paths outside the library's sources.
- API: `read` on SFTP, cloud and device files re-read the file from the start on every call; it now asks the provider for the range. Other locations refuse an offset past 64 MiB.
- API: file-plan summaries (the string `keel mcp --allow-execute` clients repeat for approval) name the first three paths; `DaemonProvider` runs a plan only when its confirmation callback accepts it, and refuses a plan with warnings without one.
- CLI: `keel mcp` opened the window when the working folder had an `mcp` folder. `mcp`, `execute`, `daemon` and `search` are now always the subcommand, and without a terminal no name is taken as a folder.
- Config: `KEEL_NET_SECRET=memory` is honoured by `keel-daemon` and the CLI too; one switch turns devices on (`[devices] enabled`, the window's Settings switch; `[net] enabled` is the fallback); a configuration saved while Devices defaulted on is switched off once. The data folder is created owner-only.
- Windows: the owner-only check of `api-plans.json`, the token and the socket salt refused files made by an elevated process or an administrator account without UAC (owned by Administrators, or an account SDDL writes as an alias), so plans vanished and tokens were replaced at every start. Owners and access entries are now compared as SIDs.
- Spacedrop: when the reply to a drop's last piece was lost, the sender asked the user again, sent the last file a second time as `name (1).ext` and left staging behind. A completed drop is now remembered for an hour and answers the sender from that record.
- Spacedrop: one device could stack unlimited offers (each held in memory with up to 1 MiB of names, each a prompt). At most 4 offers per device and 16 in all wait now (more are refused as busy), and an offer its sender stopped polling is withdrawn with its prompt.
- Spacedrop: an invalid name or a staging failure on the receiver made the sender retry and re-prompt for ten minutes; it now fails at once with the reason. Names that are Windows short names (`KEEL-P~1`) are refused on Windows, and nothing is published through a folder that leads into a staging folder or out of the inbox.
- Spacedrop: the received-files marker is synced to disk, files found complete on resume are logged as sent, the op log is written out when the node closes (a failed batch is retried once), withdrawn prompts disappear at once, and the window's device event queue is bounded like the node's.
- Tests and caches: RAR extraction temp folders and SFTP, cloud and device downloads ignored `KEEL_DATA_DIR` and could land in the real `%LOCALAPPDATA%\Keel`. Every cache and temp root now comes from one resolver (`<KEEL_DATA_DIR>/cache`, else `<KEEL_CONFIG_DIR>/cache`, else a temp folder in tests, else the platform cache folder).
- `--web`: unauthenticated connections could each send 16 MiB messages and fill all 64 connection slots. Until `auth` a message may be at most 4 KiB, at most 16 connections may wait to sign in and at most 8 come from one remote address; a connection that does not sign in within 10 s gets error -32007 before it is closed (as documented). A remote bind checks the `Host` header against its IP and `--web-host` names.
- Devices: forgetting a device drops its pending offers and staging and removes its folders from the library; day-old device download folders are swept from the temp folder; devices are off until turned on in Settings (the identity is created then); received files and pairings are no longer lost from the event queue, and an idle window shows them at once; the offer prompt lists file names and cannot overflow its total; without a Downloads folder drops go to `<data dir>/inbox` instead of being declined silently.

### Security

- API plans: the hash covers everything a plan runs (for file operations also the changes and warnings) and is checked again at `execute`; `api-plans.json` is owner-only on every platform, so a plan altered on disk is refused.
- MCP: `execute` applies a plan only after the user confirmed it through the client (MCP elicitation, showing the summary, changes and warnings); a client that cannot ask is refused unless `keel mcp --allow-execute`, and then each call must repeat the preview's summary. Nothing but `ping` is answered before `initialize`; bad or oversized lines get an error and the session goes on.
- keel-daemon: the WebSocket handshake (`--ws`, and the request head on `--web`) has a 5 s deadline and at most 64 connections are served per listener; `daemon.token` is owner-only and a token file others can access is replaced; a request on the local socket must arrive within 30 s however slowly it trickles in; the socket name is derived from the user's SID or uid and a per-user random salt; `--profile` is validated.
- CLI: `--json` prints one JSON document per invocation; a lone argument naming an existing folder opens it even when it matches a subcommand (`keel ./search` always does); the in-process host keeps extracted archives under the data folder, brings keel-net online only for devices and shares, and registers the profile's SFTP hosts and cloud accounts.

### Known limitations

- The window does not yet attach to a running daemon; only one of them can hold the library at a time.
- Short-code pairing needs internet discovery; on an offline network use the full ticket.
- The daemon's router has no SFTP or cloud providers and does not watch folders live.
- Writes from a device land only in local sources.
- Tabs on `node://` sources show the raw id as their title.
- Video in the viewer shows stills, not playback.

## [0.7.0] - 2026-10-09

Media and protection release: a fast photo and video grid with a full-window viewer, and a Protection card that shows how many copies each file has and on how many physical disks or accounts.

### Added

- Media view (the pane toolbar's **Media** button; tile sizes S/M/L, Ctrl+wheel): a virtualized square-tile grid drawn from the sidecar thumbnails. Thumbnails are decoded on worker threads and uploaded at most 8 per frame; cached textures are capped at 384 MiB (and 4,000 textures); tiles that scroll away are dropped from the queue. In the perf test 129,000 items (5 % videos, real-size 256 px and 1024 px thumbnails, M and L tiles at 1.5x and 2x) scroll at a mean frame time of 0.5 to 1.5 ms. Videos show their strip and scrub across it on hover, and their thumbnail until the strip is made. EXIF orientation is applied. The **Dates** toggle groups tiles under capture-day headers.
- Viewer: Space or Enter opens the selected photo or video full-window (the 1024 px thumbnail at once, the full image decoded in the background at up to twice the screen size), Left/Right/Home/End navigate, mouse wheel, `+`/`-`, `0` (fit) and `1` (100 %) zoom and pan, `I` toggles the info panel (dimensions, capture time, camera, lens, GPS, duration, codec), `F` favorites, Esc or Space closes. Videos show still frames from the strip; Enter or Play opens the system player. A file deleted while open closes the viewer.
- With the library off, thumbnails live in their own cache (2 GiB budget) under the cache folder. With it on, the sidecar job ("Library: media thumbnails") runs for every local source after indexing and the grid reuses its thumbnails.
- Protection (library on): every fixed, removable, network and cloud volume is listed with its failure domain (physical disk serial, server, or cloud account), state (Online, Offline, Archived, Lost, Retired; the last three set by you in the drive inventory), capacity and last seen. `Redundancy` counts copies and failure domains: two copies on one disk or one account are one domain; hard links count once; copies on Archived volumes count (flagged offline); copies on Lost or Retired volumes and drifted files do not count. "Backed up" means a copy on a volume marked as backup in a second failure domain.
- The failure domain is editable in the drive inventory: type the same name on two volumes to make them one domain (two names of one server, a disk the detection splits), or another name to split them; clear the field to go back to the detected one.
- Overview → Protection card: files not checked yet (no content hash), files with one copy, files whose copies share one failure domain, files not backed up, files changed since their last check, offline volumes; each number explains itself on hover.
- Integrity job ("Library: checking integrity"): re-hashes a sample of files on a schedule and marks a file whose bytes changed while its size and times did not (`drift`); drift clears when the file really changes.
- Details view: a Copies column with the locations on hover.
- Delete and move previews warn `SingleDomain` when the remaining copies would all sit in one failure domain, `CopiesOffline` when they would all be on offline or archived drives, and `LastCopy` only counts copies that still exist.
- Cloud sources (S3, Drive, Dropbox) index through their paged listings and count as their own failure domain; a cut-off listing never deletes records.

### Changed

- Source stores migrate to schema v7 (`record.drift`); `library.db` to v6 (`volume.domain_set`, the failure domain set by hand).
- Media metadata (`media` table) is read by the grid and the viewer.
- Sidecar failures are recorded per kind in `meta.json` (`failed`); `error` is the metadata's own error.
- Hashing set to "pause on battery" no longer pauses on input, but the media and integrity jobs always do.

### Fixed

- Deleting the only copy of a file through a junction, symlink, subst drive or bind mount warned nothing: sources are compared by their resolved roots, a walk refuses a root another source already indexes, and a surviving record of the same file at the same resolved path is no longer taken for a hard link.
- Deleting a file marked as drifted warned nothing; it is now unverified (or the last copy). Duplicates no longer list drifted files as identical copies.
- The Protection card read "0 files with one copy only" when nothing was hashed; it now leads with the files not checked yet and marks the other counts as covering checked files only.
- Linux failure domains: LVM and LUKS volumes resolve to their disks, btrfs subvolumes to the mounted device, and NFS, SMB, sshfs and WebDAV mounts are named by their server and share (stable across remounts) instead of one domain per mount.
- One failed sidecar (a strip that timed out) no longer blocks a file's other thumbnails; a timeout is retried instead of recorded; a failed thumbnail keeps the file's real metadata.
- The texture cache is bounded by memory (strips decoded to the tile height), not only by count.
- Date headers no longer reset after the app sat idle for two seconds.
- Media and integrity jobs pause on input under every hashing policy.
- A source whose root is a junction or symlink indexes the folder it leads to (it indexed nothing on Windows).
- Media work asked for while a sidecar job runs is queued instead of dropped; a just-made sidecar cannot be evicted before it is shown; big TIFFs are not read whole for EXIF; the viewer's full decode no longer stalls the UI with a texture bigger than the screen needs; copies badges and integrity checkpoints do less work per file.

### Known limitations

- Video playback in the viewer shows still frames; press Enter to open the system player.
- Date headers sort by capture time only when metadata exists; otherwise the modified time is used.
- Protection counters catch up at the next index, hash or integrity run (live watcher events do not recount).
- A disk cloned with its volume serial reads as the same disk: its files count as hard links of the original, and a source on the clone whose root has the original's file id is refused as the same folder. Give the clone a new serial.
- A Windows disk without a serial (a VHDX Dev Drive, some RAID controllers) is named by its disk number, which can change; set its failure domain by hand. A btrfs filesystem spanning several disks is named by its mounted device only.
- A source aliased in a way neither the resolved paths nor the root's file id reveal (a share of a local folder reached through the network path while the folder is also a source, when the server reports other file ids) still counts its files twice.
- TIFFs over 64 MiB are shown without their EXIF orientation.
- Remote thumbnails follow the "remote thumbnails" setting; the viewer always downloads the file you open.
- A video whose strip times out is retried each session.

## [0.6.0] - 2026-10-09

Library release: Keel now keeps an index of every file across your sources, works offline from it, finds duplicates, and previews every copy, move and delete before it runs.

### Added

- `keel-core`, a UI-free library crate (typed requests and responses, so a later daemon can wrap it). Each source has its own SQLite store (bundled SQLite, FTS5) under `<data dir>/library/<name>/`; set `KEEL_DATA_DIR` to relocate it.
- Streaming indexer with stable record identity: a renamed or moved file keeps its record, and a new file never takes over a renamed file's record. Unreadable files are kept with their error. A different or emptied root, or a cut-off cloud listing, never deletes records.
- Live watching (`notify`) with a full reconcile every 6 hours and after lost events; remote and cloud sources are re-walked on a poll interval (15 minutes by default).
- Durable jobs: progress is persisted and pending jobs resume after a restart. A step checkpoints before its side effects, so a resumed job checks whether the step already ran.
- Safe operations: copy, move, delete and rename go through validate, preview, execute. The preview is projected from the index (it works for offline sources) with warnings for the last copy, a permanent delete on SFTP or S3, content that has not been verified, and an offline source. Execute re-validates and refuses a plan that no longer matches its preview. Every operation is written to a per-library log.
- Content ids: a sampled BLAKE3 hash first, a full hash only on a collision; files up to 192 KiB are hashed whole. Hashing runs at idle priority and pauses on activity or battery.
- Duplicate finder (window and Overview card) with how much space is reclaimable.
- Library search with Everything-style syntax (`ext:`, `size:>1mb`, `dm:2026-10`, `tag:`, `in:`, quoted phrases), ranked, in under 50 ms on 2 million rows.
- Tags (with colors), favorites, recents (from the operation log) and saved views.
- App: a Library section in the sidebar (Overview, Favorites, Recents, Sources with status, Tags, Views), an Add source wizard, an Overview dashboard, and `library://` tabs that browse the last indexed state when a source is offline.
- Search backend selector now includes Library. Tag chips on rows, a tag picker (Ctrl+Shift+T) and Ctrl+D to favorite.
- Preview dialog before copy, move and delete from library views.
- Settings → Library: enable or disable, hashing policy (idle only, pause on battery, off), rescan interval and rebuild.
- Sidecar store and media metadata, the foundation for the media view: an idle-priority job makes thumbnails and metadata sidecars for every image and video of a source and fills a `media` table (dimensions, orientation, capture time, duration, camera, GPS, keywords) under a size budget. Not yet shown in the app or searchable.
- An offline source says why in its tooltip (unreachable, a different folder at its root, or an empty root); for the last two its context menu offers "Adopt new root", which indexes whatever is there now.

### Changed

- New tab stays on Ctrl+T; the tag picker is Ctrl+Shift+T.
- The cloud provider forwards `list_complete` and `remove_kind`, so a cut-off cloud listing is recognised as incomplete.
- Roadmap: Phase 6 is released; phases 7 to 9 are planned.

### Fixed

- Library stores open safely in parallel; one process owns a library at a time; ended jobs are pruned and drop their state.
- Case-insensitive names keep their real casing, including non-ASCII.
- An operation on an unreachable source fails instead of being skipped.
- Removing a source stops its walk, hashing and watchers.
- Shorter write locks during walks, so the UI stays responsive while a big source indexes.
- Delete previews warn about permanent and unverified deletes; locations in logs are redacted.
- A folder copy or move whose target appeared while it ran resumes as a merge instead of reporting done; operations run in batches with one durable checkpoint each, and a refused plan is compared by actions, paths and warnings, not counts.
- Content ids stay only while size, modification and change time (nanoseconds) are unchanged, and are BLAKE3 of the bytes (existing content ids are recomputed once). Hard links count once in duplicates and copies.
- A drive letter or mount point now holding a different (or empty) folder keeps the source offline instead of replacing its index.
- Hashing pauses for 5 seconds after input, skips network shares unless the source allows it, starts after each completed walk (not when hashing is off or paused), and stops promptly when asked.
- Filter-only library searches (`ext:`, `size:`, `dm:`) take about 1 ms on 2 million rows instead of up to 400 ms; `dm:` dates use local time.
- Tag and record ids are never reused, so a deleted tag or file never passes its tags to a new one; stores from another library merge their tags by name.
- Every file system provider states whether its listing is complete and what a delete does (no silent defaults).

### Known limitations

- Hashing skips remote and cloud sources (SFTP, S3, Drive, Dropbox); only sources on a local path are hashed, so duplicates are found among those.
- Two sources on one physical disk count as two locations for the "last copy" warning. Failure domains come in a later release.
- The Overview shows library totals but no per-source counts.
- Remote and cloud sources are found changed by re-walking on a poll interval, not by live events.
- A folder copy resumed after a crash re-runs as a merge into the half-written target.
- The library can be turned off in Settings → Library; library tabs, tags and the Library search backend are then unavailable.

## [0.5.0] - 2026-10-09

Polish release: profiles, icon themes, Miller columns, a drop zone, a command line with single instance and a global hotkey, drag-out to other Windows apps, and Keel's own search index on Windows.

### Added

- Columns view (per pane): column 0 is the tab's folder, a selected folder opens in the next column and a selected file is previewed in the last. Left/Right move between columns, Enter opens a folder, Up and Backspace step back; column widths are draggable and saved in settings.
- Drop zone: a strip above the status bar (Ctrl+Shift+Z) that holds a list of paths across navigation. Drop rows on it or press Ctrl+Shift+S to stash the selection; Paste here, Move here and Clear act on it. The stash is saved with the session (50,000 items at most).
- Profiles: Settings → Profiles creates (a copy of the current settings), renames, deletes (to the OS trash) and switches profiles; the command palette offers "Switch profile: <name>". Each profile has its own `config.toml` and session (tabs, views, column chains, drop zone).
- Icon themes: Settings → Icons lists the built-in icons and installed themes, previews them, switches at runtime and removes them. "Install from VS Code Marketplace…" downloads an icon theme extension by id (`publisher.name`), shows its license and installs only after "I accept".
- Command line: `keel [FOLDER] [--new-window] [--profile NAME] [--search QUERY]`, plus `--version` and `--help`.
- Single instance per user and profile: a later `keel` hands its folder or search to the running window and exits (setting "Reuse the running window", on by default; `--new-window` opts out).
- Global hotkey (default Ctrl+Shift+Alt+K, Settings → General, empty = off) brings Keel to the front from any app, including a minimized window.
- Drag-out (Windows): dragging rows out of the Keel window onto Explorer or another app starts a native file drag (copy by default).
- Own search index (Windows): NTFS MFT and USN journal indexer used when Everything is not running; substring, glob and `regex:` queries, `folder:` and `in:<path>` filters. "Index all drives (administrator)" in Settings → General builds the full index through a one-off elevated helper (`keel --index-service`, hidden); the index files stay owned by the user. The status bar names the active search backend.
- Animations (pane split, preview panel, drop zone, toasts, selection highlight) with a "Reduce motion" setting that makes them instant.
- Optional "One at a time per drive" transfer queue (Settings → General).
- Properties dialog on every OS (plus Explorer's sheet on Windows) and an Open with picker on Linux.
- Path box: relative paths, `~` and `~/x` (home folder), `X:dir` (means `X:\dir`) and file paths (opens the folder with the file selected).
- Copies use reflinks (APFS, Btrfs, XFS clones) before the chunked copy on macOS and Linux.

### Changed

- The folder-walk search (Linux, and Windows home-folder walk) matches regex queries with a real regex instead of plain text.
- Text previews decode non-UTF-8 files as Windows-1252 instead of showing U+FFFD.
- Thumbnails render at physical pixels (sharp on high-DPI screens); queued thumbnails that scroll away are cancelled; PDF and video thumbnails skip the 64 MiB preview cap.
- Name-sorted listings are sorted on the listing worker: refreshing a 100,000-entry folder went from 48 ms to about 1 ms (release build).
- AltGr text starts the filter; the hidden-files shortcut on macOS is Cmd+Shift+. and labels say Cmd there.
- The sidebar hides pseudo and read-only mounts on macOS and Linux. Drive and folder listings time out instead of spinning forever.
- Jobs report skipped items. Clipboard writes run on a worker with retries and a toast on failure.
- Preview renders that hang are abandoned after 15 s; the preview cache has a 256 MB budget; PDFs re-render sharp when the panel is widened; tables show at most 64 columns.
- `THIRD_PARTY.md` lists the new dependencies.

### Fixed

- Hidden tabs and pane 1 refresh when shown; renaming the open folder is noticed; `move_tab` keeps the right tab active.
- Case-only renames work on case-insensitive filesystems on macOS and Linux.
- The one-transfer-per-drive queue is first-in first-out and keys drives by their real volume (subst drives, junctions and mount points included), so no waiter starves.
- Columns view: each folder is listed once per change however many columns and tabs show it; a hung listing stops with an error instead of being asked again; every visible column is watched; context menus act on their own column.
- Drop zone: Move here removes only the items that actually moved (a cancelled conflict, Skip or a failed job keeps them); a stash mixing archive entries and plain files pastes correctly; a hung stat no longer freezes the missing-item check.
- Switching profiles applies the target profile's views, column chains and drop zone instead of writing the old ones into its session.
- Global hotkey: needs a modifier besides Shift, and the default no longer uses Ctrl+Alt alone (that is AltGr on many keyboard layouts). No "hotkey taken" toast from a process that is not the single instance.
- Drag-out validates the data object and never starts a drag whose mouse button is already up; the main window is found by its own window class, not the first titled window.

### Security

- Icon themes: SVG icons from the VS Code Marketplace are checked when a theme is installed and again when it loads. An icon is refused (and the number skipped is reported) when it has `<image>`, `<script>` or `<foreignObject>`, an entity declaration, an `href` that is not `#id` or `data:`, CSS `@import`, a `url()` to anything but `#id`, or CSS escapes. Before, an icon could make the SVG renderer read a local file or a `\\server\share` path, leaking the Windows NTLM hash and freezing the window. A theme may hold at most 10,000 icons and 64 MB of them; the license shown is cut at 64 KB.
- Single instance: only the same user can hand Keel a folder or search. On Windows the pipe's access list grants only the current user, the client refuses impersonation and checks that the pipe is served by a process of the same user (and lets only that process take the foreground). On macOS and Linux the socket lives in a folder only the user can enter (`$XDG_RUNTIME_DIR/keel-<uid>` or `<temp>/keel-<uid>`, mode 0700), and both ends check the peer's user id. Socket names hash the user name, so non-ASCII user names no longer share one name. Relative paths sent by another process are refused.
- Single instance: a second `keel` no longer hangs, with no window, when the running instance is busy or another client connected and sent nothing. Connecting and the answer share a 3 s limit; the running instance reads each client on its own thread, for at most 2 s and 64 KiB.

### Known limitations

- Drag-out to other apps works on Windows only; on macOS and Linux dragging out of the window does nothing yet.
- The single-instance code for macOS and Linux (socket in a private folder, peer user check) and the global hotkey on those systems compile and pass CI, but have not been run on real hardware. The Spotlight and `locate` search backends are still unverified there too.
- The own search index is Windows-only and needs NTFS volumes. Without a saved full index and without administrator rights it indexes the home folder (recursively) plus one level of each drive's root; run "Index all drives (administrator)" to cover everything.
- Everything, when running, is still preferred over Keel's own index; the "Index all drives" button is greyed out then.
- A stat on a hung network, SFTP or cloud path runs on its own thread that is given up on after a timeout; while a host stays dead, abandoned threads can accumulate until the calls return.
- Switching profile at runtime does rebind the single-instance name to the new profile, so a later `keel --profile <name>` reaches the right window. If another Keel already holds that profile's name, the switch is refused with a toast.
- Icon themes: only SVG and PNG icons are used, and an SVG that references anything outside itself is refused, so a theme can show fewer icons than in VS Code (the number skipped is reported on install). Font-based icon themes are not supported.
- The Linux "cut" flag for the file clipboard is still not set (other apps see a copy).
- Builds are not code-signed or notarized.
- Earlier known limitations still apply unless listed here.

## [0.3.0] - 2026-10-09

### Added
- Cloud accounts as folders: Google Drive, Dropbox and any S3-compatible bucket (Backblaze B2, AWS, MinIO). Sign in with your own OAuth client id (PKCE loopback flow); tokens and S3 keys live only in the OS keychain.
- Settings → Cloud: add/edit/remove accounts, per-account client id override, "Sign in again".
- Sidebar "Cloud" section with status dots; copy, move and delete between local, SFTP and cloud through the normal paste flow; per-service delete semantics (Drive trash, Dropbox recoverable delete, S3 permanent) with matching confirm dialogs.
- Google Docs-native files and Drive shortcuts are skipped in listings; rate limits are retried with backoff; uploads to Drive/Dropbox are single requests capped at 256 MB / 150 MB.

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

[0.6.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.6.0
[0.5.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.5.0
[0.2.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.2.0
[0.1.0]: https://github.com/Runitupshawty/keel/releases/tag/v0.1.0
