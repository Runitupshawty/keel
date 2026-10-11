//! Crash reports for keel and keel-daemon: a panic writes `<data dir>/crashes/<time>.txt`
//! (UTC, owner-only) with the version, whether the binary is signed, the OS, the panic message and location, a backtrace
//! and the last [`LOG_LINES`] log lines ([`tee`]). The text is [`scrub`]bed first: the home
//! folder becomes `~`; user names in other users' folders, profile names, e-mail addresses,
//! URL user names and hosts and network servers become placeholders; Rust source paths keep
//! only `<crate>/src/<file>`. Other file and folder names stay. At most [`KEEP`] files are
//! kept. Nothing is sent anywhere: the window offers the newest unseen report at the next
//! start ([`unseen`]) for the user to read, then copy or attach. A panic the caller catches
//! on purpose ([`quiet`]) is only logged.

pub use keel_vfs::quiet_panic::quiet;
use regex::Regex;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};

/// Crash files kept; older ones are deleted when a new one is written.
pub const KEEP: usize = 10;
/// Log lines kept in memory for the next report.
pub const LOG_LINES: usize = 50;
/// Panics written per run; a panic repeating every frame does not churn the folder.
const MAX_PER_RUN: usize = 5;
/// The newest crash file already offered (its name), in the crash folder.
const SEEN: &str = "seen";

static RING: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static LAST: Mutex<Option<(Option<PathBuf>, String)>> = Mutex::new(None);
static PANICS: AtomicUsize = AtomicUsize::new(0);

/// `<data dir>/crashes` (`KEEL_DATA_DIR` moves it).
pub fn dir() -> Option<PathBuf> {
    keel_core::data_dir().map(|d| d.join("crashes"))
}

/// `base` (a `tracing` filter such as `RUST_LOG`) with the directives every Keel log keeps,
/// whatever `base` says (later directives for the same target win): the FTP library logs
/// each command it sends at trace level, `PASS <password>` included, so it is always off;
/// the S3 signer logs its credential providers at debug.
pub fn log_filter(base: &str) -> String {
    let base = match base.trim() {
        "" => "info",
        b => b,
    };
    format!("{base},suppaftp=off,{}", keel_vfs::cloud::LOG_FILTER_HINT)
}

/// A log writer for `tracing_subscriber::fmt().with_writer(|| tee(std::io::stderr()))`:
/// writes through to `W` and keeps the last [`LOG_LINES`] lines for crash reports.
pub fn tee<W: Write>(inner: W) -> Tee<W> {
    Tee {
        inner,
        line: Vec::new(),
    }
}

pub struct Tee<W> {
    inner: W,
    /// The line being written (one event may arrive in several writes).
    line: Vec<u8>,
}

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.extend_from_slice(buf);
        if let Some(end) = self.line.iter().rposition(|&b| b == b'\n') {
            remember(&self.line[..end]);
            self.line.drain(..=end);
        }
        self.inner.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W> Drop for Tee<W> {
    fn drop(&mut self) {
        remember(&self.line);
    }
}

/// Adds the lines in `buf` (ANSI colours removed) to the ring.
fn remember(buf: &[u8]) {
    static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
    let text = String::from_utf8_lossy(buf);
    let text = ANSI.replace_all(&text, "");
    let mut ring = RING.lock().unwrap_or_else(PoisonError::into_inner);
    for line in text.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        if ring.len() == LOG_LINES {
            ring.pop_front();
        }
        ring.push_back(line.to_owned());
    }
}

/// The log lines kept so far, oldest first.
pub fn log_lines() -> Vec<String> {
    // try_lock: a panic while the ring was locked must not hang the hook.
    RING.try_lock()
        .map(|r| r.iter().cloned().collect())
        .unwrap_or_default()
}

/// What happened, in the report's words.
pub struct Panic<'a> {
    pub program: &'a str,
    pub thread: &'a str,
    pub message: &'a str,
    pub location: &'a str,
    pub backtrace: &'a str,
    pub log: &'a [String],
}

