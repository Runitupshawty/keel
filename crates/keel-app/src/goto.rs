//! Go to Folder (Ctrl+Shift+G, Phase 14 Task 67): type a path, with completion of the
//! last part from its folder's listing (any provider: local, SFTP, cloud, archives). Tab
//! completes (the one match, else the matches' common start), Up and Down pick a
//! suggestion, Enter goes (a file opens its folder with it selected, as the path box),
//! Esc closes. Paths read like the path box's (`pane::parse_path`): absolute, `~`,
//! relative to the active folder, or a `scheme://` location.

use crate::keys::Action;
use crate::worker;
use crossbeam_channel::Receiver;
use egui::{Id, Key, Modal, Modifiers};
use keel_vfs::{Kind, Router, VPath};
use std::sync::Arc;

/// Suggestions shown at most.
pub const LIMIT: usize = 12;

/// A folder's names: `(name, is a folder)`, or why it could not be listed.
type Listing = Result<Vec<(String, bool)>, String>;

#[derive(Default)]
pub struct GoTo {
    pub open: bool,
    pub text: String,
    /// The folder relative paths start from (the active pane's).
    base: Option<VPath>,
    focus: bool,
    /// The suggestion Up / Down picked.
    pick: Option<usize>,
    /// The folder last listed and its names.
    listed: Option<(VPath, Listing)>,
    /// The listing on its way.
    pending: Option<(VPath, Receiver<Listing>)>,
}

/// The separators of what the user types: `\` too on Windows (local paths).
fn is_sep(c: char, url: bool) -> bool {
    c == '/' || (cfg!(windows) && !url && c == '\\')
}

/// `text` split after its last separator: the folder part (with the separator) and the
/// partial name being typed.
pub fn split(text: &str) -> (&str, &str) {
    let url = text.contains("://");
    match text.rfind(|c| is_sep(c, url)) {
        Some(i) => text.split_at(i + 1),
        None => ("", text),
    }
}

/// The separator to append after a completed folder: the last one typed, else the
/// platform's (`/` in `scheme://` paths).
pub fn sep_of(text: &str) -> char {
    let url = text.contains("://");
    text.chars()
        .rev()
        .find(|&c| is_sep(c, url))
        .unwrap_or(if url || !cfg!(windows) { '/' } else { '\\' })
}

/// Names in `names` starting with `partial` (ignoring case): folders first, then by name;
/// names starting with a dot only when `partial` does.
pub fn matches(partial: &str, names: &[(String, bool)]) -> Vec<(String, bool)> {
    let low = partial.to_lowercase();
    let mut out: Vec<(String, bool)> = names
        .iter()
        .filter(|(n, _)| n.to_lowercase().starts_with(&low))
        .filter(|(n, _)| !n.starts_with('.') || partial.starts_with('.'))
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    out
}

/// `text` with its partial name replaced by `name` (and a separator after a folder).
pub fn complete(text: &str, name: &str, folder: bool) -> String {
    let (dir, _) = split(text);
    let mut out = format!("{dir}{name}");
    if folder {
        out.push(sep_of(text));
    }
    out
}

/// The longest start every name shares (ignoring case; the first name's spelling).
pub fn common_start(names: &[(String, bool)]) -> String {
    let Some((first, _)) = names.first() else {
        return String::new();
    };
    let mut len = first.len();
    for (n, _) in &names[1..] {
        let shared = (first.char_indices().zip(n.chars()))
            .take_while(|((_, a), b)| a.to_lowercase().eq(b.to_lowercase()))
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8());
        len = len.min(shared);
    }
    first[..len].to_owned()
}

/// What Tab does to `text`: the one match completed (a folder gets a separator), else the
/// matches' longer common start; None when there is nothing to add.
pub fn tab_complete(text: &str, found: &[(String, bool)]) -> Option<String> {
    let (_, partial) = split(text);
    match found {
        [] => None,
        [(name, folder)] => Some(complete(text, name, *folder)),
        _ => {
            let start = common_start(found);
            (start.chars().count() > partial.chars().count()).then(|| complete(text, &start, false))
        }
    }
}

impl GoTo {
    /// Opens on `base` (the active folder), keeping the last text typed.
    pub fn show(&mut self, base: VPath) {
        self.base = Some(base);
        self.open = true;
        self.focus = true;
        self.pick = None;
    }

    /// The folder whose names complete `text` (its folder part, read like the path box).
    fn folder_of(&self, text: &str) -> Option<VPath> {
        let base = self.base.as_ref()?;
        let (dir, _) = split(text);
        if dir.is_empty() {
            return Some(base.clone());
        }
        crate::pane::parse_path(dir, base)
    }

