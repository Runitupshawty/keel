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
//! not through a shell. When a shell or interpreter reads a script, the values in it are
//! quoted for it or refused; programs that only start another (sudo, doas, env, nice, time,
//! xargs, find -exec, wsl -e, …) are looked through, the first shell or interpreter named
//! deciding:
//! - `cmd /c …` (Windows) becomes `cmd [switches] /d /v:off /s /c "<script>"`: AutoRun and
//!   delayed expansion off (with `/v:on` typed, a `!` in a value is refused), and the
//!   script in one more pair of quotes that `/s` makes cmd strip exactly. Every word holding
//!   a placeholder is wrapped in double quotes; values with `%` or `"` are refused (cmd
//!   would expand or end them).
//! - POSIX shells (`sh`, `bash`, `zsh`, `dash`, `ksh`, …) with a `-c` switch, also in a
//!   cluster (`-lc`, `-ec`, `-xc`): the script gets each value single-quoted.
//! - `powershell` / `pwsh`: after `-Command` (and, for Windows PowerShell, from the first
//!   word that is not a switch, its default) each value becomes a single-quoted string with
//!   its quotes (also the typographic ones) doubled; `-File` and pwsh's default take plain
//!   arguments; `-EncodedCommand` and the values of PowerShell's own switches take none.
//! - fish, csh and other shells, `python -c`, `perl -e`, `node -e`, `ruby -e`, `php -r`,
//!   awk programs and the like, and programs that join their arguments into a command line
//!   for a shell (`ssh`, `watch`, `su`, `wsl` without `-e`): values inside their scripts
//!   are refused (pass them as arguments after the script instead).
//!
//! Inside those scripts placeholders go without quotes of their own: one inside the
//! script's quotes is refused.

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

/// A program's file name, lowercase, without `.exe`.
fn stem(program: &str) -> String {
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    match name.strip_suffix(".exe") {
        Some(s) => s.to_owned(),
        None => name,
    }
}

/// The shell `program` is (by its file name).
pub fn shell_of(program: &str) -> Shell {
    match stem(program).as_str() {
        "cmd" if cfg!(windows) => Shell::Cmd,
        s if POSIX_SHELLS.contains(&s) => Shell::Posix,
        "powershell" | "pwsh" | "powershell_ise" => Shell::Pwsh,
        _ => Shell::Direct,
    }
}

/// Shells whose `-c` script Keel quotes values for (POSIX single quotes).
const POSIX_SHELLS: &[&str] = &[
    "sh", "bash", "rbash", "zsh", "dash", "ksh", "ksh93", "mksh", "pdksh", "lksh", "ash",
    "yash", "posh", "busybox-sh",
];

/// Shells with other quoting rules: values are refused in their `-c` script.
const OTHER_SHELLS: &[&str] = &["fish", "csh", "tcsh", "nu", "elvish", "xonsh", "rc", "es"];

/// Interpreters that take a script on the command line, with the switches that give it.
/// Values are refused inside such a script (pass them as arguments after it instead).
const INLINE: &[(&str, &[&str])] = &[
    ("python", &["-c"]),
    ("python2", &["-c"]),
    ("python3", &["-c"]),
    ("py", &["-c"]),
    ("pypy", &["-c"]),
    ("pypy3", &["-c"]),
    ("perl", &["-e", "-E"]),
    ("ruby", &["-e"]),
    ("node", &["-e", "-p", "--eval", "--print"]),
    ("nodejs", &["-e", "-p", "--eval", "--print"]),
    ("bun", &["-e", "-p", "--eval", "--print"]),
    ("deno", &["eval"]),
    ("php", &["-r"]),
    ("lua", &["-e"]),
    ("luajit", &["-e"]),
    ("osascript", &["-e"]),
    ("rscript", &["-e"]),
    ("tclsh", &["-c"]),
];

/// awk and its kin: the first operand is the program (unless `-f` names a file).
const AWKS: &[&str] = &["awk", "gawk", "mawk", "nawk"];

/// Programs that join their arguments into one command line for a shell (here or on
/// another computer): values after them are refused, as Keel cannot know that shell.
const JOINERS: &[&str] = &["watch", "ssh", "parallel", "su", "runuser", "script", "wsl"];

