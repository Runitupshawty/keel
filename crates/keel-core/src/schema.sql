-- Versioned migrations. A `-- @<db> <version>` line starts the migration that takes that
-- database (`library` = library.db, `source` = source.db) to <version>; migrations run in
-- order, each in its own transaction, and never change once released (add a new version).

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
