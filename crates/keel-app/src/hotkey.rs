//! Global hotkeys (Task 24; more in Phase 16 Task 76), via the `global-hotkey` crate,
//! set in Settings → General: **Show** (default Ctrl+Shift+Alt+K) brings Keel's window
//! forward from anywhere, **New window** starts another Keel window, **Toggle** shows Keel
//! or minimizes it when it is in front (both off by default). The manager lives on the UI
//! thread (Windows delivers `WM_HOTKEY` through winit's message loop; macOS needs the main
//! thread); the handler runs there too (on Linux on the crate's X11 thread).
//!
//! The default is not Ctrl+Alt+K: on many keyboard layouts (German, Polish, …) AltGr is
//! Ctrl+Alt, so a Ctrl+Alt hotkey takes an AltGr character from every app. A hotkey needs
//! at least one modifier besides Shift (Shift+K alone would take capital K).

use global_hotkey::hotkey::{HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use parking_lot::Mutex;
use std::sync::Arc;

/// What a global hotkey does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Bring Keel's window to the front.
    Show,
    /// Start another Keel window (same profile).
    NewWindow,
    /// Show Keel, or minimize it when it is already in front.
    Toggle,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Show, Kind::NewWindow, Kind::Toggle];

    fn index(self) -> usize {
        self as usize
    }

    /// The Settings row's label ("Global hotkey" is the Show one, as before).
    pub fn label(self) -> &'static str {
        match self {
            Kind::Show => "Global hotkey",
            Kind::NewWindow => "New window hotkey",
            Kind::Toggle => "Toggle window hotkey",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Kind::Show => "Brings Keel to the front from any app, e.g. Ctrl+Shift+Alt+K.",
            Kind::NewWindow => "Opens another Keel window from any app, e.g. Ctrl+Shift+Alt+N.",
            Kind::Toggle => {
                "Shows Keel from any app, or minimizes it when it is in front, e.g. Ctrl+Shift+Alt+J."
            }
        }
    }

    /// What the hotkey does when pressed (on the thread the OS delivers it on).
    fn run(self, ctx: &egui::Context) {
        match self {
            Kind::Show => crate::single_instance::bring_to_front(ctx),
            Kind::NewWindow => {
                if let Err(e) = crate::single_instance::new_window() {
                    tracing::warn!("new window hotkey: {e}");
                }
            }
            Kind::Toggle => {
                let shown = ctx.input(|i| {
                    let v = i.viewport();
                    v.focused.unwrap_or(false) && !v.minimized.unwrap_or(false)
                });
                if shown {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    ctx.request_repaint();
                } else {
                    crate::single_instance::bring_to_front(ctx);
                }
            }
        }
    }
}

#[derive(Default)]
struct Slot {
    /// What is registered now.
    current: Option<HotKey>,
    /// The setting text last applied (changes are applied once).
    applied: Option<String>,
}

pub struct Hotkey {
    manager: GlobalHotKeyManager,
    slots: [Slot; 3],
    /// Registered hotkey ids and what each does (read by the event handler).
    ids: Arc<Mutex<Vec<(u32, Kind)>>>,
    /// Another Keel is the single instance and holds the hotkeys: failing to register one
    /// here is expected, not worth a toast.
    quiet: bool,
}

/// The hotkey in `text` ("" = none), or why it is not one.
pub fn parse(text: &str) -> Result<Option<HotKey>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let key = t
        .parse::<HotKey>()
        .map_err(|e| format!("Global hotkey \"{t}\": {e}"))?;
    if (key.mods - Modifiers::SHIFT).is_empty() {
        return Err(format!(
            "Global hotkey \"{t}\": add Ctrl, Alt or the Windows/Command key"
        ));
    }
    Ok(Some(key))
}

