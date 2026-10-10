//! Bulk rename (Ctrl+F2): a pattern with placeholders, find/replace and a case transform
//! produce new names; the preview flags conflicts; applying renames through the provider in
//! an order that never collides with itself, and returns the steps so they can be undone.

use keel_vfs::VPath;
use regex::RegexBuilder;
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Case {
    Keep,
    Lower,
    Upper,
    Title,
}

pub struct Item {
    pub path: VPath,
    pub name: String,
    pub is_dir: bool,
    pub modified: Option<SystemTime>,
}

pub struct BulkRename {
    pub items: Vec<Item>,
    /// `(parent, name)` of siblings that are not being renamed (folded when `fold`).
    pub others: HashSet<(VPath, String)>,
    pub pattern: String,
    pub start: String,
    pub step: String,
    pub find: String,
    pub replace: String,
    pub regex: bool,
    pub ignore_case: bool,
    pub case: Case,
    pub windows: bool,
    /// Compare names case-insensitively (Windows, macOS).
    pub fold: bool,
    pub focus: bool,
}

pub struct Row {
    pub old: String,
    pub new: String,
    pub problem: Option<String>,
}

impl BulkRename {
    pub fn new(items: Vec<Item>, others: impl IntoIterator<Item = (VPath, String)>) -> Self {
        let fold = cfg!(any(windows, target_os = "macos"));
        let others = others
            .into_iter()
            .map(|(d, n)| (d, fold_name(&n, fold)))
            .collect();
        Self {
            items,
            others,
            pattern: "{name}.{ext}".into(),
            start: "1".into(),
            step: "1".into(),
            find: String::new(),
            replace: String::new(),
            regex: false,
            ignore_case: false,
            case: Case::Keep,
            windows: cfg!(windows),
            fold,
            focus: true,
        }
    }

    /// A problem with the settings themselves (not with one name).
    pub fn error(&self) -> Option<String> {
        if self.start.trim().parse::<i64>().is_err() {
            return Some("Start must be a whole number".into());
        }
        if self.step.trim().parse::<i64>().is_err() {
            return Some("Step must be a whole number".into());
        }
        if self.regex && !self.find.is_empty() {
            if let Err(e) = self.matcher() {
                return Some(format!("Bad regex: {e}"));
            }
        }
        None
    }

    fn matcher(&self) -> Result<regex::Regex, regex::Error> {
        let src = if self.regex {
            self.find.clone()
        } else {
            regex::escape(&self.find)
        };
        RegexBuilder::new(&src)
            .case_insensitive(self.ignore_case)
            .build()
    }

    /// The new name of item `i`, before any conflict check.
    fn new_name(&self, i: usize, matcher: Option<&regex::Regex>) -> String {
        let start = self.start.trim().parse::<i64>().unwrap_or(1);
        let step = self.step.trim().parse::<i64>().unwrap_or(1);
        let n = start.saturating_add(step.saturating_mul(i as i64));
        let mut name = expand(&self.pattern, &self.items[i], n);
        if let Some(re) = matcher {
            name = if self.regex {
                re.replace_all(&name, self.replace.as_str()).into_owned()
            } else {
                re.replace_all(&name, regex::NoExpand(&self.replace))
                    .into_owned()
            };
        }
        apply_case(&name, self.case)
    }

    pub fn rows(&self) -> Vec<Row> {
        let matcher = if self.find.is_empty() {
            None
        } else {
            self.matcher().ok()
        };
        let news: Vec<String> = (0..self.items.len())
            .map(|i| self.new_name(i, matcher.as_ref()))
            .collect();
        let key = |i: usize| (self.items[i].path.parent(), fold_name(&news[i], self.fold));
        let mut seen: HashMap<(Option<VPath>, String), usize> = HashMap::new();
        for i in 0..news.len() {
            *seen.entry(key(i)).or_default() += 1;
        }
        (0..news.len())
            .map(|i| {
                let exists = self.items[i]
                    .path
                    .parent()
                    .is_some_and(|d| self.others.contains(&(d, fold_name(&news[i], self.fold))));
                let problem = if news[i].trim().is_empty() {
                    Some("Empty name".to_owned())
                } else if let Some(why) = crate::dialogs::invalid_name_for(&news[i], self.windows) {
                    Some(why)
                } else if seen[&key(i)] > 1 {
                    Some("Two items would get this name".to_owned())
                } else if exists {
                    Some("An item with this name already exists".to_owned())
                } else {
                    None
                };
                Row {
                    old: self.items[i].name.clone(),
                    new: news[i].clone(),
                    problem,
                }
            })
            .collect()
    }

