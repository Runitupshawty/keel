//! The inspector (Task 59): the preview panel's second tab (Ctrl+I). It follows the active
//! pane's cursor like the preview: the Overview comes from the listing at once, the rest
//! from `library.inspect` on a worker once the selection has settled for 150 ms (the same
//! operation in this process or through the daemon). It reads only.

use crate::a11y::Named;
use crate::keys::Action;
use crate::library::{count, state_text, when, LibCmd};
use egui::{Color32, RichText};
use keel_api::types::{HistoryEntry, InspectedRecord, Inspection};
use keel_search::journal::Event;
use keel_vfs::{Entry, Kind, VPath};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The selection must stay this long before the library is asked.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// Copies and history shown at first; "Show all" asks for `MAX_ALL`.
pub const MAX: usize = 50;
pub const MAX_ALL: usize = 1000;
pub fn not_in_library() -> String {
    crate::i18n::tr("inspector.not_in_library").to_owned()
}
pub fn library_off() -> String {
    crate::i18n::tr("inspector.library_off").to_owned()
}

/// The preview panel's tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelTab {
    #[default]
    Preview,
    Inspector,
}

#[derive(Default)]
pub struct Inspector {
    entry: Option<Entry>,
    /// The selection changed (or a refresh was asked for) and the library was not asked yet.
    changed: Option<Instant>,
    req: u64,
    max: usize,
    /// The answer for `entry` (an error is shown in place of the library's sections).
    pub answer: Option<Result<Inspection, String>>,
    /// Phase 15: the index journal's entries of `entry`, newest first.
    pub journal: Option<Result<Vec<Event>, String>>,
    journal_answer: JournalAnswer,
}

/// A journal lookup's answer: (request, entries or error).
type JournalAnswer = Arc<Mutex<Option<(u64, Result<Vec<Event>, String>)>>>;

/// Journal entries shown for one file.
pub const JOURNAL_MAX: usize = 20;

impl Inspector {
    /// Follows the active pane's cursor: returns (path, request, max) for the library once
    /// `entry` has been the selection for `DEBOUNCE`.
    pub fn follow(
        &mut self,
        ctx: &egui::Context,
        entry: Option<&Entry>,
    ) -> Option<(VPath, u64, usize)> {
        if self.entry.as_ref().map(|e| &e.path) != entry.map(|e| &e.path) {
            self.entry = entry.cloned();
            self.answer = None;
            self.journal = None;
            self.max = MAX;
            self.req += 1;
            self.changed = entry.map(|_| Instant::now());
        }
        let left = DEBOUNCE.saturating_sub(self.changed?.elapsed());
        if !left.is_zero() {
            ctx.request_repaint_after(left);
            return None;
        }
        self.changed = None;
        let path = self.entry.as_ref()?.path.clone();
        self.ask_journal(ctx, &path);
        Some((path, self.req, self.max))
    }

    /// Looks `path` up in the index journal on a worker (local paths only).
    fn ask_journal(&mut self, ctx: &egui::Context, path: &VPath) {
        let (req, slot, ctx) = (self.req, self.journal_answer.clone(), ctx.clone());
        let asked = crate::journal_ui::lookup(path, JOURNAL_MAX, move |result| {
            *slot.lock() = Some((req, result));
            ctx.request_repaint();
        });
        if !asked {
            self.journal = Some(Ok(Vec::new()));
        }
    }

    /// Takes the journal lookup's answer when it is for the latest request.
    pub fn poll_journal(&mut self) {
        if let Some((req, result)) = self.journal_answer.lock().take() {
            if req == self.req {
                self.journal = Some(result);
            }
        }
    }

    /// A worker's answer, kept when it is for the latest request.
    pub fn answered(&mut self, req: u64, result: Result<Inspection, String>) {
        if req == self.req {
            self.answer = Some(result);
        }
    }

    /// Asks again at once (tags changed, "Show all"); what is shown stays until it answers.
    pub fn refresh(&mut self) {
        if self.entry.is_some() {
            self.req += 1;
            self.changed = Some(Instant::now() - DEBOUNCE);
        }
    }
}

