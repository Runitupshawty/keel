//! What opendal does not cover: storage quotas and share links, from each service's own
//! API (Drive v3, Dropbox v2) with the account's access token, and S3 presigned GETs from
//! opendal's signer. Errors carry the HTTP status only, never server text or tokens.
use super::{upload::http_error, *};
use crate::{Quota, ShareLink};
use serde_json::{json, Value};

/// How long an S3 link works.
pub const S3_LINK_TTL: Duration = Duration::from_secs(60 * 60);

const DRIVE_NOTE: &str = "Link copied. It opens only for people who already have access.";
const DROPBOX_ASK: &str = "Create a link anyone can open?";
const S3_ASK: &str = "Create a link anyone can open for the next hour? It cannot be withdrawn \
                      before it expires.";

/// The service API hosts (tests point `Core::api` at a fake).
pub(super) fn api_base(kind: CloudKind) -> &'static str {
    match kind {
        CloudKind::GoogleDrive => "https://www.googleapis.com",
        CloudKind::Dropbox => "https://api.dropboxapi.com",
        CloudKind::S3 | CloudKind::WebDav => "",
    }
}
/// Where uploads go: Dropbox takes file content on a host of its own.
pub(super) fn content_base(kind: CloudKind) -> &'static str {
    match kind {
        CloudKind::Dropbox => "https://content.dropboxapi.com",
        kind => api_base(kind),
    }
}

/// The body of a 2xx answer.
fn ok(p: &VPath, (status, body): (u16, Value)) -> Result<Value> {
    match status {
        200..=299 => Ok(body),
        s => Err(http_error(p, s)),
    }
}
/// A link worth putting on the clipboard.
fn https(url: Option<&str>) -> Result<String> {
    url.filter(|u| u.starts_with("https://"))
        .map(str::to_owned)
        .context("the service answered without a link")
}
/// Drive answers a field of the query only: `'` and `\` escaped.
fn drive_quote(name: &str) -> String {
    name.replace('\\', "\\\\").replace('\'', "\\'")
}

impl Core {
    /// `p` below the account's root folder, as the service names it: `/a/b`, or `/`.
    pub(super) fn service_path(&self, p: &VPath) -> String {
        let parts: Vec<&str> = (self.account.root.as_deref().unwrap_or("").split('/'))
            .chain(p.path.split('/'))
            .filter(|s| !s.is_empty())
            .collect();
        format!("/{}", parts.join("/"))
    }

    /// One request to the service's own API (see `exchange`): the status and the JSON
    /// body (Null when it is none).
    pub(super) fn api(
        &self,
        p: &VPath,
        cancel: &AtomicBool,
        post: Option<&Value>,
        path_and_query: &str,
    ) -> Result<(u16, Value)> {
        anyhow::ensure!(self.oauth.is_some(), "this account has no service API");
        let url = format!("{}{path_and_query}", self.api);
        let body = post.map(|b| b.to_string().into_bytes());
        let a = self.exchange(p, cancel, MAX_TRIES, upload::API_TIMEOUT, &|| match &body {
            Some(body) => upload::request(
                http::Method::POST,
                &url,
                &[("content-type", "application/json")],
                body.clone(),
            ),
            None => upload::request(http::Method::GET, &url, &[], Vec::new()),
        })?;
        Ok((a.status, a.json()))
    }

