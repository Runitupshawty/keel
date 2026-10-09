//! Global hotkey (Task 24, default Ctrl+Alt+K, Settings → General): brings Keel's window
//! forward from anywhere, via the `global-hotkey` crate. Its manager lives on the UI
//! thread (Windows delivers `WM_HOTKEY` through winit's message loop; macOS needs the
//! main thread); the handler runs there too (on Linux on the crate's X11 thread).

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

pub struct Hotkey {
    manager: GlobalHotKeyManager,
    /// What is registered now.
    current: Option<HotKey>,
    /// The setting text last applied (changes are applied once).
    applied: Option<String>,
}

impl Hotkey {
    /// None when the OS offers no global hotkeys (logged; e.g. Wayland without X11).
    pub fn new(ctx: &egui::Context) -> Option<Self> {
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
        })
    }

    /// Registers the hotkey in `text` ("" = none) when it changed since the last call.
    /// Returns a message for a toast when it is not valid or another app holds it.
    pub fn sync(&mut self, text: &str) -> Option<String> {
        if self.applied.as_deref() == Some(text) {
            return None;
        }
        self.applied = Some(text.to_owned());
        let wanted = match text.trim() {
            "" => None,
            t => match t.parse::<HotKey>() {
                Ok(key) => Some(key),
                Err(e) => return Some(format!("Global hotkey \"{t}\": {e}")),
            },
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
        .on_hover_text("Brings Keel to the front from any app, e.g. Ctrl+Alt+K. Empty = off.");
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
    #[test]
    fn default_hotkey_parses() {
        let key: global_hotkey::hotkey::HotKey =
            crate::settings::Settings::default().hotkey.parse().unwrap();
        assert_eq!(key.into_string(), "control+alt+KeyK");
    }
}
