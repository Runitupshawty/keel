//! Browser glue (wasm32 only): the WebSocket to `/rpc`, the token in `localStorage` (only
//! when "remember on this device" is ticked), the address guard, the share id in the
//! address, the plain-http check, downloads, and start-up.

use crate::conn::{Event, Transport};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    ClipboardEvent, CloseEvent, HtmlInputElement, KeyboardEvent, MessageEvent, WebSocket,
};

/// Socket events, tagged with the socket's generation (events of a replaced socket are
/// dropped).
pub(crate) type Events = Rc<RefCell<Vec<(u64, Event)>>>;

const TOKEN_KEY: &str = "keel.daemon-token";

pub(crate) struct Socket {
    ws: Option<WebSocket>,
    _open: Closure<dyn FnMut()>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(CloseEvent)>,
}

/// `ws(s)://<this host>/rpc`: same origin, nothing in the address but the path.
fn rpc_url() -> Option<String> {
    let loc = web_sys::window()?.location();
    let scheme = if loc.protocol().ok()? == "https:" {
        "wss:"
    } else {
        "ws:"
    };
    Some(format!("{scheme}//{}/rpc", loc.host().ok()?))
}

impl Socket {
    pub(crate) fn open(gen: u64, events: Events, ctx: egui::Context) -> Socket {
        let push = {
            let (events, ctx) = (events.clone(), ctx.clone());
            move |ev: Event| {
                events.borrow_mut().push((gen, ev));
                ctx.request_repaint();
            }
        };
        let (p1, p2, p3) = (push.clone(), push.clone(), push.clone());
        let open = Closure::<dyn FnMut()>::new(move || p1(Event::Open));
        let message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Some(text) = e.data().as_string() {
                p2(Event::Message(text));
            }
        });
        let close = Closure::<dyn FnMut(CloseEvent)>::new(move |_| p3(Event::Closed));
        let ws = rpc_url().and_then(|url| WebSocket::new(&url).ok());
        match &ws {
            Some(ws) => {
                ws.set_onopen(Some(open.as_ref().unchecked_ref()));
                ws.set_onmessage(Some(message.as_ref().unchecked_ref()));
                ws.set_onclose(Some(close.as_ref().unchecked_ref()));
            }
            None => push(Event::Closed),
        }
        Socket {
            ws,
            _open: open,
            _message: message,
            _close: close,
        }
    }
}

impl Transport for Socket {
    fn send(&mut self, text: &str) {
        if let Some(ws) = &self.ws {
            let _ = ws.send_with_str(text);
        }
    }

    fn close(&mut self) {
        if let Some(ws) = &self.ws {
            let _ = ws.close();
        }
    }
}

impl Drop for Socket {
    /// The closures die with this: detach them first.
    fn drop(&mut self) {
        if let Some(ws) = self.ws.take() {
            ws.set_onopen(None);
            ws.set_onmessage(None);
            ws.set_onclose(None);
            let _ = ws.close();
        }
    }
}