/// The report text (not yet scrubbed).
pub fn report(p: &Panic) -> String {
    let log = match p.log.is_empty() {
        true => "(none)".to_owned(),
        false => p.log.join("\n"),
    };
    format!(
        "Keel crash report\n\
         Program: {} {}\n\
         Signature: {}\n\
         OS: {} {}\n\
         Time: {} UTC\n\
         Thread: {}\n\
         Panic: {}\n\
         At: {}\n\
         \n\
         Backtrace:\n{}\n\
         \n\
         Last log lines:\n{log}\n",
        p.program,
        env!("CARGO_PKG_VERSION"),
        crate::signature::current(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
        p.thread,
        p.message,
        p.location,
        p.backtrace.trim_end(),
    )
}

/// Removes what names the user, their machines or their profiles: `home` (and anything
/// under it) becomes `~`; Rust source paths keep `<crate>/src/<file>`; the user name in
/// `C:\Users\<name>`, `/home/<name>` and `/Users/<name>` becomes `<user>`; URLs lose their
/// user name, and their host too unless they are http(s); network paths `\\<server>\` and
/// `\\?\UNC\<server>\` lose the server; `profiles\<name>` loses the name; e-mail addresses
/// and `user@host` become `<user>@<host>`. Other file and folder names are left: the user
/// reads the report before posting it.
pub fn scrub(text: &str, home: Option<&Path>) -> String {
    static SOURCE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"[^\s"'()<>]*[\\/][^\s"'()<>\\/]+\.rs\b"#).unwrap());
    // A name with spaces is taken up to the next separator on the line (more rather than
    // less), a name without one up to a space.
    static WIN_USER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\b([a-z]:[\\/]users[\\/])(?:[^\\/"'\n]+([\\/])|[^\\/\s"']+)"#).unwrap()
    });
    static UNIX_USER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)(^|[\s"'=(\[>])(/(?:home|Users))/(?:[^/"'\n]+(/)|[^/\s"']+)"#).unwrap()
    });
    static URL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b([A-Za-z][A-Za-z0-9+.-]*)://([^\s/]*)").unwrap());
    // `\\?\` (verbatim) and `\\.\` (device) prefixes name no server.
    static UNC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\\\\[^\\\s"'?.][^\\\s"']*\\"#).unwrap());
    static VERBATIM_UNC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\\\\\?\\UNC\\[^\\\s"']+\\"#).unwrap());
    static PROFILE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)([\\/]profiles[\\/])[^\\/\s"':]+"#).unwrap());
    static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*").unwrap()
    });
    let mut text = text.to_owned();
    if let Some(home) = home.and_then(home_pattern) {
        text = home.replace_all(&text, "~$1").into_owned();
    }
    let text = SOURCE.replace_all(&text, |c: &regex::Captures| {
        let parts: Vec<&str> = c[0].split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        match parts.iter().rposition(|p| *p == "src") {
            Some(i) if i > 0 => parts[i - 1..].join("/"),
            _ => parts.last().copied().unwrap_or_default().to_owned(),
        }
    });
    let text = URL.replace_all(&text, |c: &regex::Captures| {
        let (scheme, authority) = (&c[1], &c[2]);
        let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
        let host = authority.rsplit('@').next().unwrap_or_default();
        let host = if web || host.is_empty() {
            host
        } else {
            "<host>"
        };
        match authority.contains('@') {
            true => format!("{scheme}://<user>@{host}"),
            false => format!("{scheme}://{host}"),
        }
    });
    let text = WIN_USER.replace_all(&text, "${1}<user>${2}");
    let text = UNIX_USER.replace_all(&text, "${1}${2}/<user>${3}");
    let text = VERBATIM_UNC.replace_all(&text, r"\\?\UNC\<host>\");
    let text = UNC.replace_all(&text, r"\\<host>\");
    let text = PROFILE.replace_all(&text, "${1}<profile>");
    // After URLs: their `<user>@` is no address any more.
    EMAIL.replace_all(&text, "<user>@<host>").into_owned()
}

/// `home` as a pattern: either separator, case-insensitive on Windows, ending at a
/// separator, a quote, a space or the end (`$1` is that character).
fn home_pattern(home: &Path) -> Option<Regex> {
    let home = home.to_str()?.trim_end_matches(['\\', '/']);
    let parts: Vec<String> = home.split(['\\', '/']).map(regex::escape).collect();
    if parts.iter().filter(|p| !p.is_empty()).count() < 2 {
        return None; // `/` or `C:` alone: everything would match
    }
    let case = if cfg!(windows) { "(?i)" } else { "" };
    Regex::new(&format!(r#"{case}{}([\\/\s"':]|$)"#, parts.join(r"[\\/]"))).ok()
}

/// Writes `text` as a new `<dir>/<UTC time>.txt` (owner-only, like `dir` and its parent
/// when this creates them) and deletes all but the newest [`KEEP`].
pub fn write(dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    // The data folder too: a panic before anything else made it must not leave it open.
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::private::create_dir_all(parent)?;
    }
    crate::private::create_dir_all(dir)?;
    // Time plus a process-wide sequence: several reports in one millisecond keep their
    // order, and a name freed by the rotation below is never handed out again (which once
    // made the newest file sort as the oldest on a fast machine).
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 10_000;
    let stamp = format!(
        "{}-{seq:04}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")
    );
    let mut path = dir.join(format!("{stamp}.txt"));
    let mut n = 0;
    let mut file = loop {
        match crate::private::create_new(&path) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => {
                n += 1;
                // `_` sorts after `.`: the names stay in time order.
                path = dir.join(format!("{stamp}_{n:02}.txt"));
            }
            Err(e) => return Err(e),
        }
    };
    file.write_all(text.as_bytes())?;
    drop(file);
    rotate(dir, KEEP);
    Ok(path)
}

