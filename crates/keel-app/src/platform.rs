//! OS launches: default app, open-with picker, reveal in the system file manager. Each starts a process and returns without waiting.
//! Call through `worker::spawn_local` so remote paths materialise off the UI thread.

use std::io;
use std::path::Path;
use std::process::Command;

pub fn open(path: &Path) -> io::Result<()> {
    open::that_detached(path)
}

fn run(mut cmd: Command) -> io::Result<()> {
    cmd.spawn().map(drop)
}

#[cfg(windows)]
mod sys {
    use super::*;
    use std::os::windows::process::CommandExt;

    pub fn open_with(path: &Path) -> io::Result<()> {
        // OpenAs_RunDLL reads the raw tail of the command line; quotes would be part of the name.
        let mut cmd = Command::new("rundll32.exe");
        cmd.raw_arg(format!("shell32.dll,OpenAs_RunDLL {}", path.display()));
        run(cmd)
    }

    pub fn reveal(path: &Path) -> io::Result<()> {
        let mut cmd = Command::new("explorer.exe");
        cmd.raw_arg(format!("/select,\"{}\"", path.display()));
        run(cmd)
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::*;

    pub fn open_with(path: &Path) -> io::Result<()> {
        let mut cmd = Command::new("osascript");
        cmd.args([
            "-e",
            "on run argv",
            "-e",
            "set appPath to POSIX path of (choose application as alias)",
            "-e",
            "do shell script \"open -a \" & quoted form of appPath & \" \" & quoted form of (item 1 of argv)",
            "-e",
            "end run",
        ])
        .arg(path);
        run(cmd)
    }

    pub fn reveal(path: &Path) -> io::Result<()> {
        let mut cmd = Command::new("open");
        cmd.arg("-R").arg(path);
        run(cmd)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod sys {
    use super::*;

    // ponytail: no app picker on Linux yet; opens with the default handler.
    pub fn open_with(path: &Path) -> io::Result<()> {
        super::open(path)
    }

    pub fn reveal(path: &Path) -> io::Result<()> {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let mut cmd = Command::new("xdg-open");
        cmd.arg(dir);
        run(cmd)
    }
}

pub use sys::{open_with, reveal};
