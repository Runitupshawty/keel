//! The Journal tab (Phase 15, Task 74): the index journal (`keel_search::journal`), every
//! create, delete, rename, modify and attribute change Keel's own index applied, newest
//! first. Filters by action, date range ("changed today", or From / To as typed: `today`,
//! `2h`, `2026-10-12`, `2026-10-12 14:00`), path pattern and text; follows new entries
//! while **Live** is on; **Export CSV…** saves what is shown. Queries run on a worker.
//! Also the Inspector's journal lines for one file ([`lookup`], [`entry_text`]).

use crate::a11y::Named;
use crate::keys::Action;
use keel_search::journal::{self, Action as Change, Event, Filter};
use keel_vfs::VPath;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// At most this many entries are shown (the newest).
pub const LIMIT: usize = 5000;
/// How often a live journal is read again.
pub const POLL: Duration = Duration::from_secs(2);
/// Typing in a filter field waits this long before querying.
const DEBOUNCE: Duration = Duration::from_millis(250);
pub const OFF: &str = "The index journal is off (Settings → Search → Journal)";

/// A worker's answer: (request, entries or error).
type Answer = Arc<Mutex<Option<(u64, Result<Vec<Event>, String>)>>>;

/// The Journal tab's state.
pub struct JournalView {
    /// Shown actions, in `Change::ALL` order.
    pub actions: [bool; 5],
    pub today: bool,
    pub since: String,
    pub until: String,
    pub regex: String,
    pub text: String,
    /// Read again every [`POLL`].
    pub live: bool,
    pub rows: Vec<Event>,
    pub error: Option<String>,
    /// The export's outcome.
    pub note: Option<String>,
    /// Query at this instant (filters changed, or first shown).
    due: Option<Instant>,
    last: Option<Instant>,
    req: u64,
    loading: bool,
    answer: Answer,
    notes: Arc<Mutex<Option<String>>>,
    selected: Option<i64>,
}

impl Default for JournalView {
    fn default() -> Self {
        Self {
            actions: [true; 5],
            today: false,
            since: String::new(),
            until: String::new(),
            regex: String::new(),
            text: String::new(),
            live: true,
            rows: Vec::new(),
            error: None,
            note: None,
            due: Some(Instant::now()),
            last: None,
            req: 0,
            loading: false,
            answer: Answer::default(),
            notes: Arc::default(),
            selected: None,
        }
    }
}

