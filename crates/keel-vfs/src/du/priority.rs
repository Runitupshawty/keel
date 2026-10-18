//! The CPU and I/O priority of a scan's threads ([`ScanOptions::priority`]), so a big scan
//! can run while the computer stays responsive.
//!
//! - **Windows:** the thread priority (below normal, lowest) and an I/O priority hint
//!   (low, very low) on every folder handle the scan opens to list it.
//! - **Linux:** the thread's nice value (10, 19) and its I/O class (best effort at the
//!   lowest level, idle) through `ioprio_set`.
//! - **macOS:** the thread's quality of service (utility, background) and its disk I/O
//!   policy (utility, throttle).
//!
//! Only the scan's own threads change; the rest of Keel keeps its priority. A lowered
//! priority cannot always be raised again without privileges, so threads are only ever
//! lowered (the scan pool's threads are new ones).
//!
//! [`ScanOptions::priority`]: super::ScanOptions::priority

use serde::{Deserialize, Serialize};

/// How much the scan yields to everything else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// As any other program.
    #[default]
    Normal,
    /// Below normal: the scan gives way to programs in use.
    Low,
    /// Only when nothing else needs the processor or the disk.
    Background,
}

impl Priority {
    pub const ALL: [Self; 3] = [Self::Normal, Self::Low, Self::Background];

    /// The name used in settings and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Low => "low",
            Self::Background => "background",
        }
    }
}

/// Gives the calling thread priority `p` (CPU and I/O). False when the system refused
/// some of it (or has no such setting); the scan then runs as it is.
pub fn apply(p: Priority) -> bool {
    imp::apply(p)
}

/// The calling thread's CPU priority as the system reports it, as the nearest
/// [`Priority`].
pub fn current() -> Priority {
    imp::current()
}

/// The calling thread's I/O priority as the system reports it (Windows: the hint its folder
/// handles get).
pub fn current_io() -> Priority {
    imp::current_io()
}

#[cfg(windows)]
pub(super) use imp::hint_handle;

