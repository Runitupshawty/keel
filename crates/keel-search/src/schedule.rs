//! Rescan schedules ("every N hours", "daily at HH:MM") over a clock that tests can
//! replace.

use crate::index_settings::Schedule;

/// Unix seconds and the local UTC offset. [`SystemClock`] in the app; a fake in tests.
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now(&self) -> i64;
    /// Seconds east of UTC of the local time zone at `at` (Unix seconds).
    fn utc_offset(&self, at: i64) -> i32;
}

/// The real clock and time zone.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn utc_offset(&self, at: i64) -> i32 {
        use chrono::{Local, Offset, TimeZone};
        Local
            .timestamp_opt(at, 0)
            .single()
            .map_or(0, |t| t.offset().fix().local_minus_utc())
    }
}

const DAY: i64 = 24 * 3600;

/// "HH:MM" as seconds after local midnight; None when malformed.
pub fn parse_time_of_day(text: &str) -> Option<i64> {
    let (h, m) = text.trim().split_once(':')?;
    let (h, m): (i64, i64) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some(h * 3600 + m * 60)
}

impl Schedule {
    /// The first scheduled rescan after a scan at `last` (Unix seconds), None when
    /// never (or the time is malformed).
    pub fn next_after(&self, last: i64, clock: &dyn Clock) -> Option<i64> {
        match self {
            Schedule::Never => None,
            Schedule::EveryHours(0) => None,
            Schedule::EveryHours(hours) => Some(last.saturating_add(i64::from(*hours) * 3600)),
            Schedule::Daily(time) => {
                let at = parse_time_of_day(time)?;
                let offset = i64::from(clock.utc_offset(last));
                let local = last + offset;
                let midnight = local.div_euclid(DAY) * DAY;
                let mut next = midnight + at;
                if next <= local {
                    next += DAY;
                }
                // Back to UTC with the offset of that day (a DST change in between).
                let guess = next - offset;
                Some(next - i64::from(clock.utc_offset(guess)))
            }
        }
    }

    /// True when a rescan is due at `now` after a scan at `last`.
    pub fn due(&self, last: i64, now: i64, clock: &dyn Clock) -> bool {
        self.next_after(last, clock).is_some_and(|t| now >= t)
    }

    /// Seconds from `now` until the next scheduled rescan (0 when overdue).
    pub fn wait(&self, last: i64, now: i64, clock: &dyn Clock) -> Option<u64> {
        self.next_after(last, clock)
            .map(|t| u64::try_from(t - now).unwrap_or(0))
    }
}

/// A clock tests move by hand.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::atomic::{AtomicI64, Ordering::Relaxed};

    pub struct FakeClock {
        pub now: AtomicI64,
        pub offset: i32,
    }

    impl FakeClock {
        pub fn new(now: i64, offset: i32) -> Self {
            Self {
                now: AtomicI64::new(now),
                offset,
            }
        }

        pub fn advance(&self, secs: i64) {
            self.now.fetch_add(secs, Relaxed);
        }
    }

    impl super::Clock for FakeClock {
        fn now(&self) -> i64 {
            self.now.load(Relaxed)
        }

        fn utc_offset(&self, _at: i64) -> i32 {
            self.offset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeClock;
    use super::*;

    // 2026-10-12 00:00:00 UTC.
    const MIDNIGHT: i64 = 1_791_763_200;

    #[test]
    fn every_n_hours_counts_from_the_last_scan() {
        let clock = FakeClock::new(MIDNIGHT, 0);
        let s = Schedule::EveryHours(6);
        let last = MIDNIGHT + 3600;
        assert_eq!(s.next_after(last, &clock), Some(last + 6 * 3600));
        assert!(!s.due(last, last + 6 * 3600 - 1, &clock));
        assert!(s.due(last, last + 6 * 3600, &clock));
        assert_eq!(s.wait(last, last + 3600, &clock), Some(5 * 3600));
        assert_eq!(s.wait(last, last + 7 * 3600, &clock), Some(0));
        assert_eq!(Schedule::EveryHours(0).next_after(last, &clock), None);
        assert!(!Schedule::Never.due(0, i64::MAX, &clock));
    }

    #[test]
    fn daily_runs_at_the_local_time_after_the_last_scan() {
        // UTC+2: 03:30 local is 01:30 UTC.
        let clock = FakeClock::new(MIDNIGHT, 2 * 3600);
        let s = Schedule::Daily("03:30".into());
        // Scanned at 00:00 UTC (02:00 local): due the same day at 01:30 UTC.
        assert_eq!(s.next_after(MIDNIGHT, &clock), Some(MIDNIGHT + 3600 + 1800));
        // Scanned at 02:00 UTC (04:00 local), after today's run: tomorrow.
        let late = MIDNIGHT + 2 * 3600;
        assert_eq!(
            s.next_after(late, &clock),
            Some(MIDNIGHT + DAY + 3600 + 1800)
        );
        // Scanned exactly at the run time: the next day's.
        let exact = MIDNIGHT + 3600 + 1800;
        assert_eq!(s.next_after(exact, &clock), Some(exact + DAY));
        // Keel closed over the run time: due at once on the next start.
        assert!(s.due(MIDNIGHT, MIDNIGHT + 3 * DAY, &clock));
        // West of UTC: 23:00 local at UTC-5 is 04:00 UTC the next day.
        let west = FakeClock::new(MIDNIGHT, -5 * 3600);
        let s = Schedule::Daily("23:00".into());
        assert_eq!(s.next_after(MIDNIGHT, &west), Some(MIDNIGHT + 4 * 3600));
    }

    #[test]
    fn malformed_times_never_run() {
        let clock = FakeClock::new(MIDNIGHT, 0);
        for bad in ["", "3", "24:00", "12:60", "ab:cd", "-1:00"] {
            assert_eq!(
                Schedule::Daily(bad.into()).next_after(MIDNIGHT, &clock),
                None,
                "{bad:?}"
            );
        }
        assert_eq!(parse_time_of_day(" 7:05 "), Some(7 * 3600 + 300));
    }

    #[test]
    fn system_clock_is_sane() {
        let c = SystemClock;
        assert!(c.now() > MIDNIGHT - 365 * DAY);
        assert!(c.utc_offset(c.now()).abs() <= 14 * 3600);
    }
}
