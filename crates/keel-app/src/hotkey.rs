//! Global hotkey (Task 24, default Ctrl+Shift+Alt+K, Settings → General): brings Keel's
//! window forward from anywhere, via the `global-hotkey` crate. Its manager lives on the UI
//! thread (Windows delivers `WM_HOTKEY` through winit's message loop; macOS needs the
//! main thread); the handler runs there too (on Linux on the crate's X11 thread).
//!
//! The default is not Ctrl+Alt+K: on many keyboard layouts (German, Polish, …) AltGr is
//! Ctrl+Alt, so a Ctrl+Alt hotkey takes an AltGr character from every app. A hotkey needs
//! at least one modifier besides Shift (Shift+K alone would take capital K).

use global_hotkey::hotkey::{HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

pub struct Hotkey {
    manager: GlobalHotKeyManager,
    /// What is registered now.
    current: Option<HotKey>,
    /// The setting text last applied (changes are applied once).
    applied: Option<String>,
    /// Another Keel is the single instance and holds the hotkey: failing to register it here
    /// is expected, not worth a toast.
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
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed {
                crate::single_instance::bring_to_front(&ctx);
            }
        }));
        Some(Self {
            manager,
            current: None,
            applied: None,
            quiet,
        })
    }

    /// Registers the hotkey in `text` ("" = none) when it changed since the last call.
    /// Returns a message for a toast when it is not valid or another app holds it.
    pub fn sync(&mut self, text: &str) -> Option<String> {
        if self.applied.as_deref() == Some(text) {
            return None;
        }
        self.applied = Some(text.to_owned());
        let wanted = match parse(text) {
            Ok(key) => key,
            Err(e) => return Some(e),
        };
        if wanted == self.current {
            return None;
        }
        if let Some(old) = self.current.take() {
            let _ = self.manager.unregister(old);
        }
        let key = wanted?;
        match self.manager.register(key) {
            Ok(()) => {
                self.current = Some(key);
                None
            }
            Err(e) if self.quiet => {
                tracing::debug!(
                    "global hotkey {}: {e} (the running Keel has it)",
                    text.trim()
                );
                None
            }
            Err(e) => Some(format!("Global hotkey \"{}\": {e}", text.trim())),
        }
    }
}

/// The Settings → General field: edits a copy and applies it when the field loses focus
/// (so a half-typed "Ctrl+A" never grabs Ctrl+A system-wide).
pub fn field(ui: &mut egui::Ui, hotkey: &mut String) {
    let id = ui.id().with("hotkey-draft");
    let mut draft = ui
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| hotkey.clone());
    let r = ui
        .add(egui::TextEdit::singleline(&mut draft).hint_text("off"))
        .on_hover_text(
            "Brings Keel to the front from any app, e.g. Ctrl+Shift+Alt+K. Needs Ctrl, Alt or \
             Win besides Shift; avoid Ctrl+Alt alone (it is AltGr on many layouts). Empty = off.",
        );
    if r.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, draft));
    } else {
        if r.lost_focus() {
            *hotkey = draft.trim().to_owned();
        }
        ui.data_mut(|d| d.remove::<String>(id));
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn default_hotkey_parses() {
        let key = parse(&crate::settings::Settings::default().hotkey)
            .unwrap()
            .unwrap();
        assert_eq!(key.into_string(), "shift+control+alt+KeyK");
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
}