impl Hotkey {
    /// None when the OS offers no global hotkeys (logged; e.g. Wayland without X11).
    /// `quiet`: this process is not the single instance (see the field).
    pub fn new(ctx: &egui::Context, quiet: bool) -> Option<Self> {
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| tracing::warn!("global hotkey: {e}"))
            .ok()?;
        let ctx = ctx.clone();
        let ids: Arc<Mutex<Vec<(u32, Kind)>>> = Arc::default();
        let read = ids.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed {
                let kind = read
                    .lock()
                    .iter()
                    .find(|(id, _)| *id == e.id)
                    .map(|(_, k)| *k);
                if let Some(kind) = kind {
                    kind.run(&ctx);
                }
            }
        }));
        Some(Self {
            manager,
            slots: Default::default(),
            ids,
            quiet,
        })
    }

    /// Registers `kind`'s hotkey in `text` ("" = none) when it changed since the last call.
    /// Returns a message for a toast when it is not valid or another app holds it.
    pub fn sync(&mut self, kind: Kind, text: &str) -> Option<String> {
        let slot = &mut self.slots[kind.index()];
        if slot.applied.as_deref() == Some(text) {
            return None;
        }
        slot.applied = Some(text.to_owned());
        let wanted = match parse(text) {
            Ok(key) => key,
            Err(e) => return Some(e),
        };
        if wanted == slot.current {
            return None;
        }
        if let Some(old) = slot.current.take() {
            let _ = self.manager.unregister(old);
            self.ids.lock().retain(|(id, _)| *id != old.id());
        }
        let key = wanted?;
        match self.manager.register(key) {
            Ok(()) => {
                slot.current = Some(key);
                self.ids.lock().push((key.id(), kind));
                None
            }
            Err(e) if self.quiet => {
                tracing::debug!(
                    "global hotkey {}: {e} (the running Keel has it)",
                    text.trim()
                );
                None
            }
            Err(e) => Some(format!("{} \"{}\": {e}", kind.label(), text.trim())),
        }
    }
}

/// A Settings → General field: edits a copy and applies it when the field loses focus
/// (so a half-typed "Ctrl+A" never grabs Ctrl+A system-wide).
pub fn field(ui: &mut egui::Ui, kind: Kind, hotkey: &mut String) -> egui::Response {
    let id = ui.id().with(("hotkey-draft", kind.index()));
    let mut draft = ui
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| hotkey.clone());
    let r = ui
        .add(egui::TextEdit::singleline(&mut draft).hint_text("off"))
        .on_hover_text(format!(
            "{} Needs Ctrl, Alt or Win besides Shift; avoid Ctrl+Alt alone (it is AltGr on \
             many layouts). Empty = off.",
            kind.hint()
        ));
    if r.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, draft));
    } else {
        if r.lost_focus() {
            *hotkey = draft.trim().to_owned();
        }
        ui.data_mut(|d| d.remove::<String>(id));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::{parse, Kind};

    #[test]
    fn default_hotkey_parses() {
        let s = crate::settings::Settings::default();
        let key = parse(&s.hotkey).unwrap().unwrap();
        assert_eq!(key.into_string(), "shift+control+alt+KeyK");
        // The new ones are off until set.
        assert_eq!(parse(&s.hotkey_new_window), Ok(None));
        assert_eq!(parse(&s.hotkey_toggle_window), Ok(None));
    }

    #[test]
    fn needs_a_modifier_besides_shift() {
        assert_eq!(parse(" "), Ok(None));
        for bad in ["K", "Shift+K", "shift+F5", "Ctrl+Nope"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        for good in ["Ctrl+K", "Alt+Shift+K", "Super+K", "Ctrl+Shift+Alt+K"] {
            assert!(parse(good).unwrap().is_some(), "{good}");
        }
    }

    #[test]
    fn each_kind_has_its_own_row() {
        let labels: Vec<&str> = Kind::ALL.iter().map(|k| k.label()).collect();
        assert_eq!(
            labels,
            ["Global hotkey", "New window hotkey", "Toggle window hotkey"]
        );
        for (i, k) in Kind::ALL.iter().enumerate() {
            assert_eq!(k.index(), i);
        }
    }
}
