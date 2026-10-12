//! User-defined cleanups (Task 85, WinDirStat's): a command line with placeholders, run on
//! the selected files, folders or drives. Stored per profile (`[[cleanups]]` in the
//! profile's `config.toml`), edited in Settings → Cleanups, run from the context menu
//! (Cleanups) and the command palette ("Run cleanup: <title>").
//!
//! Placeholders: `%p` the full path, `%n` the name, `%s` the path without a trailing
//! separator (`C:` for `C:\`), `%d` the folder (a file's folder, or the folder itself),
//! `%e` the extension (no dot; empty for folders), `%%` a percent sign. Any other `%x` is
//! an error, so a typo never runs.
//!
//! The command line is split into words first (Windows: the C runtime's rules, Unix: a
//! shell's quotes and backslashes), then each placeholder is replaced inside its word, so a
//! path with spaces, quotes or any Unicode stays one argument. The program runs directly,
//! not through a shell. When it is a shell, the script it gets is quoted for that shell:
//! `cmd /c …` (Windows) wraps every word holding a placeholder in double quotes and refuses
//! values with `%` or `"` (cmd would expand or end them); `sh|bash|zsh|dash|ksh -c '…'`
//! gets each value single-quoted; `powershell|pwsh -Command …` gets each value as a
//! single-quoted string with its quotes (also the typographic ones) doubled. Inside those
//! scripts placeholders go without quotes of their own.

use crate::keys::Action;
use keel_vfs::VPath;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the context menu finds the profile's cleanups (egui temp data, set each frame).
pub const MENU_ID: &str = "keel-cleanups";

/// One cleanup asks for at most this many runs (a recursive one on a big tree).
pub const MAX_RUNS: usize = 10_000;

/// A user-defined cleanup, as stored in the profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserCleanup {
    pub title: String,
    pub command: String,
    /// Works for files, folders, drives (the roots of drives and shares).
    pub files: bool,
    pub folders: bool,
    pub drives: bool,
    /// Also run for every folder below a selected folder or drive (deepest first, then
    /// the folder itself).
    pub recurse: bool,
    /// Show the expanded command lines and ask before running (always the first time).
    pub ask: bool,
    /// Refresh the folders shown once the runs end (with `wait`).
    pub refresh: bool,
    /// Windows: run in a console window of its own (else hidden).
    pub console: bool,
    /// Run one after another, each waiting for the last to exit, as a job with progress
    /// and Cancel; off: start them all and do not wait.
    pub wait: bool,
    /// The expanded command was shown and confirmed once; a changed command line asks
    /// again.
    pub confirmed: bool,
}

impl Default for UserCleanup {
    fn default() -> Self {
        Self {
            title: "New cleanup".into(),
            command: String::new(),
            files: true,
            folders: true,
            drives: false,
            recurse: false,
            ask: true,
            refresh: true,
            console: false,
            wait: true,
            confirmed: false,
        }
    }
}

impl UserCleanup {
    /// "files and folders".
    pub fn scope_text(&self) -> String {
        let kinds: Vec<&str> = [
            (self.files, "files"),
            (self.folders, "folders"),
            (self.drives, "drives"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, k)| *k)
        .collect();
        match kinds.as_slice() {
            [] => "nothing".into(),
            [one] => (*one).into(),
            [a, b] => format!("{a} and {b}"),
            [a, b, c] => format!("{a}, {b} and {c}"),
            _ => unreachable!(),
        }
    }

    fn takes(&self, kind: TargetKind) -> bool {
        match kind {
            TargetKind::File => self.files,
            TargetKind::Folder => self.folders,
            TargetKind::Drive => self.drives,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    File,
    Folder,
    Drive,
}

/// What the placeholders of one run stand for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub kind: TargetKind,
    /// `%p`
    pub full: String,
    /// `%n`
    pub name: String,
    /// `%s`
    pub trimmed: String,
    /// `%d`
    pub folder: String,
    /// `%e`
    pub ext: String,
}

impl Target {
    /// The placeholders for local `path` (`dir`: a folder; a folder with no parent is a
    /// drive).
    pub fn of(path: &Path, dir: bool) -> Target {
        let full = path.to_string_lossy().into_owned();
        let kind = match (dir, path.parent()) {
            (false, _) => TargetKind::File,
            (true, None) => TargetKind::Drive,
            (true, Some(_)) => TargetKind::Folder,
        };
        let trimmed = match full.trim_end_matches(std::path::is_separator) {
            "" => full.clone(),
            t => t.to_owned(),
        };
        let name = match path.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => trimmed.clone(),
        };
        let folder = match kind {
            TargetKind::File => path
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => full.clone(),
        };
        let ext = match kind {
            TargetKind::File => path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => String::new(),
        };
        Target {
            kind,
            full,
            name,
            trimmed,
            folder,
            ext,
        }
    }
}

