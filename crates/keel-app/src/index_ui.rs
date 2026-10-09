//! The search backend in the UI (Task 24 follow-up to Task 25): the status-bar indicator
//! and backend note, and Settings → General's "Index all drives (administrator)".

use crate::state::{AppState, Msg};
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};

/// Longest backend note shown in the status bar; the full text is the tooltip.
const NOTE_CHARS: usize = 60;

/// The status-bar indicator: text and tooltip.
pub fn indicator(name: Option<&str>, reason: Option<&str>) -> (String, &'static str) {
    match (name, reason) {
        (None, _) => ("Search: …".into(), "Loading the search backend"),
        (Some(name), None) => (format!("{name}: ok"), "Search is available"),
        (Some(name), Some(_)) => (format!("{name}: not running"), "Click to check again"),
    }
}

/// The backend's note (`Searcher::status`) cut to `NOTE_CHARS`: (shown, tooltip).
pub fn note(status: &str) -> (String, &str) {
    let shown = match status.char_indices().nth(NOTE_CHARS) {
        Some((cut, _)) => format!("{}…", status[..cut].trim_end()),
        None => status.to_owned(),
    };
    (shown, status)
}

/// Draws the indicator and note (right-to-left layout: the note lands left of it).
pub fn status_bar(ui: &mut egui::Ui, s: &AppState) -> bool {
    let name = s.searcher.as_ref().map(|x| x.name());
    let (text, tip) = indicator(name, s.search_reason.as_deref());
    let clicked = ui
        .add(egui::Button::new(text).frame(false))
        .on_hover_text(tip)
        .clicked();
    if let Some(status) = s.searcher.as_ref().and_then(|x| x.status()) {
        let (shown, full) = note(&status);
        ui.separator();
        ui.weak(shown).on_hover_text(full);
    }
    clicked
}

/// A full index is being built (one at a time).
static INDEXING: AtomicBool = AtomicBool::new(false);

/// Settings → General: "Index all drives (administrator)". `searcher`: the active
/// backend's name; Everything already covers every drive, so the button is off then.
pub fn full_index_button(ui: &mut egui::Ui, searcher: Option<&str>, tx: &Sender<Msg>) {
    let busy = INDEXING.load(Ordering::Acquire);
    let why_not = match searcher {
        _ if !cfg!(windows) => Some("Only on Windows (NTFS)"),
        Some("Everything") => Some("Everything is running and already covers every drive"),
        _ if busy => Some("Indexing…"),
        _ => None,
    };
    let r = ui.add_enabled(
        why_not.is_none(),
        egui::Button::new("Index all drives (administrator)"),
    );
    let r = match why_not {
        Some(why) => r.on_disabled_hover_text(why),
        None => r.on_hover_text("Reads every NTFS drive once, after a Windows admin prompt"),
    };
    if r.clicked() {
        start_full_index(tx.clone(), ui.ctx().clone());
    }
}

#[cfg(windows)]
fn start_full_index(tx: Sender<Msg>, ctx: egui::Context) {
    if INDEXING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("keel-full-index".into())
        .spawn(move || {
            let msg = match keel_search::request_full_index() {
                Ok(()) => Msg::Info("All drives indexed".into()),
                Err(e) => Msg::Toast(format!("Index all drives: {e:#}")),
            };
            INDEXING.store(false, Ordering::Release);
            crate::worker::send(&tx, &ctx, msg);
        });
    if let Err(e) = spawned {
        INDEXING.store(false, Ordering::Release);
        tracing::error!("spawn keel-full-index: {e}");
    }
}

#[cfg(not(windows))]
fn start_full_index(_: Sender<Msg>, _: egui::Context) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator_names_the_backend() {
        assert_eq!(indicator(None, None).0, "Search: …");
        assert_eq!(indicator(Some("Keel index"), None).0, "Keel index: ok");
        assert_eq!(
            indicator(Some("Everything"), Some("not running")).0,
            "Everything: not running"
        );
    }

    #[test]
    fn long_notes_are_cut_with_the_full_text_as_tooltip() {
        assert_eq!(
            note("user folders only"),
            ("user folders only".into(), "user folders only")
        );
        let long =
            "Indexing user folders only; run 'Index all drives' as administrator for everything.";
        let (shown, full) = note(long);
        assert_eq!(full, long);
        assert!(shown.ends_with('…'));
        assert!(shown.chars().count() <= NOTE_CHARS + 1);
        assert!(long.starts_with(shown.trim_end_matches('…')));
        // Cuts on characters, not bytes.
        let wide = "é".repeat(70);
        assert_eq!(note(&wide).0.chars().count(), NOTE_CHARS + 1);
    }
}
