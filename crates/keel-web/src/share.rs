//! The share-sheet flow (tested natively). "Share → Keel" posts the files to the daemon's
//! `/share`, which parks them and opens the client at `/?share=<id>&files=<n>&bytes=<b>`.
//! The client takes the id from the address (and removes it), asks "Open N shared files?"
//! (any link can carry an id, so nothing is claimed unasked), claims the upload with
//! `share.claim` once the user says yes and it is signed in (the token goes over `/rpc`,
//! never with the share), lets the user pick one paired device, and sends exactly the
//! claimed files to exactly that device with `spacedrop.send`, which the plan dialog
//! previews and executes.

use serde_json::{json, Value};

/// A claimed file on the daemon's machine.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedFile {
    pub name: String,
    pub path: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    None,
    /// The address named an upload: the user is asked before it is claimed. The counts
    /// come from the address (unchecked until the claim lists the files).
    Asking {
        id: String,
        files: Option<u64>,
        bytes: Option<u64>,
    },
    /// The user said open: claimed once signed in.
    Waiting(String),
    /// `share.claim` sent.
    Claiming(String),
    /// The files are ours; `peer` is the device the user picked (none until then).
    Claimed {
        files: Vec<SharedFile>,
        peer: Option<String>,
    },
    Failed(String),
}

