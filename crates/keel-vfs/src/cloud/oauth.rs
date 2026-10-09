//! OAuth 2.0 authorization-code flow with PKCE over a loopback redirect (RFC 8252): the
//! browser signs in, the provider redirects to `http://127.0.0.1:<random port>/`, and the
//! code is exchanged for tokens. No client secret is needed for Dropbox; Google "Desktop
//! app" clients send theirs (Google documents it as not confidential).
use super::CloudKind;
use crate::{ConnStatus, RemoteEvent};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use crossbeam_channel::Sender;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime},
};

/// How long the browser hand-off may take before the flow gives up.
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, PartialEq, Eq)]
pub struct OAuthTokens {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_at: SystemTime,
}
/// Never prints the tokens.
impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("refresh", &self.refresh.is_some())
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// An app registration: public client id, plus Google's installed-app secret if any.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub id: String,
    pub secret: Option<String>,
}
impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Endpoints {
    pub auth: String,
    pub token: String,
    pub scope: &'static str,
    pub extra: &'static [(&'static str, &'static str)],
}
impl Endpoints {
    pub(crate) fn for_kind(kind: CloudKind) -> Result<Self> {
        Ok(match kind {
            CloudKind::GoogleDrive => Self {
                auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token: "https://oauth2.googleapis.com/token".into(),
                scope: "https://www.googleapis.com/auth/drive",
                // Offline + consent: always return a refresh token.
                extra: &[("access_type", "offline"), ("prompt", "consent")],
            },
            CloudKind::Dropbox => Self {
                auth: "https://www.dropbox.com/oauth2/authorize".into(),
                token: "https://api.dropboxapi.com/oauth2/token".into(),
                scope: "files.content.read files.content.write files.metadata.read",
                extra: &[("token_access_type", "offline")],
            },
            CloudKind::S3 => anyhow::bail!("S3 uses access keys, not OAuth"),
        })
    }
}

/// Signs in through the system browser and returns the tokens (store them with
/// `cloud::store_tokens`). Blocks up to `AUTH_TIMEOUT`: call on a worker thread; set
/// `cancel` to give up early. Progress goes to `events` as `Status` for
/// `cloud-auth:<kind>`.
pub fn oauth_authorize(
    kind: CloudKind,
    client: &OAuthClient,
    events: &Sender<RemoteEvent>,
    cancel: &AtomicBool,
) -> Result<OAuthTokens> {
    let id = format!("cloud-auth:{kind:?}");
    let status = |status, detail: &str| {
        let _ = events.send(RemoteEvent::Status {
            host_id: id.clone(),
            status,
            detail: detail.into(),
        });
    };
    status(ConnStatus::Connecting, "waiting for sign-in in the browser");
    let result = authorize(
        &Endpoints::for_kind(kind)?,
        client,
        cancel,
        AUTH_TIMEOUT,
        &|url| open::that_detached(url).context("could not open the browser"),
    );
    match &result {
        Ok(_) => status(ConnStatus::Connected, "signed in"),
        Err(e) => status(ConnStatus::Failed, &format!("{e:#}")),
    }
    result
}

fn random(bytes: usize) -> Result<String> {
    let mut buf = vec![0; bytes];
    getrandom::fill(&mut buf).map_err(|_| anyhow::anyhow!("no OS randomness"))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

const DONE_PAGE: &str = "<!doctype html><title>Keel</title><p>Signed in. You can close this \
                         tab and return to Keel.</p>";
const FAILED_PAGE: &str = "<!doctype html><title>Keel</title><p>Sign-in failed. Return to Keel \
                           and try again.</p>";
fn page(html: &str, code: u16) -> tiny_http::Response<io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(html)
        .with_status_code(code)
        .with_header(
            tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8")
                .expect("static header"),
        )
}