    pub(super) fn quota(&self) -> Result<Quota> {
        let root = VPath {
            scheme: "cloud".into(),
            authority: self.account.id.clone(),
            path: "/".into(),
        };
        match self.account.kind {
            CloudKind::GoogleDrive => {
                let v = ok(
                    &root,
                    self.api(&root, &NEVER, None, "/drive/v3/about?fields=storageQuota")?,
                )?;
                // int64 values come as strings.
                let field = |f: &str| {
                    let v = &v["storageQuota"][f];
                    v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64())
                };
                Ok(Quota {
                    used: field("usage").context("the answer has no storage usage")?,
                    // No limit: unlimited storage.
                    total: field("limit"),
                })
            }
            CloudKind::Dropbox => {
                let v = ok(
                    &root,
                    self.api(
                        &root,
                        &NEVER,
                        Some(&Value::Null),
                        "/2/users/get_space_usage",
                    )?,
                )?;
                let a = &v["allocation"];
                // A team member may have a limit of their own within the team's space.
                let own = a["user_within_team_space_allocated"]
                    .as_u64()
                    .filter(|n| *n > 0);
                Ok(Quota {
                    used: v["used"]
                        .as_u64()
                        .context("the answer has no storage usage")?,
                    total: own.or(a["allocated"].as_u64()).filter(|n| *n > 0),
                })
            }
            CloudKind::S3 | CloudKind::WebDav => anyhow::bail!("the service reports no quota"),
        }
    }

    /// Drive's id for `p`, one lookup per folder from the top (the first of two items
    /// with one name, like listings).
    pub(super) fn drive_id(&self, p: &VPath, cancel: &AtomicBool) -> Result<String> {
        let mut id = "root".to_owned();
        for name in self.service_path(p).split('/').filter(|s| !s.is_empty()) {
            let q = format!(
                "'{id}' in parents and name = '{}' and trashed = false",
                drive_quote(name)
            );
            let q: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
            let v = ok(
                p,
                self.api(
                    p,
                    cancel,
                    None,
                    &format!("/drive/v3/files?q={q}&fields=files(id)"),
                )?,
            )?;
            id = v["files"][0]["id"]
                .as_str()
                .ok_or_else(|| not_found(p))?
                .to_owned();
            // It goes into the next query and a URL path.
            anyhow::ensure!(
                id.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
                "the service answered an unexpected file id"
            );
        }
        Ok(id)
    }

    pub(super) fn share_link(&self, p: &VPath, create: bool) -> Result<ShareLink> {
        self.validate(p)?;
        anyhow::ensure!(p.parent().is_some(), "the account's root has no link");
        let ready = |url: String, note: &str| ShareLink::Ready {
            url,
            note: note.to_owned(),
        };
        let ask = |q: &str| ShareLink::Confirm {
            question: q.to_owned(),
        };
        match self.account.kind {
            CloudKind::GoogleDrive => {
                let id = self.drive_id(p, &NEVER)?;
                let path = format!("/drive/v3/files/{id}?fields=webViewLink");
                let v = ok(p, self.api(p, &NEVER, None, &path)?)?;
                Ok(ready(https(v["webViewLink"].as_str())?, DRIVE_NOTE))
            }
            CloudKind::Dropbox => {
                let path = self.service_path(p);
                let args = json!({ "path": path, "direct_only": true });
                let v = ok(
                    p,
                    self.api(p, &NEVER, Some(&args), "/2/sharing/list_shared_links")?,
                )?;
                let existing =
                    (v["links"].as_array().into_iter().flatten()).find_map(|l| l["url"].as_str());
                if let Some(url) = existing {
                    return Ok(ready(https(Some(url))?, "Link copied."));
                }
                if !create {
                    return Ok(ask(DROPBOX_ASK));
                }
                let args = json!({ "path": path });
                let (status, v) = self.api(
                    p,
                    &NEVER,
                    Some(&args),
                    "/2/sharing/create_shared_link_with_settings",
                )?;
                // Made meanwhile (another client): Dropbox answers 409 with that link.
                let link = match status {
                    409 => v["error"]["shared_link_already_exists"]["metadata"]["url"].clone(),
                    _ => ok(p, (status, v))?["url"].clone(),
                };
                if status == 409 && link.is_null() {
                    return Err(http_error(p, 409));
                }
                Ok(ready(
                    https(link.as_str())?,
                    "Link created and copied. Anyone with it can open it.",
                ))
            }
            CloudKind::S3 if !create => Ok(ask(S3_ASK)),
            CloudKind::S3 => {
                let k = key(p, false);
                let signed = self
                    .op
                    .read()
                    .presign_read(&k, S3_LINK_TTL)
                    .map_err(|e| wire(&e, p))?;
                Ok(ready(
                    signed.uri().to_string(),
                    "Link copied. It works for 1 hour.",
                ))
            }
            CloudKind::WebDav => anyhow::bail!("WebDAV accounts have no share links"),
        }
    }
}