impl JournalView {
    /// The query the filters ask for, or what is wrong with them.
    pub fn filter(&self) -> Result<Filter, String> {
        let actions: Vec<Change> = Change::ALL
            .into_iter()
            .zip(self.actions)
            .filter_map(|(a, on)| on.then_some(a))
            .collect();
        let when = |text: &str, parse: fn(&str) -> Option<i64>, what: &str| {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                parse(text)
                    .map(Some)
                    .ok_or_else(|| format!("{what}: not a time ({text})"))
            }
        };
        let mut since = when(&self.since, journal::parse_since, "From")?;
        if self.today {
            let today = journal::start_of_today();
            since = Some(since.map_or(today, |s| s.max(today)));
        }
        let path_regex = match self.regex.trim() {
            "" => None,
            re => Some(
                regex::RegexBuilder::new(re)
                    .case_insensitive(cfg!(windows))
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| format!("Path pattern: {e}"))?,
            ),
        };
        Ok(Filter {
            // All on is no filter; all off shows nothing.
            actions: if actions.len() == Change::ALL.len() {
                Vec::new()
            } else {
                actions
            },
            since,
            until: when(&self.until, journal::parse_until, "To")?,
            path_regex,
            text: self.text.trim().to_owned(),
            limit: LIMIT,
            ..Filter::default()
        })
    }

    /// Query again soon (a filter changed).
    pub fn changed(&mut self) {
        self.due = Some(Instant::now() + DEBOUNCE);
    }

    /// Takes a worker's answer and starts a query when one is due. Returns when to look
    /// again.
    pub fn tick(&mut self, ctx: &egui::Context) -> Option<Duration> {
        if let Some(note) = self.notes.lock().take() {
            self.note = Some(note);
        }
        if let Some((req, result)) = self.answer.lock().take() {
            if req == self.req {
                self.loading = false;
                self.last = Some(Instant::now());
                match result {
                    Ok(rows) => {
                        self.rows = rows;
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        }
        let now = Instant::now();
        if self.live && !self.loading && self.due.is_none() {
            self.due = Some(self.last.map_or(now, |l| l + POLL));
        }
        let due = self.due?;
        if now < due {
            return Some(due - now);
        }
        self.due = None;
        let filter = match self.filter() {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(e);
                self.rows.clear();
                return None;
            }
        };
        if filter.actions.is_empty() && !self.actions.iter().any(|on| *on) {
            self.rows.clear();
            self.error = None;
            return None;
        }
        self.req += 1;
        self.loading = true;
        let (req, answer, ctx) = (self.req, self.answer.clone(), ctx.clone());
        crate::worker::spawn("keel-journal", move || {
            let result = match journal::global() {
                Some(j) => j.query(&filter).map_err(|e| format!("{e:#}")),
                None => Err(OFF.to_owned()),
            };
            *answer.lock() = Some((req, result));
            ctx.request_repaint();
        });
        None
    }

    /// Saves the shown entries as CSV where the user picks (the dialog runs on a worker).
    fn export(&self, ctx: &egui::Context) {
        let text = journal::csv(&self.rows);
        let n = self.rows.len();
        let (notes, ctx) = (self.notes.clone(), ctx.clone());
        crate::worker::spawn("keel-journal-export", move || {
            let Some(file) = rfd::FileDialog::new()
                .set_title("Export journal")
                .set_file_name("keel-journal.csv")
                .add_filter("CSV", &["csv"])
                .save_file()
            else {
                return;
            };
            *notes.lock() = Some(match std::fs::write(&file, text) {
                Ok(()) => format!("Exported {n} entries to {}", file.display()),
                Err(e) => format!("Export failed: {e}"),
            });
            ctx.request_repaint();
        });
    }
}

/// One entry for the Inspector: when, then what happened.
pub fn entry_text(e: &Event) -> String {
    let name = |p: &str| p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned();
    let what = match (e.action, &e.old_path) {
        (Change::Rename, Some(old)) => {
            let (from, to) = (parent(old), parent(&e.path));
            if from == to {
                format!("Renamed from {}", name(old))
            } else {
                format!("Moved from {old}")
            }
        }
        (a, _) => a.label().to_owned(),
    };
    format!("{}  {what}", journal::local_text(e.time))
}

fn parent(p: &str) -> &str {
    p.rfind(['/', '\\']).map_or("", |i| &p[..i])
}

/// The Inspector's lookup: the newest journal entries of `path` (as the path, or as a
/// rename's old path), on a worker; `done` gets them. Nothing for a path that is not
/// local.
pub fn lookup(
    path: &VPath,
    max: usize,
    done: impl FnOnce(Result<Vec<Event>, String>) + Send + 'static,
) -> bool {
    let Some(local) = path.to_local_path() else {
        return false;
    };
    crate::worker::spawn("keel-journal-lookup", move || {
        let result = match journal::global() {
            Some(j) => j
                .query(&Filter {
                    path: Some(journal::path_text(&local)),
                    limit: max,
                    ..Filter::default()
                })
                .map_err(|e| format!("{e:#}")),
            None => Err(OFF.to_owned()),
        };
        done(result);
    })
}

/// The Journal tab.
pub fn ui(ui: &mut egui::Ui, v: &mut JournalView, out: &mut Vec<Action>) {
    if let Some(wait) = v.tick(ui.ctx()) {
        ui.ctx().request_repaint_after(wait);
    }
    ui.add_space(4.0);
    ui.heading("Index journal");
    ui.weak(
        "Every change Keel's own index applied: files and folders created, deleted, \
         renamed, modified and with changed attributes.",
    );
    ui.add_space(4.0);
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        for (i, a) in Change::ALL.into_iter().enumerate() {
            changed |= ui.checkbox(&mut v.actions[i], a.label()).changed();
        }
        ui.separator();
        changed |= ui.checkbox(&mut v.today, "Changed today").changed();
        ui.checkbox(&mut v.live, "Live")
            .on_hover_text("Show new entries as they come");
    });
    ui.horizontal_wrapped(|ui| {
        let mut field = |ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str, w| {
            ui.label(label);
            let r = ui.add(
                egui::TextEdit::singleline(text)
                    .hint_text(hint)
                    .desired_width(w),
            );
            changed |= r.named(label).changed();
        };
        field(ui, "From", &mut v.since, "today, 2h, 2026-10-12", 120.0);
        field(ui, "To", &mut v.until, "2026-10-12 18:00", 120.0);
        field(ui, "Path pattern", &mut v.regex, "regex", 140.0);
        field(ui, "Search", &mut v.text, "part of a path", 160.0);
    });
    ui.horizontal(|ui| {
        if ui.button("Refresh").clicked() {
            v.due = Some(Instant::now());
        }
        let export = ui.add_enabled(!v.rows.is_empty(), egui::Button::new("Export CSV…"));
        if export.tip("Save the entries shown as CSV").clicked() {
            v.export(ui.ctx());
        }
        if v.loading {
            crate::anim::spinner(ui);
        }
        match &v.error {
            Some(e) => {
                ui.colored_label(ui.visuals().warn_fg_color, e);
            }
            None => {
                let more = if v.rows.len() >= LIMIT {
                    " (newest)"
                } else {
                    ""
                };
                ui.weak(format!("{} entries{more}", v.rows.len()));
            }
        }
        if let Some(note) = &v.note {
            ui.weak(note);
        }
    });
    if changed {
        v.changed();
    }
    ui.separator();
    if v.rows.is_empty() && v.error.is_none() && !v.loading {
        ui.weak("No entries match.");
        return;
    }
    table(ui, v, out);
}