/// "Folder", "Link", "PDF file", "File".
fn kind_text(e: &Entry) -> String {
    match (&e.kind, e.is_link) {
        (_, true) => crate::i18n::tr("inspector.kind_link").into(),
        (Kind::Dir, _) => crate::i18n::tr("inspector.kind_folder").into(),
        _ if e.ext.is_empty() => crate::i18n::tr("inspector.kind_file").into(),
        _ => crate::i18n::tr("inspector.kind_ext").replace("{ext}", &e.ext.to_uppercase()),
    }
}

/// One history row: when, then what happened and where to or from.
pub fn history_text(h: &HistoryEntry) -> String {
    let name = |p: &str| p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned();
    let tag = h.tag.as_deref().unwrap_or("?");
    let fav = tag == "Favorites";
    let what = match (h.kind.as_str(), &h.from, &h.to) {
        ("copy", Some(f), _) => crate::i18n::tr("inspector.copied_from").replace("{path}", f),
        ("copy", _, Some(t)) => crate::i18n::tr("inspector.copied_to").replace("{path}", t),
        ("move", Some(f), _) => crate::i18n::tr("inspector.moved_from").replace("{path}", f),
        ("move", _, Some(t)) => crate::i18n::tr("inspector.moved_to").replace("{path}", t),
        ("rename", Some(f), _) => {
            crate::i18n::tr("inspector.renamed_from").replace("{name}", &name(f))
        }
        ("rename", _, Some(t)) => {
            crate::i18n::tr("inspector.renamed_to").replace("{name}", &name(t))
        }
        (k @ ("merge" | "flatten" | "organize"), Some(f), _) => {
            crate::i18n::tr("inspector.moved_from_kind")
                .replace("{path}", f)
                .replace("{kind}", k)
        }
        (k @ ("merge" | "flatten" | "organize"), _, Some(t)) => {
            crate::i18n::tr("inspector.moved_to_kind")
                .replace("{path}", t)
                .replace("{kind}", k)
        }
        ("delete", ..) => crate::i18n::tr("inspector.deleted").into(),
        ("tag", ..) if fav => crate::i18n::tr("inspector.added_to_favorites").into(),
        ("untag", ..) if fav => crate::i18n::tr("inspector.removed_from_favorites").into(),
        ("tag", ..) => crate::i18n::tr("inspector.tagged").replace("{tag}", tag),
        ("untag", ..) => crate::i18n::tr("inspector.untagged").replace("{tag}", tag),
        (kind, ..) => kind.to_owned(),
    };
    let status = match h.ok {
        Some(true) => "",
        Some(false) => crate::i18n::tr("inspector.failed"),
        None => crate::i18n::tr("inspector.running"),
    };
    format!("{}  {what}{status}", when(Some(h.ts)))
}

fn heading(ui: &mut egui::Ui, title: &str, muted: Color32) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).small().strong().color(muted));
}

/// A label / value row; `copy`: a Copy button that puts the value on the clipboard.
fn row(ui: &mut egui::Ui, label: &str, value: &str, copy: Option<&str>) {
    ui.weak(label);
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(value).truncate());
        if let Some(what) = copy {
            if ui
                .small_button(crate::i18n::tr("menu.copy"))
                .tip(what)
                .clicked()
            {
                ui.ctx().copy_text(value.to_owned());
            }
        }
    });
    ui.end_row();
}

fn grid(ui: &mut egui::Ui, id: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([10.0, 3.0])
        .show(ui, body);
}

fn size(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::DECIMAL)
}

