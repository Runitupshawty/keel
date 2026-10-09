use crate::ShellKind;
use std::path::Path;

pub(crate) fn cd_line(kind: ShellKind, dir: &Path) -> Option<String> {
    let path = dir.to_string_lossy();
    if path.chars().any(char::is_control) {
        return None;
    }
    let quoted = || path.replace('\'', "'\\''");
    Some(match kind {
        ShellKind::PowerShell => {
            format!("Set-Location -LiteralPath '{}'\r", path.replace('\'', "''"))
        }
        // cmd expands %variables% and (with delayed expansion) !variables! even in quotes.
        ShellKind::Cmd if path.contains(['%', '!', '"']) => return None,
        ShellKind::Cmd => format!("cd /d \"{path}\"\r"),
        ShellKind::Wsl => format!("cd \"$(wslpath '{}')\"\r", quoted()),
        ShellKind::Posix => format!("cd '{}'\r", quoted()),
    })
}

/// Whether the text left of the cursor looks like this shell's default prompt.
pub(crate) fn is_prompt(kind: ShellKind, line: &str) -> bool {
    let line = line.trim_end();
    match kind {
        ShellKind::PowerShell => line.starts_with("PS ") && line.ends_with('>'),
        ShellKind::Cmd => line.as_bytes().get(1) == Some(&b':') && line.ends_with('>'),
        ShellKind::Wsl | ShellKind::Posix => line.ends_with(['$', '#', '%']),
    }
}

/// Idle-shell heuristic, not a process-tree query. The shell counts as idle when the
/// reader has drained the PTY (a short read), the text left of the cursor looks like the
/// shell's default prompt (`PS ...>`, `C:\...>`, or ending in `$ # %`), and nothing has
/// been typed since the last Enter / Ctrl+C. A cd asked for while busy is kept (latest
/// wins) and sent at the next idle prompt, so a half-typed line or a running program
/// never receives it.
// ponytail: custom prompts (oh-my-posh, starship) never look idle, so Follow pane is a
// no-op there; shell integration (OSC 133 marks) is the upgrade path.
#[derive(Default)]
pub(crate) struct Idle {
    pub ready: bool,
    typing: bool,
    pub pending: Option<String>,
}
impl Idle {
    pub fn input(&mut self, bytes: &[u8]) {
        self.ready = false;
        self.typing = !(bytes.ends_with(b"\r") || bytes.ends_with(b"\n") || bytes == b"\x03");
    }
    /// Records a drained read; returns a pending cd line to send now.
    pub fn output(&mut self, prompt: bool) -> Option<String> {
        self.ready = !self.typing && prompt;
        let line = self.ready.then(|| self.pending.take()).flatten()?;
        self.input(line.as_bytes());
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_needs_submitted_input_and_a_prompt() {
        let mut idle = Idle::default();
        assert_eq!(idle.output(true), None); // first prompt after startup
        assert!(idle.ready);
        idle.input(b"di"); // half-typed command: never cd over it
        idle.pending = Some("cd x\r".into());
        assert_eq!(idle.output(true), None);
        idle.input(b"r\r");
        assert_eq!(idle.output(false), None); // command running, no prompt yet
        assert_eq!(idle.output(true).as_deref(), Some("cd x\r"));
        assert!(!idle.ready && idle.pending.is_none());
    }
    #[test]
    fn prompts_per_shell() {
        assert!(is_prompt(ShellKind::PowerShell, r"PS D:\Work> "));
        assert!(!is_prompt(ShellKind::PowerShell, ">>> "));
        assert!(is_prompt(ShellKind::Cmd, r"D:\Work>"));
        assert!(!is_prompt(ShellKind::Cmd, "More? "));
        assert!(is_prompt(ShellKind::Posix, "james@box:~$ "));
        assert!(is_prompt(ShellKind::Posix, "box% "));
        assert!(!is_prompt(ShellKind::Posix, "> "));
    }
    #[test]
    fn cd_quotes_paths_without_executing_path_text() {
        assert_eq!(
            cd_line(ShellKind::PowerShell, Path::new("a'b")),
            Some("Set-Location -LiteralPath 'a''b'\r".into())
        );
        assert_eq!(
            cd_line(ShellKind::Posix, Path::new("a'b")),
            Some("cd 'a'\\''b'\r".into())
        );
        assert_eq!(
            cd_line(ShellKind::Cmd, Path::new(r"D:\a & b")),
            Some("cd /d \"D:\\a & b\"\r".into())
        );
        assert_eq!(
            cd_line(ShellKind::Wsl, Path::new(r"D:\a b")),
            Some("cd \"$(wslpath 'D:\\a b')\"\r".into())
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
}
