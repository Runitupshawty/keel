//! The search backend in the UI (Task 24 follow-up to Task 25): the status-bar indicator
//! and backend note, and Settings → General's "Index all drives (administrator)".

use crate::state::{AppState, Msg};
use crossbeam_channel::Sender;
use keel_search::SearchState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Longest backend note shown in the status bar; the full text is the tooltip.
const NOTE_CHARS: usize = 60;

/// The status-bar indicator: text and tooltip. Indexing is not "not running".
pub fn indicator(
    name: Option<&str>,
    state: SearchState,
    reason: Option<&str>,
) -> (String, &'static str) {
    match (name, state, reason) {
        (None, ..) => ("Search: …".into(), "Loading the search backend"),
        (Some(name), SearchState::Indexing { done }, _) => (
            format!("{name}: indexing ({done} so far)"),
            "Building the file index; search works when it is done",
        ),
        (Some(name), _, None) => (format!("{name}: ok"), "Search is available"),
        (Some(name), _, Some(_)) => (format!("{name}: not running"), "Click to check again"),
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
/// True when search should be probed again: a click, or the backend's ready
/// notification (its index finished), which also marks the Ctrl+P folder list stale.
pub fn status_bar(ui: &mut egui::Ui, s: &mut AppState) -> bool {
    let name = s.searcher.as_ref().map(|x| x.name());
    let state = s
        .searcher
        .as_ref()
        .map_or(SearchState::Unavailable, |x| x.state());
    if matches!(state, SearchState::Indexing { .. }) {
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }
    // A folder list being built now may predate the index: take the notice after.
    let ready = !s.jump.indexing && s.searcher.as_ref().is_some_and(|x| x.take_ready());
    if ready {
        s.jump.indexed_at = None;
    }
    let (text, tip) = indicator(name, state, s.search_reason.as_deref());
    let clicked = ui
        .add(egui::Button::new(text).frame(false))
        .on_hover_text(tip)
        .clicked();
    if let Some(status) = s.searcher.as_ref().and_then(|x| x.status()) {
        let (shown, full) = note(&status);
        ui.separator();
        ui.weak(shown).on_hover_text(full);
    }
    clicked || ready
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
        let ready = SearchState::Ready;
        assert_eq!(indicator(None, ready, None).0, "Search: …");
        assert_eq!(
            indicator(Some("Keel index"), ready, None).0,
            "Keel index: ok"
        );
        assert_eq!(
            indicator(Some("Everything"), ready, Some("not running")).0,
            "Everything: not running"
        );
        // Review finding 22: a walk in progress is not "not running".
        let indexing = SearchState::Indexing { done: 42 };
        assert_eq!(
            indicator(Some("Keel index"), indexing, Some("Keel is indexing")).0,
            "Keel index: indexing (42 so far)"
        );
    }

    /// The ready notification re-probes search and marks the Ctrl+P list stale.
    #[test]
    fn ready_notification_refreshes_status_and_folder_list() {
        use keel_search::{Hit, Query, Searcher};
        use std::sync::Arc;

        struct Indexer(AtomicBool);
        impl Searcher for Indexer {
            fn query(&self, _: &Query) -> anyhow::Result<Vec<Hit>> {
                Ok(Vec::new())
            }
            fn available(&self) -> bool {
                true
            }
            fn take_ready(&self) -> bool {
                self.0.swap(false, Ordering::Relaxed)
            }
        }

        let dir = keel_vfs::VPath::local(std::env::temp_dir());
        let ctx = egui::Context::default();
        let mut s = AppState::new(ctx.clone(), Arc::new(keel_vfs::Router::new()), dir);
        let indexer = Arc::new(Indexer(AtomicBool::new(true)));
        s.searcher = Some(indexer.clone());
        s.search_reason = Some("Keel is indexing your files".into());
        s.jump.indexed_at = Some(std::time::Instant::now());
        let frame = |s: &mut AppState| {
            let mut probe = false;
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| probe = status_bar(ui, s));
            });
            probe
        };
        assert!(frame(&mut s), "ready: probe again");
        assert!(s.jump.stale(), "Ctrl+P rebuilds from the finished index");
        assert!(!frame(&mut s), "announced once");

        // While a folder list is being built, the notice waits for it.
        indexer.0.store(true, Ordering::Relaxed);
        s.jump.indexing = true;
        assert!(!frame(&mut s));
        s.jump.indexing = false;
        assert!(frame(&mut s));
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