/// The Inspector tab.
pub fn ui(ui: &mut egui::Ui, ins: &mut Inspector, muted: Color32, out: &mut Vec<Action>) {
    let Some(entry) = ins.entry.clone() else {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new(crate::i18n::tr("inspector.select_to_inspect")).color(muted))
        });
        return;
    };
    let mut show_all = false;
    ins.poll_journal();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(&entry.name).strong()).truncate());
            heading(ui, crate::i18n::tr("tab.overview"), muted);
            grid(ui, "inspector-overview", |ui| {
                row(
                    ui,
                    crate::i18n::tr("toolbar.kind"),
                    &kind_text(&entry),
                    None,
                );
                if entry.kind != Kind::Dir {
                    row(ui, crate::i18n::tr("toolbar.size"), &size(entry.size), None);
                }
                let modified = crate::view_details::date_text(&entry);
                if !modified.is_empty() {
                    row(ui, crate::i18n::tr("inspector.modified"), &modified, None);
                }
                row(
                    ui,
                    crate::i18n::tr("inspector.path"),
                    &entry.path.display(),
                    Some(crate::i18n::tr("menu.copy_path")),
                );
            });
            journal(ui, ins.journal.as_ref(), muted, out);
            let lib = match &ins.answer {
                None => {
                    ui.add_space(6.0);
                    crate::anim::spinner(ui);
                    return;
                }
                Some(Err(e)) => {
                    heading(ui, crate::i18n::tr("tab.library"), muted);
                    ui.weak(e);
                    return;
                }
                Some(Ok(i)) => i.library.as_ref(),
            };
            let Some(lib) = lib else {
                for title in [
                    crate::i18n::tr("tab.library"),
                    crate::i18n::tr("inspector.copies"),
                    crate::i18n::tr("inspector.history"),
                    crate::i18n::tr("sidebar.tags"),
                ] {
                    heading(ui, title, muted);
                    ui.weak(not_in_library());
                }
                return;
            };
            library(ui, lib, muted, out);
            heading(ui, crate::i18n::tr("inspector.history"), muted);
            if lib.history.is_empty() {
                ui.weak(crate::i18n::tr("inspector.no_operations"));
            }
            for h in &lib.history {
                ui.add(egui::Label::new(history_text(h)).truncate());
            }
            if lib.more_history
                && ins.max < MAX_ALL
                && ui.button(crate::i18n::tr("inspector.show_all")).clicked()
            {
                show_all = true;
            }
            heading(ui, crate::i18n::tr("sidebar.tags"), muted);
            ui.horizontal_wrapped(|ui| {
                if lib.favorite {
                    ui.label(crate::i18n::tr("inspector.favorite"));
                }
                for t in &lib.tags {
                    let tag = keel_core::Tag {
                        id: t.id,
                        name: t.name.clone(),
                        color: t.color.clone(),
                        parent: t.parent,
                    };
                    crate::library_ui::chip(ui, &tag);
                }
                if !lib.favorite && lib.tags.is_empty() {
                    ui.weak(crate::i18n::tr("inspector.no_tags"));
                }
                if ui
                    .button(crate::i18n::tr("inspector.tags_button"))
                    .on_hover_text(crate::i18n::tr("inspector.tags_tip"))
                    .clicked()
                {
                    out.push(Action::Library(LibCmd::TagPicker));
                }
            });
        });
    if show_all {
        ins.max = MAX_ALL;
        ins.refresh();
    }
}

/// Phase 15: what the index journal recorded for the entry (Keel's own history of
/// operations is below, with the library).
fn journal(
    ui: &mut egui::Ui,
    entries: Option<&Result<Vec<Event>, String>>,
    muted: Color32,
    out: &mut Vec<Action>,
) {
    heading(ui, crate::i18n::tr("inspector.index_journal"), muted);
    match entries {
        None => {
            crate::anim::spinner(ui);
        }
        Some(Err(e)) => {
            ui.weak(e);
        }
        Some(Ok(list)) if list.is_empty() => {
            ui.weak(crate::i18n::tr("inspector.no_changes"));
        }
        Some(Ok(list)) => {
            for e in list {
                ui.add(egui::Label::new(crate::journal_ui::entry_text(e)).truncate())
                    .on_hover_text(&e.path);
            }
            if list.len() >= JOURNAL_MAX
                && ui
                    .small_button(crate::i18n::tr("inspector.open_journal"))
                    .clicked()
            {
                out.push(Action::OpenJournal);
            }
        }
    }
}