/// Seconds since the epoch.
pub(crate) fn now() -> f64 {
    js_sys::Date::now() / 1000.0
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

/// The token remembered on this device, if the user asked for that.
pub(crate) fn remembered_token() -> Option<String> {
    storage()?.get_item(TOKEN_KEY).ok()?
}

pub(crate) fn remember_token(token: Option<&str>) {
    if let Some(s) = storage() {
        let _ = match token {
            Some(t) => s.set_item(TOKEN_KEY, t),
            None => s.remove_item(TOKEN_KEY),
        };
    }
}

/// The sign-in box: the real `<input type=password id=token>` of index.html, laid over the
/// login form. egui's own text field is drawn on the canvas, so a phone offers no Paste for
/// it and password managers cannot fill it.
pub(crate) struct TokenField {
    input: HtmlInputElement,
    enter: Rc<Cell<bool>>,
    _key: Closure<dyn FnMut(KeyboardEvent)>,
    _paste: Closure<dyn FnMut(ClipboardEvent)>,
}

impl TokenField {
    /// `ctx` repaints when Enter is pressed in the box.
    pub(crate) fn find(ctx: &egui::Context) -> Option<TokenField> {
        let input = web_sys::window()?
            .document()?
            .get_element_by_id("token")?
            .dyn_into::<HtmlInputElement>()
            .ok()?;
        let enter = Rc::new(Cell::new(false));
        let (pressed, ctx) = (enter.clone(), ctx.clone());
        let key = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                pressed.set(true);
                ctx.request_repaint();
            }
        });
        // eframe cancels every paste on the page (its handler is on the document), so the
        // box takes the pasted text itself and keeps the event away from eframe. A token is
        // pasted whole: it replaces what was in the box.
        let target = input.clone();
        let paste = Closure::<dyn FnMut(ClipboardEvent)>::new(move |e: ClipboardEvent| {
            e.stop_propagation();
            if let Some(text) = e.clipboard_data().and_then(|d| d.get_data("text").ok()) {
                e.prevent_default();
                target.set_value(text.trim());
            }
        });
        input
            .add_event_listener_with_callback("keydown", key.as_ref().unchecked_ref())
            .ok()?;
        input
            .add_event_listener_with_callback("paste", paste.as_ref().unchecked_ref())
            .ok()?;
        Some(TokenField {
            input,
            enter,
            _key: key,
            _paste: paste,
        })
    }

    /// Shows the box over `rect` (egui points are CSS pixels here: Keel sets no zoom).
    pub(crate) fn show(&self, rect: egui::Rect) {
        let style = self.input.style();
        for (name, px) in [
            ("left", rect.left()),
            ("top", rect.top()),
            ("width", rect.width()),
            ("height", rect.height()),
        ] {
            let _ = style.set_property(name, &format!("{px}px"));
        }
        self.input.set_hidden(false);
    }

    /// Hides and empties the box (signed in, or no longer asking).
    pub(crate) fn hide(&self) {
        if !self.input.hidden() {
            self.input.set_value("");
            self.input.set_hidden(true);
        }
    }

    /// The token in the box once Enter was pressed there or `submit` is true (the Connect
    /// button); the box is emptied.
    pub(crate) fn take(&self, submit: bool) -> Option<String> {
        let entered = self.enter.replace(false);
        let token = self.input.value().trim().to_owned();
        if !(submit || entered) || token.is_empty() {
            return None;
        }
        self.input.set_value("");
        Some(token)
    }
}

/// Refuses a token in the address: true when one was there (it is removed from the address
/// bar and the current history entry, and never used).
pub(crate) fn scrub_address() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let loc = window.location();
    let (query, fragment) = (
        loc.search().unwrap_or_default(),
        loc.hash().unwrap_or_default(),
    );
    if !crate::guard::url_carries_token(&query, &fragment) {
        return false;
    }
    if let (Ok(history), Ok(path)) = (window.history(), loc.pathname()) {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&path));
    }
    true
}

/// The address's query (`?share=<id>` after a share), then removed from the address bar
/// and the current history entry, so a reload does not claim the share again.
pub(crate) fn take_query() -> String {
    let Some(window) = web_sys::window() else {
        return String::new();
    };
    let loc = window.location();
    let query = loc.search().unwrap_or_default();
    if !query.is_empty() {
        if let (Ok(history), Ok(path)) = (window.history(), loc.pathname()) {
            let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&path));
        }
    }
    query
}

/// Plain http from a non-loopback host (`crate::guard::insecure`).
pub(crate) fn insecure() -> bool {
    let Some(loc) = web_sys::window().map(|w| w.location()) else {
        return false;
    };
    crate::guard::insecure(
        &loc.protocol().unwrap_or_default(),
        &loc.hostname().unwrap_or_default(),
    )
}

/// Downloads a `/file/<token>` link (same origin, no referrer).
pub(crate) fn download(url: &str) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Ok(a) = doc.create_element("a") else {
        return;
    };
    let Ok(a) = a.dyn_into::<web_sys::HtmlAnchorElement>() else {
        return;
    };
    a.set_href(url);
    a.set_download("");
    a.set_rel("noreferrer");
    a.click();
}

/// A Unix time as `YYYY-MM-DD HH:MM` (local).
pub(crate) fn date(secs: i64) -> String {
    let d = js_sys::Date::new(&JsValue::from_f64(secs as f64 * 1000.0));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date(),
        d.get_hours(),
        d.get_minutes()
    )
}

#[wasm_bindgen(start)]
pub fn start() {
    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("keel"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok());
        let Some(canvas) = canvas else {
            web_sys::console::error_1(&"keel-web: no <canvas id=keel>".into());
            return;
        };
        let started = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| Ok(Box::new(crate::app::WebApp::new(cc)))),
            )
            .await;
        if let Err(e) = started {
            web_sys::console::error_1(&e);
        }
    });
}
