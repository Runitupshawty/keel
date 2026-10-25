//! The search backend in the UI (Task 24 follow-up to Task 25): the status-bar indicator
//! and backend note, and Settings → General's "Index all drives (administrator)", the
//! index service (Install / Uninstall) and "Hide files in folders you cannot open".

use crate::state::{AppState, Msg};
use crossbeam_channel::Sender;
use keel_search::SearchState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Longest backend note shown in the status bar; the full text is the tooltip.
const NOTE_CHARS: usize = 60;

/// The status-bar indicator: text and tooltip. Indexing is not "not running".
pub fn indicator(name: Option<&str>, state: SearchState, reason: Option<&str>) -> (String, String) {
    match (name, state, reason) {
        (None, ..) => (
            crate::i18n::tr("index_ui.search_loading").to_owned(),
            crate::i18n::tr("index_ui.search_loading_tip").to_owned(),
        ),
        (Some(name), SearchState::Indexing { done }, _) => (
            crate::i18n::tr("index_ui.indexing")
                .replace("{name}", name)
                .replace("{done}", &done.to_string()),
            crate::i18n::tr("index_ui.indexing_tip").to_owned(),
        ),
        (Some(name), _, None) => (
            crate::i18n::tr("index_ui.ok").replace("{name}", name),
            crate::i18n::tr("index_ui.ok_tip").to_owned(),
        ),
        (Some(name), _, Some(_)) => (
            crate::i18n::tr("index_ui.not_running").replace("{name}", name),
            crate::i18n::tr("index_ui.not_running_tip").to_owned(),
        ),
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
    // The backend included a new volume or removed an offline one: save it.
    if let Some(changed) = s.searcher.as_ref().and_then(|x| x.index_settings_changed()) {
        s.settings.search_index = changed;
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
        _ if !cfg!(windows) => Some(crate::i18n::tr("index_ui.windows_only")),
        Some("Everything") => Some(crate::i18n::tr("index_ui.everything_running")),
        _ if busy => Some(crate::i18n::tr("index_ui.indexing_busy")),
        _ => None,
    };
    let r = ui.add_enabled(
        why_not.is_none(),
        egui::Button::new(crate::i18n::tr("index_ui.full_index_label")),
    );
    let r = match why_not {
        Some(why) => r.on_disabled_hover_text(why),
        None => r.on_hover_text(crate::i18n::tr("index_ui.full_index_tip")),
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

/// Keel's own index leaves out files in folders this user cannot open (Settings →
/// General → Index privacy; on by default).
pub fn apply_hide_unlistable(on: bool) {
    #[cfg(windows)]
    keel_search::set_hide_unlistable(on);
    #[cfg(not(windows))]
    let _ = on;
}

/// An install or uninstall of the index service is running (one at a time).
#[cfg(windows)]
static SERVICE_BUSY: AtomicBool = AtomicBool::new(false);

/// What Settings shows for the index service's state.
#[cfg(windows)]
pub fn service_label(state: keel_search::ServiceState) -> &'static str {
    use keel_search::ServiceState as S;
    match state {
        S::NotInstalled => crate::i18n::tr("index_ui.service_not_installed"),
        S::Stopped => crate::i18n::tr("index_ui.service_stopped"),
        S::Starting => crate::i18n::tr("index_ui.service_starting"),
        S::Running => crate::i18n::tr("index_ui.service_running"),
        S::Stopping => crate::i18n::tr("index_ui.service_stopping"),
        S::Unknown => crate::i18n::tr("index_ui.service_unknown"),
    }
}

/// The service state, asked of Windows at most every 2 s (Settings redraws often).
#[cfg(windows)]
fn cached_service_state() -> keel_search::ServiceState {
    use parking_lot::Mutex;
    use std::time::Instant;
    static CACHE: Mutex<Option<(Instant, keel_search::ServiceState)>> = Mutex::new(None);
    let mut cache = CACHE.lock();
    match *cache {
        Some((at, state)) if at.elapsed() < Duration::from_secs(2) => state,
        _ => {
            let state = keel_search::service_state();
            *cache = Some((Instant::now(), state));
            state
        }
    }
}

/// Settings → General → Index service: its state and Install… / Uninstall… (each
/// asks Windows for administrator rights once).
#[cfg(windows)]
pub fn service_row(ui: &mut egui::Ui, tx: &Sender<Msg>) {
    use keel_search::ServiceState as S;
    let state = cached_service_state();
    let busy = SERVICE_BUSY.load(Ordering::Acquire);
    ui.horizontal(|ui| {
        ui.label(service_label(state))
            .on_hover_text(crate::i18n::tr("index_ui.service_tip"));
        let (text, install, tip) = if state == S::NotInstalled {
            (
                crate::i18n::tr("index_ui.install_label"),
                true,
                crate::i18n::tr("index_ui.install_tip"),
            )
        } else {
            (
                crate::i18n::tr("index_ui.uninstall_label"),
                false,
                crate::i18n::tr("index_ui.uninstall_tip"),
            )
        };
        let r = ui.add_enabled(!busy && state != S::Unknown, egui::Button::new(text));
        if r.on_hover_text(tip).clicked() {
            start_service_change(install, tx.clone(), ui.ctx().clone());
        }
    });
}

#[cfg(not(windows))]
pub fn service_row(_: &mut egui::Ui, _: &Sender<Msg>) {}

#[cfg(windows)]
fn start_service_change(install: bool, tx: Sender<Msg>, ctx: egui::Context) {
    if SERVICE_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("keel-index-service".into())
        .spawn(move || {
            let msg = match (keel_search::request_service_change(install), install) {
                (Ok(()), true) => Msg::Info("Keel index service installed".into()),
                (Ok(()), false) => Msg::Info("Keel index service removed".into()),
                (Err(e), _) => Msg::Toast(format!("Index service: {e:#}")),
            };
            SERVICE_BUSY.store(false, Ordering::Release);
            crate::worker::send(&tx, &ctx, msg);
        });
    if let Err(e) = spawned {
        SERVICE_BUSY.store(false, Ordering::Release);
        tracing::error!("spawn keel-index-service: {e}");
    }
}

/// `keel --install-service` / `--uninstall-service`: does it when elevated (`user`:
/// the account to serve, from the unelevated Keel that asked), else asks for elevation
/// and runs itself again. Returns the exit code.
pub fn service_cli(install: bool, user: Option<&str>) -> i32 {
    #[cfg(windows)]
    {
        let done = if keel_search::is_elevated() {
            if install {
                keel_search::install_service(user)
                    .map(|exe| format!("Installed the Keel index service ({})", exe.display()))
            } else {
                keel_search::uninstall_service().map(|was| {
                    if was {
                        "Removed the Keel index service".to_owned()
                    } else {
                        "The Keel index service was not installed".to_owned()
                    }
                })
            }
        } else {
            keel_search::request_service_change(install).map(|()| {
                if install {
                    "Installed the Keel index service".to_owned()
                } else {
                    "Removed the Keel index service".to_owned()
                }
            })
        };
        match done {
            Ok(message) => {
                println!("{message}");
                0
            }
            Err(e) => {
                eprintln!("keel: {e:#}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (install, user);
        eprintln!("keel: the index service is Windows only");
        2
    }
}

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

    #[cfg(windows)]
    #[test]
    fn service_states_have_labels() {
        use keel_search::ServiceState as S;
        assert_eq!(service_label(S::NotInstalled), "Not installed");
        assert_eq!(service_label(S::Running), "Running");
        assert_eq!(service_label(S::Stopped), "Installed (starts with Keel)");
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

    #[test]
    fn index_ui_in_spanish() {
        crate::i18n::set("es");
        assert_eq!(indicator(None, SearchState::Ready, None).0, "Búsqueda: …");
        assert_eq!(
            indicator(None, SearchState::Ready, None).1,
            "Cargando el motor de búsqueda"
        );
        assert_eq!(
            indicator(Some("Keel index"), SearchState::Indexing { done: 42 }, None).0,
            "Keel index: indexando (42 hasta ahora)"
        );
        assert_eq!(
            indicator(Some("Keel index"), SearchState::Indexing { done: 42 }, None).1,
            "Creando el índice de archivos; la búsqueda funcionará al terminar"
        );
        assert_eq!(
            indicator(Some("Keel index"), SearchState::Ready, None).0,
            "Keel index: ok"
        );
        assert_eq!(
            indicator(Some("Keel index"), SearchState::Ready, None).1,
            "La búsqueda está disponible"
        );
        assert_eq!(
            indicator(Some("Everything"), SearchState::Ready, Some("no")).0,
            "Everything: no en ejecución"
        );
        assert_eq!(
            indicator(Some("Everything"), SearchState::Ready, Some("no")).1,
            "Haz clic para comprobar de nuevo"
        );
        assert_eq!(
            crate::i18n::tr("index_ui.windows_only"),
            "Solo en Windows (NTFS)"
        );
        assert_eq!(
            crate::i18n::tr("index_ui.everything_running"),
            "Everything está en ejecución y ya cubre todas las unidades"
        );
        assert_eq!(crate::i18n::tr("index_ui.indexing_busy"), "Indexando…");
        assert_eq!(
            crate::i18n::tr("index_ui.full_index_label"),
            "Indexar todas las unidades (administrador)"
        );
        assert_eq!(
            crate::i18n::tr("index_ui.full_index_tip"),
            "Lee cada unidad NTFS una vez, tras un aviso de administrador de Windows"
        );
        crate::i18n::set("en");
    }
}