    /// The names for `text`'s folder: listed on a worker (once per folder).
    fn names(&mut self, router: &Arc<Router>, ctx: &egui::Context, text: &str) -> Listing {
        let Some(dir) = self.folder_of(text) else {
            return Ok(Vec::new());
        };
        if let Some((d, rx)) = &self.pending {
            if let Ok(listing) = rx.try_recv() {
                self.listed = Some((d.clone(), listing));
                self.pending = None;
            }
        }
        match &self.listed {
            Some((d, listing)) if *d == dir => return listing.clone(),
            _ => {}
        }
        if self.pending.as_ref().is_none_or(|(d, _)| *d != dir) {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (router, ctx2, at) = (router.clone(), ctx.clone(), dir.clone());
            worker::spawn("keel-goto", move || {
                let listing = router
                    .provider_for(&at)
                    .ok_or_else(|| format!("No provider for {}", at.display()))
                    .and_then(|p| p.list(&at).map_err(|e| format!("{e:#}")))
                    .map(|entries| {
                        entries
                            .into_iter()
                            .map(|e| (e.name, e.kind == Kind::Dir))
                            .collect()
                    });
                let _ = tx.send(listing);
                ctx2.request_repaint();
            });
            self.pending = Some((dir, rx));
        }
        Ok(Vec::new())
    }