fn table(ui: &mut egui::Ui, v: &mut JournalView, out: &mut Vec<Action>) {
    use egui_extras::{Column, TableBuilder};
    let cell =
        |ui: &mut egui::Ui, text: &str| ui.add(egui::Label::new(text).truncate().selectable(false));
    TableBuilder::new(ui)
        .id_salt("journal-table")
        .striped(true)
        .resizable(true)
        .sense(egui::Sense::click())
        .max_scroll_height(f32::INFINITY)
        .column(Column::initial(140.0).at_least(80.0).clip(true))
        .column(Column::initial(80.0).at_least(50.0).clip(true))
        .column(Column::initial(380.0).at_least(80.0).clip(true))
        .column(Column::remainder().at_least(80.0).clip(true))
        .header(20.0, |mut h| {
            for title in ["Time", "Action", "Path", "Old path"] {
                h.col(|ui| {
                    ui.strong(title);
                });
            }
        })
        .body(|body| {
            body.rows(18.0, v.rows.len(), |mut row| {
                let e = &v.rows[row.index()];
                row.set_selected(v.selected == Some(e.id));
                row.col(|ui| {
                    cell(ui, &journal::local_text(e.time));
                });
                row.col(|ui| {
                    cell(ui, e.action.label());
                });
                row.col(|ui| {
                    cell(ui, &e.path).on_hover_text(&e.path);
                });
                row.col(|ui| {
                    cell(ui, e.old_path.as_deref().unwrap_or(""));
                });
                let r = row.response();
                if r.clicked() {
                    v.selected = Some(e.id);
                }
                if r.double_clicked() && e.action != Change::Delete {
                    // Show it in its folder (a new tab keeps the journal).
                    out.push(Action::JumpTo {
                        path: VPath::local(&e.path),
                        file: true,
                        new_tab: true,
                        other_pane: false,
                    });
                }
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_follow_the_checkboxes_and_fields() {
        let mut v = JournalView::default();
        let f = v.filter().unwrap();
        assert!(f.actions.is_empty() && f.since.is_none() && f.until.is_none());
        assert_eq!(f.limit, LIMIT);
        v.actions = [true, false, true, false, false];
        v.today = true;
        v.regex = r"\.pdf$".into();
        v.text = " Report ".into();
        v.until = "2h".into();
        let f = v.filter().unwrap();
        assert_eq!(f.actions, [Change::Create, Change::Rename]);
        assert_eq!(f.since, Some(journal::start_of_today()));
        assert!(f.until.is_some());
        assert!(f.path_regex.unwrap().is_match("a.pdf"));
        assert_eq!(f.text, "Report");
        // A later From than today wins.
        v.since = "1m".into();
        assert!(
            v.filter().unwrap().since.unwrap() > journal::start_of_today() || {
                // Just after midnight "1m ago" may be yesterday.
                journal::now_ms() - journal::start_of_today() < 60_000
            }
        );
        v.since = "soon".into();
        assert!(v.filter().unwrap_err().starts_with("From"));
        v.since.clear();
        v.regex = "(".into();
        assert!(v.filter().unwrap_err().starts_with("Path pattern"));
    }

    #[test]
    fn inspector_lines_say_what_happened() {
        let at = journal::local_text(0);
        let text = |e: Event| entry_text(&e.at(0)).strip_prefix(&at).unwrap().to_owned();
        let p = std::path::Path::new;
        assert_eq!(text(Event::new(Change::Create, p("/a/x.txt"))), "  Created");
        assert_eq!(
            text(Event::renamed(p("/a/old.txt"), p("/a/new.txt"))),
            "  Renamed from old.txt"
        );
        let moved = text(Event::renamed(p("/a/x.txt"), p("/b/x.txt")));
        assert!(moved.starts_with("  Moved from ") && moved.ends_with("x.txt"));
        assert_eq!(
            text(Event::new(Change::Attributes, p("/a/x.txt"))),
            "  Attributes"
        );
    }

    /// The tab reads the process's journal on a worker and follows new entries.
    #[test]
    fn the_tab_lists_the_journal_and_follows_it() {
        let dir = std::env::temp_dir().join(format!("keel-journal-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        journal::use_path(Some(dir.join(journal::FILE_NAME)));
        let mark = format!("keel-journal-ui-test-{}", std::process::id());
        let path = std::env::temp_dir().join(&mark).join("a.txt");
        journal::record(Event::new(Change::Create, &path));
        let ctx = egui::Context::default();
        let mut v = JournalView {
            text: mark.clone(),
            ..JournalView::default()
        };
        let wait = |v: &mut JournalView, n: usize| {
            let end = Instant::now() + Duration::from_secs(20);
            loop {
                v.tick(&ctx);
                if v.rows.len() == n {
                    return;
                }
                assert!(Instant::now() < end, "{:?} {:?}", v.rows, v.error);
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait(&mut v, 1);
        assert_eq!(v.rows[0].action, Change::Create);
        journal::record(Event::new(Change::Delete, &path));
        wait(&mut v, 2);
        assert_eq!(v.rows[0].action, Change::Delete, "newest first");
        // The Inspector's lookup of that file.
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(lookup(&VPath::local(&path), 10, move |r| {
            let _ = tx.send(r);
        }));
        let found = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(found.len(), 2);
        journal::use_path(None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
