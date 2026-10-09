use crate::ShellKind;
use std::path::Path;
use std::time::{Duration, Instant};

/// Characters that open or close a PowerShell single-quoted string: ' ‘ ’ ‚ ‛.
const PS_QUOTES: [char; 5] = ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];

/// A `cd` line that cannot run any part of the path as code, or `None` when the path
/// cannot be quoted safely for this shell (it is then never sent).
pub(crate) fn cd_line(kind: ShellKind, dir: &Path) -> Option<String> {
    let path = dir.to_string_lossy();
    if path.chars().any(char::is_control) {
        return None;
    }
    Some(match kind {
        // Any two quote characters in a row are one literal quote, so doubling each keeps
        // the string closed only by the final `'`.
        ShellKind::PowerShell => {
            let mut quoted = String::with_capacity(path.len() + 2);
            for c in path.chars() {
                quoted.push(c);
                if PS_QUOTES.contains(&c) {
                    quoted.push(c);
                }
            }
            format!("Set-Location -LiteralPath '{quoted}'\r")
        }
        // cmd expands %variables% and (with delayed expansion) !variables! even in quotes.
        ShellKind::Cmd if path.contains(['%', '!', '"']) => return None,
        ShellKind::Cmd => format!("cd /d \"{path}\"\r"),
        // Translated here, never by running `wslpath` inside the shell.
        ShellKind::Wsl => posix_cd(&crate::wslpath(dir))?,
        ShellKind::Posix => posix_cd(&path)?,
    })
}

/// fish reads `\'` as an escape even inside single quotes, so no one quoting works for
/// sh, bash, zsh and fish alike: paths containing `'` or `\` are refused.
fn posix_cd(path: &str) -> Option<String> {
    (!path.contains(['\'', '\\'])).then(|| format!("cd '{path}'\r"))
}

/// Whether the text left of the cursor looks like this shell's default LOCAL prompt.
/// `host` is this machine's name ("" = unknown, any host accepted).
///
/// Limits (heuristic, not a process-tree query): PowerShell `PS <path>>` and cmd `C:\...>`
/// prompts of a remote Windows host reached over ssh look local; posix prompts must be
/// `user@host...` (with `host` matching this machine when known) or a `~`/absolute path,
/// ending in `$ # %` — so `psql`'s `db=#`, `>>>` or a bare `box%` never count, and custom
/// prompts (oh-my-posh, starship) never look idle. On Unix the session additionally
/// requires the shell to own the terminal's foreground process group.
pub(crate) fn is_prompt(kind: ShellKind, line: &str, host: &str) -> bool {
    let line = line.trim_end();
    match kind {
        ShellKind::PowerShell => line.starts_with("PS ") && line.ends_with('>'),
        ShellKind::Cmd => line.as_bytes().get(1) == Some(&b':') && line.ends_with('>'),
        ShellKind::Wsl | ShellKind::Posix => {
            let Some(body) = line.strip_suffix(['$', '#', '%']) else {
                return false;
            };
            let body = body
                .trim_end()
                .trim_start_matches('[')
                .trim_end_matches(']');
            match body.split_once('@') {
                Some((user, rest)) => {
                    let machine = rest.split([':', ' ']).next().unwrap_or_default();
                    let label = |s: &str| s.split('.').next().unwrap_or_default().to_owned();
                    !user.is_empty()
                        && !user.contains(char::is_whitespace)
                        && !machine.is_empty()
                        && (host.is_empty() || label(machine).eq_ignore_ascii_case(&label(host)))
                }
                None => body.starts_with(['~', '/']),
            }
        }
    }
}

/// How long the PTY must stay silent after a prompt before a queued `cd` is typed.
pub(crate) const QUIET: Duration = Duration::from_millis(300);

