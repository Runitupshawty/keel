//! What a `library.changed` makes the web client read again, as the desktop window does
//! (keel-app's `library::refresh_for`): only what that kind of change can touch, and
//! changes arriving together read once ([`Coalesce`], [`WINDOW`]).

use serde_json::Value;

/// Changes within this many seconds of the first one are read again together.
pub const WINDOW: f64 = 0.25;

/// What to read again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    pub sources: bool,
    pub jobs: bool,
    /// The panes' listings.
    pub listing: bool,
    /// The selected file (its tags and favorite state).
    pub stat: bool,
    pub devices: bool,
    pub inbox: bool,
}

impl Refresh {
    pub const ALL: Refresh = Refresh {
        sources: true,
        jobs: true,
        listing: true,
        stat: true,
        devices: true,
        inbox: true,
    };

    pub fn is_empty(&self) -> bool {
        *self == Refresh::default()
    }

    fn or(self, o: Refresh) -> Refresh {
        Refresh {
            sources: self.sources || o.sources,
            jobs: self.jobs || o.jobs,
            listing: self.listing || o.listing,
            stat: self.stat || o.stat,
            devices: self.devices || o.devices,
            inbox: self.inbox || o.inbox,
        }
    }
}

/// What a `library.changed` (its `method` and `kind`) touches; an unknown kind, or none
/// (an older daemon), everything.
pub fn of(params: &Value) -> Refresh {
    let kind = params["kind"].as_str().unwrap_or_default();
    // A sidecar job's news: thumbnails are made on request here.
    if params["method"] == "job" {
        return Refresh::default();
    }
    let none = Refresh::default();
    match kind {
        // `library.sync`: another device's tags and favorites arrived.
        "tags.add" | "tags.remove" | "tags.set" | "favorites.set" | "library.sync" => {
            Refresh { stat: true, ..none }
        }
        // Job starters: `job.progress` follows the job.
        "hashing.set" | "integrity.check" | "media.index" | "jobs.cancel" | "spacedrop.send" => {
            Refresh { jobs: true, ..none }
        }
        "sources.index" => Refresh {
            sources: true,
            jobs: true,
            ..none
        },
        "sources.add" | "sources.remove" => Refresh {
            sources: true,
            listing: true,
            ..none
        },
        "plan" => Refresh {
            jobs: true,
            listing: true,
            stat: true,
            ..none
        },
        "devices.pair_with"
        | "devices.forget"
        | "devices.settings_set"
        | "shares.grant"
        | "shares.revoke" => Refresh {
            devices: true,
            ..none
        },
        "spacedrop.answer" => Refresh {
            inbox: true,
            ..none
        },
        // Nothing this client shows.
        "protection.recount" | "recents.note" | "volumes.set" | "devices.pair_code"
        | "mounts.add" | "mounts.remove" => none,
        _ => Refresh::ALL,
    }
}

/// Changes waiting to be read again, from the first one's arrival.
#[derive(Debug, Default)]
pub struct Coalesce {
    pending: Refresh,
    since: Option<f64>,
}

impl Coalesce {
    /// A change at `now` (seconds).
    pub fn add(&mut self, r: Refresh, now: f64) {
        if r.is_empty() {
            return;
        }
        self.pending = self.pending.or(r);
        self.since.get_or_insert(now);
    }

    /// What to read now, once [`WINDOW`] has passed since the first change.
    pub fn due(&mut self, now: f64) -> Option<Refresh> {
        let since = self.since?;
        if now - since < WINDOW {
            return None;
        }
        self.since = None;
        Some(std::mem::take(&mut self.pending))
    }

    /// Seconds until [`Coalesce::due`] has something, if anything waits.
    pub fn wait(&self, now: f64) -> Option<f64> {
        self.since.map(|s| (s + WINDOW - now).max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn change(kind: &str) -> Refresh {
        of(&json!({"method": "execute", "kind": kind}))
    }

    #[test]
    fn each_kind_reads_only_what_it_touches() {
        let tagged = change("tags.add");
        assert_eq!(
            tagged,
            Refresh {
                stat: true,
                ..Refresh::default()
            }
        );
        assert!(change("hashing.set").jobs && !change("hashing.set").listing);
        assert!(change("sources.add").sources && change("sources.add").listing);
        assert!(change("plan").listing && change("plan").jobs);
        assert!(change("shares.revoke").devices && !change("shares.revoke").sources);
        assert!(change("spacedrop.answer").inbox);
        let synced = of(&json!({"method": "library.sync", "kind": "library.sync"}));
        assert_eq!(synced, tagged, "another device's tags");
        for no_op in [
            "protection.recount",
            "recents.note",
            "volumes.set",
            "devices.pair_code",
            "mounts.add",
        ] {
            assert!(change(no_op).is_empty(), "{no_op}");
        }
        // A sidecar job's news changes nothing shown here.
        let news = json!({"method": "job", "kind": "media.index", "job": 3, "done": false});
        assert!(of(&news).is_empty());
        // Unknown, or an older daemon that names none: everything.
        assert_eq!(change("something.new"), Refresh::ALL);
        assert_eq!(of(&json!({"method": "execute"})), Refresh::ALL);
    }

    #[test]
    fn changes_within_the_window_are_read_once() {
        let mut c = Coalesce::default();
        assert_eq!(c.due(0.0), None);
        assert_eq!(c.wait(0.0), None);
        c.add(change("recents.note"), 0.0);
        assert_eq!(c.wait(0.0), None, "a no-op waits for nothing");
        c.add(change("tags.add"), 1.0);
        c.add(change("hashing.set"), 1.1);
        c.add(change("tags.remove"), 1.2);
        assert_eq!(c.due(1.2), None);
        assert!((c.wait(1.1).unwrap() - 0.15).abs() < 1e-9);
        let r = c.due(1.25).unwrap();
        assert!(r.stat && r.jobs && !r.listing && !r.sources, "{r:?}");
        assert_eq!(c.due(2.0), None, "read once");
        // The next change starts a new window.
        c.add(change("sources.add"), 3.0);
        assert_eq!(c.due(3.1), None);
        assert!(c.due(3.3).unwrap().sources);
    }
}
