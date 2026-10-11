//! Panics a caller catches on purpose (a damaged file a parser chokes on, a job that fails
//! and says so): `quiet(|| std::panic::catch_unwind(..))` marks this thread's panics inside
//! as expected, and the crash report hook (`keel_api::crash`) only logs them: no report
//! file, no "Keel crashed last time" dialog, and they do not use up the per-run budget.

use std::cell::Cell;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Runs `f` with panics on this thread marked as caught (nests).
pub fn quiet<R>(f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            DEPTH.with(|d| d.set(d.get() - 1));
        }
    }
    DEPTH.with(|d| d.set(d.get() + 1));
    let _reset = Reset;
    f()
}

/// Whether a panic on this thread now is inside [`quiet`].
pub fn is_quiet() -> bool {
    DEPTH.try_with(|d| d.get() > 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_nests_and_ends_with_its_closure() {
        assert!(!is_quiet());
        quiet(|| {
            assert!(is_quiet());
            quiet(|| assert!(is_quiet()));
            assert!(is_quiet(), "the inner one ended, the outer one still runs");
            assert!(!std::thread::spawn(is_quiet).join().unwrap(), "per thread");
        });
        assert!(!is_quiet());
    }
}
