use std::process::{Command, Stdio};

use crate::{command_hits, Hit, Query, Searcher};

/// macOS search through Spotlight's `mdfind`.
pub(crate) struct Spotlight;

impl Spotlight {
    /// `None` when `mdfind` cannot run.
    pub(crate) fn new() -> Option<Self> {
        Command::new("mdfind")
            .args(["-count", "-onlyin", "/tmp", "keel"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
            .then_some(Self)
    }
}

impl Searcher for Spotlight {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        // `in:<folder>` is Spotlight's own `-onlyin`.
        let (within, text) = match query.regex {
            false => crate::take_within(&query.text),
            true => (None, query.text.clone()),
        };
        let mut command = Command::new("mdfind");
        command.arg("-onlyin").arg(within.as_deref().unwrap_or("/"));
        if query.folders_only && text.is_empty() {
            command.arg("kMDItemContentTypeTree == \"public.folder\"");
        } else if query.folders_only {
            command.arg("-name").arg(&text);
        } else {
            command.arg(&text);
        }
        command_hits(command, query, None)
    }

    fn available(&self) -> bool {
        true
    }
}
