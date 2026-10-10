//! The connection to keel-daemon's `/rpc`, independent of the browser so it is tested
//! natively: authenticate with the first message, then JSON-RPC; reconnect with backoff
//! after a drop; stop (and ask for the token again) when the daemon refuses the token.

use serde_json::{json, Value};
use std::collections::HashSet;

/// What a socket reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Open,
    Message(String),
    Closed,
}

/// A WebSocket to `/rpc` (the browser's, or a test double).
pub trait Transport {
    fn send(&mut self, text: &str);
    fn close(&mut self);
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// No token yet.
    NeedToken,
    Connecting,
    /// `auth` sent, waiting for its answer.
    Authenticating,
    Online,
    /// Waiting to reconnect at `at` (seconds, the caller's clock).
    Retrying {
        at: f64,
    },
    /// The daemon refused the token: no retries until a new one is entered.
    Refused(String),
}

impl State {
    pub fn label(&self) -> String {
        match self {
            State::NeedToken => "not connected".into(),
            State::Connecting => "connecting…".into(),
            State::Authenticating => "signing in…".into(),
            State::Online => "connected".into(),
            State::Retrying { .. } => "offline, retrying".into(),
            State::Refused(why) => format!("token refused: {why}"),
        }
    }
}

/// An answer to one call.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub id: u64,
    pub result: Result<Value, String>,
}

/// Seconds to wait before reconnect attempt `n` (from 0): 1, 2, 4 … 30.
pub fn backoff(n: u32) -> f64 {
    f64::from(1u32 << n.min(5)).min(30.0)
}

/// The id of the `auth` message.
const AUTH_ID: u64 = 0;

pub struct Conn<T> {
    pub state: State,
    token: Option<String>,
    socket: Option<T>,
    next: u64,
    pending: HashSet<u64>,
    attempt: u32,
    replies: Vec<Reply>,
    notes: Vec<(String, Value)>,
}

impl<T: Transport> Default for Conn<T> {
    fn default() -> Self {
        Self {
            state: State::NeedToken,
            token: None,
            socket: None,
            next: AUTH_ID + 1,
            pending: HashSet::new(),
            attempt: 0,
            replies: Vec::new(),
            notes: Vec::new(),
        }
    }
}

impl<T: Transport> Conn<T> {
    /// Starts connecting with `token` (`open` creates the socket).
    pub fn sign_in(&mut self, token: String, open: impl FnOnce() -> T) {
        self.drop_socket();
        self.token = Some(token);
        self.attempt = 0;
        self.state = State::Connecting;
        self.socket = Some(open());
    }

    /// Forgets the token and disconnects.
    pub fn sign_out(&mut self) {
        self.drop_socket();
        self.token = None;
        self.state = State::NeedToken;
    }

    fn drop_socket(&mut self) {
        if let Some(mut s) = self.socket.take() {
            s.close();
        }
        for id in std::mem::take(&mut self.pending) {
            self.replies.push(Reply {
                id,
                result: Err("connection lost".into()),
            });
        }
    }

    /// Reconnects when a retry is due.
    pub fn tick(&mut self, now: f64, open: impl FnOnce() -> T) {
        if let State::Retrying { at } = self.state {
            if now >= at && self.token.is_some() {
                self.state = State::Connecting;
                self.socket = Some(open());
            }
        }
    }

    pub fn on_event(&mut self, ev: Event, now: f64) {
        match ev {
            Event::Open => {
                let token = self.token.clone().unwrap_or_default();
                if let Some(s) = &mut self.socket {
                    let auth = json!({"jsonrpc": "2.0", "id": AUTH_ID, "method": "auth",
                        "params": {"token": token}});
                    s.send(&auth.to_string());
                    self.state = State::Authenticating;
                }
            }
            Event::Closed => {
                self.drop_socket();
                if !matches!(self.state, State::Refused(_) | State::NeedToken) {
                    self.state = State::Retrying {
                        at: now + backoff(self.attempt),
                    };
                    self.attempt += 1;
                }
            }
            Event::Message(text) => self.on_message(&text),
        }
    }