/// The flow with injectable endpoints and browser (tests drive both).
pub(crate) fn authorize(
    endpoints: &Endpoints,
    client: &OAuthClient,
    cancel: &AtomicBool,
    timeout: Duration,
    open_browser: &dyn Fn(&str) -> Result<()>,
) -> Result<OAuthTokens> {
    anyhow::ensure!(
        !client.id.trim().is_empty(),
        "no OAuth client id configured"
    );
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|_| anyhow::anyhow!("cannot listen on the loopback interface"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("loopback listener has no port")?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/");
    let verifier = random(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random(16)?;
    let mut url = url::Url::parse(&endpoints.auth).context("invalid authorization endpoint")?;
    url.query_pairs_mut()
        .append_pair("client_id", &client.id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", endpoints.scope)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .extend_pairs(endpoints.extra);
    open_browser(url.as_str())?;
    let deadline = Instant::now() + timeout;
    let code = loop {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "sign-in cancelled");
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !left.is_zero(),
            "sign-in timed out after {} s",
            timeout.as_secs()
        );
        let Some(request) = server
            .recv_timeout(left.min(Duration::from_millis(100)))
            .context("loopback listener failed")?
        else {
            continue;
        };
        let query = match url::Url::parse(&format!("http://127.0.0.1{}", request.url())) {
            Ok(u) if u.path() == "/" => u
                .query_pairs()
                .into_owned()
                .collect::<HashMap<String, String>>(),
            // Favicon and other stray requests: not ours.
            _ => {
                let _ = request.respond(tiny_http::Response::empty(404));
                continue;
            }
        };
        // The state is single-use: a mismatch (forged or stale tab) ends the attempt.
        if query.get("state") != Some(&state) {
            let _ = request.respond(page(FAILED_PAGE, 400));
            anyhow::bail!("sign-in response refused: it does not match this request (state)");
        }
        let Some(code) = query.get("code").filter(|_| !query.contains_key("error")) else {
            let _ = request.respond(page(FAILED_PAGE, 400));
            anyhow::bail!("sign-in was declined or failed in the browser");
        };
        let _ = request.respond(page(DONE_PAGE, 200));
        break code.clone();
    };
    exchange(
        endpoints,
        client,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
        ],
        None,
    )
}

/// New tokens from a refresh token. A revoked or expired grant fails with
/// `io::ErrorKind::PermissionDenied` ("sign in again"), never a panic.
pub(crate) fn refresh(
    endpoints: &Endpoints,
    client: &OAuthClient,
    refresh_token: &str,
) -> Result<OAuthTokens> {
    exchange(
        endpoints,
        client,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
        Some(refresh_token),
    )
}

#[derive(serde::Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}
#[derive(serde::Deserialize)]
struct ErrorReply {
    error: String,
}

fn unreachable_endpoint(_: reqwest::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "token endpoint unreachable",
    )
}