/// The Library section, then Copies (a file) or the folder's totals.
fn library(ui: &mut egui::Ui, lib: &InspectedRecord, muted: Color32, out: &mut Vec<Action>) {
    heading(ui, crate::i18n::tr("tab.library"), muted);
    grid(ui, "inspector-library", |ui| {
        row(
            ui,
            crate::i18n::tr("inspector.source"),
            &lib.source_label,
            None,
        );
        let v = &lib.volume;
        let state = state_text(crate::backend::volume_state(v.state));
        let backup = if v.backup {
            crate::i18n::tr("inspector.backup")
        } else {
            ""
        };
        row(
            ui,
            crate::i18n::tr("inspector.volume"),
            &format!("{} · {state}{backup}", v.label),
            None,
        );
        if let Some(id) = &lib.content_id {
            row(
                ui,
                crate::i18n::tr("inspector.content_id"),
                id,
                Some(crate::i18n::tr("inspector.copy_content_id")),
            );
        }
        row(
            ui,
            crate::i18n::tr("inspector.first_seen"),
            &seen(lib.first_seen),
            None,
        );
        if lib.folder.is_none() {
            row(
                ui,
                crate::i18n::tr("inspector.last_hashed"),
                &seen(lib.last_hashed),
                None,
            );
        }
        if let Some(c) = &lib.copies {
            let copies = plural(
                c.copies,
                crate::i18n::tr("inspector.copy_one"),
                crate::i18n::tr("inspector.copy_many"),
            );
            let domains = plural(
                c.failure_domains,
                crate::i18n::tr("inspector.domain_one"),
                crate::i18n::tr("inspector.domain_many"),
            );
            let mut text = crate::i18n::tr("inspector.copies_in")
                .replace("{copies}", &copies)
                .replace("{domains}", &domains);
            if c.backed_up {
                text.push_str(crate::i18n::tr("inspector.backed_up"));
            }
            row(ui, crate::i18n::tr("inspector.protection"), &text, None);
        }
    });
    if let Some(f) = &lib.folder {
        heading(ui, crate::i18n::tr("inspector.folder"), muted);
        grid(ui, "inspector-folder", |ui| {
            let known = |v: Option<u64>, f: fn(u64) -> String| {
                v.map_or(crate::i18n::tr("inspector.not_known_yet").to_owned(), f)
            };
            row(
                ui,
                crate::i18n::tr("toolbar.size"),
                &known(f.bytes, size),
                None,
            );
            row(
                ui,
                crate::i18n::tr("inspector.files"),
                &known(f.files, count),
                None,
            );
            row(
                ui,
                crate::i18n::tr("inspector.folders"),
                &count(f.folders),
                None,
            );
            if let Some(b) = &f.biggest {
                let s = b
                    .bytes
                    .map(|b| format!(" ({})", size(b)))
                    .unwrap_or_default();
                row(
                    ui,
                    crate::i18n::tr("inspector.biggest"),
                    &format!("{}{s}", b.name),
                    None,
                );
            }
        });
        return;
    }
    heading(ui, crate::i18n::tr("inspector.copies"), muted);
    let Some(c) = &lib.copies else { return };
    if lib.content_id.is_none() {
        ui.weak(crate::i18n::tr("inspector.not_hashed"));
    } else if c.locations.is_empty() {
        ui.weak(crate::i18n::tr("inspector.no_other_copy"));
    }
    for l in &c.locations {
        let state = l.state.map(|s| state_text(crate::backend::volume_state(s)));
        let mut tip = format!("{} · {}", l.source_label, l.volume);
        if let Some(state) = state {
            tip.push_str(&format!(" · {state}"));
        }
        if l.claimed {
            tip.push_str(crate::i18n::tr("inspector.claimed"));
        }
        let r = ui.add(egui::Button::new(&l.path).truncate()).tip(&l.path);
        if r.on_hover_text(tip).clicked() {
            out.push(Action::OpenPath(crate::backend::vpath_of(&l.path)));
        }
    }
    if lib.more_copies {
        ui.weak(crate::i18n::tr("inspector.more_copies"));
    }
}

