//! Change feeds (`Provider::changes`): Google Drive's `changes.list` and Dropbox's
//! `list_folder/continue` with the account's access token, and for S3 a fingerprint of a
//! small bucket (one list page) that says whether anything changed at all. WebDAV has
//! none.
use super::share::ok;
use super::*;
use crate::{ChangeCursor, ChangeFeed, ChangeKind, ChangedPath, FeedError};
use serde_json::json;

/// Drive items by id: what a change's id and parents are resolved through.
#[derive(Default)]
pub(super) struct DriveIds {
    /// The account root folder's id.
    root: String,
    /// id -> (name, first parent's id).
    items: HashMap<String, (String, Option<String>)>,
}

/// Deepest folder chain followed up to the account's root.
const MAX_DEPTH: usize = 128;
/// Objects an S3 fingerprint covers: one ListObjectsV2 page.
const S3_PAGE: usize = 1000;
const FOLDER: &str = "application/vnd.google-apps.folder";

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
/// A Drive id goes into a query and a URL path: letters, digits, `-` and `_` only.
fn drive_safe(id: &str) -> Result<&str> {
    anyhow::ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "the service answered an unexpected file id"
    );
    Ok(id)
}
fn rejected() -> anyhow::Error {
    FeedError::CursorRejected.into()
}

impl Core {
    fn account_root(&self) -> VPath {
        VPath {
            scheme: "cloud".into(),
            authority: self.account.id.clone(),
            path: "/".into(),
        }
    }

    /// `rel` (`/a/b`, below the account's root) as a change; its parent's cached listing
    /// (and anything cached under it) is stale now.
    fn changed(&self, rel: &str, kind: ChangeKind) -> ChangedPath {
        self.invalidate(rel);
        ChangedPath {
            path: VPath {
                path: rel.to_owned(),
                ..self.account_root()
            },
            kind,
        }
    }

    /// `Provider::changes` takes no cancel flag, so its requests pass `NEVER`; the
    /// account's `stop` (`cancel_requests`) still ends them.
    pub(super) fn changes(&self, cursor: Option<ChangeCursor>) -> Result<ChangeFeed> {
        match self.account.kind {
            CloudKind::GoogleDrive => self.drive_changes(cursor),
            CloudKind::Dropbox => self.dropbox_changes(cursor),
            CloudKind::S3 => self.s3_changes(cursor),
            CloudKind::WebDav => Err(FeedError::Unsupported.into()),
        }
    }

    // --- Google Drive ----------------------------------------------------------------

    /// The account's root folder id (`root` is only an alias; parents name the real id).
    fn drive_root_id(&self) -> Result<String> {
        let root = self.account_root();
        let id = self.drive_id(&root, &NEVER)?;
        if id != "root" {
            return Ok(id);
        }
        let v = ok(
            &root,
            self.api(&root, &NEVER, None, "/drive/v3/files/root?fields=id")?,
        )?;
        Ok(drive_safe(v["id"].as_str().unwrap_or(""))?.to_owned())
    }

    /// Every item of the drive (not in the trash) by id, one request per 1,000.
    /// ponytail: the whole drive is held (about 100 bytes an item); keep only the items
    /// under the account's root if a drive of millions of files needs it.
    fn drive_prime(&self) -> Result<DriveIds> {
        let root = self.account_root();
        let mut ids = DriveIds {
            root: self.drive_root_id()?,
            items: HashMap::new(),
        };
        let mut page: Option<String> = None;
        loop {
            let mut q = format!(
                "/drive/v3/files?q={}&fields={}&pageSize=1000&spaces=drive",
                enc("trashed = false"),
                enc("nextPageToken,files(id,name,parents,mimeType)")
            );
            if let Some(token) = &page {
                q.push_str(&format!("&pageToken={}", enc(token)));
            }
            let v = ok(&root, self.api(&root, &NEVER, None, &q)?)?;
            for f in v["files"].as_array().into_iter().flatten() {
                let mime = f["mimeType"].as_str().unwrap_or("");
                if mime.starts_with("application/vnd.google-apps.") && mime != FOLDER {
                    continue;
                }
                if let (Some(id), Some(name)) = (f["id"].as_str(), f["name"].as_str()) {
                    let parent = f["parents"][0].as_str().map(str::to_owned);
                    ids.items.insert(id.to_owned(), (name.to_owned(), parent));
                }
            }
            match v["nextPageToken"].as_str() {
                Some(next) => page = Some(next.to_owned()),
                None => return Ok(ids),
            }
        }
    }