/// POSTs to the token endpoint. Server text never reaches errors (only the HTTP status
/// and the standard `invalid_grant` code are interpreted).
fn exchange(
    endpoints: &Endpoints,
    client: &OAuthClient,
    form: &[(&str, &str)],
    keep_refresh: Option<&str>,
) -> Result<OAuthTokens> {
    let mut form = form.to_vec();
    form.push(("client_id", &client.id));
    if let Some(secret) = client.secret.as_deref().filter(|s| !s.is_empty()) {
        form.push(("client_secret", secret));
    }
    let http = super::http_client()?;
    let (status, body) = crate::sftp::conn::runtime().block_on(async {
        let reply = http
            .post(&endpoints.token)
            .form(&form)
            .send()
            .await
            .map_err(unreachable_endpoint)?;
        let status = reply.status();
        let body = reply.bytes().await.map_err(unreachable_endpoint)?;
        io::Result::Ok((status, body))
    })?;
    if !status.is_success() {
        let invalid_grant =
            serde_json::from_slice::<ErrorReply>(&body).is_ok_and(|e| e.error == "invalid_grant");
        if invalid_grant {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the cloud sign-in expired or was revoked; sign in again",
            )
            .into());
        }
        anyhow::bail!(
            "token endpoint refused the request (HTTP {})",
            status.as_u16()
        );
    }
    let reply: TokenReply =
        serde_json::from_slice(&body).context("token endpoint sent an invalid reply")?;
    anyhow::ensure!(
        !reply.access_token.is_empty(),
        "token endpoint sent no access token"
    );
    Ok(OAuthTokens {
        access: reply.access_token,
        refresh: reply.refresh_token.or(keep_refresh.map(str::to_owned)),
        expires_at: SystemTime::now() + Duration::from_secs(reply.expires_in.unwrap_or(3600)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        thread,
    };

    /// A fake authorization server: `/auth` redirects straight back to the loopback with a
    /// code (or with a forged state), `/token` checks the PKCE verifier and hands out tokens.
    struct FakeAuth {
        endpoints: Endpoints,
        token_requests: Arc<Mutex<Vec<HashMap<String, String>>>>,
    }
    fn fake_auth(forge_state: bool) -> FakeAuth {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let token_requests = Arc::new(Mutex::new(Vec::new()));
        let seen = token_requests.clone();
        thread::spawn(move || {
            let mut challenge = String::new();
            for mut request in server.incoming_requests() {
                let url = url::Url::parse(&format!("http://x{}", request.url())).unwrap();
                let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
                match url.path() {
                    "/auth" => {
                        challenge = q["code_challenge"].clone();
                        assert_eq!(q["code_challenge_method"], "S256");
                        assert_eq!(q["client_id"], "public-client");
                        let state = if forge_state { "forged" } else { &q["state"] };
                        let to = format!("{}?code=the-code&state={state}", q["redirect_uri"]);
                        let header = tiny_http::Header::from_bytes("Location", to).unwrap();
                        let _ =
                            request.respond(tiny_http::Response::empty(302).with_header(header));
                    }
                    "/token" => {
                        let mut body = String::new();
                        request.as_reader().read_to_string(&mut body).unwrap();
                        let form: HashMap<String, String> =
                            url::form_urlencoded::parse(body.as_bytes())
                                .into_owned()
                                .collect();
                        let ok = form.get("code").map(String::as_str) == Some("the-code")
                            && URL_SAFE_NO_PAD
                                .encode(Sha256::digest(form["code_verifier"].as_bytes()))
                                == challenge;
                        seen.lock().unwrap().push(form);
                        let reply = if ok {
                            tiny_http::Response::from_string(
                                r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#,
                            )
                        } else {
                            tiny_http::Response::from_string(r#"{"error":"invalid_grant"}"#)
                                .with_status_code(400)
                        };
                        let _ = request.respond(reply);
                    }
                    _ => {
                        let _ = request.respond(tiny_http::Response::empty(404));
                    }
                }
            }
        });
        FakeAuth {
            endpoints: Endpoints {
                auth: format!("http://127.0.0.1:{port}/auth"),
                token: format!("http://127.0.0.1:{port}/token"),
                scope: "files",
                extra: &[],
            },
            token_requests,
        }
    }
    fn client() -> OAuthClient {
        OAuthClient {
            id: "public-client".into(),
            secret: None,
        }
    }
    /// The "browser": follows the authorization redirect to the loopback listener.
    fn browser(url: &str) -> Result<()> {
        let url = url.to_owned();
        thread::spawn(move || {
            let http = crate::cloud::http_client().unwrap();
            let _ = crate::sftp::conn::runtime().block_on(async {
                // Our client never follows redirects: follow the one hop by hand.
                let reply = http.get(&url).send().await?;
                let to = reply.headers()["location"].to_str().unwrap().to_owned();
                http.get(to).send().await
            });
        });
        Ok(())
    }

    #[test]
    fn code_is_exchanged_for_tokens_with_pkce() {
        let fake = fake_auth(false);
        let tokens = authorize(
            &fake.endpoints,
            &client(),
            &AtomicBool::new(false),
            Duration::from_secs(10),
            &browser,
        )
        .unwrap();
        assert_eq!(tokens.access, "at-1");
        assert_eq!(tokens.refresh.as_deref(), Some("rt-1"));
        assert!(tokens.expires_at > SystemTime::now() + Duration::from_secs(3000));
        let requests = fake.token_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["grant_type"], "authorization_code");
        assert!(requests[0]["redirect_uri"].starts_with("http://127.0.0.1:"));
        assert!(!requests[0].contains_key("client_secret"));
        assert!(
            !format!("{tokens:?}").contains("at-1"),
            "Debug hides tokens"
        );
    }

    #[test]
    fn forged_state_is_refused_without_a_token_request() {
        let fake = fake_auth(true);
        let err = authorize(
            &fake.endpoints,
            &client(),
            &AtomicBool::new(false),
            Duration::from_secs(10),
            &browser,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("state"), "{err:#}");
        assert!(fake.token_requests.lock().unwrap().is_empty());
    }

    #[test]
    fn timeout_and_cancel_end_the_wait() {
        let fake = fake_auth(false);
        let started = Instant::now();
        let err = authorize(
            &fake.endpoints,
            &client(),
            &AtomicBool::new(false),
            Duration::from_millis(300),
            &|_| Ok(()), // the user never finishes in the browser
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let err = authorize(
            &fake.endpoints,
            &client(),
            &AtomicBool::new(true),
            Duration::from_secs(60),
            &|_| Ok(()),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
    }

    #[test]
    fn refresh_maps_a_revoked_grant_to_sign_in_again() {
        let fake = fake_auth(false);
        // The fake only accepts "the-code": a refresh is answered like a revoked grant.
        let err = refresh(&fake.endpoints, &client(), "rt-old").unwrap_err();
        assert_eq!(
            err.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
        assert!(!format!("{err:#}").contains("rt-old"));
        let sent = &fake.token_requests.lock().unwrap()[0];
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["refresh_token"], "rt-old");
    }
}