    /// The dialog (when open). Some: go there.
    pub fn ui(&mut self, ctx: &egui::Context, router: &Arc<Router>) -> Option<Action> {
        if !self.open {
            return None;
        }
        let mut out = None;
        let text = self.text.clone();
        let listing = self.names(router, ctx, &text);
        let found: Vec<(String, bool)> = match &listing {
            Ok(names) => matches(split(&text).1, names),
            Err(_) => Vec::new(),
        };
        let modal = Modal::new(Id::new("keel-goto")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Go to Folder");
            let (up, down, enter, tab) = ui.input_mut(|i| {
                (
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::Tab),
                )
            });
            let shown = found.len().min(LIMIT);
            if up + down > 0 && shown > 0 {
                let at = self.pick.map_or(-1, |p| p as isize) + down as isize - up as isize;
                self.pick = (at >= 0).then(|| (at as usize).min(shown - 1));
            }
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.text)
                    .hint_text(if cfg!(windows) {
                        r"C:\Users or ~\Documents"
                    } else {
                        "/usr/local or ~/Documents"
                    })
                    .desired_width(f32::INFINITY),
            );
            let r = crate::a11y::Named::named(r, "Folder path");
            let mut caret_to_end = false;
            if tab {
                let next = match self.pick.and_then(|p| found.get(p)) {
                    Some((name, folder)) => Some(complete(&self.text, name, *folder)),
                    None => tab_complete(&self.text, &found),
                };
                if let Some(next) = next {
                    self.text = next;
                    self.pick = None;
                    caret_to_end = true;
                }
            }
            if std::mem::take(&mut self.focus) || !r.has_focus() || caret_to_end {
                r.request_focus();
                let mut state = egui::TextEdit::load_state(ui.ctx(), r.id).unwrap_or_default();
                let end = egui::text::CCursor::new(self.text.chars().count());
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                state.store(ui.ctx(), r.id);
            }
            if r.changed() {
                self.pick = None;
            }
            if enter {
                let typed = match self.pick.and_then(|p| found.get(p)) {
                    Some((name, folder)) => complete(&self.text, name, *folder),
                    None => self.text.clone(),
                };
                let base = self.base.clone();
                match base.and_then(|b| crate::pane::parse_path(&typed, &b)) {
                    Some(to) => {
                        self.text = typed;
                        out = Some(Action::OpenPath(to));
                    }
                    None => self.pick = None,
                }
            }
            ui.add_space(4.0);
            if let Err(why) = &listing {
                ui.weak(why);
            }
            for (row, (name, folder)) in found.iter().take(LIMIT).enumerate() {
                let label = if *folder {
                    format!("{name}{}", sep_of(&self.text))
                } else {
                    name.clone()
                };
                let picked = self.pick == Some(row);
                let b = egui::Button::new(label)
                    .selected(picked)
                    .frame(picked)
                    .min_size(egui::vec2(ui.available_width(), 0.0));
                if ui.add(b).clicked() {
                    self.text = complete(&self.text, name, *folder);
                    self.pick = None;
                    self.focus = true;
                }
            }
            if found.len() > LIMIT {
                ui.weak(format!("{} more", found.len() - LIMIT));
            }
            ui.add_space(4.0);
            ui.weak("Tab completes · Enter goes · Esc closes");
        });
        if modal.should_close() || out.is_some() {
            self.open = false;
            self.pending = None;
            self.listed = None;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[(&str, bool)]) -> Vec<(String, bool)> {
        list.iter().map(|(n, d)| (n.to_string(), *d)).collect()
    }

    #[test]
    fn splits_the_partial_name_off_the_folder() {
        assert_eq!(split("/usr/lo"), ("/usr/", "lo"));
        assert_eq!(split("docs"), ("", "docs"));
        assert_eq!(split("/"), ("/", ""));
        assert_eq!(split("sftp://host/srv/w"), ("sftp://host/srv/", "w"));
        if cfg!(windows) {
            assert_eq!(split(r"C:\Users\Pu"), (r"C:\Users\", "Pu"));
            assert_eq!(sep_of(r"C:\Users"), '\\');
            assert_eq!(sep_of("C:/Users"), '/');
        } else {
            assert_eq!(split(r"a\b"), ("", r"a\b"));
            assert_eq!(sep_of("x"), '/');
        }
        assert_eq!(sep_of("sftp://h/x"), '/');
    }

    #[test]
    fn matches_ignore_case_list_folders_first_and_hide_dot_names() {
        let all = names(&[
            ("Downloads", true),
            ("docs.txt", false),
            ("Documents", true),
            (".dotfile", false),
            ("Music", true),
        ]);
        let found = matches("do", &all);
        assert_eq!(
            found,
            names(&[
                ("Documents", true),
                ("Downloads", true),
                ("docs.txt", false)
            ])
        );
        assert_eq!(matches(".", &all), names(&[(".dotfile", false)]));
        assert_eq!(matches("", &all).len(), 4, "dot names only when typed");
    }

    #[test]
    fn tab_completes_one_match_or_the_common_start() {
        let all = names(&[("Documents", true), ("Downloads", true), ("Desktop", true)]);
        let text = "/home/u/Doc";
        let found = matches(split(text).1, &all);
        assert_eq!(
            tab_complete(text, &found).as_deref(),
            Some("/home/u/Documents/")
        );
        let text = "/home/u/d";
        let found = matches(split(text).1, &all);
        assert_eq!(found.len(), 3);
        // Nothing longer than "d" is shared ("De" vs "Do").
        assert_eq!(tab_complete(text, &found), None);
        let found = matches("do", &all);
        assert_eq!(
            tab_complete("/home/u/do", &found).as_deref(),
            Some("/home/u/Do")
        );
        assert_eq!(tab_complete("/x/zz", &[]), None);
        // A file completes without a separator.
        let files = names(&[("notes.txt", false)]);
        assert_eq!(tab_complete("n", &files).as_deref(), Some("notes.txt"));
    }

    #[test]
    fn common_start_is_unicode_safe() {
        let all = names(&[("Übung 1", true), ("übung 2", true)]);
        assert_eq!(common_start(&all), "Übung ");
        let all = names(&[("日本語a", true), ("日本b", true)]);
        assert_eq!(common_start(&all), "日本");
        assert_eq!(common_start(&[]), "");
    }

    /// The dialog lists the folder on a worker, completes with Tab and goes with Enter.
    #[test]
    fn dialog_completes_from_the_folder_and_goes() {
        let dir = std::env::temp_dir().join(format!("keel-goto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Projects")).unwrap();
        std::fs::create_dir_all(dir.join("Pictures")).unwrap();
        let base = VPath::local(&dir);
        let router = Arc::new(Router::new());
        let ctx = egui::Context::default();
        let mut goto = GoTo::default();
        goto.show(base.clone());
        goto.text = "Pro".into();
        let frame = |goto: &mut GoTo, events: Vec<egui::Event>| {
            let mut out = None;
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| out = goto.ui(ctx, &router),
            );
            out
        };
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while goto.listed.is_none() && std::time::Instant::now() < deadline {
            frame(&mut goto, vec![]);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(goto.listed.is_some(), "listed on the worker");
        assert_eq!(frame(&mut goto, vec![key(Key::Tab)]), None);
        assert_eq!(goto.text, format!("Projects{}", sep_of("")));
        let went = frame(&mut goto, vec![key(Key::Enter)]);
        assert_eq!(went, Some(Action::OpenPath(base.join("Projects"))));
        assert!(!goto.open);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