    /// One item asked for by id and remembered (None: gone, trashed or not visible).
    fn drive_fetch(&self, id: &str) -> Result<Option<(String, Option<String>)>> {
        let root = self.account_root();
        let path = format!(
            "/drive/v3/files/{}?fields={}",
            drive_safe(id)?,
            enc("name,parents,trashed")
        );
        let (status, v) = self.api(&root, &NEVER, None, &path)?;
        if status == 404 {
            return Ok(None);
        }
        let v = ok(&root, (status, v))?;
        if v["trashed"].as_bool() == Some(true) {
            return Ok(None);
        }
        let Some(name) = v["name"].as_str() else {
            return Ok(None);
        };
        let item = (name.to_owned(), v["parents"][0].as_str().map(str::to_owned));
        if let Some(ids) = self.drive_ids.lock().as_mut() {
            ids.items.insert(id.to_owned(), item.clone());
        }
        Ok(Some(item))
    }

    /// `id`'s path below the account's root (`/a/b`), from the remembered items; with
    /// `fetch`, unknown folders on the way up are asked for. None: not under the root.
    fn drive_path(&self, id: &str, fetch: bool) -> Result<Option<String>> {
        let mut names = Vec::new();
        let mut at = id.to_owned();
        for _ in 0..MAX_DEPTH {
            let known = {
                let ids = self.drive_ids.lock();
                let Some(ids) = ids.as_ref() else {
                    return Ok(None);
                };
                if at == ids.root {
                    names.reverse();
                    return Ok(Some(format!("/{}", names.join("/"))));
                }
                ids.items.get(&at).cloned()
            };
            let item = match known {
                Some(item) => Some(item),
                None if fetch => self.drive_fetch(&at)?,
                None => None,
            };
            let Some((name, Some(parent))) = item else {
                return Ok(None);
            };
            // Listings leave such names out too.
            if name.is_empty() || name.contains(['/', '\\']) {
                return Ok(None);
            }
            names.push(name);
            at = parent;
        }
        Ok(None)
    }

    /// `changes.list` from `cursor` (a page token). The first call (no cursor) takes the
    /// start token and remembers every item, so later moves know where an item was.
    fn drive_changes(&self, cursor: Option<ChangeCursor>) -> Result<ChangeFeed> {
        let root = self.account_root();
        let Some(ChangeCursor(token)) = cursor else {
            // The token first: what changes while the items are read comes again.
            let v = ok(
                &root,
                self.api(&root, &NEVER, None, "/drive/v3/changes/startPageToken")?,
            )?;
            let token = v["startPageToken"]
                .as_str()
                .context("the answer has no start token")?
                .to_owned();
            let ids = self.drive_prime()?;
            *self.drive_ids.lock() = Some(ids);
            return Ok(ChangeFeed {
                changes: Vec::new(),
                cursor: ChangeCursor(token),
                more: false,
            });
        };
        // Started by another instance (the account was connected again): where items
        // were is not known, so the caller walks and starts over.
        if self.drive_ids.lock().is_none() {
            return Err(rejected());
        }
        let fields = "nextPageToken,newStartPageToken,\
                      changes(fileId,removed,file(name,parents,trashed,mimeType))";
        let q = format!(
            "/drive/v3/changes?pageToken={}&pageSize=1000&includeRemoved=true&spaces=drive&fields={}",
            enc(&token),
            enc(fields)
        );
        let (status, v) = self.api(&root, &NEVER, None, &q)?;
        // An unknown or expired page token.
        if matches!(status, 400 | 404 | 410) {
            return Err(rejected());
        }
        let v = ok(&root, (status, v))?;
        let mut changes = Vec::new();
        for c in v["changes"].as_array().into_iter().flatten() {
            let Some(id) = c["fileId"].as_str() else {
                continue;
            };
            let file = &c["file"];
            let old = self.drive_path(id, false)?;
            let mime = file["mimeType"].as_str().unwrap_or("");
            let gone = c["removed"].as_bool() == Some(true)
                || file["trashed"].as_bool() == Some(true)
                || file["name"].as_str().is_none();
            if gone || (mime.starts_with("application/vnd.google-apps.") && mime != FOLDER) {
                if let Some(ids) = self.drive_ids.lock().as_mut() {
                    ids.items.remove(id);
                }
                // ponytail: an item deleted for good that was never seen here (it went
                // straight past the trash) stays in the index until the next full walk.
                if let Some(old) = old {
                    changes.push(self.changed(&old, ChangeKind::Removed));
                }
                continue;
            }
            let name = file["name"].as_str().unwrap_or_default().to_owned();
            let parent = file["parents"][0].as_str().map(str::to_owned);
            if let Some(ids) = self.drive_ids.lock().as_mut() {
                ids.items.insert(id.to_owned(), (name, parent));
            }
            let new = self.drive_path(id, true)?;
            match (old, new) {
                (Some(old), Some(new)) if old == new => {
                    changes.push(self.changed(&new, ChangeKind::Modified))
                }
                (old, new) => {
                    if let Some(old) = old {
                        changes.push(self.changed(&old, ChangeKind::Removed));
                    }
                    if let Some(new) = new {
                        changes.push(self.changed(&new, ChangeKind::Created));
                    }
                }
            }
        }
        let (cursor, more) = match (v["nextPageToken"].as_str(), v["newStartPageToken"].as_str()) {
            (Some(next), _) => (next, true),
            (None, Some(start)) => (start, false),
            (None, None) => anyhow::bail!("the answer has no page token"),
        };
        Ok(ChangeFeed {
            changes,
            cursor: ChangeCursor(cursor.to_owned()),
            more,
        })
    }