    fn on_message(&mut self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            self.notes.push((
                method.to_owned(),
                v.get("params").cloned().unwrap_or_default(),
            ));
            return;
        }
        let Some(id) = v.get("id").and_then(Value::as_u64) else {
            return;
        };
        let result = match v.get("error") {
            Some(e) => Err(e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_owned()),
            None => Ok(v.get("result").cloned().unwrap_or_default()),
        };
        if id == AUTH_ID && self.state == State::Authenticating {
            match result {
                Ok(_) => {
                    self.state = State::Online;
                    self.attempt = 0;
                    self.call("subscribe", json!({}));
                }
                Err(why) => {
                    self.state = State::Refused(why);
                    self.drop_socket();
                }
            }
            return;
        }
        if self.pending.remove(&id) {
            self.replies.push(Reply { id, result });
        }
    }

    /// Sends a call; None while not online.
    pub fn call(&mut self, method: &str, params: Value) -> Option<u64> {
        if self.state != State::Online {
            return None;
        }
        let id = self.next;
        self.next += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.socket.as_mut()?.send(&msg.to_string());
        self.pending.insert(id);
        Some(id)
    }

    pub fn take_replies(&mut self) -> Vec<Reply> {
        std::mem::take(&mut self.replies)
    }

    /// Notifications (`job.progress`, `library.changed`, `net.event`) since the last call.
    pub fn take_notes(&mut self) -> Vec<(String, Value)> {
        std::mem::take(&mut self.notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Mock {
        sent: Rc<RefCell<Vec<Value>>>,
        closed: Rc<RefCell<bool>>,
    }

    impl Transport for Mock {
        fn send(&mut self, text: &str) {
            self.sent
                .borrow_mut()
                .push(serde_json::from_str(text).unwrap());
        }
        fn close(&mut self) {
            *self.closed.borrow_mut() = true;
        }
    }

    fn online(m: &Mock) -> Conn<Mock> {
        let mut c = Conn::default();
        c.sign_in("secret".into(), || m.clone());
        c.on_event(Event::Open, 0.0);
        c.on_event(
            Event::Message(r#"{"jsonrpc":"2.0","id":0,"result":{"ok":true}}"#.into()),
            0.0,
        );
        c
    }

    #[test]
    fn authenticates_first_then_calls() {
        let m = Mock::default();
        let mut c: Conn<Mock> = Conn::default();
        assert_eq!(
            c.call("version", json!({})),
            None,
            "no calls before sign-in"
        );
        c.sign_in("secret".into(), || m.clone());
        assert_eq!(c.call("version", json!({})), None, "no calls before auth");
        c.on_event(Event::Open, 0.0);
        assert_eq!(m.sent.borrow()[0]["method"], "auth");
        assert_eq!(m.sent.borrow()[0]["params"]["token"], "secret");
        c.on_event(
            Event::Message(r#"{"jsonrpc":"2.0","id":0,"result":{"ok":true}}"#.into()),
            0.0,
        );
        assert_eq!(c.state, State::Online);
        assert_eq!(m.sent.borrow()[1]["method"], "subscribe");
        let id = c.call("version", json!({})).unwrap();
        c.on_event(
            Event::Message(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"api":1}}}}"#
            )),
            0.0,
        );
        c.on_event(
            Event::Message(r#"{"jsonrpc":"2.0","method":"job.progress","params":{"id":3}}"#.into()),
            0.0,
        );
        let replies = c.take_replies();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].result, Ok(json!({"api": 1})));
        assert_eq!(
            c.take_notes(),
            vec![("job.progress".into(), json!({"id": 3}))]
        );
    }

    #[test]
    fn refused_token_stops_retrying() {
        let m = Mock::default();
        let mut c: Conn<Mock> = Conn::default();
        c.sign_in("wrong".into(), || m.clone());
        c.on_event(Event::Open, 0.0);
        c.on_event(
            Event::Message(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32007,"message":"bad token"}}"#.into(),
            ),
            0.0,
        );
        assert_eq!(c.state, State::Refused("bad token".into()));
        assert!(*m.closed.borrow());
        c.on_event(Event::Closed, 1.0);
        let mut opened = false;
        c.tick(100.0, || {
            opened = true;
            m.clone()
        });
        assert!(!opened);
        assert!(matches!(c.state, State::Refused(_)));
    }

    #[test]
    fn drops_reconnect_with_backoff_and_fail_pending_calls() {
        let m = Mock::default();
        let mut c = online(&m);
        let id = c.call("list", json!({"path": "library://s/"})).unwrap();
        c.on_event(Event::Closed, 10.0);
        assert_eq!(c.state, State::Retrying { at: 11.0 });
        let lost = Reply {
            id,
            result: Err("connection lost".into()),
        };
        assert!(c.take_replies().contains(&lost));
        let mut opens = 0;
        c.tick(10.5, || {
            opens += 1;
            m.clone()
        });
        assert_eq!(opens, 0, "too early");
        c.tick(11.0, || {
            opens += 1;
            m.clone()
        });
        assert_eq!((opens, c.state.clone()), (1, State::Connecting));
        // A second drop waits longer; a successful sign-in resets the backoff.
        c.on_event(Event::Closed, 20.0);
        assert_eq!(c.state, State::Retrying { at: 22.0 });
        assert_eq!(backoff(10), 30.0);
        c.sign_out();
        assert_eq!(c.state, State::NeedToken);
        c.on_event(Event::Closed, 30.0);
        assert_eq!(c.state, State::NeedToken);
    }
}