/// How the program reads what it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    /// Arguments as they are.
    Direct,
    /// `cmd /c`: the rest of the command line is a cmd.exe script (Windows).
    Cmd,
    /// `sh -c`: the word after `-c` is a POSIX shell script.
    Posix,
    /// `powershell -Command`: the words after it are a PowerShell script.
    Pwsh,
}

/// The shell `program` is (by its file name).
pub fn shell_of(program: &str) -> Shell {
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    match stem {
        "cmd" if cfg!(windows) => Shell::Cmd,
        "sh" | "bash" | "zsh" | "dash" | "ksh" => Shell::Posix,
        "powershell" | "pwsh" => Shell::Pwsh,
        _ => Shell::Direct,
    }
}

/// One run: the program, its arguments, and the folder it runs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expanded {
    /// The program and its arguments, each passed as it is (but for `raw`).
    pub argv: Vec<String>,
    /// `cmd /c`: everything after the program, as cmd.exe reads it (passed unchanged).
    pub raw: Option<String>,
    /// The working folder: `%d`.
    pub dir: String,
}

impl Expanded {
    /// The command line as shown before it runs (and as Windows passes it).
    pub fn line(&self) -> String {
        let program = quote_program(&self.argv[0]);
        if let Some(raw) = &self.raw {
            return format!("{program} {raw}");
        }
        let rest = self.argv[1..].iter().map(|a| {
            if cfg!(windows) {
                quote_msvc(a)
            } else {
                quote_posix(a)
            }
        });
        std::iter::once(program)
            .chain(rest)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The words of a command line, by this system's rules.
pub fn split(line: &str) -> Result<Vec<String>, String> {
    if cfg!(windows) {
        Ok(split_windows(line))
    } else {
        split_unix(line)
    }
}

/// Expands `template` for `target`.
pub fn expand(template: &str, target: &Target) -> Result<Expanded, String> {
    let words = split(template)?;
    let Some(first) = words.first() else {
        return Err("The command line is empty".into());
    };
    let program = substitute(first, target, &|v| Ok(v.to_owned()))?;
    if program.trim().is_empty() {
        return Err("The command line names no program".into());
    }
    let shell = shell_of(&program);
    let rest = &words[1..];
    let plain = |w: &String| substitute(w, target, &|v| Ok(v.to_owned()));
    let mut argv = vec![program];
    let mut raw = None;
    // Where the script starts (the word after the switch), for the shells.
    let script_at = |switch: &dyn Fn(&str) -> bool| rest.iter().position(|w| switch(w.as_str()));
    match shell {
        Shell::Direct => {
            for w in rest {
                argv.push(plain(w)?);
            }
        }
        Shell::Cmd => {
            let switch = |w: &str| w.eq_ignore_ascii_case("/c") || w.eq_ignore_ascii_case("/k");
            match script_at(&|w| switch(w)) {
                None => {
                    for w in rest {
                        argv.push(plain(w)?);
                    }
                }
                Some(_) => raw = Some(cmd_script(template, target)?),
            }
        }
        Shell::Posix => {
            let at = script_at(&|w| w == "-c");
            for (i, w) in rest.iter().enumerate() {
                argv.push(match at {
                    Some(at) if i == at + 1 => substitute(w, target, &|v| Ok(quote_posix(v)))?,
                    _ => plain(w)?,
                });
            }
        }
        Shell::Pwsh => {
            let at = script_at(&|w| {
                let w = w.to_ascii_lowercase();
                w.len() >= 2 && "-command".starts_with(&w) && w.starts_with("-c")
            });
            for (i, w) in rest.iter().enumerate() {
                argv.push(match at {
                    Some(at) if i > at => substitute(w, target, &|v| Ok(quote_powershell(v)))?,
                    _ => plain(w)?,
                });
            }
        }
    }
    Ok(Expanded {
        argv,
        raw,
        dir: target.folder.clone(),
    })
}

/// `cmd /c`: the command line after the program, for cmd.exe. Words are kept as typed (an
/// `&` stays an operator, a `"&"` a name); a word with a placeholder is replaced whole by
/// its value in double quotes; `%%` elsewhere is a percent sign.
fn cmd_script(template: &str, target: &Target) -> Result<String, String> {
    let mut parts = Vec::new();
    for (word, span) in split_windows_spans(template).into_iter().skip(1) {
        parts.push(if has_placeholder(&word) {
            let value = substitute(&word, target, &|v| check_cmd(v).map(|()| v.to_owned()))?;
            quote_cmd(&value)
        } else if word.contains('%') {
            let value = substitute(&word, target, &|v| Ok(v.to_owned()))?;
            if value.contains([' ', '\t']) {
                format!("\"{value}\"")
            } else {
                value
            }
        } else {
            template[span].to_owned()
        });
    }
    Ok(parts.join(" "))
}

fn has_placeholder(word: &str) -> bool {
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == '%' && matches!(chars.next(), Some('p' | 'n' | 's' | 'd' | 'e')) {
            return true;
        }
    }
    false
}

/// `word` with its placeholders replaced by their values passed through `quote`.
fn substitute(
    word: &str,
    t: &Target,
    quote: &dyn Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let value = match chars.next() {
            Some('%') => {
                out.push('%');
                continue;
            }
            Some('p') => &t.full,
            Some('n') => &t.name,
            Some('s') => &t.trimmed,
            Some('d') => &t.folder,
            Some('e') => &t.ext,
            Some(other) => {
                return Err(format!(
                    "%{other} is not a placeholder (%p %n %s %d %e, %% for a percent sign)"
                ))
            }
            None => return Err("A lone % ends the command line (%% for a percent sign)".into()),
        };
        out.push_str(&quote(value)?);
    }
    Ok(out)
}