    // --- Dropbox ---------------------------------------------------------------------

    /// `list_folder` of the account's root, recursive with deletions: a cursor for now
    /// (`get_latest_cursor`), or what changed since one (`continue`; `reset`: walk again).
    /// ponytail: polled; `list_folder/longpoll` would block a worker for 30 s or more.
    fn dropbox_changes(&self, cursor: Option<ChangeCursor>) -> Result<ChangeFeed> {
        let root = self.account_root();
        let base = self.service_path(&root);
        let Some(ChangeCursor(cursor)) = cursor else {
            let args = json!({
                "path": if base == "/" { "" } else { base.as_str() },
                "recursive": true,
                "include_deleted": true,
            });
            let v = ok(
                &root,
                self.api(
                    &root,
                    &NEVER,
                    Some(&args),
                    "/2/files/list_folder/get_latest_cursor",
                )?,
            )?;
            return Ok(ChangeFeed {
                changes: Vec::new(),
                cursor: ChangeCursor(v["cursor"].as_str().context("no cursor")?.to_owned()),
                more: false,
            });
        };
        let args = json!({ "cursor": cursor });
        let (status, v) = self.api(&root, &NEVER, Some(&args), "/2/files/list_folder/continue")?;
        // `reset` (the cursor is too old), or the root folder is gone.
        if status == 409 {
            return Err(rejected());
        }
        let v = ok(&root, (status, v))?;
        let skip = base.split('/').filter(|s| !s.is_empty()).count();
        let mut changes = Vec::new();
        for e in v["entries"].as_array().into_iter().flatten() {
            // ponytail: path_display has the right casing for the last name only; an
            // older folder name spelt otherwise is matched by the next full walk.
            let Some(path) = e["path_display"].as_str() else {
                continue;
            };
            let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            if parts.len() <= skip {
                continue;
            }
            let rel = format!("/{}", parts[skip..].join("/"));
            let kind = match e[".tag"].as_str() {
                Some("deleted") => ChangeKind::Removed,
                Some("file" | "folder") => ChangeKind::Modified,
                _ => continue,
            };
            changes.push(self.changed(&rel, kind));
        }
        Ok(ChangeFeed {
            changes,
            cursor: ChangeCursor(
                v["cursor"]
                    .as_str()
                    .context("the answer has no cursor")?
                    .to_owned(),
            ),
            more: v["has_more"].as_bool() == Some(true),
        })
    }

    // --- S3 --------------------------------------------------------------------------

    /// No feed, but a bucket (or prefix) of at most one list page has a fingerprint:
    /// object count, bytes and newest LastModified. Unchanged: nothing to do; changed: the
    /// whole root is `Unknown` (walk it). Bigger: `Unsupported` (walked on the interval).
    fn s3_changes(&self, cursor: Option<ChangeCursor>) -> Result<ChangeFeed> {
        let root = self.account_root();
        let opts = options::ListOptions {
            recursive: true,
            ..Default::default()
        };
        let objects = self.call(&root, |op| {
            op.lister_options("/", opts.clone())?
                .take(S3_PAGE + 1)
                .collect::<opendal::Result<Vec<_>>>()
        })?;
        if objects.len() > S3_PAGE {
            return Err(FeedError::Unsupported.into());
        }
        let bytes: u64 = objects.iter().map(|o| o.metadata().content_length()).sum();
        let newest = (objects.iter())
            .filter_map(|o| o.metadata().last_modified())
            .map(SystemTime::from)
            .max()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let now = ChangeCursor(format!("s3:{}:{bytes}:{newest}", objects.len()));
        let changes = match cursor {
            Some(was) if was != now => vec![self.changed("/", ChangeKind::Unknown)],
            _ => Vec::new(),
        };
        Ok(ChangeFeed {
            changes,
            cursor: now,
            more: false,
        })
    }
}