/// How the values of one word's placeholders are put in.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Put {
    /// As they are: the word is one argument.
    Plain,
    /// The word is a POSIX shell script: single-quoted.
    Posix,
    /// The word is (part of) a PowerShell script: single-quoted PowerShell strings.
    Pwsh,
    /// No value may go in: why.
    Refuse(String),
}

/// How each word of a command line (the program first, already expanded) takes values.
/// Programs that start another (sudo, doas, env, nice, time, xargs, find -exec, …) are
/// looked through: the first word naming a shell or interpreter decides how what follows
/// it is read; everything before it stays plain arguments.
fn puts(words: &[String]) -> Vec<Put> {
    let mut put = vec![Put::Plain; words.len()];
    let mut i = 0;
    while i < words.len() {
        let name = stem(&words[i]);
        let name = name.as_str();
        if name == "cmd" && cfg!(windows) {
            if i > 0 {
                refuse_after(&mut put, i, "cmd started by another program");
            }
            return put;
        }
        if POSIX_SHELLS.contains(&name) || OTHER_SHELLS.contains(&name) {
            if let Some(at) = posix_script(words, i) {
                put[at] = if POSIX_SHELLS.contains(&name) {
                    Put::Posix
                } else {
                    Put::Refuse(format!(
                        "{name} quotes differently from a POSIX shell: pass the value as an \
                         argument after the script"
                    ))
                };
            }
            return put;
        }
        if matches!(name, "powershell" | "pwsh" | "powershell_ise") {
            powershell(words, i, name != "pwsh", &mut put);
            return put;
        }
        if let Some((_, switches)) = INLINE
            .iter()
            .find(|(n, _)| *n == name || (n.starts_with("python") && name.starts_with(n)))
        {
            inline_script(words, i, switches, &mut put);
            return put;
        }
        if AWKS.contains(&name) {
            awk_program(words, i, &mut put);
            return put;
        }
        if name == "env" {
            // `env -S "…"` splits its string into a command line.
            for (j, w) in words.iter().enumerate().skip(i + 1) {
                if w == "-S" || w.starts_with("-S") || w.starts_with("--split-string") {
                    let at = if w == "-S" { j + 1 } else { j };
                    if at < put.len() {
                        put[at] = Put::Refuse("env -S splits it into a command line".into());
                    }
                }
            }
        }
        if name == "flock" {
            // `flock … -c "…"` runs the string through sh -c.
            if let Some(j) = (i + 1..words.len()).find(|&j| words[j] == "-c" || words[j] == "--command") {
                if j + 1 < put.len() {
                    put[j + 1] = Put::Posix;
                }
                return put;
            }
        }
        if JOINERS.contains(&name) {
            if name == "wsl" {
                // `wsl -e` / `--exec` runs the rest directly: keep looking after it.
                if let Some(j) = (i + 1..words.len()).find(|&j| words[j] == "-e" || words[j] == "--exec") {
                    i = j + 1;
                    continue;
                }
            }
            refuse_after(&mut put, i, &format!("{name} joins its arguments into a command line for a shell"));
            return put;
        }
        i += 1;
    }
    put
}

/// Values after word `i` are refused.
fn refuse_after(put: &mut [Put], i: usize, why: &str) {
    for p in put.iter_mut().skip(i + 1) {
        *p = Put::Refuse(format!(
            "{why}: Keel cannot quote a value for it (run the program directly)"
        ));
    }
}

/// The script word of a POSIX-style shell at `i`: the first operand after its options
/// when one of them is a short-option cluster holding `c` (`-c`, `-lc`, `-ec`, `-xc`).
fn posix_script(words: &[String], i: usize) -> Option<usize> {
    let mut script = false;
    let mut j = i + 1;
    while j < words.len() {
        let w = words[j].as_str();
        if w == "--" || w == "-" {
            j += 1;
            break;
        }
        if w.len() < 2 || !(w.starts_with('-') || w.starts_with('+')) {
            break;
        }
        if w.starts_with("--") {
            // Long options; two take a value.
            j += if matches!(w, "--rcfile" | "--init-file") { 2 } else { 1 };
            continue;
        }
        let letters = &w[1..];
        if w.starts_with('-') && letters.contains('c') {
            script = true;
        }
        // `-o option`, `+o option`, `-O shopt`: the next word is the option's name.
        j += if letters.ends_with(['o', 'O']) { 2 } else { 1 };
    }
    (script && j < words.len()).then_some(j)
}

