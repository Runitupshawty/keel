//! A ReadDirectoryChangesW watcher with a chosen buffer size (the `notify` crate's is
//! fixed at 16 KiB) that reports an overflowed buffer, so the index can rescan instead
//! of silently missing changes. One thread per watched folder. The first read is queued
//! (overlapped) before `watch` returns: Windows records a folder's changes only from the
//! first read on, so a change made right after `watch` is never missed.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FILE_LIST_DIRECTORY: u32 = 0x1;
const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
/// File and folder names, attributes, size, last write and creation.
const FILTER: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x40;
const ERROR_INVALID_PARAMETER: i32 = 87;
/// A read finished with more changes than its buffer held.
const ERROR_NOTIFY_ENUM_DIR: i32 = 1022;
/// The largest buffer a network share accepts.
const NETWORK_MAX: usize = 64 * 1024;
pub const MIN_BUFFER: usize = 4 * 1024;
pub const MAX_BUFFER: usize = 4 * 1024 * 1024;

#[link(name = "kernel32")]
extern "system" {
    fn ReadDirectoryChangesW(
        directory: *mut c_void,
        buffer: *mut c_void,
        length: u32,
        subtree: i32,
        filter: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
        routine: *mut c_void,
    ) -> i32;
    fn CancelIoEx(file: *mut c_void, overlapped: *mut c_void) -> i32;
    fn GetOverlappedResult(
        file: *mut c_void,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
}

/// OVERLAPPED; no event: one read at a time, so the wait is on the folder handle.
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut c_void,
}

/// A read's buffer and OVERLAPPED, both on the heap: they stay put while a read is queued.
struct Read {
    /// u64 words: the buffer must be DWORD-aligned.
    buf: Vec<u64>,
    len: usize,
    ov: Box<Overlapped>,
}

// SAFETY: `ov.event` is always null; the buffer and OVERLAPPED move between threads only
// while no read is queued or with the read queued on them (heap addresses do not move).
unsafe impl Send for Read {}

impl Read {
    /// Queues the next read; a share that refuses the buffer size gets 64 KiB.
    fn arm(&mut self, dir: &File) -> std::io::Result<()> {
        loop {
            *self.ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                event: std::ptr::null_mut(),
            };
            // SAFETY: the handle is open; `buf` holds `len` bytes and, like `ov`, stays
            // allocated until the read completes (the caller waits for it or cancels it).
            let ok = unsafe {
                ReadDirectoryChangesW(
                    dir.as_raw_handle(),
                    self.buf.as_mut_ptr().cast(),
                    self.len as u32,
                    1,
                    FILTER,
                    std::ptr::null_mut(),
                    std::ptr::from_mut(&mut *self.ov).cast(),
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER) && self.len > NETWORK_MAX {
                self.len = NETWORK_MAX;
                self.buf.truncate(self.len / 8);
                continue;
            }
            return Err(error);
        }
    }
}

/// What a watch reports.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Paths that changed (created, deleted, renamed from or to, modified).
    Paths(Vec<PathBuf>),
    /// Renames as (old, new) pairs (both paths are also in the `Paths` sent before).
    Renamed(Vec<(PathBuf, PathBuf)>),
    /// More changes than the buffer holds: some were lost.
    Overflow,
    /// The watch ended (the folder or its volume went away).
    Lost,
}