/// The C runtime's split (CommandLineToArgvW): spaces separate words outside double
/// quotes; 2n backslashes before a quote are n backslashes and the quote opens or closes,
/// 2n+1 are n and a literal quote; other backslashes are themselves.
pub fn split_windows(line: &str) -> Vec<String> {
    split_windows_spans(line)
        .into_iter()
        .map(|(w, _)| w)
        .collect()
}

/// [`split_windows`], with where each word stands in `line` (as typed, quotes included).
fn split_windows_spans(line: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !started && (quoted || !matches!(c, ' ' | '\t' | '\n' | '\r')) {
            start = i;
        }
        match c {
            '\\' => {
                let mut n = 1;
                while chars.peek().map(|p| p.1) == Some('\\') {
                    chars.next();
                    n += 1;
                }
                if chars.peek().map(|p| p.1) == Some('"') {
                    word.extend(std::iter::repeat_n('\\', n / 2));
                    if n % 2 == 1 {
                        chars.next();
                        word.push('"');
                    }
                } else {
                    word.extend(std::iter::repeat_n('\\', n));
                }
                started = true;
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ' ' | '\t' | '\n' | '\r' if !quoted => {
                if started {
                    words.push((std::mem::take(&mut word), start..i));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push((word, start..line.len()));
    }
    words
}

/// A shell's split: spaces separate words; `'…'` is literal; in `"…"` a backslash escapes
/// `"`, `\`, `$` and `` ` ``; elsewhere a backslash escapes the next character.
pub fn split_unix(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("A ' quote is not closed".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("A \" quote is not closed".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("A \" quote is not closed".into()),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some('\n') | None => {}
                    Some(c) => word.push(c),
                }
            }
            ' ' | '\t' | '\n' | '\r' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// One argument for the C runtime's split (what Rust's `Command` does on Windows).
pub fn quote_msvc(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\r', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
                continue;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(c);
                continue;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// The program, as shown: in double quotes when it has spaces (Windows) or anything a
/// shell reads (elsewhere).
fn quote_program(p: &str) -> String {
    if cfg!(windows) {
        if p.contains([' ', '\t']) {
            format!("\"{p}\"")
        } else {
            p.to_owned()
        }
    } else {
        quote_posix(p)
    }
}

/// One word for a POSIX shell: as it is when it holds only safe characters, else in single
/// quotes (a `'` inside becomes `'\''`).
pub fn quote_posix(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A PowerShell single-quoted string: its quotes, the typographic ones too, doubled.
pub fn quote_powershell(s: &str) -> String {
    let mut out = String::from('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Why cmd.exe cannot be given `value` safely (it expands `%` even inside quotes, and a
/// `"` would end them).
fn check_cmd(value: &str) -> Result<(), String> {
    match value.chars().find(|c| matches!(c, '%' | '"' | '\n' | '\r')) {
        Some(c) => Err(format!(
            "{value} holds {c:?}, which cmd.exe would read as part of the command: run the \
             program directly, without cmd /c"
        )),
        None => Ok(()),
    }
}

/// A word holding a placeholder value, for cmd.exe: in double quotes (a trailing backslash
/// doubled, so a program reading the C runtime's rules keeps the quote).
fn quote_cmd(value: &str) -> String {
    let body = value.trim_end_matches('\\');
    let tail = value.len() - body.len();
    format!("\"{body}{}\"", "\\".repeat(tail * 2))
}

/// A local item picked to run on (`dir`: it is a folder).
pub type Picked = (PathBuf, bool);

/// The targets of `c` for `picked`: those its scope takes; with `recurse`, every folder
/// below a folder or drive first (deepest first, links not followed).
pub fn targets(c: &UserCleanup, picked: &[Picked]) -> Result<Vec<Target>, String> {
    let mut out = Vec::new();
    for (path, dir) in picked {
        let t = Target::of(path, *dir);
        if c.recurse && t.kind != TargetKind::File && c.folders {
            below(path, &mut out, 0)?;
        }
        if c.takes(t.kind) {
            out.push(t);
        }
        if out.len() > MAX_RUNS {
            return Err(too_many());
        }
    }
    if out.is_empty() {
        return Err(format!(
            "\"{}\" works for {}: nothing selected is one",
            c.title,
            c.scope_text()
        ));
    }
    Ok(out)
}

fn too_many() -> String {
    format!("That is more than {MAX_RUNS} runs: pick fewer folders")
}

/// The folders below `dir`, deepest first.
fn below(dir: &Path, out: &mut Vec<Target>, depth: usize) -> Result<(), String> {
    if depth > 256 {
        return Ok(());
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut subs: Vec<PathBuf> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()))
        .map(|e| e.path())
        .collect();
    subs.sort();
    for sub in subs {
        below(&sub, out, depth + 1)?;
        out.push(Target::of(&sub, true));
        if out.len() > MAX_RUNS {
            return Err(too_many());
        }
    }
    Ok(())
}

/// A cleanup ready to run: every expanded command, shown before it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    /// Its place in the profile's list (to mark it confirmed).
    pub index: usize,
    pub cleanup: UserCleanup,
    pub runs: Vec<Expanded>,
    /// Folders to refresh after.
    pub refresh: Vec<VPath>,
}

/// Expands cleanup `index` for `picked`.
pub fn prepare(index: usize, c: &UserCleanup, picked: &[Picked]) -> Result<Prepared, String> {
    if c.command.trim().is_empty() {
        return Err(format!(
            "\"{}\" has no command line (Settings → Cleanups)",
            c.title
        ));
    }
    let runs = targets(c, picked)?
        .iter()
        .map(|t| expand(&c.command, t))
        .collect::<Result<Vec<_>, _>>()?;
    let mut refresh: Vec<VPath> = Vec::new();
    for (path, dir) in picked {
        let mut add = |p: &Path| {
            let v = VPath::local(p);
            if !refresh.contains(&v) {
                refresh.push(v);
            }
        };
        if let Some(parent) = path.parent() {
            add(parent);
        }
        if *dir {
            add(path);
        }
    }
    Ok(Prepared {
        index,
        cleanup: c.clone(),
        runs,
        refresh,
    })
}

/// The process for one run (not started).
pub fn command(e: &Expanded, console: bool) -> std::process::Command {
    let mut c = std::process::Command::new(&e.argv[0]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x10;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        match &e.raw {
            Some(raw) => {
                c.raw_arg(raw);
            }
            None => {
                c.args(&e.argv[1..]);
            }
        }
        c.creation_flags(if console {
            CREATE_NEW_CONSOLE
        } else {
            CREATE_NO_WINDOW
        });
    }
    #[cfg(not(windows))]
    {
        let _ = console;
        c.args(&e.argv[1..]);
    }
    if !e.dir.is_empty() {
        c.current_dir(&e.dir);
    }
    // With a console of its own the standard handles are left alone: a windowed Keel has
    // none, and when none is set Windows gives the program its new console's.
    if !console {
        c.stdin(std::process::Stdio::null());
    }
    c
}

/// Runs a confirmed cleanup on a job's thread: one run after another (with `wait`), or all
/// started at once. Errs with what failed.
pub fn run(
    p: &Prepared,
    report: &dyn Fn(keel_vfs::Progress),
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<()> {
    use std::process::Stdio;
    use std::sync::atomic::Ordering;
    let c = &p.cleanup;
    let mut progress = keel_vfs::Progress {
        total_items: p.runs.len(),
        ..Default::default()
    };
    let mut failed: Vec<String> = Vec::new();
    for e in &p.runs {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("operation cancelled");
        }
        progress.current = e.line();
        report(progress.clone());
        let mut cmd = command(e, c.console);
        if !c.console {
            cmd.stdout(Stdio::null());
            cmd.stderr(if c.wait {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        }
        match cmd.spawn() {
            Err(err) => failed.push(format!("{}: {err}", e.argv[0])),
            Ok(child) if !c.wait => {
                // Reaped on a thread of its own; nobody waits for it.
                let _ = crate::worker::spawn("keel-cleanup-child", move || {
                    let mut child = child;
                    let _ = child.wait();
                });
            }
            Ok(child) => match child.wait_with_output() {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    let said = String::from_utf8_lossy(&out.stderr);
                    let last = said.lines().rev().find(|l| !l.trim().is_empty());
                    failed.push(match (out.status.code(), last) {
                        (Some(code), Some(l)) => format!("exit code {code}: {}", l.trim()),
                        (Some(code), None) => format!("exit code {code}: {}", e.line()),
                        (None, _) => format!("stopped: {}", e.line()),
                    });
                }
                Err(err) => failed.push(format!("{}: {err}", e.argv[0])),
            },
        }
        progress.done_items += 1;
    }
    report(progress);
    match failed.as_slice() {
        [] => Ok(()),
        [one] => anyhow::bail!("{}: {one}", c.title),
        [first, ..] => anyhow::bail!(
            "{}: {} of {} runs failed; the first: {first}",
            c.title,
            failed.len(),
            p.runs.len()
        ),
    }
}

/// The confirmation dialog's body: every expanded command line; Some(action) on Run.
pub fn confirm_ui(ui: &mut egui::Ui, p: &Prepared, cancel: &mut bool) -> Option<Action> {
    ui.strong(format!("Run \"{}\"?", p.cleanup.title));
    ui.label(match p.runs.len() {
        1 => "This command runs:".to_owned(),
        n => format!("These {n} commands run, one after another:"),
    });
    egui::ScrollArea::vertical()
        .max_height(220.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for e in p.runs.iter().take(500) {
                ui.add(egui::Label::new(egui::RichText::new(e.line()).monospace()).wrap());
            }
            if p.runs.len() > 500 {
                ui.weak(format!("and {} more", p.runs.len() - 500));
            }
        });
    if !p.cleanup.wait {
        ui.weak("They all start at once; Keel does not wait for them.");
    }
    ui.add_space(8.0);
    let mut out = None;
    ui.horizontal(|ui| {
        let run = ui.button("Run");
        crate::a11y::first_focus(&run);
        if run.clicked() {
            out = Some(Action::CleanupConfirmed(Box::new(p.clone())));
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

/// The example path the editor expands the command line for.
fn example() -> Target {
    let path = if cfg!(windows) {
        r"C:\Users\Public\My Documents\report v2.txt"
    } else {
        "/home/me/My Documents/report v2.txt"
    };
    Target::of(Path::new(path), false)
}

/// Settings → Cleanups: the list and the editor of the selected one.
pub fn settings_page(ui: &mut egui::Ui, list: &mut Vec<UserCleanup>) {
    use crate::a11y::Named as _;
    let id = egui::Id::new("keel-cleanups-selected");
    let mut selected: usize = ui.data(|d| d.get_temp(id)).unwrap_or(0);
    ui.label(
        "Commands you run on the selected files, folders or drives from the context menu \
         (Cleanups) or the command palette (\"Run cleanup\").",
    );
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("Add").clicked() {
            list.push(UserCleanup::default());
            selected = list.len() - 1;
        }
        let some = selected < list.len();
        if ui.add_enabled(some, egui::Button::new("Remove")).clicked() {
            list.remove(selected);
            selected = selected.saturating_sub(1);
        }
        if ui
            .add_enabled(some && selected > 0, egui::Button::new("Move up"))
            .clicked()
        {
            list.swap(selected, selected - 1);
            selected -= 1;
        }
        if ui
            .add_enabled(
                some && selected + 1 < list.len(),
                egui::Button::new("Move down"),
            )
            .clicked()
        {
            list.swap(selected, selected + 1);
            selected += 1;
        }
    });
    egui::ScrollArea::vertical()
        .id_salt("keel-cleanups-list")
        .max_height(120.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            if list.is_empty() {
                ui.weak("No cleanups yet");
            }
            for (i, c) in list.iter().enumerate() {
                if ui.selectable_label(i == selected, &c.title).clicked() {
                    selected = i;
                }
            }
        });
    ui.separator();
    if let Some(c) = list.get_mut(selected) {
        egui::Grid::new("keel-cleanup-editor")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Title");
                ui.add(egui::TextEdit::singleline(&mut c.title).desired_width(320.0))
                    .named("Title");
                ui.end_row();
                ui.label("Command line");
                let r = ui
                    .add(
                        egui::TextEdit::singleline(&mut c.command)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(320.0),
                    )
                    .named("Command line");
                if r.changed() {
                    c.confirmed = false;
                }
                ui.end_row();
                ui.label("");
                ui.weak("%p path  %n name  %s path without trailing \\ or /  %d folder  %e extension  %% percent");
                ui.end_row();
                ui.label("Example");
                match expand(&c.command, &example()) {
                    Ok(e) => {
                        ui.add(egui::Label::new(egui::RichText::new(e.line()).monospace()).wrap());
                    }
                    Err(why) if c.command.trim().is_empty() => {
                        ui.weak(why);
                    }
                    Err(why) => {
                        ui.colored_label(ui.visuals().error_fg_color, why);
                    }
                }
                ui.end_row();
                ui.label("Works for");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut c.files, "Files");
                    ui.checkbox(&mut c.folders, "Folders");
                    ui.checkbox(&mut c.drives, "Drives");
                });
                ui.end_row();
                ui.label("Options");
                ui.vertical(|ui| {
                    ui.checkbox(&mut c.recurse, "Recurse into subfolders")
                        .on_hover_text("Also run for every folder below, deepest first");
                    ui.checkbox(&mut c.ask, "Ask before running")
                        .on_hover_text("Show the expanded commands first (always the first time)");
                    ui.checkbox(&mut c.wait, "Wait for each to finish")
                        .on_hover_text("One after another, as a job you can cancel");
                    ui.add_enabled(c.wait, egui::Checkbox::new(&mut c.refresh, "Refresh after"));
                    if cfg!(windows) {
                        ui.checkbox(&mut c.console, "Show a console window");
                    }
                });
                ui.end_row();
            });
    }
    ui.data_mut(|d| d.insert_temp(id, selected));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(full: &str, name: &str, folder: &str, ext: &str) -> Target {
        Target {
            kind: TargetKind::File,
            full: full.into(),
            name: name.into(),
            trimmed: full.into(),
            folder: folder.into(),
            ext: ext.into(),
        }
    }

    /// Paths with spaces, quotes (legal on Unix), Unicode, shell characters.
    const TRICKY: &[&str] = &[
        "plain",
        "with space",
        "",
        "it's",
        "say \"hi\"",
        r"C:\dir\",
        r"C:\a b\c\\",
        "naïve résumé 日本語 🎉",
        "$(rm -rf x) `y` ; & | > < ^ % !",
        "tab\there",
        "back\\slash\"quote",
        "‘curly’ ‚low‛",
    ];

    #[test]
    fn msvc_quoting_round_trips() {
        for &a in TRICKY {
            let line = format!("prog {} {}", quote_msvc(a), quote_msvc("next"));
            assert_eq!(split_windows(&line), ["prog", a, "next"], "{a:?} → {line}");
        }
        assert_eq!(quote_msvc("plain"), "plain");
        assert_eq!(quote_msvc("a b"), "\"a b\"");
        assert_eq!(quote_msvc(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote_msvc(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn posix_quoting_round_trips() {
        for &a in TRICKY {
            let line = format!("prog {} next", quote_posix(a));
            assert_eq!(
                split_unix(&line).unwrap(),
                ["prog", a, "next"],
                "{a:?} → {line}"
            );
        }
        assert_eq!(quote_posix("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(quote_posix("it's"), r"'it'\''s'");
    }

    #[test]
    fn windows_split_follows_the_c_runtime() {
        assert_eq!(
            split_windows(r#"del /q "C:\My Files\%n"  x"#),
            ["del", "/q", r"C:\My Files\%n", "x"]
        );
        assert_eq!(
            split_windows(r#"a\\\"b "c\\" d\e"#),
            [r#"a\"b"#, r"c\", r"d\e"]
        );
        assert_eq!(split_windows(r#""" x"#), ["", "x"]);
        assert!(split_windows("   ").is_empty());
    }

    #[test]
    fn unix_split_reads_quotes_and_escapes() {
        assert_eq!(
            split_unix(r#"rm -- "my file" 'it''s' a\ b "\$x""#).unwrap(),
            ["rm", "--", "my file", "its", "a b", "$x"]
        );
        assert!(split_unix("echo 'open").is_err());
        assert!(split_unix("echo \"open").is_err());
    }

    #[test]
    fn placeholders_expand_inside_words() {
        let t = file("/m d/ünï.txt", "ünï.txt", "/m d", "txt");
        let words = |cmd: &str| {
            let words = split_unix(cmd).unwrap();
            words
                .iter()
                .map(|w| substitute(w, &t, &|v| Ok(v.to_owned())))
                .collect::<Result<Vec<_>, _>>()
        };
        assert_eq!(
            words("x %p %n %s %d %e 100%% %d/%n.bak").unwrap(),
            [
                "x",
                "/m d/ünï.txt",
                "ünï.txt",
                "/m d/ünï.txt",
                "/m d",
                "txt",
                "100%",
                "/m d/ünï.txt.bak"
            ]
        );
        assert!(words("x %q")
            .unwrap_err()
            .contains("%q is not a placeholder"));
        assert!(words("x 100%").unwrap_err().contains("lone %"));
    }

    #[test]
    fn values_stay_one_argument_for_a_program() {
        for &v in TRICKY.iter().filter(|v| !v.is_empty()) {
            let t = file(v, v, "/d", "");
            let e = expand("tool --path %p --name=%n", &t).unwrap();
            assert_eq!(e.argv[1..], ["--path", v, &format!("--name={v}")], "{v:?}");
            assert_eq!(e.raw, None);
            assert_eq!(e.dir, "/d");
        }
    }

    #[test]
    fn a_posix_shell_script_gets_quoted_values() {
        for &v in TRICKY {
            let t = file(v, "n", "/d", "");
            let e = expand(r#"sh -c "rm -- %p && echo done" keel"#, &t).unwrap();
            assert_eq!(e.argv[..2], ["sh", "-c"]);
            assert_eq!(e.argv[3], "keel");
            // The shell reads the script back into the value, whatever it holds.
            let script = split_unix(&e.argv[2]).unwrap();
            assert_eq!(script, ["rm", "--", v, "&&", "echo", "done"], "{v:?}");
        }
    }

    #[test]
    fn powershell_gets_single_quoted_strings() {
        let t = file("C:\\it's ‘x’\\a $b.txt", "n", "C:\\", "txt");
        let e = expand("pwsh -NoProfile -Command Remove-Item -LiteralPath %p", &t).unwrap();
        assert_eq!(
            e.argv,
            [
                "pwsh",
                "-NoProfile",
                "-Command",
                "Remove-Item",
                "-LiteralPath",
                "'C:\\it''s ‘‘x’’\\a $b.txt'"
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn cmd_scripts_quote_whole_words_and_refuse_percent() {
        let t = file(r"C:\a b&c\x (1).txt", "x (1).txt", r"C:\a b&c", "txt");
        let e = expand(r#"cmd /d /c del /q "%p" & echo %%x %d\*.bak"#, &t).unwrap();
        assert_eq!(e.argv, ["cmd"]);
        assert_eq!(
            e.raw.as_deref(),
            Some(r#"/d /c del /q "C:\a b&c\x (1).txt" & echo %x "C:\a b&c\*.bak""#)
        );
        let drive = Target::of(Path::new(r"C:\"), true);
        let e = expand("cmd /c dir %p", &drive).unwrap();
        assert_eq!(e.raw.as_deref(), Some(r#"/c dir "C:\\""#));
        let pct = file(r"C:\100% done.txt", "n", r"C:\", "txt");
        assert!(expand("cmd /c type %p", &pct)
            .unwrap_err()
            .contains("cmd.exe"));
        // Run directly, the same path is fine.
        assert!(expand("notepad %p", &pct).is_ok());
    }

    #[test]
    fn shells_are_known_by_file_name() {
        assert_eq!(shell_of("/bin/bash"), Shell::Posix);
        assert_eq!(
            shell_of(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Shell::Pwsh
        );
        assert_eq!(shell_of("git"), Shell::Direct);
        assert_eq!(shell_of("CMD.EXE") == Shell::Cmd, cfg!(windows));
    }

    #[test]
    fn targets_follow_scope_and_recurse_deepest_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("top dir");
        std::fs::create_dir_all(root.join("a/deep")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/f.txt"), "x").unwrap();
        let picked: Vec<Picked> = vec![(root.clone(), true), (root.join("a/f.txt"), false)];
        let folders_only = UserCleanup {
            files: false,
            recurse: true,
            command: "x".into(),
            ..UserCleanup::default()
        };
        let got: Vec<String> = targets(&folders_only, &picked)
            .unwrap()
            .iter()
            .map(|t| t.full.clone())
            .collect();
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        assert_eq!(
            got,
            [
                s(root.join("a").join("deep")),
                s(root.join("a")),
                s(root.join("b")),
                s(root.clone())
            ]
        );
        let files_only = UserCleanup {
            files: true,
            folders: false,
            ..folders_only.clone()
        };
        let got = targets(&files_only, &picked).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].ext, "txt");
        assert_eq!(got[0].folder, s(root.join("a")));
        let drives_only = UserCleanup {
            files: false,
            folders: false,
            drives: true,
            ..folders_only
        };
        assert!(targets(&drives_only, &picked)
            .unwrap_err()
            .contains("drives"));
    }

    #[test]
    fn cleanups_are_kept_in_the_profile_and_a_bad_one_drops_alone() {
        let c = UserCleanup {
            title: "Delete .bak".into(),
            command: r#"cmd /c del /q "%d\*.bak""#.into(),
            recurse: true,
            confirmed: true,
            ..UserCleanup::default()
        };
        let s = crate::settings::Settings {
            cleanups: vec![c.clone()],
            ..Default::default()
        };
        let text = toml::to_string(&s).unwrap();
        assert!(text.contains("[[cleanups]]"), "{text}");
        let (back, dropped) = crate::settings::parse_lenient(&text).unwrap();
        assert_eq!(back.cleanups, [c]);
        assert!(dropped.is_empty());
        let text = "[[cleanups]]\ntitle = 'ok'\n[[cleanups]]\ntitle = 5\n";
        let (back, dropped) = crate::settings::parse_lenient(text).unwrap();
        assert_eq!(back.cleanups.len(), 1);
        assert_eq!(back.cleanups[0].title, "ok");
        assert!(back.cleanups[0].ask, "missing keys take defaults");
        assert_eq!(dropped, ["[[cleanups]] #2"]);
    }

    #[test]
    fn a_drive_trims_its_separator() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        let t = Target::of(Path::new(root), true);
        assert_eq!(t.kind, TargetKind::Drive);
        assert_eq!(t.full, root);
        if cfg!(windows) {
            assert_eq!((t.trimmed.as_str(), t.name.as_str()), ("C:", "C:"));
        }
        assert_eq!(t.folder, root);
        assert_eq!(t.ext, "");
    }

    #[test]
    fn prepare_needs_a_command_and_lists_folders_to_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.log");
        std::fs::write(&f, "x").unwrap();
        let mut c = UserCleanup::default();
        assert!(prepare(0, &c, &[(f.clone(), false)])
            .unwrap_err()
            .contains("no command"));
        c.command = if cfg!(windows) {
            "cmd /c del %p"
        } else {
            "rm -- %p"
        }
        .into();
        let p = prepare(3, &c, &[(f.clone(), false)]).unwrap();
        assert_eq!(p.index, 3);
        assert_eq!(p.runs.len(), 1);
        assert_eq!(p.refresh, [VPath::local(dir.path())]);
        assert!(p.runs[0].line().contains("x.log"));
    }

    /// Runs a real command on a temp file whose name needs quoting; `wait` collects a
    /// failing exit code.
    #[test]
    fn runs_with_awkward_names_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("it's a (test) & ünï.txt");
        std::fs::write(&f, "x").unwrap();
        let c = UserCleanup {
            title: "Remove".into(),
            command: if cfg!(windows) {
                "cmd /c del /q %p".into()
            } else {
                "sh -c 'rm -- %p'".into()
            },
            ..UserCleanup::default()
        };
        let p = prepare(0, &c, &[(f.clone(), false)]).unwrap();
        let no = std::sync::atomic::AtomicBool::new(false);
        run(&p, &|_| {}, &no).unwrap();
        assert!(!f.exists(), "{}", p.runs[0].line());
        let failing = UserCleanup {
            command: if cfg!(windows) {
                "cmd /c exit 3".into()
            } else {
                "sh -c 'exit 3'".into()
            },
            ..c
        };
        let p = prepare(0, &failing, &[(dir.path().to_owned(), true)]).unwrap();
        let err = run(&p, &|_| {}, &no).unwrap_err();
        assert!(format!("{err:#}").contains("exit code 3"), "{err:#}");
        let stop = std::sync::atomic::AtomicBool::new(true);
        assert!(crate::jobs::is_cancel(
            &run(&p, &|_| {}, &stop).unwrap_err()
        ));
    }
}
