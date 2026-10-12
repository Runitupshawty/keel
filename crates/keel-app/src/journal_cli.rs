//! `keel journal` (Phase 15): the index journal from the command line, read straight from
//! its file (no library, no daemon). Prints the newest `--max` entries that pass the
//! filters, oldest first, as lines, JSON lines (`--json`) or CSV (`--csv`); `--watch`
//! then prints new entries as the index applies them.

use keel_search::journal::{self, Event, Filter, Journal};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// How often `--watch` reads the journal again.
pub const POLL: Duration = Duration::from_millis(500);
/// Entries printed when `--max` is not given.
pub const DEFAULT_MAX: usize = 100;

#[derive(Clone, Debug, Default)]
pub struct Args {
    pub watch: bool,
    pub action: Vec<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub path: Option<String>,
    pub search: Option<String>,
    pub max: Option<usize>,
    pub csv: bool,
    pub json: bool,
}

/// The filter the arguments ask for, or a usage error.
pub fn filter(a: &Args) -> Result<Filter, String> {
    let mut actions = Vec::new();
    for name in &a.action {
        match journal::Action::parse(name) {
            Some(x) => actions.push(x),
            None => {
                return Err(format!(
                    "unknown action {name:?} (create, delete, rename, modify, attributes)"
                ))
            }
        }
    }
    let when = |text: &Option<String>, parse: fn(&str) -> Option<i64>, flag: &str| {
        text.as_deref()
            .map(|t| parse(t).ok_or_else(|| format!("{flag}: not a time ({t})")))
            .transpose()
    };
    let path_regex = a
        .path
        .as_deref()
        .map(|re| {
            regex::RegexBuilder::new(re)
                .case_insensitive(cfg!(windows))
                .size_limit(1 << 20)
                .build()
                .map_err(|e| format!("--path: {e}"))
        })
        .transpose()?;
    Ok(Filter {
        actions,
        since: when(&a.since, journal::parse_since, "--since")?,
        until: when(&a.until, journal::parse_until, "--until")?,
        path_regex,
        text: a.search.clone().unwrap_or_default(),
        limit: a.max.unwrap_or(DEFAULT_MAX),
        ..Filter::default()
    })
}

/// One entry as printed.
pub fn line(e: &Event, a: &Args) -> String {
    if a.csv {
        // The CSV of one entry, without its header.
        return journal::csv(std::slice::from_ref(e))
            .lines()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
    }
    if a.json {
        return serde_json::to_string(e).unwrap_or_default();
    }
    let mut text = format!(
        "{}  {:<10}  {}",
        journal::local_text(e.time),
        e.action.as_str(),
        e.path
    );
    if let Some(old) = &e.old_path {
        text.push_str(&format!("  (from {old})"));
    }
    text
}

/// Prints the journal at `file`; with `--watch`, follows it while `keep_going` says so.
/// Exit codes as the other subcommands: 0, 1 (the journal failed), 2 (usage).
pub fn run(
    file: Option<PathBuf>,
    a: &Args,
    out: &mut Box<dyn Write>,
    keep_going: &dyn Fn() -> bool,
) -> i32 {
    use crate::commands::{FAILED, OK, USAGE};
    let filter = match filter(a) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("keel: {e}");
            return USAGE;
        }
    };
    let Some(file) = file else {
        eprintln!("keel: no folder for the index");
        return FAILED;
    };
    if a.csv && writeln!(out, "time,action,path,old_path,folder,size").is_err() {
        return OK;
    }
    if !file.is_file() && !a.watch {
        // Nothing indexed a change yet.
        return OK;
    }
    let j = match Journal::open(&file, journal::JournalSettings::default().max_bytes()) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("keel: the index journal: {e:#}");
            return FAILED;
        }
    };
    match follow(&j, filter, a, out, keep_going) {
        Ok(()) => OK,
        Err(e) => {
            eprintln!("keel: the index journal: {e:#}");
            FAILED
        }
    }
}