/// Idle-shell heuristic. The shell counts as idle when the reader has drained the PTY (a
/// short read), the text left of the cursor looks like the shell's default local prompt
/// (see `is_prompt`), nothing has been typed since the last Enter / Ctrl+C, and no output
/// has arrived for `QUIET`. A `cd` is always queued (latest wins) and typed only once the
/// shell is idle, so a half-typed line or a running program never receives it.
#[derive(Default)]
pub(crate) struct Idle {
    pub ready: bool,
    typing: bool,
    pub pending: Option<String>,
    last_output: Option<Instant>,
}
impl Idle {
    pub fn input(&mut self, bytes: &[u8]) {
        self.ready = false;
        self.typing = !(bytes.ends_with(b"\r") || bytes.ends_with(b"\n") || bytes == b"\x03");
    }
    /// Records a drained (`prompt` computed) or partial read at `now`.
    pub fn output(&mut self, prompt: bool, now: Instant) {
        self.last_output = Some(now);
        self.ready = !self.typing && prompt;
    }
    /// The queued line once the shell has been idle for `QUIET` (counted as typed input),
    /// else how long to wait before asking again (`None`: until the next output or cd).
    pub fn due(&mut self, now: Instant) -> Result<String, Option<Duration>> {
        if !self.ready || self.pending.is_none() {
            return Err(None);
        }
        let quiet = self
            .last_output
            .map_or(QUIET, |t| now.saturating_duration_since(t));
        if quiet < QUIET {
            return Err(Some(QUIET - quiet));
        }
        let line = self.pending.take().unwrap_or_default();
        self.input(line.as_bytes());
        Ok(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrolling_back_more_than_one_screen_is_safe() {
        // vt100 0.15 underflowed here (offset > rows); 0.16 clamps.
        let mut parser = vt100::Parser::new(3, 10, 10_000);
        for i in 0..20 {
            parser.process(format!("line{i}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(9);
        assert_eq!(parser.screen().scrollback(), 9);
        assert_eq!(parser.screen().cell(0, 4).unwrap().contents(), "9");
        assert!(parser.screen().contents().starts_with("line9"));
    }
    #[test]
    fn idle_needs_submitted_input_a_prompt_and_quiet() {
        let t0 = Instant::now();
        let mut idle = Idle::default();
        idle.output(true, t0); // first prompt after startup
        assert!(idle.ready);
        assert_eq!(idle.due(t0), Err(None)); // nothing queued
        idle.input(b"di"); // half-typed command: never cd over it
        idle.pending = Some("cd x\r".into());
        idle.output(true, t0);
        assert_eq!(idle.due(t0 + QUIET), Err(None));
        idle.input(b"r\r");
        idle.output(false, t0); // command running, no prompt yet
        assert_eq!(idle.due(t0 + QUIET), Err(None));
        idle.output(true, t0);
        // A prompt-like line followed by more output within QUIET is not idle yet.
        assert_eq!(idle.due(t0 + QUIET / 3), Err(Some(QUIET - QUIET / 3)));
        assert_eq!(idle.due(t0 + QUIET).as_deref(), Ok("cd x\r"));
        assert!(!idle.ready && idle.pending.is_none());
    }
    #[test]
    fn prompts_per_shell() {
        assert!(is_prompt(ShellKind::PowerShell, r"PS D:\Work> ", ""));
        assert!(!is_prompt(ShellKind::PowerShell, ">>> ", ""));
        assert!(is_prompt(ShellKind::Cmd, r"D:\Work>", ""));
        assert!(!is_prompt(ShellKind::Cmd, "More? ", ""));
        assert!(is_prompt(ShellKind::Posix, "james@box:~$ ", ""));
        assert!(is_prompt(ShellKind::Posix, "[james@box tmp]$ ", ""));
        assert!(is_prompt(ShellKind::Posix, "james@Mac ~ % ", ""));
        assert!(is_prompt(ShellKind::Posix, "~/src $ ", ""));
        assert!(is_prompt(ShellKind::Posix, "/tmp # ", ""));
        // m18: only local-looking prompts count.
        assert!(!is_prompt(ShellKind::Posix, "box% ", ""));
        assert!(!is_prompt(ShellKind::Posix, "> ", ""));
        assert!(!is_prompt(ShellKind::Posix, "postgres=# ", ""));
        assert!(!is_prompt(ShellKind::Posix, "total cost: 5$", ""));
        // WSL knows this machine's name: an ssh session's prompt names another host.
        assert!(is_prompt(
            ShellKind::Wsl,
            "james@DESKTOP:/mnt/d$ ",
            "desktop"
        ));
        assert!(is_prompt(
            ShellKind::Wsl,
            "james@desktop.lan:~$ ",
            "DESKTOP"
        ));
        assert!(!is_prompt(
            ShellKind::Wsl,
            "james@fileserver:~$ ",
            "DESKTOP"
        ));
    }
    #[test]
    fn cd_quotes_paths_without_executing_path_text() {
        assert_eq!(
            cd_line(ShellKind::PowerShell, Path::new("a'b")),
            Some("Set-Location -LiteralPath 'a''b'\r".into())
        );
        assert_eq!(
            cd_line(ShellKind::Cmd, Path::new(r"D:\a & b")),
            Some("cd /d \"D:\\a & b\"\r".into())
        );
        assert_eq!(
            cd_line(ShellKind::Posix, Path::new("/home/a b")),
            Some("cd '/home/a b'\r".into())
        );
        for kind in [
            ShellKind::PowerShell,
            ShellKind::Cmd,
            ShellKind::Wsl,
            ShellKind::Posix,
        ] {
            assert!(cd_line(kind, Path::new("a\nb")).is_none());
        }
        assert!(cd_line(ShellKind::Cmd, Path::new(r"D:\%TEMP%")).is_none());
    }
    /// B1: PowerShell also ends single-quoted strings at U+2018..U+201B.
    #[test]
    fn powershell_doubles_every_quote_character() {
        let name = "x\u{2019};New-Item pwned;\u{2018}\u{201A}\u{201B}'";
        assert_eq!(
            cd_line(ShellKind::PowerShell, Path::new(name)),
            Some(
                "Set-Location -LiteralPath 'x\u{2019}\u{2019};New-Item pwned;\
                 \u{2018}\u{2018}\u{201A}\u{201A}\u{201B}\u{201B}'''\r"
                    .into()
            )
        );
    }
    /// M8: fish treats `\'` as an escape inside single quotes; WSL paths are translated in
    /// Rust, never by `$(wslpath ...)` in the shell.
    #[test]
    fn posix_and_wsl_refuse_quotes_and_backslashes() {
        assert!(cd_line(ShellKind::Posix, Path::new("/tmp/it's")).is_none());
        assert!(cd_line(ShellKind::Posix, Path::new(r"/tmp/a\")).is_none());
        assert!(cd_line(ShellKind::Wsl, Path::new(r"C:\a\it's")).is_none());
        assert_eq!(
            cd_line(ShellKind::Wsl, Path::new(r"D:\a b")),
            Some("cd '/mnt/d/a b'\r".into())
        );
    }
    /// B1 live: Windows PowerShell runs the generated line for folders whose names try to
    /// break out of the quotes; it must land in that folder and create nothing.
    #[cfg(target_os = "windows")]
    #[test]
    fn powershell_live_cd_into_hostile_folder_is_inert() {
        use std::os::windows::process::CommandExt;
        let Some(shell) = crate::available_shells()
            .into_iter()
            .find(|s| s.kind == ShellKind::PowerShell)
        else {
            eprintln!("SKIP: no PowerShell");
            return;
        };
        let base = std::env::temp_dir().join(format!("keel-ps-quote-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let names = [
            "x\u{2019};New-Item pwned1;\u{2019}",
            "x\u{2018};New-Item pwned2;\u{2018}",
            "x\u{201A};New-Item pwned3;\u{201B}",
            "x';New-Item pwned4;'",
        ];
        for name in names {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let line = cd_line(ShellKind::PowerShell, &dir).unwrap();
            // The cwd is printed as UTF-8 byte values so the console code page cannot
            // garble it.
            let script = format!(
                "{}; [Console]::Out.Write([Text.Encoding]::UTF8.GetBytes((Get-Location).Path) -join ',')",
                line.trim_end()
            );
            let out = std::process::Command::new(&shell.program)
                .args(["-NoProfile", "-NonInteractive", "-Command"])
                .raw_arg(format!("\"{script}\""))
                .current_dir(&base)
                .output()
                .unwrap();
            let bytes: Vec<u8> = String::from_utf8_lossy(&out.stdout)
                .trim()
                .split(',')
                .filter_map(|b| b.parse().ok())
                .collect();
            let landed = String::from_utf8(bytes).unwrap();
            for marker in ["pwned1", "pwned2", "pwned3", "pwned4"] {
                assert!(!base.join(marker).exists(), "{name}: code ran");
                assert!(!dir.join(marker).exists(), "{name}: code ran");
            }
            // Canonicalize both sides: CI runners put TEMP under an 8.3 short name
            // (RUNNER~1) while PowerShell reports the long one.
            let canon = |p: &std::path::Path| {
                std::fs::canonicalize(p)
                    .map(|c| {
                        c.to_string_lossy()
                            .trim_start_matches(r"\?\")
                            .to_lowercase()
                    })
                    .unwrap_or_else(|_| p.to_string_lossy().to_lowercase())
            };
            assert_eq!(
                canon(std::path::Path::new(&landed)),
                canon(&dir),
                "stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