    /// `(item, new name)` for the items that change, or None while anything is wrong.
    pub fn renames(&self) -> Option<Vec<(VPath, String)>> {
        if self.error().is_some() {
            return None;
        }
        let rows = self.rows();
        if rows.iter().any(|r| r.problem.is_some()) {
            return None;
        }
        let out: Vec<_> = rows
            .into_iter()
            .zip(&self.items)
            .filter(|(r, _)| r.old != r.new)
            .map(|(r, it)| (it.path.clone(), r.new))
            .collect();
        (!out.is_empty()).then_some(out)
    }
}

fn fold_name(name: &str, fold: bool) -> String {
    if fold {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

/// Stem and extension (without the dot) of a file name; folders and dotfiles have none.
fn split_ext(name: &str, is_dir: bool) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && !is_dir && i + 1 < name.len() => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// Expands `{name} {ext} {n} {n:3} {date} {parent}`; anything else stays as typed. A dot
/// right before `{ext}` goes with it when the item has no extension.
pub fn expand(pattern: &str, item: &Item, n: i64) -> String {
    let (stem, ext) = split_ext(&item.name, item.is_dir);
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let key = &after[..close];
        let value = match key {
            "name" => Some(stem.to_owned()),
            "ext" => {
                if ext.is_empty() && out.ends_with('.') {
                    out.pop();
                }
                Some(ext.to_owned())
            }
            "n" => Some(n.to_string()),
            "date" => Some(
                item.modified
                    .map(|t| {
                        chrono::DateTime::<chrono::Local>::from(t)
                            .format("%Y-%m-%d")
                            .to_string()
                    })
                    .unwrap_or_default(),
            ),
            "parent" => Some(
                item.path
                    .parent()
                    .map(|p| p.name().to_owned())
                    .unwrap_or_default(),
            ),
            _ => key
                .strip_prefix("n:")
                .and_then(|w| w.parse::<usize>().ok())
                .map(|w| {
                    let w = w.min(18);
                    if n < 0 {
                        format!("-{:0w$}", n.unsigned_abs(), w = w.saturating_sub(1))
                    } else {
                        format!("{n:0w$}")
                    }
                }),
        };
        match value {
            Some(v) => out.push_str(&v),
            None => {
                out.push('{');
                out.push_str(key);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Case transform of the part before the last dot (the extension is left alone).
pub fn apply_case(name: &str, case: Case) -> String {
    if case == Case::Keep {
        return name.to_owned();
    }
    let (stem, ext) = split_ext(name, false);
    let stem = match case {
        Case::Lower => stem.to_lowercase(),
        Case::Upper => stem.to_uppercase(),
        _ => {
            let mut start = true;
            stem.chars()
                .map(|c| {
                    let out: String = if start {
                        c.to_uppercase().collect()
                    } else {
                        c.to_lowercase().collect()
                    };
                    start = !c.is_alphanumeric();
                    out
                })
                .collect()
        }
    };
    if ext.is_empty() {
        stem
    } else {
        format!("{stem}.{ext}")
    }
}

// ---- applying -------------------------------------------------------------------------

struct Slot {
    from: VPath,
    to: VPath,
    tmp: Option<VPath>,
}

pub struct Outcome {
    /// Every rename that happened, in order (undo reverses and swaps them).
    pub done: Vec<(VPath, VPath)>,
    /// `name: reason` of the items that did not get their name.
    pub failed: Vec<String>,
    pub renamed: usize,
}

/// Runs `renames` through `rename`. An item whose target is another renamed item's current
/// name goes through a temporary name first: blocked -> temp, the rest -> final, temp -> final.
pub fn execute(
    renames: &[(VPath, String)],
    fold: bool,
    mut rename: impl FnMut(&VPath, &VPath) -> anyhow::Result<()>,
) -> Outcome {
    let slots: Vec<Slot> = renames
        .iter()
        .enumerate()
        .map(|(i, (from, name))| {
            let dir = from.parent().unwrap_or_else(|| from.clone());
            let key = fold_name(name, fold);
            let blocked = renames.iter().enumerate().any(|(j, (other, _))| {
                j != i && other.parent() == from.parent() && fold_name(other.name(), fold) == key
            });
            Slot {
                from: from.clone(),
                to: dir.join(name),
                tmp: blocked.then(|| dir.join(&format!("{}.keel-tmp-{i}", from.name()))),
            }
        })
        .collect();
    let mut out = Outcome {
        done: Vec::new(),
        failed: Vec::new(),
        renamed: 0,
    };
    let mut ok = vec![true; slots.len()];
    let mut step = |out: &mut Outcome, a: &VPath, b: &VPath| -> Result<(), String> {
        match rename(a, b) {
            Ok(()) => {
                out.done.push((a.clone(), b.clone()));
                Ok(())
            }
            Err(e) => Err(format!("{}: {e:#}", a.name())),
        }
    };
    for (i, s) in slots.iter().enumerate() {
        if let Some(tmp) = &s.tmp {
            if let Err(e) = step(&mut out, &s.from, tmp) {
                out.failed.push(e);
                ok[i] = false;
            }
        }
    }
    for (i, s) in slots.iter().enumerate() {
        if s.tmp.is_none() {
            if let Err(e) = step(&mut out, &s.from, &s.to) {
                out.failed.push(e);
                ok[i] = false;
            }
        }
    }
    for (i, s) in slots.iter().enumerate() {
        let Some(tmp) = s.tmp.as_ref().filter(|_| ok[i]) else {
            continue;
        };
        if let Err(e) = step(&mut out, tmp, &s.to) {
            out.failed.push(e);
            ok[i] = false;
            // Put it back under its own name rather than leave the temporary one.
            let _ = step(&mut out, tmp, &s.from);
        }
    }
    out.renamed = ok.iter().filter(|b| **b).count();
    out
}

/// The steps that undo `done`: reversed and swapped.
pub fn undo_steps(done: &[(VPath, VPath)]) -> Vec<(VPath, VPath)> {
    done.iter()
        .rev()
        .map(|(a, b)| (b.clone(), a.clone()))
        .collect()
}

/// Runs `steps` in order; used for undo.
pub fn run_steps(
    steps: &[(VPath, VPath)],
    mut rename: impl FnMut(&VPath, &VPath) -> anyhow::Result<()>,
) -> Outcome {
    let mut out = Outcome {
        done: Vec::new(),
        failed: Vec::new(),
        renamed: 0,
    };
    for (a, b) in steps {
        match rename(a, b) {
            Ok(()) => out.done.push((a.clone(), b.clone())),
            Err(e) => out.failed.push(format!("{}: {e:#}", a.name())),
        }
    }
    out.renamed = out.done.len();
    out
}

/// Toast text for an outcome and whether it is all good.
pub fn summary(o: &Outcome, total: usize, verb: &str) -> (String, bool) {
    if o.failed.is_empty() {
        return (format!("{verb} {}", crate::jobs::items(o.renamed)), true);
    }
    let shown: Vec<&str> = o.failed.iter().take(3).map(String::as_str).collect();
    let more = o.failed.len().saturating_sub(3);
    let mut text = format!(
        "{verb} {} of {total} items. Failed: {}",
        o.renamed,
        shown.join("; ")
    );
    if more > 0 {
        text.push_str(&format!("; and {more} more"));
    }
    (text, false)
}

// ---- dialog ---------------------------------------------------------------------------

/// Draws the dialog body; returns the renames to apply, and sets `cancel` on Cancel.
pub fn ui(
    ui: &mut egui::Ui,
    m: &mut BulkRename,
    cancel: &mut bool,
) -> Option<Vec<(VPath, String)>> {
    ui.set_width(560.0);
    ui.strong(format!("Rename {}", crate::jobs::items(m.items.len())));
    ui.add_space(4.0);
    egui::Grid::new("bulk-rename-fields")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Pattern");
            let r = ui.add(egui::TextEdit::singleline(&mut m.pattern).desired_width(f32::INFINITY));
            if std::mem::take(&mut m.focus) {
                r.request_focus();
            }
            r.on_hover_text("{name} {ext} {n} {n:3} {date} {parent}");
            ui.end_row();
            ui.label("Counter");
            ui.horizontal(|ui| {
                ui.label("start");
                ui.add(egui::TextEdit::singleline(&mut m.start).desired_width(50.0));
                ui.label("step");
                ui.add(egui::TextEdit::singleline(&mut m.step).desired_width(50.0));
            });
            ui.end_row();
            ui.label("Find");
            ui.add(egui::TextEdit::singleline(&mut m.find).desired_width(f32::INFINITY));
            ui.end_row();
            ui.label("Replace with");
            ui.add(egui::TextEdit::singleline(&mut m.replace).desired_width(f32::INFINITY));
            ui.end_row();
            ui.label("");
            ui.horizontal(|ui| {
                ui.checkbox(&mut m.regex, "regex");
                ui.checkbox(&mut m.ignore_case, "case-insensitive");
            });
            ui.end_row();
            ui.label("Case");
            ui.horizontal(|ui| {
                for (c, t) in [
                    (Case::Keep, "keep"),
                    (Case::Lower, "lower"),
                    (Case::Upper, "UPPER"),
                    (Case::Title, "Title"),
                ] {
                    ui.selectable_value(&mut m.case, c, t);
                }
            });
            ui.end_row();
        });
    ui.add_space(6.0);
    let error = m.error();
    let rows = if error.is_some() {
        Vec::new()
    } else {
        m.rows()
    };
    if let Some(e) = &error {
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    let bad = ui.visuals().error_fg_color;
    egui::ScrollArea::vertical()
        .max_height(240.0)
        .show(ui, |ui| {
            egui::Grid::new("bulk-rename-preview")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    for r in &rows {
                        ui.label(&r.old);
                        ui.label("→");
                        match &r.problem {
                            Some(p) => {
                                ui.colored_label(bad, &r.new);
                                ui.colored_label(bad, p);
                            }
                            None => {
                                ui.label(&r.new);
                                ui.label("");
                            }
                        }
                        ui.end_row();
                    }
                });
        });
    ui.add_space(8.0);
    let renames = m.renames();
    let mut out = None;
    ui.horizontal(|ui| {
        let apply = ui.add_enabled(renames.is_some(), egui::Button::new("Apply"));
        let no = ui.button("Cancel");
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter)) && !no.has_focus();
        if apply.clicked() || enter {
            out = renames.clone();
        }
        *cancel |= no.clicked();
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    fn dir() -> VPath {
        VPath::local(PathBuf::from("/data/pics"))
    }

    fn item(name: &str) -> Item {
        Item {
            path: dir().join(name),
            name: name.into(),
            is_dir: false,
            modified: None,
        }
    }

    fn model(names: &[&str]) -> BulkRename {
        let mut m = BulkRename::new(names.iter().map(|n| item(n)).collect(), []);
        m.windows = false;
        m.fold = false;
        m
    }

    fn news(m: &BulkRename) -> Vec<String> {
        m.rows().into_iter().map(|r| r.new).collect()
    }

    #[test]
    fn placeholders_counters_and_padding() {
        let mut m = model(&["a.txt", "b.txt", "c"]);
        m.pattern = "{parent}-{n:3}_{name}.{ext}".into();
        m.start = "8".into();
        m.step = "2".into();
        assert_eq!(news(&m), ["pics-008_a.txt", "pics-010_b.txt", "pics-012_c"]);
        m.pattern = "{n}{x}".into();
        assert_eq!(news(&m), ["8{x}", "10{x}", "12{x}"]);
        m.start = "-5".into();
        m.step = "1".into();
        m.pattern = "{n:3}".into();
        assert_eq!(news(&m)[0], "-05");
        // The shape is what matters; the day depends on the time zone.
        m.items[0].modified =
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(86400 * 365));
        m.pattern = "{date}".into();
        assert_eq!(news(&m)[0].len(), 10);
    }

    #[test]
    fn find_replace_plain_regex_and_case() {
        let mut m = model(&["IMG_1.JPG", "img_2.jpg"]);
        m.find = "img".into();
        m.replace = "pic".into();
        m.ignore_case = true;
        assert_eq!(news(&m), ["pic_1.JPG", "pic_2.jpg"]);
        m.ignore_case = false;
        assert_eq!(news(&m), ["IMG_1.JPG", "pic_2.jpg"]);
        m.regex = true;
        m.find = r"(\d)".into();
        m.replace = "<$1>".into();
        assert_eq!(news(&m), ["IMG_<1>.JPG", "img_<2>.jpg"]);
        m.find = "(".into();
        assert!(m.error().is_some() && m.renames().is_none());
        // Plain replace does not expand `$`.
        m.regex = false;
        m.find = "_".into();
        m.replace = "$1".into();
        assert_eq!(news(&m)[1], "img$12.jpg");
    }

    #[test]
    fn case_transforms_keep_the_extension() {
        assert_eq!(
            apply_case("Foo bar-BAZ.TxT", Case::Lower),
            "foo bar-baz.TxT"
        );
        assert_eq!(apply_case("Foo bar.txt", Case::Upper), "FOO BAR.txt");
        assert_eq!(
            apply_case("hELLO wORLD-x_y.md", Case::Title),
            "Hello World-X_Y.md"
        );
        assert_eq!(apply_case("a.b", Case::Keep), "a.b");
    }

    #[test]
    fn conflicts_are_flagged() {
        let problem = |m: &BulkRename| m.rows().into_iter().map(|r| r.problem).collect::<Vec<_>>();
        let mut m = model(&["a.txt", "b.txt"]);
        m.others.insert((dir(), "taken.txt".into()));
        m.pattern = "same.txt".into();
        assert!(problem(&m)
            .iter()
            .all(|p| p.as_deref() == Some("Two items would get this name")));
        m.items.truncate(1);
        m.pattern = "taken.txt".into();
        assert!(problem(&m)[0]
            .as_deref()
            .unwrap()
            .contains("already exists"));
        m.pattern = String::new();
        assert_eq!(problem(&m)[0].as_deref(), Some("Empty name"));
        m.pattern = "a/b".into();
        assert!(problem(&m)[0].is_some());
        assert!(m.renames().is_none());
        m.pattern = "ok.txt".into();
        assert_eq!(m.renames().unwrap().len(), 1);
        // Windows rules and case folding.
        m.windows = true;
        m.pattern = "CON.txt".into();
        assert!(problem(&m)[0].as_deref().unwrap().contains("reserved"));
        m.fold = true;
        m.others.insert((dir(), "low.txt".into()));
        m.pattern = "LOW.txt".into();
        assert!(problem(&m)[0].is_some());
    }

    /// A fake folder of names; rename fails when the target exists or equals `fail`.
    fn fake_rename<'a>(
        fs: &'a RefCell<Vec<String>>,
        log: &'a RefCell<Vec<String>>,
        fail: &'a str,
    ) -> impl FnMut(&VPath, &VPath) -> anyhow::Result<()> + 'a {
        move |a, b| {
            let mut fs = fs.borrow_mut();
            anyhow::ensure!(b.name() != fail, "refused");
            anyhow::ensure!(!fs.iter().any(|n| n == b.name()), "exists");
            let i = fs
                .iter()
                .position(|n| n == a.name())
                .ok_or_else(|| anyhow::anyhow!("gone"))?;
            fs[i] = b.name().to_owned();
            log.borrow_mut().push(format!("{}>{}", a.name(), b.name()));
            Ok(())
        }
    }

    fn strings(v: &[&str]) -> RefCell<Vec<String>> {
        RefCell::new(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn swap_goes_through_temporary_names_and_undoes_in_reverse() {
        let renames = vec![
            (dir().join("a"), "b".to_owned()),
            (dir().join("b"), "a".to_owned()),
        ];
        let (fs, log) = (strings(&["a", "b"]), strings(&[]));
        let out = execute(&renames, false, fake_rename(&fs, &log, ""));
        assert!(out.failed.is_empty() && out.renamed == 2);
        assert_eq!(
            *log.borrow(),
            [
                "a>a.keel-tmp-0",
                "b>b.keel-tmp-1",
                "a.keel-tmp-0>b",
                "b.keel-tmp-1>a"
            ]
        );
        assert_eq!(*fs.borrow(), ["b", "a"]);
        let log2 = strings(&[]);
        let back = run_steps(&undo_steps(&out.done), fake_rename(&fs, &log2, ""));
        assert!(back.failed.is_empty());
        assert_eq!(
            *log2.borrow(),
            [
                "a>b.keel-tmp-1",
                "b>a.keel-tmp-0",
                "b.keel-tmp-1>b",
                "a.keel-tmp-0>a"
            ]
        );
        assert_eq!(*fs.borrow(), ["a", "b"]);
    }

    #[test]
    fn chain_orders_free_items_first_and_failure_is_partial() {
        // a -> b is blocked by b -> c; x -> y is free.
        let renames = vec![
            (dir().join("a"), "b".to_owned()),
            (dir().join("b"), "c".to_owned()),
            (dir().join("x"), "y".to_owned()),
        ];
        let (fs, log) = (strings(&["a", "b", "x"]), strings(&[]));
        let out = execute(&renames, false, fake_rename(&fs, &log, ""));
        assert_eq!(
            *log.borrow(),
            ["a>a.keel-tmp-0", "b>c", "x>y", "a.keel-tmp-0>b"]
        );
        assert_eq!(out.renamed, 3);
        let (fs, log) = (strings(&["a", "b", "x"]), strings(&[]));
        let out = execute(&renames, false, fake_rename(&fs, &log, "y"));
        assert_eq!((out.renamed, out.failed.len()), (2, 1));
        let (text, ok) = summary(&out, 3, "Renamed");
        assert!(!ok && text.starts_with("Renamed 2 of 3 items. Failed: x: "));
    }
}
