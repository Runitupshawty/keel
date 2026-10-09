use crate::{
    grid::{cd_line, is_prompt, Idle},
    Shell, ShellKind,
};
use anyhow::Result;
use parking_lot::{Mutex, MutexGuard};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;

struct Resources {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    reader: JoinHandle<()>,
    waiter: JoinHandle<()>,
}
pub struct Session {
    grid: Arc<Mutex<vt100::Parser>>,
    idle: Arc<Mutex<Idle>>,
    alive: Arc<AtomicBool>,
    kind: ShellKind,
    resources: Option<Resources>,
}
impl Session {
    /// Starts a PTY, a bounded-chunk parser thread and a child-reaping thread.
    pub fn spawn(
        shell: &Shell,
        cwd: &Path,
        cols: u16,
        rows: u16,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(size(cols, rows))?;
        let mut command = CommandBuilder::new(&shell.program);
        command.args(&shell.args);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        if shell.kind == ShellKind::Wsl {
            command.args(["--cd", &crate::wslpath(cwd)]);
        }
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut child = pair.slave.spawn_command(command)?;
        let mut killer = child.clone_killer();
        drop(pair.slave);
        let grid = Arc::new(Mutex::new(vt100::Parser::new(
            rows.max(1),
            cols.max(1),
            10_000,
        )));
        let idle = Arc::new(Mutex::new(Idle::default()));
        let kind = shell.kind;
        let alive = Arc::new(AtomicBool::new(true));
        let reader_thread = {
            let grid = grid.clone();
            let idle = idle.clone();
            let notify = notify.clone();
            let writer = writer.clone();
            std::thread::Builder::new()
                .name("keel-pty-read".into())
                .spawn(move || {
                    // Never hold the grid for an entire PTY burst. Fair unlock prevents a
                    // continuous producer starving the painter, even in debug builds.
                    let mut bytes = [0; 4096];
                    let mut query = Vec::new();
                    loop {
                        let n = match reader.read(&mut bytes) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        // portable-pty enables ConPTY's INHERIT_CURSOR: startup waits for
                        // a cursor-position report. vt100 parses display state but does
                        // not answer device queries. Keep the short CSI across reads.
                        for &b in &bytes[..n] {
                            if b == 0x1b {
                                query.clear();
                                query.push(b);
                            } else if !query.is_empty() {
                                query.push(b);
                                if query.len() > 2 && (0x40..=0x7e).contains(&b) {
                                    let answer = match query.as_slice() {
                                        b"\x1b[6n" => {
                                            let (r, c) = grid.lock().screen().cursor_position();
                                            Some(format!("\x1b[{};{}R", r + 1, c + 1))
                                        }
                                        b"\x1b[5n" => Some("\x1b[0n".into()),
                                        b"\x1b[c" | b"\x1b[0c" => Some("\x1b[?1;2c".into()),
                                        _ => None,
                                    };
                                    if let Some(answer) = answer {
                                        let _ = writer.lock().write_all(answer.as_bytes());
                                    }
                                    query.clear();
                                } else if query.len() > 32 {
                                    query.clear();
                                }
                            }
                        }
                        for chunk in bytes[..n].chunks(512) {
                            let mut parser = grid.lock();
                            parser.process(chunk);
                            MutexGuard::unlock_fair(parser);
                        }
                        // A full buffer means more output is queued: not at a prompt yet.
                        let prompt = n < bytes.len() && {
                            let parser = grid.lock();
                            let screen = parser.screen();
                            let (row, col) = screen.cursor_position();
                            is_prompt(kind, &screen.contents_between(row, 0, row, col))
                        };
                        let cd = idle.lock().output(prompt);
                        if let Some(line) = cd {
                            let mut w = writer.lock();
                            let _ = w.write_all(line.as_bytes()).and_then(|_| w.flush());
                        }
                        notify(); // The app coalesces notifications to one pending message.
                    }
                    notify();
                })
        };
        let reader = match reader_thread {
            Ok(t) => t,
            Err(e) => {
                let _ = killer.kill();
                let _ = child.wait();
                return Err(e.into());
            }
        };
        let waiter = {
            let alive = alive.clone();
            std::thread::Builder::new()
                .name("keel-pty-wait".into())
                .spawn(move || {
                    let _ = child.wait();
                    alive.store(false, Ordering::Release);
                    notify();
                })
        };
        let waiter = match waiter {
            Ok(t) => t,
            Err(e) => {
                let _ = killer.kill();
                return Err(e.into());
            }
        };
        Ok(Self {
            grid,
            idle,
            alive,
            kind: shell.kind,
            resources: Some(Resources {
                master: Mutex::new(pair.master),
                writer,
                killer: Mutex::new(killer),
                reader,
                waiter,
            }),
        })
    }
    /// May block on PTY backpressure; call on an input worker, never on the UI thread.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let r = self
            .resources
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("terminal closed"))?;
        self.idle.lock().input(bytes);
        let mut writer = r.writer.lock();
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if let Some(r) = &self.resources {
            r.master.lock().resize(size(cols, rows))?;
        }
        self.grid
            .lock()
            .screen_mut()
            .set_size(rows.max(1), cols.max(1));
        Ok(())
    }
    pub fn grid(&self) -> MutexGuard<'_, vt100::Parser> {
        self.grid.lock()
    }
    /// Sends a `cd` line now if the shell looks idle, else queues it for the next idle
    /// prompt (latest wins). See `grid::Idle` for the heuristic.
    pub fn cd(&self, dir: &Path) {
        let Some(line) = cd_line(self.kind, dir) else {
            return;
        };
        let mut idle = self.idle.lock();
        if !self.is_alive() || !idle.ready {
            idle.pending = Some(line);
            return;
        }
        drop(idle);
        let _ = self.write(line.as_bytes());
    }
    /// Kills the child without waiting (usable through a shared `Arc`). The cloned
    /// killer only signals (TerminateProcess / SIGHUP), so this never blocks; it also
    /// unblocks a PTY write stuck on a dead reader.
    pub fn terminate(&self) {
        if !self.alive.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(r) = &self.resources {
            let _ = r.killer.lock().kill();
        }
    }
    /// Terminates the child; resource cleanup is off-thread because ConPTY close may block.
    pub fn kill(&mut self) {
        self.terminate();
        if let Some(r) = self.resources.take() {
            let _ = std::thread::Builder::new()
                .name("keel-pty-close".into())
                .spawn(move || {
                    drop(r.writer);
                    drop(r.master);
                    let _ = r.waiter.join();
                    let _ = r.reader.join();
                });
        }
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}
fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.max(1),
        rows: rows.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{available_shells, ShellKind};
    use std::time::{Duration, Instant};
    fn command(burst: bool) -> Shell {
        #[cfg(target_os = "windows")]
        let (kind, args) = if burst {
            (
                ShellKind::PowerShell,
                vec![
                    "-NoProfile",
                    "-Command",
                    "[Console]::Write(('x'*50000000)); [Console]::WriteLine('BURST_DONE')",
                ],
            )
        } else {
            (ShellKind::Cmd, vec!["/d", "/c", "echo hi"])
        };
        #[cfg(not(target_os = "windows"))]
        let (kind, args) = (
            ShellKind::Posix,
            vec![
                "-c",
                if burst {
                    "yes | head -c 50000000; printf BURST_DONE"
                } else {
                    "echo hi"
                },
            ],
        );
        let mut shell = available_shells()
            .into_iter()
            .find(|s| s.kind == kind)
            .unwrap();
        shell.args = args.into_iter().map(str::to_owned).collect();
        shell
    }
    #[test]
    fn spawned_echo_reaches_grid_within_two_seconds() {
        let session = Session::spawn(
            &command(false),
            &std::env::temp_dir(),
            80,
            24,
            Arc::new(|| {}),
        )
        .unwrap();
        let start = Instant::now();
        while !session.grid().screen().contents().contains("hi")
            && start.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            session.grid().screen().contents().contains("hi"),
            "grid={:?}, alive={}",
            session.grid().screen().contents(),
            session.is_alive()
        );
    }
    #[test]
    fn child_exit_is_detected_and_notified() {
        let notified = Arc::new(AtomicBool::new(false));
        let flag = notified.clone();
        let notify = Arc::new(move || flag.store(true, Ordering::Release));
        let session =
            Session::spawn(&command(false), &std::env::temp_dir(), 80, 24, notify).unwrap();
        let start = Instant::now();
        while session.is_alive() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!session.is_alive(), "exited child still reported alive");
        assert!(notified.load(Ordering::Acquire));
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn cd_reaches_an_idle_cmd_prompt() {
        let dir = std::env::temp_dir().join(format!("keel-cd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shell = available_shells()
            .into_iter()
            .find(|s| s.kind == ShellKind::Cmd)
            .unwrap();
        let session =
            Session::spawn(&shell, &std::env::temp_dir(), 120, 24, Arc::new(|| {})).unwrap();
        let wait = |needle: &str| {
            let start = Instant::now();
            while !session.grid().screen().contents().contains(needle) {
                assert!(start.elapsed() < Duration::from_secs(10), "no {needle:?}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait(">");
        session.cd(&dir); // sent now or queued until the first prompt is seen
        wait(&format!("{}>", dir.display()));
        let _ = std::fs::remove_dir(&dir);
    }
    #[test]
    fn fifty_mb_burst_keeps_grid_lock_under_fifty_ms() {
        let session = Session::spawn(
            &command(true),
            &std::env::temp_dir(),
            80,
            24,
            Arc::new(|| {}),
        )
        .unwrap();
        // Shared CI runners stall threads for tens of ms on their own; the 50 ms bound is
        // the real target on a developer machine.
        let limit = if std::env::var_os("CI").is_some() {
            Duration::from_millis(400)
        } else {
            Duration::from_millis(50)
        };
        let start = Instant::now();
        let mut samples = 0;
        loop {
            let at = Instant::now();
            let grid = session.grid();
            assert!(
                at.elapsed() < limit,
                "grid lock stalled: {:?}",
                at.elapsed()
            );
            let done = grid.screen().contents().contains("BURST_DONE");
            drop(grid);
            samples += 1;
            if done {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "burst never completed"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(samples > 1);
    }
    #[test]
    fn resize_updates_grid_and_kill_stops_child() {
        let shell = available_shells()
            .into_iter()
            .find(|s| matches!(s.kind, ShellKind::Cmd | ShellKind::Posix))
            .unwrap();
        let mut session =
            Session::spawn(&shell, &std::env::temp_dir(), 80, 24, Arc::new(|| {})).unwrap();
        assert!(session.is_alive());
        session.resize(100, 30).unwrap();
        assert_eq!(session.grid().screen().size(), (30, 100));
        session.kill();
        assert!(!session.is_alive());
    }
}