fn follow(
    j: &Journal,
    filter: Filter,
    a: &Args,
    out: &mut Box<dyn Write>,
    keep_going: &dyn Fn() -> bool,
) -> anyhow::Result<()> {
    // Taken first: an entry added while the query runs is not missed by --watch.
    let mut last = j.last_id()?;
    let mut rows = j.query(&filter)?;
    rows.reverse();
    for e in &rows {
        if writeln!(out, "{}", line(e, a)).is_err() {
            return Ok(()); // the reader went away (`| head`)
        }
        last = last.max(e.id);
    }
    let _ = out.flush();
    if !a.watch {
        return Ok(());
    }
    let next = Filter {
        limit: 0,
        oldest_first: true,
        ..filter
    };
    while keep_going() {
        std::thread::sleep(POLL);
        // Taken before the query: anything newer is looked at next time.
        let mark = j.last_id()?;
        let new = j.query(&Filter {
            after_id: Some(last),
            ..next.clone()
        })?;
        // Entries the filters skip still move the mark.
        last = mark.max(new.last().map_or(last, |e| e.id));
        for e in &new {
            if writeln!(out, "{}", line(e, a)).is_err() {
                return Ok(());
            }
        }
        if out.flush().is_err() {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_search::journal::Action;
    use parking_lot::Mutex;
    use std::path::Path;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().clone()).unwrap()
        }
    }

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-journal-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn prints_filters_and_watches() {
        let dir = temp();
        let file = dir.join(journal::FILE_NAME);
        let buf = Buf::default();
        let mut out: Box<dyn Write> = Box::new(buf.clone());
        let args = Args::default();
        // No journal yet: nothing, and fine.
        assert_eq!(run(Some(file.clone()), &args, &mut out, &|| true), 0);
        assert!(buf.text().is_empty());
        let j = Journal::open(&file, 1 << 20).unwrap();
        j.record_all(&[
            Event::new(Action::Create, Path::new("/d/a.txt")).at(1_000),
            Event::new(Action::Modify, Path::new("/d/a.txt")).at(2_000),
            Event::renamed(Path::new("/d/a.txt"), Path::new("/d/b.pdf")).at(3_000),
        ])
        .unwrap();
        assert_eq!(run(Some(file.clone()), &args, &mut out, &|| true), 0);
        let text = buf.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].contains("create") && lines[0].ends_with("a.txt"),
            "oldest first"
        );
        assert!(lines[2].contains("rename") && lines[2].ends_with("a.txt)"));
        buf.0.lock().clear();
        let renames = Args {
            action: vec!["renamed".into()],
            json: true,
            ..Args::default()
        };
        assert_eq!(run(Some(file.clone()), &renames, &mut out, &|| true), 0);
        let e: Event = serde_json::from_str(buf.text().trim()).unwrap();
        assert_eq!(e.action, Action::Rename);
        buf.0.lock().clear();
        let newest = Args {
            max: Some(1),
            csv: true,
            ..Args::default()
        };
        assert_eq!(run(Some(file.clone()), &newest, &mut out, &|| true), 0);
        let text = buf.text();
        assert!(text.starts_with("time,action,path,old_path,folder,size\n"));
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains(",rename,"));
        // Usage errors.
        let bad = |a: Args| {
            let mut sink: Box<dyn Write> = Box::new(Buf::default());
            run(Some(file.clone()), &a, &mut sink, &|| true)
        };
        let action = Args {
            action: vec!["explode".into()],
            ..Args::default()
        };
        assert_eq!(bad(action), 2);
        let since = Args {
            since: Some("soon".into()),
            ..Args::default()
        };
        assert_eq!(bad(since), 2);
        let regex = Args {
            path: Some("(".into()),
            ..Args::default()
        };
        assert_eq!(bad(regex), 2);

        // --watch prints what comes after, filtered, until told to stop.
        buf.0.lock().clear();
        let polls = Arc::new(Mutex::new(0));
        let writer = {
            let file = file.clone();
            std::thread::spawn(move || {
                let j = Journal::open(&file, 1 << 20).unwrap();
                std::thread::sleep(POLL);
                j.record(Event::new(Action::Delete, Path::new("/d/b.pdf")))
                    .unwrap();
                j.record(Event::new(Action::Create, Path::new("/d/skip.txt")))
                    .unwrap();
            })
        };
        let watch = Args {
            watch: true,
            action: vec!["delete".into()],
            ..Args::default()
        };
        let p = polls.clone();
        let code = run(Some(file.clone()), &watch, &mut out, &move || {
            *p.lock() += 1;
            *p.lock() < 8
        });
        writer.join().unwrap();
        assert_eq!(code, 0);
        let text = buf.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("delete") && text.trim_end().ends_with("b.pdf"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