#[cfg(windows)]
mod imp {
    use super::Priority;
    use std::cell::Cell;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FileIoPriorityHintInfo, SetFileInformationByHandle, FILE_IO_PRIORITY_HINT_INFO,
        IoPriorityHintLow, IoPriorityHintVeryLow,
    };
    use windows::Win32::System::Threading::{
        GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
    };

    thread_local! {
        /// The I/O hint this thread's folder handles get.
        static IO: Cell<Priority> = const { Cell::new(Priority::Normal) };
    }

    pub fn apply(p: Priority) -> bool {
        let level = match p {
            Priority::Normal => THREAD_PRIORITY_NORMAL,
            Priority::Low => THREAD_PRIORITY_BELOW_NORMAL,
            Priority::Background => THREAD_PRIORITY_LOWEST,
        };
        IO.with(|c| c.set(p));
        // SAFETY: the pseudo handle of the calling thread needs no closing.
        unsafe { SetThreadPriority(GetCurrentThread(), level) }.is_ok()
    }

    pub fn current() -> Priority {
        // SAFETY: as above.
        let level = unsafe { GetThreadPriority(GetCurrentThread()) };
        if level <= THREAD_PRIORITY_LOWEST.0 {
            Priority::Background
        } else if level < THREAD_PRIORITY_NORMAL.0 {
            Priority::Low
        } else {
            Priority::Normal
        }
    }

    pub fn current_io() -> Priority {
        IO.with(Cell::get)
    }

    /// Lowers the I/O priority of reads through `handle` to this thread's hint.
    pub fn hint_handle(handle: HANDLE) {
        let hint = match IO.with(Cell::get) {
            Priority::Normal => return,
            Priority::Low => IoPriorityHintLow,
            Priority::Background => IoPriorityHintVeryLow,
        };
        let info = FILE_IO_PRIORITY_HINT_INFO {
            PriorityHint: hint,
        };
        // SAFETY: `info` is the FILE_IO_PRIORITY_HINT_INFO of the size passed; `handle` is
        // open for the call.
        let r = unsafe {
            SetFileInformationByHandle(
                handle,
                FileIoPriorityHintInfo,
                (&info as *const FILE_IO_PRIORITY_HINT_INFO).cast(),
                std::mem::size_of::<FILE_IO_PRIORITY_HINT_INFO>() as u32,
            )
        };
        if let Err(e) = r {
            tracing::trace!("du: I/O priority hint: {e}");
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Priority;

    const WHO_PROCESS: libc::c_long = 1;
    const CLASS_BEST_EFFORT: libc::c_long = 2;
    const CLASS_IDLE: libc::c_long = 3;
    const CLASS_SHIFT: libc::c_long = 13;

    fn tid() -> libc::c_long {
        // SAFETY: gettid has no arguments and cannot fail.
        unsafe { libc::syscall(libc::SYS_gettid) }
    }

    pub fn apply(p: Priority) -> bool {
        let (nice, class, level) = match p {
            Priority::Normal => (0, CLASS_BEST_EFFORT, 4),
            Priority::Low => (10, CLASS_BEST_EFFORT, 7),
            Priority::Background => (19, CLASS_IDLE, 0),
        };
        // On Linux the nice value of PRIO_PROCESS with a thread id is that thread's own.
        // SAFETY: plain integer arguments.
        let cpu = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid() as libc::id_t, nice) } == 0;
        // SAFETY: ioprio_set(which, who, ioprio) takes integers only.
        let io = unsafe {
            libc::syscall(
                libc::SYS_ioprio_set,
                WHO_PROCESS,
                tid(),
                (class << CLASS_SHIFT) | level,
            )
        } == 0;
        cpu && io
    }

    pub fn current() -> Priority {
        // SAFETY: plain integer arguments; -1 is also a valid answer (a raised priority).
        let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid() as libc::id_t) };
        match nice {
            n if n >= 19 => Priority::Background,
            n if n >= 10 => Priority::Low,
            _ => Priority::Normal,
        }
    }

    pub fn current_io() -> Priority {
        // SAFETY: ioprio_get(which, who) takes integers only.
        let v = unsafe { libc::syscall(libc::SYS_ioprio_get, WHO_PROCESS, tid()) };
        if v < 0 {
            return Priority::Normal;
        }
        match (v >> CLASS_SHIFT, v & 0xff) {
            (CLASS_IDLE, _) => Priority::Background,
            (CLASS_BEST_EFFORT, l) if l >= 7 => Priority::Low,
            _ => Priority::Normal,
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Priority;

    // <pthread/qos.h>, <sys/resource.h>
    const QOS_CLASS_DEFAULT: u32 = 0x15;
    const QOS_CLASS_UTILITY: u32 = 0x11;
    const QOS_CLASS_BACKGROUND: u32 = 0x09;
    const IOPOL_TYPE_DISK: i32 = 0;
    const IOPOL_SCOPE_THREAD: i32 = 1;
    const IOPOL_DEFAULT: i32 = 0;
    const IOPOL_THROTTLE: i32 = 3;
    const IOPOL_UTILITY: i32 = 4;

    extern "C" {
        fn pthread_set_qos_class_self_np(qos: u32, relative: i32) -> i32;
        fn qos_class_self() -> u32;
        fn setiopolicy_np(kind: i32, scope: i32, policy: i32) -> i32;
        fn getiopolicy_np(kind: i32, scope: i32) -> i32;
    }

    pub fn apply(p: Priority) -> bool {
        let (qos, io) = match p {
            Priority::Normal => (QOS_CLASS_DEFAULT, IOPOL_DEFAULT),
            Priority::Low => (QOS_CLASS_UTILITY, IOPOL_UTILITY),
            Priority::Background => (QOS_CLASS_BACKGROUND, IOPOL_THROTTLE),
        };
        // SAFETY: plain integer arguments; both act on the calling thread only.
        unsafe {
            let cpu = pthread_set_qos_class_self_np(qos, 0) == 0;
            let disk = setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, io) == 0;
            cpu && disk
        }
    }

    pub fn current() -> Priority {
        // SAFETY: no arguments.
        match unsafe { qos_class_self() } {
            QOS_CLASS_BACKGROUND => Priority::Background,
            QOS_CLASS_UTILITY => Priority::Low,
            _ => Priority::Normal,
        }
    }

    pub fn current_io() -> Priority {
        // SAFETY: plain integer arguments.
        match unsafe { getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD) } {
            IOPOL_THROTTLE => Priority::Background,
            IOPOL_UTILITY => Priority::Low,
            _ => Priority::Normal,
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use super::Priority;

    pub fn apply(p: Priority) -> bool {
        p == Priority::Normal
    }

    pub fn current() -> Priority {
        Priority::Normal
    }

    pub fn current_io() -> Priority {
        Priority::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each priority applied on a thread of its own reads back the same from the system.
    #[test]
    fn priority_round_trips_through_the_system() {
        let base = std::thread::spawn(|| (current(), current_io())).join().unwrap();
        for p in [Priority::Low, Priority::Background] {
            let (ok, cpu, io) = std::thread::spawn(move || (apply(p), current(), current_io()))
                .join()
                .unwrap();
            if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
                assert!(ok, "{p:?} refused");
                assert_eq!((cpu, io), (p, p), "{p:?}");
            }
        }
        // Normal is only reachable from a thread that was not lowered already (a test run
        // under `nice` starts lowered).
        if base == (Priority::Normal, Priority::Normal) {
            let read = std::thread::spawn(|| (apply(Priority::Normal), current(), current_io()))
                .join()
                .unwrap();
            assert_eq!(read, (true, Priority::Normal, Priority::Normal));
        }
    }

    /// The setting's names (settings files and scan files keep them).
    #[test]
    fn priority_names_round_trip() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct S {
            priority: Priority,
        }
        for p in Priority::ALL {
            let text = toml::to_string(&S { priority: p }).unwrap();
            assert_eq!(text.trim(), format!("priority = \"{}\"", p.as_str()));
            assert_eq!(toml::from_str::<S>(&text).unwrap(), S { priority: p });
        }
    }
}
