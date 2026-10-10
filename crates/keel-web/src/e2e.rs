//! Browser-test hook (cargo feature `e2e`, never in the release bundle): when the page was
//! opened with `?e2e=1`, `window.__keel = { select(name), act(name, arg), state() }` lets
//! tests/web-e2e drive the app through its own actions (`app::Act`) instead of clicking at
//! guessed positions on the egui canvas. Calls are queued and applied on the next frame;
//! `state()` is a JSON snapshot taken at the end of the last frame.

use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

#[derive(Default)]
struct Shared {
    /// (action, argument): `select` is ("select", name).
    queue: Vec<(String, Option<String>)>,
    state: String,
}

pub struct Hook(Rc<RefCell<Shared>>);

impl Hook {
    /// Installs `window.__keel` when the address carries `e2e=1`.
    pub fn install(ctx: &egui::Context) -> Option<Hook> {
        let window = web_sys::window()?;
        let query = window.location().search().ok()?;
        if !query
            .trim_start_matches('?')
            .split('&')
            .any(|p| p == "e2e=1")
        {
            return None;
        }
        let shared = Rc::new(RefCell::new(Shared {
            state: "{}".into(),
            ..Shared::default()
        }));
        let push = {
            let (shared, ctx) = (shared.clone(), ctx.clone());
            move |name: String, arg: Option<String>| {
                shared.borrow_mut().queue.push((name, arg));
                ctx.request_repaint();
            }
        };
        let select = {
            let push = push.clone();
            Closure::<dyn Fn(String)>::new(move |name: String| push("select".into(), Some(name)))
        };
        let act = Closure::<dyn Fn(String, Option<String>)>::new(push);
        let state = {
            let shared = shared.clone();
            Closure::<dyn Fn() -> String>::new(move || shared.borrow().state.clone())
        };
        let hook = js_sys::Object::new();
        for (name, f) in [
            ("select", select.as_ref()),
            ("act", act.as_ref()),
            ("state", state.as_ref()),
        ] {
            js_sys::Reflect::set(&hook, &name.into(), f).ok()?;
        }
        js_sys::Reflect::set(&window, &"__keel".into(), &hook).ok()?;
        // They live as long as the page.
        select.forget();
        act.forget();
        state.forget();
        Some(Hook(shared))
    }

    /// The calls since the last frame.
    pub fn take(&self) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut self.0.borrow_mut().queue)
    }

    pub fn publish(&self, state: String) {
        self.0.borrow_mut().state = state;
    }
}