/// PowerShell at `i`: `-Command` (and Windows PowerShell's default when the first word is
/// not a switch) makes every later word script; `-EncodedCommand` takes no values; `-File`
/// and `pwsh`'s default make the rest arguments. Switches start with `-` or `/`; a value
/// given to one of PowerShell's own switches takes no placeholders (so a switch Keel takes
/// for one with a value never leaves the word after it unquoted).
fn powershell(words: &[String], i: usize, windows_powershell: bool, put: &mut [Put]) {
    const VALUED: &[&str] = &[
        "executionpolicy",
        "windowstyle",
        "version",
        "inputformat",
        "outputformat",
        "psconsolefile",
        "configurationname",
        "workingdirectory",
        "settingsfile",
        "configurationfile",
        "custompipename",
    ];
    fn set_from(put: &mut [Put], j: usize, p: &Put) {
        for q in put.iter_mut().skip(j) {
            *q = p.clone();
        }
    }
    let switch = Put::Refuse("a PowerShell switch takes no placeholders".into());
    set_from(put, i + 1, &switch);
    let mut j = i + 1;
    while j < words.len() {
        let w = words[j].as_str();
        let Some(name) = w.strip_prefix(['-', '/']).filter(|n| !n.is_empty()) else {
            // The first word that is not a switch: Windows PowerShell's -Command, pwsh's
            // -File.
            let rest = if windows_powershell {
                Put::Pwsh
            } else {
                Put::Plain
            };
            set_from(put, j, &rest);
            return;
        };
        let name = name.trim_start_matches('-').to_ascii_lowercase();
        let is = |full: &str, min: usize| name.len() >= min && full.starts_with(name.as_str());
        if name == "cwa" || (name.len() > "command".len() && is("commandwithargs", 8)) {
            // The script, then its arguments.
            set_from(put, j + 1, &Put::Plain);
            if let Some(p) = put.get_mut(j + 1) {
                *p = Put::Pwsh;
            }
            return;
        }
        if is("command", 1) {
            set_from(put, j + 1, &Put::Pwsh);
            return;
        }
        if name == "e" || name == "ec" || is("encodedcommand", 2) {
            let why = "an encoded PowerShell command takes no values: use -Command";
            set_from(put, j + 1, &Put::Refuse(why.into()));
            return;
        }
        if is("file", 1) {
            // The script file, then its arguments.
            set_from(put, j + 1, &Put::Plain);
            return;
        }
        let valued = matches!(name.as_str(), "ep" | "ex" | "wd")
            || VALUED
                .iter()
                .any(|v| is(v, 2) || (name.len() == 1 && v.starts_with(name.as_str())));
        j += if valued { 2 } else { 1 };
    }
}

/// An interpreter at `i` whose script comes after one of `switches` (or glued to a short
/// one: `-c'…'`): values are refused in that script.
fn inline_script(words: &[String], i: usize, switches: &[&str], put: &mut [Put]) {
    let why = |name: &str| {
        Put::Refuse(format!(
            "a value inside a {name} script could run as code: pass it as an argument after \
             the script (sys.argv, ARGV, process.argv…)"
        ))
    };
    let name = stem(&words[i]);
    for j in i + 1..words.len() {
        let w = words[j].as_str();
        if switches.contains(&w) {
            if j + 1 < put.len() {
                put[j + 1] = why(&name);
            }
            return;
        }
        let glued = switches
            .iter()
            .any(|s| s.len() == 2 && s.starts_with('-') && w.len() > 2 && w.starts_with(s));
        if glued {
            put[j] = why(&name);
            return;
        }
    }
}