fn seen(t: Option<i64>) -> String {
    t.map_or(crate::i18n::tr("inspector.not_recorded").to_owned(), |t| {
        when(Some(t))
    })
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", count(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(kind: &str, from: Option<&str>, to: Option<&str>, tag: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            ts: 1_700_000_000,
            kind: kind.into(),
            ok: Some(true),
            from: from.map(str::to_owned),
            to: to.map(str::to_owned),
            tag: tag.map(str::to_owned),
        }
    }

    #[test]
    fn history_rows_say_what_happened_and_where() {
        let at = when(Some(1_700_000_000));
        let text = |e: &HistoryEntry| history_text(e).strip_prefix(&at).unwrap().to_owned();
        for (e, want) in [
            (
                h("copy", Some("/a/x.txt"), None, None),
                "Copied from /a/x.txt",
            ),
            (
                h("copy", None, Some("/b/x.txt"), None),
                "Copied to /b/x.txt",
            ),
            (
                h("move", Some("/a/x.txt"), None, None),
                "Moved from /a/x.txt",
            ),
            (h("move", None, Some("/b/x.txt"), None), "Moved to /b/x.txt"),
            (
                h("rename", Some(r"D:\a\old.txt"), None, None),
                "Renamed from old.txt",
            ),
            (
                h("rename", None, Some("/a/new.txt"), None),
                "Renamed to new.txt",
            ),
            (h("delete", None, None, None), "Deleted"),
            (
                h("flatten", Some("/a/x/f.txt"), None, None),
                "Moved from /a/x/f.txt (flatten)",
            ),
            (h("tag", None, None, Some("receipts")), "Tagged receipts"),
            (
                h("untag", None, None, Some("receipts")),
                "Untagged receipts",
            ),
            (
                h("tag", None, None, Some("Favorites")),
                "Added to Favorites",
            ),
            (
                h("untag", None, None, Some("Favorites")),
                "Removed from Favorites",
            ),
        ] {
            assert_eq!(text(&e), format!("  {want}"), "{e:?}");
        }
        let mut failed = h("move", None, Some("/b/x.txt"), None);
        failed.ok = Some(false);
        assert!(history_text(&failed).ends_with("Moved to /b/x.txt (failed)"));
        failed.ok = None;
        assert!(history_text(&failed).ends_with(" (running)"));
    }

    fn entry(name: &str) -> Entry {
        crate::tab::test_entry(&VPath::local(std::env::temp_dir()), name, Kind::File, 1)
    }

    /// The library is asked once per selection, after the debounce; a stale answer is
    /// dropped.
    #[test]
    fn follows_the_selection_after_the_debounce() {
        let ctx = egui::Context::default();
        let mut ins = Inspector::default();
        let (a, b) = (entry("a.txt"), entry("b.txt"));
        assert!(ins.follow(&ctx, Some(&a)).is_none(), "not yet");
        assert!(ins.follow(&ctx, Some(&b)).is_none(), "moved on");
        std::thread::sleep(DEBOUNCE);
        let (path, req, max) = ins.follow(&ctx, Some(&b)).expect("settled");
        assert_eq!((path, max), (b.path.clone(), MAX));
        assert!(ins.follow(&ctx, Some(&b)).is_none(), "asked once");
        ins.answered(req - 1, Err("stale".into()));
        assert!(ins.answer.is_none());
        ins.answered(req, Err("x".into()));
        assert!(ins.answer.is_some());
        ins.refresh();
        assert!(
            ins.follow(&ctx, Some(&b)).is_some(),
            "refresh asks again at once"
        );
        assert!(ins.follow(&ctx, None).is_none());
        assert!(ins.answer.is_none());
    }
}
