-- Versioned migrations. A `-- @<db> <version>` line starts the migration that takes that
-- database (`library` = library.db, `source` = source.db) to <version>; migrations run in
-- order, together in one write transaction, and never change once released (add a new
-- version).

-- @library 1
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE source(id TEXT PRIMARY KEY, def TEXT NOT NULL, created INTEGER NOT NULL);
-- Durable jobs: `state` is the job's last checkpoint (JSON); queued/running rows resume on open.
CREATE TABLE job(
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    status TEXT NOT NULL,
    progress REAL NOT NULL DEFAULT 0,
    log TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL);
CREATE INDEX job_status ON job(status);
-- Every executed operation; payload and result are redacted before they are written.
CREATE TABLE op_log(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    result TEXT NOT NULL);

-- @source 1
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- One row per file/folder. `fs_id` is the stable identity (volume serial + file id, dev+inode,
-- or a parent+name hash for remotes); `path` is relative to the source root ('' = the root).
-- kind: 0 file, 1 dir, 2 dangling link. flags: 1 unreadable, 2 hidden, 4 link.
CREATE TABLE record(
    id INTEGER PRIMARY KEY,
    parent INTEGER,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    mtime INTEGER,
    ctime INTEGER,
    fs_id TEXT NOT NULL,
    cas_id BLOB,
    sampled_hash BLOB,
    gen INTEGER NOT NULL,
    flags INTEGER NOT NULL DEFAULT 0,
    error TEXT);