/// awk at `i`: its first operand is the program unless `-f` gives a file.
fn awk_program(words: &[String], i: usize, put: &mut [Put]) {
    let mut j = i + 1;
    while j < words.len() {
        let w = words[j].as_str();
        if w == "--" {
            j += 1;
            break;
        }
        if w == "-f" || w.starts_with("-f") || w.starts_with("--file") {
            return;
        }
        if matches!(w, "-F" | "-v") {
            j += 2;
            continue;
        }
        if w.len() > 1 && w.starts_with('-') {
            j += 1;
            continue;
        }
        break;
    }
    if j < put.len() {
        put[j] = Put::Refuse(
            "a value inside an awk program could run as code: pass it as an operand or with \
             -v instead"
                .into(),
        );
    }
}

/// Where a placeholder of `word` stands inside quotes of the script language: there the
/// quoted value would itself be inside quotes (a POSIX `"…"` or a PowerShell `"…"` still
/// expands `$(…)`), so it is refused. `pwsh`: PowerShell's quotes (typographic too, and
/// `''` / `` ` `` escapes); else POSIX quotes.
fn placeholder_in_quotes(word: &str, pwsh: bool) -> bool {
    let (mut single, mut double) = (false, false);
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '%' if !single && !double => {
                chars.next(); // `%%` or a placeholder outside quotes
            }
            '%' => {
                if matches!(chars.peek(), Some('p' | 'n' | 's' | 'd' | 'e')) {
                    return true;
                }
                chars.next();
            }
            '\\' if !pwsh && !single => {
                chars.next_if(|&c| c != '%');
            }
            '`' if pwsh && !single => {
                chars.next_if(|&c| c != '%');
            }
            '\'' if !double => single = !single,
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' if pwsh && !double => {
                single = !single
            }
            '"' if !single => double = !double,
            '\u{201C}' | '\u{201D}' | '\u{201E}' if pwsh && !single => double = !double,
            _ => {}
        }
    }
    false
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
    if shell_of(&program) == Shell::Cmd {
        return Ok(Expanded {
            argv: vec![program],
            raw: Some(cmd_script(template, target)?),
            dir: target.folder.clone(),
        });
    }
    let mut named = words.clone();
    named[0] = program.clone();
    let mut argv = vec![program];
    for (w, put) in words.iter().zip(puts(&named)).skip(1) {
        let pwsh = put == Put::Pwsh;
        argv.push(match put {
            Put::Plain => substitute(w, target, &|v| Ok(v.to_owned()))?,
            _ if !has_placeholder(w) => substitute(w, target, &|v| Ok(v.to_owned()))?,
            Put::Refuse(why) => return Err(format!("{w}: {why}")),
            Put::Posix | Put::Pwsh if placeholder_in_quotes(w, pwsh) => {
                return Err(format!(
                    "{w}: put the placeholder outside quotes in the script; Keel quotes the \
                     value itself"
                ))
            }
            Put::Posix => substitute(w, target, &|v| Ok(quote_posix(v)))?,
            Put::Pwsh => substitute(w, target, &|v| Ok(quote_powershell(v)))?,
        });
    }
    Ok(Expanded {
        argv,
        raw: None,
        dir: target.folder.clone(),
    })
}

/// Whether `w` is cmd.exe's script switch (`/c`, `/k`, or `/r`, which is `/c`), maybe with
/// text glued to it.
fn cmd_switch(w: &str) -> bool {
    let w = w.to_ascii_lowercase();
    ["/c", "/k", "/r"].iter().any(|s| w.starts_with(s))
}