/// The crash files in `dir`, oldest first.
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    files.sort();
    files
}

fn rotate(dir: &Path, keep: usize) {
    let files = list(dir);
    for old in &files[..files.len().saturating_sub(keep)] {
        if let Err(e) = std::fs::remove_file(old) {
            tracing::warn!("remove {}: {e}", old.display());
        }
    }
}

/// The newest crash file when it was not offered yet ([`mark_seen`]).
pub fn unseen(dir: &Path) -> Option<PathBuf> {
    let newest = list(dir).pop()?;
    let seen = std::fs::read_to_string(dir.join(SEEN)).unwrap_or_default();
    let name = newest.file_name()?.to_str()?;
    (name > seen.trim()).then_some(newest)
}

/// Remembers `file` (and everything older) as offered.
pub fn mark_seen(dir: &Path, file: &Path) {
    let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if let Err(e) = std::fs::write(dir.join(SEEN), name) {
        tracing::warn!("{}: {e}", dir.join(SEEN).display());
    }
}

/// This run's newest report: its file (None when it could not be written) and text.
pub fn last() -> Option<(Option<PathBuf>, String)> {
    LAST.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Writes a crash file for each of the first panics of the run (any thread), logs where,
/// then runs the default hook. A panic inside [`quiet`] is only logged: no file, not
/// counted. `program` is `keel` or `keel-daemon`.
pub fn install_panic_hook(program: &'static str, dir: Option<PathBuf>) {
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_owned());
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("(no message)");
        let location = info
            .location()
            .map_or_else(|| "unknown".to_owned(), ToString::to_string);
        if keel_vfs::quiet_panic::is_quiet() {
            tracing::warn!("caught panic at {location}: {message}");
            return;
        }
        if PANICS.fetch_add(1, Ordering::Relaxed) < MAX_PER_RUN {
            let text = scrub(
                &report(&Panic {
                    program,
                    thread: std::thread::current().name().unwrap_or("unnamed"),
                    message,
                    location: &location,
                    backtrace: &std::backtrace::Backtrace::force_capture().to_string(),
                    log: &log_lines(),
                }),
                home.as_deref(),
            );
            let path = dir.as_deref().and_then(|d| match write(d, &text) {
                Ok(path) => Some(path),
                Err(e) => {
                    eprintln!("could not write a crash report in {}: {e}", d.display());
                    None
                }
            });
            if let Some(path) = &path {
                tracing::error!("crash report written to {}", path.display());
            }
            *LAST.lock().unwrap_or_else(PoisonError::into_inner) = Some((path, text));
        }
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-crash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn scrub_removes_home_users_hosts_and_source_folders() {
        let (home, sep) = if cfg!(windows) {
            (Path::new(r"C:\Users\Jane Doe"), r"\")
        } else {
            (Path::new("/home/jane doe"), "/")
        };
        let cargo = format!(
            "   at {}{sep}.cargo{sep}registry{sep}src{sep}index.crates.io-1949cf8c6b5b557f{sep}\
             egui-0.31.1{sep}src{sep}context.rs:123:45",
            home.display()
        );
        let raw = [
            r"open C:\Users\Jane Doe\Documents\taxes.pdf: denied",
            r"open c:/users/jane doe/Pictures/x.jpg",
            "open /home/jane doe/notes.txt",
            r"list D:\Users\bob\Desktop failed",
            "list /home/alice/Music and /Users/carol/Movies",
            "sftp://bob@nas.local:22/home/bob/photos timed out",
            "webdav://files.example.org/dav and https://alice:pw@example.com/x",
            r"share \\server7\family\a.txt",
            cargo.as_str(),
            "   at /rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/std/src/panicking.rs:665:5",
            r"At: crates\keel-app\src\pane.rs:12:5",
            "see https://github.com/Runitupshawty/keel/issues",
        ]
        .join("\n");
        let clean = scrub(&raw, Some(home));
        for gone in [
            "Jane",
            "jane",
            "bob",
            "alice",
            "carol",
            "nas.local",
            "files.example.org",
            "pw@",
            "server7",
            "90b35a",
            ".cargo",
        ] {
            assert!(!clean.contains(gone), "{gone} left in:\n{clean}");
        }
        let lines: Vec<&str> = clean.lines().collect();
        if cfg!(windows) {
            assert_eq!(lines[0], r"open ~\Documents\taxes.pdf: denied");
            assert_eq!(lines[1], "open ~/Pictures/x.jpg");
            assert_eq!(lines[2], "open /home/<user>/notes.txt");
        } else {
            assert_eq!(
                lines[0],
                r"open C:\Users\<user>\Documents\taxes.pdf: denied"
            );
            assert_eq!(lines[2], "open ~/notes.txt");
        }
        assert_eq!(lines[3], r"list D:\Users\<user>\Desktop failed");
        assert_eq!(lines[4], "list /home/<user>/Music and /Users/<user>/Movies");
        assert_eq!(
            lines[5],
            "sftp://<user>@<host>/home/<user>/photos timed out"
        );
        assert_eq!(
            lines[6],
            "webdav://<host>/dav and https://<user>@example.com/x"
        );
        assert_eq!(lines[7], r"share \\<host>\family\a.txt");
        assert_eq!(lines[8], "   at egui-0.31.1/src/context.rs:123:45");
        assert_eq!(lines[9], "   at std/src/panicking.rs:665:5");
        assert_eq!(lines[10], "At: keel-app/src/pane.rs:12:5");
        assert_eq!(
            lines[11],
            "see https://github.com/Runitupshawty/keel/issues"
        );
        // A home that is a prefix of another folder's name leaves that folder alone.
        let other = if cfg!(windows) {
            r"C:\Users\Jane Doette\x"
        } else {
            "/home/jane doette/x"
        };
        assert!(!scrub(other, Some(home)).starts_with('~'));
    }

    #[test]
    fn scrub_removes_servers_profiles_and_addresses() {
        let raw = [
            r"open \\?\UNC\fileserver9\share\x.txt",
            r"open \\?\unc\fileserver9\share\x.txt",
            r"open \\?\C:\data\x.txt and \\.\pipe\keel-1",
            r"read D:\Keel\profiles\work-secret\config.toml",
            "read /srv/keel/profiles/work-secret/config.toml",
            "mail jane.doe+keel@contoso.example failed",
            "ssh jane@nas.local: refused",
            "sftp://bob@nas.local/x and https://alice:pw@example.com/x",
        ]
        .join("\n");
        let clean = scrub(&raw, None);
        for gone in [
            "fileserver9",
            "work-secret",
            "jane",
            "contoso",
            "nas.local",
            "bob",
        ] {
            assert!(!clean.contains(gone), "{gone} left in:\n{clean}");
        }
        assert_eq!(
            clean.lines().collect::<Vec<_>>(),
            [
                r"open \\?\UNC\<host>\share\x.txt",
                r"open \\?\UNC\<host>\share\x.txt",
                r"open \\?\C:\data\x.txt and \\.\pipe\keel-1",
                r"read D:\Keel\profiles\<profile>\config.toml",
                "read /srv/keel/profiles/<profile>/config.toml",
                "mail <user>@<host> failed",
                "ssh <user>@<host>: refused",
                "sftp://<user>@<host>/x and https://<user>@example.com/x",
            ]
        );
    }

    #[test]
    fn report_has_the_facts() {
        let log = vec!["INFO started".to_owned(), "WARN slow disk".to_owned()];
        let text = report(&Panic {
            program: "keel",
            thread: "main",
            message: "index out of bounds",
            location: "crates/keel-app/src/pane.rs:1:2",
            backtrace: "0: frame",
            log: &log,
        });
        assert!(text.contains(concat!("Program: keel ", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains(std::env::consts::OS));
        let signature = format!(
            "Signature: {}
",
            crate::signature::current()
        );
        assert!(text.contains(&signature), "{text}");
        assert!(text.contains("Panic: index out of bounds"));
        assert!(text.contains("At: crates/keel-app/src/pane.rs:1:2"));
        assert!(text.contains("0: frame"));
        assert!(text.contains("INFO started\nWARN slow disk"));
        assert!(!text.contains("Profile"), "no profile, not even hashed");
    }

    #[test]
    fn tee_keeps_the_last_lines_without_colours() {
        let mut out = Vec::new();
        let mut w = tee(&mut out);
        // writeln! writes in pieces; the ring keeps whole lines.
        for i in 0..(LOG_LINES + 10) {
            writeln!(w, "\x1b[32mINFO\x1b[0m line {i}").unwrap();
        }
        write!(w, "no newline").unwrap();
        drop(w);
        assert!(
            String::from_utf8(out).unwrap().contains("\x1b[32m"),
            "passed through"
        );
        let lines = log_lines();
        assert_eq!(lines.len(), LOG_LINES);
        assert_eq!(
            lines[LOG_LINES - 2..],
            [format!("INFO line {}", LOG_LINES + 9), "no newline".into()]
        );
        assert!(!lines.iter().any(|l| l.contains('\x1b')));
    }

    #[test]
    fn keeps_the_newest_ten_files() {
        let dir = tmp("rotate");
        let mut written = Vec::new();
        for i in 0..(KEEP + 4) {
            written.push(write(&dir, &format!("report {i}")).unwrap());
        }
        std::fs::write(dir.join(SEEN), "x").unwrap();
        let kept = list(&dir);
        assert_eq!(kept, written[4..], "the oldest went");
        assert_eq!(
            std::fs::read_to_string(kept.last().unwrap()).unwrap(),
            format!("report {}", KEEP + 3)
        );
        assert!(dir.join(SEEN).exists(), "only crash files rotate");
        assert!(
            crate::private::is_private_dir(&dir).unwrap(),
            "owner-only folder"
        );
        let newest = crate::private::read(kept.last().unwrap()).unwrap();
        assert!(newest.is_some(), "owner-only file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_panic_writes_a_scrubbed_report_and_a_caught_one_does_not() {
        let dir = tmp("hook");
        install_panic_hook("keel-test", Some(dir.clone()));
        // More caught panics than the per-run budget: no file, and none counts.
        for i in 0..=MAX_PER_RUN {
            let caught = quiet(|| std::panic::catch_unwind(|| panic!("damaged file {i}")));
            assert!(caught.is_err());
        }
        assert_eq!(
            list(&dir),
            Vec::<PathBuf>::new(),
            "caught panics write nothing"
        );
        let home = directories::BaseDirs::new().unwrap().home_dir().to_owned();
        let place = home.join("Documents").join("private.txt");
        let _ = std::thread::Builder::new()
            .name("crasher".into())
            .spawn(move || panic!("cannot open {}", place.display()))
            .unwrap()
            .join();
        let files = list(&dir);
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(&files[0]).unwrap();
        assert!(
            text.starts_with("Keel crash report\nProgram: keel-test "),
            "{text}"
        );
        assert!(text.contains("Thread: crasher"), "{text}");
        let sep = std::path::MAIN_SEPARATOR;
        assert!(
            text.contains(&format!(
                "Panic: cannot open ~{sep}Documents{sep}private.txt"
            )),
            "{text}"
        );
        assert!(text.contains("At: keel-api/src/crash.rs:"), "{text}");
        assert!(!text.contains(home.to_str().unwrap()), "{text}");
        assert_eq!(last().unwrap(), (Some(files[0].clone()), text));
        let _ = std::panic::take_hook();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_crash_file_is_offered_once() {
        let dir = tmp("seen");
        assert_eq!(unseen(&dir), None, "no folder");
        let first = write(&dir, "one").unwrap();
        assert_eq!(unseen(&dir).as_deref(), Some(first.as_path()));
        mark_seen(&dir, &first);
        assert_eq!(unseen(&dir), None);
        let second = write(&dir, "two").unwrap();
        assert_eq!(unseen(&dir).as_deref(), Some(second.as_path()));
        mark_seen(&dir, &second);
        assert_eq!(unseen(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