-- Only native ids are looked up by fs_id; a parent+name hash ('h:...') is found by
-- parent+name (indexing random hashes would cost a 2M-row walk dearly).
CREATE INDEX record_fs_id ON record(fs_id) WHERE substr(fs_id, 1, 2) <> 'h:';
CREATE INDEX record_parent ON record(parent, name);
CREATE INDEX record_cas ON record(cas_id) WHERE cas_id IS NOT NULL;
CREATE VIRTUAL TABLE record_fts USING fts5(
    name, path, content=record, content_rowid=id, tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER record_ai AFTER INSERT ON record BEGIN
    INSERT INTO record_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TRIGGER record_ad AFTER DELETE ON record BEGIN
    INSERT INTO record_fts(record_fts, rowid, name, path) VALUES ('delete', old.id, old.name, old.path);
    DELETE FROM record_tag WHERE record = old.id;
END;
CREATE TRIGGER record_au AFTER UPDATE OF name, path ON record BEGIN
    INSERT INTO record_fts(record_fts, rowid, name, path) VALUES ('delete', old.id, old.name, old.path);
    INSERT INTO record_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TABLE tag(id INTEGER PRIMARY KEY, name TEXT NOT NULL, color TEXT, parent INTEGER);
CREATE TABLE record_tag(record INTEGER NOT NULL, tag INTEGER NOT NULL, PRIMARY KEY(record, tag));

-- @source 2
-- Content identity: sampled-hash collisions are looked up per hashed file.
CREATE INDEX record_sampled ON record(sampled_hash) WHERE sampled_hash IS NOT NULL;

-- @source 3
-- Library search: prefix indexes for 2- and 3-character prefixes (short prefix queries stay
-- fast); the table is rebuilt from `record`.
DROP TRIGGER record_ai;
DROP TRIGGER record_ad;
DROP TRIGGER record_au;
DROP TABLE record_fts;
CREATE VIRTUAL TABLE record_fts USING fts5(
    name, path, content=record, content_rowid=id, tokenize='unicode61 remove_diacritics 2',
    prefix='2 3');
INSERT INTO record_fts(record_fts) VALUES ('rebuild');
CREATE TRIGGER record_ai AFTER INSERT ON record BEGIN
    INSERT INTO record_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TRIGGER record_ad AFTER DELETE ON record BEGIN
    INSERT INTO record_fts(record_fts, rowid, name, path) VALUES ('delete', old.id, old.name, old.path);
    DELETE FROM record_tag WHERE record = old.id;
END;
CREATE TRIGGER record_au AFTER UPDATE OF name, path ON record BEGIN
    INSERT INTO record_fts(record_fts, rowid, name, path) VALUES ('delete', old.id, old.name, old.path);
    INSERT INTO record_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;

-- @library 2
-- Tags (copied into every source store's `tag` table), recents and saved views.
CREATE TABLE tag(id INTEGER PRIMARY KEY, name TEXT NOT NULL, color TEXT, parent INTEGER);
CREATE UNIQUE INDEX tag_name ON tag(coalesce(parent, 0), name COLLATE NOCASE);
-- Reserved: Favorites.
INSERT INTO tag(id, name, color, parent) VALUES (1, 'Favorites', '#f5c518', NULL);
CREATE TABLE opened(source TEXT NOT NULL, record INTEGER NOT NULL, ts INTEGER NOT NULL,
    PRIMARY KEY(source, record));
CREATE TABLE view(id INTEGER PRIMARY KEY, name TEXT NOT NULL, query TEXT NOT NULL, layout TEXT NOT NULL);

-- @library 3
-- Whether an operation completed, apart from its (redacted) result text.
ALTER TABLE op_log ADD COLUMN ok INTEGER;
UPDATE op_log SET ok = (result LIKE 'ok%') WHERE result <> 'running';

-- @source 4
-- Record counts kept by triggers, so library stats need no scan.
CREATE TABLE counts(id INTEGER PRIMARY KEY CHECK (id = 1), records INTEGER NOT NULL,
    files INTEGER NOT NULL, bytes INTEGER NOT NULL);
INSERT INTO counts SELECT 1, count(*), coalesce(sum(kind = 0), 0),
    coalesce(sum(CASE WHEN kind = 0 THEN size END), 0) FROM record;
CREATE TRIGGER record_count_ai AFTER INSERT ON record BEGIN
    UPDATE counts SET records = records + 1, files = files + (new.kind = 0),
        bytes = bytes + (CASE WHEN new.kind = 0 THEN new.size ELSE 0 END);
END;
CREATE TRIGGER record_count_ad AFTER DELETE ON record BEGIN
    UPDATE counts SET records = records - 1, files = files - (old.kind = 0),
        bytes = bytes - (CASE WHEN old.kind = 0 THEN old.size ELSE 0 END);
END;
CREATE TRIGGER record_count_au AFTER UPDATE OF kind, size ON record
    WHEN old.kind IS NOT new.kind OR old.size IS NOT new.size BEGIN
    UPDATE counts SET files = files - (old.kind = 0) + (new.kind = 0),
        bytes = bytes - (CASE WHEN old.kind = 0 THEN old.size ELSE 0 END)
            + (CASE WHEN new.kind = 0 THEN new.size ELSE 0 END);
END;

-- @source 5
-- 0.6.0, the first release with libraries. Record ids are never reused (AUTOINCREMENT): tags
-- and recents keyed by a record id never move to another file. `mtime` is in nanoseconds and
-- `ctime` is the change time (status change on unix, ChangeTime on Windows; it was the
-- creation time) in nanoseconds: a same-size edit that restores the mtime still drops the
-- content id. Content ids restart: `cas_id` is now BLAKE3 of the bytes, set only once
-- confirmed. Partial indexes serve filter-only searches.
DROP TRIGGER record_ai;
DROP TRIGGER record_ad;
DROP TRIGGER record_au;
DROP TRIGGER record_count_ai;
DROP TRIGGER record_count_ad;
DROP TRIGGER record_count_au;
CREATE TABLE record_new(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    parent INTEGER,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    mtime INTEGER,
    ctime INTEGER,
    fs_id TEXT NOT NULL,
    cas_id BLOB,
    sampled_hash BLOB,
    gen INTEGER NOT NULL,
    flags INTEGER NOT NULL DEFAULT 0,
    error TEXT);
INSERT INTO record_new(id, parent, name, path, kind, size, mtime, ctime, fs_id, gen, flags, error)
    SELECT id, parent, name, path, kind, size,
        CASE WHEN abs(mtime) < 9000000000 THEN mtime * 1000000000 END, NULL,
        fs_id, gen, flags, error
    FROM record;
DROP TABLE record;
ALTER TABLE record_new RENAME TO record;
CREATE INDEX record_fs_id ON record(fs_id) WHERE substr(fs_id, 1, 2) <> 'h:';
CREATE INDEX record_parent ON record(parent, name);
CREATE INDEX record_cas ON record(cas_id) WHERE cas_id IS NOT NULL;
CREATE INDEX record_sampled ON record(sampled_hash) WHERE sampled_hash IS NOT NULL;
CREATE INDEX record_mtime ON record(mtime) WHERE parent IS NOT NULL;
CREATE INDEX record_size ON record(size) WHERE kind = 0;
CREATE INDEX record_kind ON record(kind, mtime) WHERE parent IS NOT NULL;
CREATE TRIGGER record_ai AFTER INSERT ON record BEGIN
    INSERT INTO record_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TRIGGER record_ad AFTER DELETE ON record BEGIN
    INSERT INTO record_fts(record_fts, rowid, name, path) VALUES ('delete', old.id, old.name, old.path);
    DELETE FROM record_tag WHERE record = old.id;
END;
CREATE TRIGGER record_au AFTER UPDATE OF name, path ON record BEGIN
    INSERT INTO record_fts(record_fts, rowid, name, path) VALUES ('delete', old.id, old.name, old.path);
    INSERT INTO record_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TRIGGER record_count_ai AFTER INSERT ON record BEGIN
    UPDATE counts SET records = records + 1, files = files + (new.kind = 0),
        bytes = bytes + (CASE WHEN new.kind = 0 THEN new.size ELSE 0 END);
END;
CREATE TRIGGER record_count_ad AFTER DELETE ON record BEGIN
    UPDATE counts SET records = records - 1, files = files - (old.kind = 0),
        bytes = bytes - (CASE WHEN old.kind = 0 THEN old.size ELSE 0 END);
END;
CREATE TRIGGER record_count_au AFTER UPDATE OF kind, size ON record
    WHEN old.kind IS NOT new.kind OR old.size IS NOT new.size BEGIN
    UPDATE counts SET files = files - (old.kind = 0) + (new.kind = 0),
        bytes = bytes - (CASE WHEN old.kind = 0 THEN old.size ELSE 0 END)
            + (CASE WHEN new.kind = 0 THEN new.size ELSE 0 END);
END;

-- @library 4
-- Tag ids are never reused (a deleted tag's leftover links never reach a new tag). Jobs get
-- a cursor (the small part of their state written each step, laid over `state` on resume)
-- and a structured result. Content ids restarted with source version 5.
CREATE TABLE tag_new(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, color TEXT,
    parent INTEGER);
INSERT INTO tag_new(id, name, color, parent) SELECT id, name, color, parent FROM tag;
DROP TABLE tag;
ALTER TABLE tag_new RENAME TO tag;
CREATE UNIQUE INDEX tag_name ON tag(coalesce(parent, 0), name COLLATE NOCASE);
ALTER TABLE job ADD COLUMN cursor TEXT;
ALTER TABLE job ADD COLUMN result TEXT;
DELETE FROM meta WHERE key = 'unique_content';

-- @source 6
-- Media facts from sidecars (SidecarJob), one row per image/video record. `key` and `pkey` are
-- its sidecar folder names (by content id when known; by path + mtime + size).
CREATE TABLE media(
    record INTEGER PRIMARY KEY,
    key TEXT NOT NULL,
    pkey TEXT NOT NULL,
    width INTEGER,
    height INTEGER,
    orientation INTEGER,
    taken_at INTEGER,
    duration_ms INTEGER,
    camera TEXT,
    gps_lat REAL,
    gps_lon REAL);
CREATE INDEX media_taken ON media(taken_at) WHERE taken_at IS NOT NULL;
-- Camera and XMP keywords, searchable (rowid = record).
CREATE VIRTUAL TABLE media_fts USING fts5(camera, keywords, tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER record_media_ad AFTER DELETE ON record BEGIN
    DELETE FROM media WHERE record = old.id;
    DELETE FROM media_fts WHERE rowid = old.id;
END;

-- @library 5
-- Protection (Task 33): volumes (drives, shares, cloud accounts, hosts) and their failure
-- domains; each source is on one volume. `state` is 'auto' (online/offline follows the
-- sources) or a state set by hand ('archived', 'lost', 'retired'). Capacity is as last seen.
CREATE TABLE volume(
    id TEXT PRIMARY KEY,
    label TEXT NOT NULL,
    kind TEXT NOT NULL,
    domain TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'auto',
    last_seen INTEGER NOT NULL DEFAULT 0,
    backup INTEGER NOT NULL DEFAULT 0,
    used INTEGER,
    total INTEGER);
ALTER TABLE source ADD COLUMN volume_id TEXT;

-- @source 7
-- Integrity (Task 33): when a re-hash found the bytes changed although size, mtime and
-- change time did not (unix seconds; NULL = never). A real change resets it with the hashes.
ALTER TABLE record ADD COLUMN drift INTEGER;
CREATE INDEX record_drift ON record(drift) WHERE drift IS NOT NULL;

-- @library 6
-- A failure domain set by hand in the drive inventory (NULL: the detected `domain`).
ALTER TABLE volume ADD COLUMN domain_set TEXT;

-- @source 8
-- Devices (Task 36): the content id a paired device claims for a file of a device source.
-- It is that device's word, never checked here: it may show where a content is said to be,
-- but it never counts as a copy (protection, duplicates). A walk of a device source moves
-- any id an earlier version wrote into `cas_id` here.
ALTER TABLE record ADD COLUMN remote_cas BLOB;
CREATE INDEX record_remote_cas ON record(remote_cas) WHERE remote_cas IS NOT NULL;

-- @library 7
-- Copy and move jobs: what the running batch placed (a file with its size and modified
-- time, or a folder it made), written every few files or MiB, so a job resumed after a
-- crash skips them and refuses a target changed since. Cleared when a batch or the job ends.
CREATE TABLE job_file(
    job INTEGER NOT NULL,
    src TEXT NOT NULL,
    placed TEXT NOT NULL,
    PRIMARY KEY(job, src)) WITHOUT ROWID;

-- @library 8
-- Library sync (tags, favorites, content ids) between paired devices. Every tag gets a
-- stable id (`uid`, random; Favorites is 'favorites' everywhere) that devices agree on.
-- `sync_log`: this device's own changes, in order (`seq`), each with its Lamport time and
-- what changed (`op`, JSON); a peer pulls the entries after the last seq it has.
-- `sync_key`: the winning change per key (last writer wins by Lamport time, then device
-- id; '' = this device), tombstones included. `sync_peer`: how far each device's log was
-- read, and when. `tag_alias`: another device's tag merged into a local one by name.
ALTER TABLE tag ADD COLUMN uid TEXT;
UPDATE tag SET uid = CASE WHEN id = 1 THEN 'favorites' ELSE lower(hex(randomblob(16))) END;
CREATE UNIQUE INDEX tag_uid ON tag(uid);
CREATE TRIGGER tag_uid_ai AFTER INSERT ON tag WHEN new.uid IS NULL BEGIN
    UPDATE tag SET uid = lower(hex(randomblob(16))) WHERE id = new.id;
END;
CREATE TABLE sync_log(
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    lamport INTEGER NOT NULL,
    key TEXT NOT NULL,
    op TEXT NOT NULL,
    live INTEGER NOT NULL,
    ts INTEGER NOT NULL);
CREATE INDEX sync_log_key ON sync_log(key);
CREATE TABLE sync_key(
    key TEXT PRIMARY KEY,
    lamport INTEGER NOT NULL,
    device TEXT NOT NULL,
    op TEXT NOT NULL,
    live INTEGER NOT NULL,
    ts INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE sync_peer(
    device TEXT PRIMARY KEY,
    seq INTEGER NOT NULL DEFAULT 0,
    last_sync INTEGER,
    received INTEGER NOT NULL DEFAULT 0);
CREATE TABLE tag_alias(uid TEXT PRIMARY KEY, tag INTEGER NOT NULL);

-- @source 9
-- Folder sizes (Task 46): every folder's apparent size and file count below it (links and
-- what they point at not counted). Triggers mark in `folder_dirty` each folder whose total
-- may have changed (the parent of a record inserted, removed, resized or moved, and a new
-- folder itself); `index::refresh_totals` brings the marked folders and their parent chains
-- up to date after each watcher batch and each full walk. Marking every folder here makes
-- the next refresh fill the table in one pass.
CREATE TABLE folder_total(
    record INTEGER PRIMARY KEY,
    bytes INTEGER NOT NULL,
    files INTEGER NOT NULL);
CREATE TABLE folder_dirty(record INTEGER PRIMARY KEY);
INSERT INTO folder_dirty SELECT id FROM record WHERE kind = 1;
CREATE TRIGGER record_total_ai AFTER INSERT ON record BEGIN
    INSERT OR IGNORE INTO folder_dirty SELECT new.parent WHERE new.parent IS NOT NULL;
    INSERT OR IGNORE INTO folder_dirty SELECT new.id WHERE new.kind = 1;
END;
CREATE TRIGGER record_total_ad AFTER DELETE ON record BEGIN
    INSERT OR IGNORE INTO folder_dirty SELECT old.parent WHERE old.parent IS NOT NULL;
    DELETE FROM folder_total WHERE record = old.id;
END;
CREATE TRIGGER record_total_au AFTER UPDATE OF parent, kind, size, flags ON record
    WHEN old.parent IS NOT new.parent OR old.kind IS NOT new.kind OR old.size IS NOT new.size
        OR (old.flags & 4) IS NOT (new.flags & 4) BEGIN
    INSERT OR IGNORE INTO folder_dirty SELECT old.parent WHERE old.parent IS NOT NULL;
    INSERT OR IGNORE INTO folder_dirty SELECT new.parent WHERE new.parent IS NOT NULL;
    INSERT OR IGNORE INTO folder_dirty SELECT new.id WHERE new.kind = 1;
END;

-- @source 10
-- The inspector (Task 59): when the index first saw a record and when its bytes were last
-- hashed (unix seconds; NULL = before this version, or not hashed here).
ALTER TABLE record ADD COLUMN seen INTEGER;
ALTER TABLE record ADD COLUMN hashed INTEGER;

-- @source 11
-- Folder styles (Task 56): a folder's color (`#rrggbb`) and icon (one emoji, or
-- `theme:<name>`, an icon theme's folder icon of that name), by record like tag links: a
-- move or rename the index records keeps the record and its style; the folder leaving the
-- index drops it.
CREATE TABLE folder_style(
    record INTEGER PRIMARY KEY,
    color TEXT,
    icon TEXT);
CREATE TRIGGER record_style_ad AFTER DELETE ON record BEGIN
    DELETE FROM folder_style WHERE record = old.id;
END;

-- @source 12
-- Text inside documents (Task 62): one `record_text` row per file the text job tried, with
-- the size and mtime it read (a change re-extracts), the characters kept and the status
-- (0 done, 1 skipped: no reader, 2 skipped: over the size cap, 3 failed). The text is in
-- `record_text_body` (rows with text only), indexed by `record_content`, an FTS5 table over
-- it (external content: the text is stored once; `snippet()` reads it from the body). The
-- text job keeps the index in step with the body; a record leaving the index drops all three.
CREATE TABLE record_text(
    record INTEGER PRIMARY KEY,
    mtime INTEGER,
    size INTEGER NOT NULL,
    chars INTEGER NOT NULL DEFAULT 0,
    truncated INTEGER NOT NULL DEFAULT 0,
    status INTEGER NOT NULL);
CREATE TABLE record_text_body(record INTEGER PRIMARY KEY, text TEXT NOT NULL);
CREATE VIRTUAL TABLE record_content USING fts5(
    text, content=record_text_body, content_rowid=record,
    tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER record_text_ad AFTER DELETE ON record BEGIN
    INSERT INTO record_content(record_content, rowid, text)
        SELECT 'delete', record, text FROM record_text_body WHERE record = old.id;
    DELETE FROM record_text_body WHERE record = old.id;
    DELETE FROM record_text WHERE record = old.id;
END;

-- @source 13
-- Comments (Task 67, Get Info): a file's or folder's comment, by record like tag links and
-- folder styles: a move or rename the index records keeps it, the record leaving the index
-- drops it. (Merging with another branch that also added a source migration 13: renumber.)
CREATE TABLE record_comment(record INTEGER PRIMARY KEY, text TEXT NOT NULL);
CREATE TRIGGER record_comment_ad AFTER DELETE ON record BEGIN
    DELETE FROM record_comment WHERE record = old.id;
END;

-- @library 9
-- Bookmarks (Task 76): a saved view also keeps its search backend (`library` searches the
-- library index, `system` the platform's), sort (`<column>` or `<column>:desc`, '' = the
-- default), columns (a JSON array of column names, '[]' = the default set), match toggles
-- (a JSON object of the toggles that are on, '{}' = none), the filter it applies ('' =
-- Everything), and where it sits in the Bookmarks list: a folder ('/'-separated, '' = top
-- level), its position there and an optional shortcut key ('' = none, else unique).
ALTER TABLE view ADD COLUMN backend TEXT NOT NULL DEFAULT 'library';
ALTER TABLE view ADD COLUMN sort TEXT NOT NULL DEFAULT '';
ALTER TABLE view ADD COLUMN columns TEXT NOT NULL DEFAULT '[]';
ALTER TABLE view ADD COLUMN matching TEXT NOT NULL DEFAULT '{}';
ALTER TABLE view ADD COLUMN filter TEXT NOT NULL DEFAULT '';
ALTER TABLE view ADD COLUMN folder TEXT NOT NULL DEFAULT '';
ALTER TABLE view ADD COLUMN position INTEGER NOT NULL DEFAULT 0;
ALTER TABLE view ADD COLUMN shortcut TEXT NOT NULL DEFAULT '';
UPDATE view SET position = id;