/// `cmd /c`: the command line after the program, for cmd.exe. The switches typed before
/// `/c` stay, then `/d` (no AutoRun), `/v:off` (unless `/v:on` was typed: then a `!` in a
/// value is refused), `/s` and the script switch, and the script in one more pair of
/// double quotes, which `/s` makes cmd strip exactly (without `/s` it strips the first and
/// last quote only sometimes, which unquotes a value starting the script). Script words
/// are kept as typed (an `&` stays an operator, a `"&"` a name); a word with a placeholder
/// is replaced whole by its value in double quotes; `%%` elsewhere is a percent sign.
fn cmd_script(template: &str, target: &Target) -> Result<String, String> {
    let words: Vec<_> = split_windows_spans(template).into_iter().skip(1).collect();
    let Some(at) = words.iter().position(|(w, _)| cmd_switch(w)) else {
        if words.iter().any(|(w, _)| has_placeholder(w)) {
            return Err("cmd without /c or /k gets no values: write cmd /c <command>".into());
        }
        return Ok(words
            .iter()
            .map(|(_, span)| template[span.clone()].to_owned())
            .collect::<Vec<_>>()
            .join(" "));
    };
    let (switch, _) = &words[at];
    if switch.len() != 2 {
        return Err(format!(
            "{switch}: put a space after {} (cmd /c <command>)",
            &switch[..2]
        ));
    }
    let (mut delayed, mut autorun_off, mut strip) = (false, false, false);
    let mut line = String::new();
    for (w, span) in &words[..at] {
        if has_placeholder(w) || w.contains('%') {
            return Err(format!("{w}: placeholders go after {switch}"));
        }
        let lower = w.to_ascii_lowercase();
        if lower.starts_with("/v") {
            delayed = lower != "/v:off";
        }
        autorun_off |= lower == "/d";
        strip |= lower == "/s";
        line += &template[span.clone()];
        line.push(' ');
    }
    if !autorun_off {
        line += "/d ";
    }
    if !delayed {
        line += "/v:off ";
    }
    if !strip {
        line += "/s ";
    }
    line += &format!("{switch} \"");
    let mut parts = Vec::new();
    for (word, span) in &words[at + 1..] {
        parts.push(if has_placeholder(word) {
            let value = substitute(word, target, &|v| check_cmd(v, delayed).map(|()| v.to_owned()))?;
            quote_cmd(&value)
        } else if word.contains('%') {
            let value = substitute(word, target, &|v| Ok(v.to_owned()))?;
            if value.contains([' ', '\t']) {
                format!("\"{value}\"")
            } else {
                value
            }
        } else {
            template[span.clone()].to_owned()
        });
    }
    line += &parts.join(" ");
    line.push('"');
    Ok(line)
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

/// Why cmd.exe cannot be given `value` safely (it expands `%` even inside quotes, a `"`
/// would end them, and with delayed expansion on (`delayed`) it expands `!…!` too).
fn check_cmd(value: &str, delayed: bool) -> Result<(), String> {
    let bad = |c: char| matches!(c, '%' | '"' | '\n' | '\r') || (delayed && c == '!');
    match value.chars().find(|&c| bad(c)) {
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
        // The script in one more pair of quotes, which /s makes cmd strip exactly.
        assert_eq!(
            e.raw.as_deref(),
            Some(r#"/d /v:off /s /c "del /q "C:\a b&c\x (1).txt" & echo %x "C:\a b&c\*.bak"""#)
        );
        let drive = Target::of(Path::new(r"C:\"), true);
        let e = expand("cmd /c dir %p", &drive).unwrap();
        assert_eq!(e.raw.as_deref(), Some(r#"/d /v:off /s /c "dir "C:\\"""#));
        let pct = file(r"C:\100% done.txt", "n", r"C:\", "txt");
        assert!(expand("cmd /c type %p", &pct)
            .unwrap_err()
            .contains("cmd.exe"));
        // Run directly, the same path is fine.
        assert!(expand("notepad %p", &pct).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn cmd_scripts_that_start_with_a_quote_keep_it() {
        let t = file(
            r"C:\d\x & calc & (1) ^ y.txt",
            "x & calc & (1) ^ y.txt",
            r"C:\d",
            "txt",
        );
        // "Open with the associated app": the value starts the script.
        let e = expand("cmd /c %p", &t).unwrap();
        assert_eq!(
            e.raw.as_deref(),
            Some(r#"/d /v:off /s /c ""C:\d\x & calc & (1) ^ y.txt"""#)
        );
        let e = expand(r#"cmd /c "%p""#, &t).unwrap();
        assert_eq!(
            e.raw.as_deref(),
            Some(r#"/d /v:off /s /c ""C:\d\x & calc & (1) ^ y.txt"""#)
        );
        // A quoted program first.
        let e = expand(r#"cmd /C "C:\Program Files\Tool\tool.exe" %p"#, &t).unwrap();
        assert_eq!(
            e.raw.as_deref(),
            Some(r#"/d /v:off /s /C ""C:\Program Files\Tool\tool.exe" "C:\d\x & calc & (1) ^ y.txt"""#)
        );
        // `!` is plain text with delayed expansion off; refused once /v:on is typed.
        let bang = file(r"C:\d\Report!x!final.txt", "n", r"C:\d", "txt");
        let e = expand("cmd /c del %p", &bang).unwrap();
        assert!(e.raw.unwrap().contains("/v:off"));
        let e = expand("cmd /v:on /c del %p", &bang).unwrap_err();
        assert!(e.contains('!'), "{e}");
        let e = expand("cmd /V:ON /c echo hi", &bang).unwrap();
        assert_eq!(e.raw.as_deref(), Some(r#"/V:ON /d /s /c "echo hi""#));
        // A switch glued to the script, or no script switch at all: refused.
        assert!(expand(r#"cmd /c"%p""#, &t).unwrap_err().contains("space"));
        assert!(expand("cmd %p", &t).unwrap_err().contains("/c"));
        assert!(expand("cmd /q %p /c dir", &t).is_err());
    }

    /// The scripts above, run by cmd.exe on names holding `&`, `(`, `)`, `^` and spaces.
    #[cfg(windows)]
    #[test]
    fn cmd_runs_names_with_operators_as_names() {
        let dir = tempfile::tempdir().unwrap();
        let no = std::sync::atomic::AtomicBool::new(false);
        let cleanup = |command: &str| UserCleanup {
            title: "t".into(),
            command: command.into(),
            ..UserCleanup::default()
        };
        // A batch file run by its name (the value starts the script).
        let script = dir.path().join("run me & mkdir hacked & (1) ^ x.cmd");
        std::fs::write(&script, "@mkdir \"%~dp0ran\"\r\n").unwrap();
        let p = prepare(0, &cleanup("cmd /c %p"), &[(script.clone(), false)]).unwrap();
        run(&p, &|_| {}, &no).unwrap();
        assert!(dir.path().join("ran").is_dir(), "{}", p.runs[0].line());
        assert!(!dir.path().join("hacked").exists(), "{}", p.runs[0].line());
        // A quoted program first, the value after it.
        let tool = dir.path().join("copy it.cmd");
        std::fs::write(&tool, "@copy /y \"%~1\" \"%~dp0copied.txt\" >nul\r\n").unwrap();
        let data = dir.path().join("a & mkdir hacked2 & (1) ^ b.txt");
        std::fs::write(&data, "payload").unwrap();
        let p = prepare(
            0,
            &cleanup(r#"cmd /c "%d\copy it.cmd" %p"#),
            &[(data, false)],
        )
        .unwrap();
        run(&p, &|_| {}, &no).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("copied.txt")).unwrap(),
            "payload",
            "{}",
            p.runs[0].line()
        );
        assert!(!dir.path().join("hacked2").exists());
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

    /// The script word of `template` read back by a POSIX shell, for each tricky value.
    fn posix_script_reads_back(template: &str, at: usize) {
        for &v in TRICKY {
            let t = file(v, "n", "/d", "");
            let e = expand(template, &t).unwrap_or_else(|e| panic!("{template}: {e}"));
            let script = split_unix(&e.argv[at]).unwrap();
            assert!(script.iter().any(|w| w == v), "{template} {v:?}: {script:?}");
        }
    }

    #[test]
    fn shell_switch_clusters_and_wrappers_still_get_quoted_values() {
        for (template, at) in [
            (r#"bash -lc "trash %p""#, 2),
            (r#"sh -ec "rm -- %p""#, 2),
            (r#"bash -xc "rm -- %p""#, 2),
            (r#"bash -o pipefail -c "rm -- %p""#, 4),
            (r#"bash --norc -c "rm -- %p""#, 3),
            (r#"sudo sh -c "rm -- %p""#, 3),
            (r#"sudo -u root -- bash -c "rm -- %p""#, 6),
            (r#"doas sh -c "rm -- %p""#, 3),
            (r#"env LANG=C bash -c "rm -- %p""#, 4),
            (r#"nice -n 5 sh -c "rm -- %p""#, 5),
            (r#"time sh -c "rm -- %p""#, 3),
            (r#"xargs -0 sh -c "rm -- %p""#, 4),
            (r#"wsl -e sh -c "rm -- %p""#, 4),
            (r#"/usr/bin/zsh -c "rm -- %p""#, 2),
            (r#"flock /tmp/lock -c "rm -- %p""#, 3),
        ] {
            posix_script_reads_back(template, at);
        }
        // Values after the script are arguments: plain.
        let t = file("$(x) y", "n", "/d", "");
        let e = expand(r#"sh -c "rm -- \"$1\"" sh %p"#, &t).unwrap();
        assert_eq!(e.argv[4], "$(x) y");
        let e = expand("bash script.sh %p", &t).unwrap();
        assert_eq!(e.argv[2], "$(x) y");
        // A placeholder inside the script's own quotes would be quoted twice: refused.
        let e = expand(r#"sh -c "rm \"%p\"""#, &t).unwrap_err();
        assert!(e.contains("outside quotes"), "{e}");
    }

    #[test]
    fn scripts_of_other_interpreters_take_no_values() {
        let t = file("x');import os;os.system('id", "n", "/d", "");
        for template in [
            r#"python3 -c "import os; os.remove('%p')""#,
            r#"python -c "print(1)%p""#,
            r#"perl -e "unlink '%p'""#,
            r#"node -e "require('fs').rmSync('%p')""#,
            r#"ruby -e "File.delete('%p')""#,
            r#"php -r "unlink('%p');""#,
            r#"awk "BEGIN { system(\"rm %p\") }""#,
            r#"fish -c "rm %p""#,
            r#"sudo fish -c "rm %p""#,
            "ssh host rm %p",
            "watch rm %p",
            "wsl rm %p",
            r#"env -S "rm %p""#,
            "su root -c %p",
        ] {
            assert!(expand(template, &t).is_err(), "{template}");
        }
        // As arguments after the script they are fine.
        for template in [
            r#"python3 -c "import os,sys; os.remove(sys.argv[1])" %p"#,
            r#"perl -e "unlink $ARGV[0]" %p"#,
            r#"awk "{ print }" %p"#,
            "wsl -e rm -- %p",
            "rm -- %p",
            "grep -l sh %p",
        ] {
            let e = expand(template, &t).unwrap_or_else(|e| panic!("{template}: {e}"));
            assert_eq!(e.argv.last().unwrap(), &t.full, "{template}");
        }
    }

    #[test]
    fn powershell_without_command_still_gets_quoted_values() {
        let t = file("C:\\d\\a;Stop-Computer;$(x).txt", "n", "C:\\d", "txt");
        let quoted = quote_powershell(&t.full);
        // Windows PowerShell's default is -Command; `/` switches too.
        for template in [
            "powershell Remove-Item -LiteralPath %p",
            "powershell.exe -NoProfile -ExecutionPolicy Bypass Remove-Item -LiteralPath %p",
            "powershell /Command Remove-Item -LiteralPath %p",
            "powershell /c Remove-Item -LiteralPath %p",
            "powershell -nop -w hidden -c Remove-Item -LiteralPath %p",
            "pwsh -c Remove-Item -LiteralPath %p",
            "sudo pwsh -Command Remove-Item -LiteralPath %p",
        ] {
            let e = expand(template, &t).unwrap_or_else(|e| panic!("{template}: {e}"));
            assert_eq!(e.argv.last().unwrap(), &quoted, "{template}");
        }
        // A script file and its arguments: plain (pwsh's default is -File).
        for template in [
            "pwsh -File clean.ps1 %p",
            "pwsh clean.ps1 %p",
            "powershell -File clean.ps1 %p",
            r#"pwsh -cwa "Remove-Item -LiteralPath $args[0]" %p"#,
        ] {
            let e = expand(template, &t).unwrap_or_else(|e| panic!("{template}: {e}"));
            assert_eq!(e.argv.last().unwrap(), &t.full, "{template}");
        }
        // No values in an encoded command, in a switch's value, or inside the script's
        // own quotes.
        for template in [
            "powershell -EncodedCommand AAAA %p",
            "powershell -ec AAAA %p",
            "pwsh -WorkingDirectory %d -Command Get-ChildItem",
            r#"pwsh -Command "Remove-Item \"%p\"""#,
            r#"pwsh -Command "Remove-Item '%p'""#,
        ] {
            assert!(expand(template, &t).is_err(), "{template}");
        }
    }
}
