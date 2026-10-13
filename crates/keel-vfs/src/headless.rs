//! No desktop side effects under tests: `cargo test` must not open files, folders, apps,
//! browsers, dialogs, Keel windows or UAC prompts on the developer's screen. Every such
//! launch asks [`skip`] first.

use std::process::Command;
use std::sync::{Mutex, OnceLock, PoisonError};

/// Whether Keel leaves the desktop alone: in test binaries (any crate's) and in a process
/// started with `KEEL_HEADLESS=1` (tests set it on the real binaries they run).
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::in_test_binary() || std::env::var_os("KEEL_HEADLESS").is_some_and(|v| v == "1")
    })
}

static SKIPPED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// True when [`on`]: `what` (the launch the user would have seen) is recorded for
/// [`skipped`] and the caller must not start it.
pub fn skip(what: impl std::fmt::Display) -> bool {
    if on() {
        let mut skipped = SKIPPED.lock().unwrap_or_else(PoisonError::into_inner);
        skipped.push(what.to_string());
    }
    on()
}

/// The launches [`skip`] stopped in this process, oldest first.
pub fn skipped() -> Vec<String> {
    SKIPPED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// `cmd` with no console window of its own (Windows), for console helpers.
pub fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    #[test]
    fn launches_are_recorded_not_started() {
        assert!(super::on(), "a test binary is headless");
        assert!(super::skip("open headless-probe"));
        assert!(super::skipped().iter().any(|s| s == "open headless-probe"));
    }
}