/// The upload id in an address's query (`?share=<24 hex digits>`), nothing else.
pub fn share_id(query: &str) -> Option<String> {
    query
        .trim_start_matches('?')
        .split('&')
        .find_map(|p| p.strip_prefix("share="))
        .filter(|id| id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

/// A number parameter of an address's query.
fn number(query: &str, key: &str) -> Option<u64> {
    query
        .trim_start_matches('?')
        .split('&')
        .find_map(|p| p.strip_prefix(key)?.strip_prefix('='))
        .and_then(|v| v.parse().ok())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Flow {
    pub state: State,
}

impl Flow {
    /// From `location.search` at start.
    pub fn from_query(query: &str) -> Self {
        Self {
            state: share_id(query).map_or(State::None, |id| State::Asking {
                id,
                files: number(query, "files"),
                bytes: number(query, "bytes"),
            }),
        }
    }

    /// The user said open: the upload is claimed (now, or once signed in).
    pub fn accept(&mut self) {
        if let State::Asking { id, .. } = &self.state {
            self.state = State::Waiting(id.clone());
        }
    }

    /// One file's "Send to device…". Refused (false) while the share sheet's files are being
    /// asked about, claimed or sent, so they are not dropped.
    pub fn send_one(&mut self, file: SharedFile) -> bool {
        if !matches!(self.state, State::None | State::Failed(_)) {
            return false;
        }
        self.state = State::Claimed {
            files: vec![file],
            peer: None,
        };
        true
    }

    pub fn active(&self) -> bool {
        self.state != State::None
    }

    /// Signed in: the claim to send (once per sign-in; a claim the connection lost is
    /// tried again).
    pub fn claim(&mut self) -> Option<(&'static str, Value)> {
        let id = match &self.state {
            State::Waiting(id) | State::Claiming(id) => id.clone(),
            _ => return None,
        };
        self.state = State::Claiming(id.clone());
        Some(("share.claim", json!({ "id": id })))
    }

    pub fn on_claimed(&mut self, result: Result<Value, String>) {
        if !matches!(self.state, State::Claiming(_)) {
            return;
        }
        let files = result.and_then(|v| {
            v["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|f| {
                            Some(SharedFile {
                                name: f["name"].as_str()?.to_owned(),
                                path: f["path"].as_str()?.to_owned(),
                                size: f["size"].as_u64()?,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|f| !f.is_empty())
                .ok_or_else(|| "the share holds no files".to_owned())
        });
        self.state = match files {
            Ok(files) => State::Claimed { files, peer: None },
            Err(why) => State::Failed(format!("Cannot open the shared files: {why}")),
        };
    }

    /// The user picked the device to send to.
    pub fn pick(&mut self, id: &str) {
        if let State::Claimed { peer, .. } = &mut self.state {
            *peer = Some(id.to_owned());
        }
    }

    pub fn picked(&self) -> Option<&str> {
        match &self.state {
            State::Claimed { peer, .. } => peer.as_deref(),
            _ => None,
        }
    }

    /// The `spacedrop.send` parameters: the claimed files and the picked device; None
    /// until the user picked one (there is no default device).
    pub fn send_params(&self) -> Option<Value> {
        match &self.state {
            State::Claimed {
                files,
                peer: Some(peer),
            } => {
                let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
                Some(json!({ "peer": peer, "paths": paths }))
            }
            _ => None,
        }
    }

    /// Sent (the drop's job finished), or dismissed.
    pub fn close(&mut self) {
        self.state = State::None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef01234567";

    #[test]
    fn takes_only_a_well_formed_share_id() {
        assert_eq!(share_id(&format!("?share={ID}")).as_deref(), Some(ID));
        assert_eq!(
            share_id(&format!("?x=1&share={}", ID.to_uppercase())).as_deref(),
            Some(ID)
        );
        assert_eq!(share_id("?share=../../etc"), None);
        assert_eq!(share_id(&format!("?share={ID}0")), None);
        assert_eq!(share_id(""), None);
        // An id never looks like a token to the address guard.
        assert!(!crate::guard::url_carries_token(
            &format!("?share={ID}"),
            ""
        ));
        assert!(!Flow::from_query("?view=grid").active());
    }

    #[test]
    fn asks_before_claiming() {
        let mut f = Flow::from_query(&format!("?share={ID}&files=3&bytes=12345"));
        assert_eq!(
            f.state,
            State::Asking {
                id: ID.into(),
                files: Some(3),
                bytes: Some(12345)
            }
        );
        assert!(f.active(), "the question shows");
        assert!(f.claim().is_none(), "nothing is claimed unasked");
        assert!(!f.send_one(file("x")), "a pending share is not dropped");
        f.accept();
        assert_eq!(f.state, State::Waiting(ID.into()));
        assert!(!f.send_one(file("x")));
        assert!(f.claim().is_some());
        assert!(!f.send_one(file("x")), "nor while it is claimed");
        // Dismissed: nothing claimed.
        let mut f = Flow::from_query(&format!("?share={ID}&files=x"));
        assert!(matches!(
            f.state,
            State::Asking {
                files: None,
                bytes: None,
                ..
            }
        ));
        f.close();
        assert!(f.claim().is_none());
        // With no share going on, "Send to device…" opens the window with that file.
        assert!(f.send_one(file("a.jpg")));
        assert!(matches!(&f.state, State::Claimed { files, .. } if files[0].name == "a.jpg"));
        assert!(!f.send_one(file("b.jpg")), "one at a time");
    }

    fn file(name: &str) -> SharedFile {
        SharedFile {
            name: name.into(),
            path: format!("/x/{name}"),
            size: 1,
        }
    }

    #[test]
    fn claims_once_signed_in_then_sends_only_to_the_picked_device() {
        let mut f = Flow::from_query(&format!("?share={ID}"));
        f.accept();
        assert_eq!(f.state, State::Waiting(ID.into()));
        assert_eq!(f.send_params(), None);
        let (method, params) = f.claim().unwrap();
        assert_eq!((method, params), ("share.claim", json!({"id": ID})));
        // The connection dropped before the answer: claimed again after signing in.
        assert!(f.claim().is_some());
        f.on_claimed(Ok(json!({"id": ID, "files": [
            {"name": "a.jpg", "path": "/data/shares/x/a.jpg", "size": 3},
            {"name": "b.txt", "path": "/data/shares/x/b.txt", "size": 4},
        ]})));
        assert!(matches!(&f.state, State::Claimed { files, peer: None } if files.len() == 2));
        assert_eq!(f.send_params(), None, "no device picked: nothing to send");
        assert!(f.claim().is_none(), "claimed once");
        f.pick("peer-b");
        f.pick("peer-c");
        assert_eq!(f.picked(), Some("peer-c"));
        assert_eq!(
            f.send_params(),
            Some(
                json!({"peer": "peer-c", "paths": ["/data/shares/x/a.jpg", "/data/shares/x/b.txt"]})
            )
        );
        f.close();
        assert!(!f.active());
        assert_eq!(f.send_params(), None);
    }

    #[test]
    fn a_refused_or_empty_claim_fails() {
        let mut f = Flow::from_query(&format!("?share={ID}"));
        f.accept();
        f.on_claimed(Ok(json!({"files": []})));
        assert_eq!(f.state, State::Waiting(ID.into()), "an answer to no claim");
        f.claim();
        f.on_claimed(Err("this share was opened already".into()));
        assert!(matches!(&f.state, State::Failed(m) if m.contains("opened already")));
        f.pick("peer");
        assert_eq!(f.send_params(), None);
        let mut f = Flow::from_query(&format!("?share={ID}"));
        f.accept();
        f.claim();
        f.on_claimed(Ok(json!({"files": []})));
        assert!(matches!(f.state, State::Failed(_)));
    }
}