/// A running watch; dropping it stops the thread.
pub struct Watch {
    dir: Arc<File>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Watches `root` and everything below it with a buffer of `buffer` bytes (clamped
/// to [`MIN_BUFFER`]..=[`MAX_BUFFER`]; a share that refuses it gets 64 KiB).
pub fn watch(
    root: &Path,
    buffer: usize,
    send: impl Fn(Event) + Send + 'static,
) -> std::io::Result<Watch> {
    let dir = Arc::new(
        std::fs::OpenOptions::new()
            .read(true)
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED)
            .open(root)?,
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (thread_dir, thread_stop, root) = (dir.clone(), stop.clone(), root.to_path_buf());
    // The thread starts first and takes the read once it is queued: a thread that cannot
    // start never leaves a queued read on a freed buffer.
    let (hand_over, take) = std::sync::mpsc::channel::<Read>();
    let thread = std::thread::Builder::new()
        .name("keel-watch".into())
        .spawn(move || {
            let Ok(mut read) = take.recv() else { return };
            loop {
                let mut got = 0u32;
                // SAFETY: the handle stays open while `thread_dir` lives; `read.ov` is the
                // queued read's OVERLAPPED.
                let ok = unsafe {
                    GetOverlappedResult(thread_dir.as_raw_handle(), &mut *read.ov, &mut got, 1)
                };
                if thread_stop.load(Relaxed) {
                    break;
                }
                let error = (ok == 0).then(std::io::Error::last_os_error);
                let bytes: Vec<u8> = match &error {
                    None => (read.buf.iter())
                        .flat_map(|w| w.to_ne_bytes())
                        .take((got as usize).min(read.len))
                        .collect(),
                    Some(_) => Vec::new(),
                };
                // Queued again before the events go out; changes meanwhile are kept.
                let rearmed = read.arm(&thread_dir);
                match error {
                    Some(e) if e.raw_os_error() != Some(ERROR_NOTIFY_ENUM_DIR) => {
                        send(Event::Lost);
                        if rearmed.is_ok() {
                            // SAFETY: the handle is open; the read just queued is cancelled
                            // and waited for before its buffer goes.
                            unsafe {
                                CancelIoEx(thread_dir.as_raw_handle(), std::ptr::null_mut());
                                GetOverlappedResult(
                                    thread_dir.as_raw_handle(),
                                    &mut *read.ov,
                                    &mut got,
                                    1,
                                );
                            }
                        }
                        break;
                    }
                    Some(_) => send(Event::Overflow),
                    None if bytes.is_empty() => send(Event::Overflow),
                    None => {
                        let records: Vec<(u32, PathBuf)> = parse_actions(&bytes)
                            .into_iter()
                            .map(|(action, name)| {
                                (action, root.join(std::ffi::OsString::from_wide(&name)))
                            })
                            .collect();
                        let renames = rename_pairs(&records);
                        let paths: Vec<PathBuf> = records.into_iter().map(|(_, p)| p).collect();
                        if !paths.is_empty() {
                            send(Event::Paths(paths));
                        }
                        if !renames.is_empty() {
                            send(Event::Renamed(renames));
                        }
                    }
                }
                if rearmed.is_err() {
                    send(Event::Lost);
                    break;
                }
            }
        })?;
    let len = buffer.clamp(MIN_BUFFER, MAX_BUFFER);
    let mut read = Read {
        buf: vec![0u64; len / 8],
        len,
        ov: Box::new(Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: std::ptr::null_mut(),
        }),
    };
    // On error `hand_over` drops with nothing queued and the thread ends.
    read.arm(&dir)?;
    let _ = hand_over.send(read);
    Ok(Watch {
        dir,
        stop,
        thread: Some(thread),
    })
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Cancel the queued read until the thread is out of its wait.
        let end = Instant::now() + Duration::from_secs(2);
        while !thread.is_finished() && Instant::now() < end {
            // SAFETY: the handle is open (this struct holds the file).
            unsafe { CancelIoEx(self.dir.as_raw_handle(), std::ptr::null_mut()) };
            std::thread::sleep(Duration::from_millis(5));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
}

/// The relative names of [`parse_actions`].
#[cfg(test)]
pub fn parse(buf: &[u8]) -> Vec<Vec<u16>> {
    parse_actions(buf).into_iter().map(|(_, n)| n).collect()
}

const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

/// (old, new) for each renamed-from record followed by its renamed-to record.
pub fn rename_pairs(records: &[(u32, PathBuf)]) -> Vec<(PathBuf, PathBuf)> {
    records
        .windows(2)
        .filter(|w| {
            w[0].0 == FILE_ACTION_RENAMED_OLD_NAME && w[1].0 == FILE_ACTION_RENAMED_NEW_NAME
        })
        .map(|w| (w[0].1.clone(), w[1].1.clone()))
        .collect()
}

/// Each record of a buffer of FILE_NOTIFY_INFORMATION records: its action (FILE_ACTION_*)
/// and relative name (UTF-16, as stored). Every offset and length is checked: a short or
/// garbled buffer ends the list, never reads past it.
pub fn parse_actions(buf: &[u8]) -> Vec<(u32, Vec<u16>)> {
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_ne_bytes(
            buf.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    let mut names = Vec::new();
    let mut at = 0usize;
    while let (Some(next), Some(len)) = (u32_at(at), u32_at(at + 8)) {
        let start = at + 12;
        let Some(name) = start
            .checked_add(len as usize)
            .and_then(|end| buf.get(start..end))
        else {
            break;
        };
        if !name.is_empty() && name.len() % 2 == 0 {
            names.push((
                u32_at(at + 4).unwrap_or(0),
                name.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_ne_bytes(*c))
                    .collect(),
            ));
        }
        if next == 0 || next % 4 != 0 {
            break;
        }
        at = match at.checked_add(next as usize) {
            Some(n) => n,
            None => break,
        };
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let wide: Vec<u16> = name.encode_utf16().collect();
        let mut b = Vec::new();
        b.extend(next.to_ne_bytes());
        b.extend(action.to_ne_bytes());
        b.extend(((wide.len() * 2) as u32).to_ne_bytes());
        for w in wide {
            b.extend(w.to_ne_bytes());
        }
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b
    }

    #[test]
    fn parses_records_and_survives_garbage() {
        let first = record(0, 1, "a\\b.txt");
        // `record` pads to 4 bytes, so the next one starts right after it.
        let next = record(0, 4, "old").len() as u32;
        let mut buf = record(next, 4, "old");
        buf.extend(record(0, 5, "new name"));
        let names: Vec<String> = parse(&buf)
            .iter()
            .map(|n| String::from_utf16_lossy(n))
            .collect();
        assert_eq!(names, ["old", "new name"]);
        assert_eq!(parse(&first).len(), 1);
        // The actions come along; a renamed-from record then a renamed-to one is a pair.
        let records: Vec<(u32, PathBuf)> = parse_actions(&buf)
            .into_iter()
            .map(|(a, n)| (a, PathBuf::from(String::from_utf16_lossy(&n))))
            .collect();
        assert_eq!(records.iter().map(|r| r.0).collect::<Vec<_>>(), [4, 5]);
        assert_eq!(
            rename_pairs(&records),
            [(PathBuf::from("old"), PathBuf::from("new name"))]
        );
        assert!(rename_pairs(&records[1..]).is_empty());
        // Truncated, lying lengths, huge offsets, odd offsets: no panic.
        for cut in 0..buf.len() {
            let _ = parse(&buf[..cut]);
        }
        let mut lie = record(0, 1, "x");
        lie[8..12].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(parse(&lie).is_empty());
        let mut far = record(u32::MAX - 3, 1, "x");
        far.extend(record(0, 1, "y"));
        assert_eq!(parse(&far).len(), 1);
        let mut seed = 0x9e37_79b9_u32;
        for _ in 0..2000 {
            let len = (seed % 200) as usize;
            let junk: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            let _ = parse(&junk);
        }
    }

    #[test]
    fn reports_changes_and_stops_on_drop() {
        let root = std::env::temp_dir().join(format!("keel-rdcw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let w = watch(&root, 8 * 1024, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        // Written once, right away: the first read is queued before `watch` returns.
        std::fs::write(root.join("sub").join("new.txt"), b"x").unwrap();
        let mut seen = Vec::new();
        let end = Instant::now() + Duration::from_secs(10);
        while !seen.contains(&root.join("sub").join("new.txt")) {
            assert!(Instant::now() < end, "no event, got {seen:?}");
            if let Ok(Event::Paths(p)) = rx.recv_timeout(Duration::from_millis(100)) {
                seen.extend(p);
            }
        }
        let started = Instant::now();
        drop(w);
        assert!(started.elapsed() < Duration::from_secs(2), "drop is prompt");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
