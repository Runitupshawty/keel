use std::process::{Command, Stdio};

use crate::{command_hits, Hit, Query, Searcher};

/// Linux search through `plocate` or `locate`.
pub(crate) struct Locate(&'static str);

impl Locate {
    /// `None` when no locate database is readable.
    pub(crate) fn new() -> Option<Self> {
        ["plocate", "locate"].into_iter().find_map(|tool| {
            // "/" matches every indexed path, so success means the tool runs
            // and its database is readable.
            Command::new(tool)
                .args(["-l", "1", "--", "/"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
                .then_some(Self(tool))
        })
    }
}

impl Searcher for Locate {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut command = Command::new(self.0);
        if !query.match_case {
            command.arg("-i");
        }
        if query.regex {
            command.arg("--regex");
        }
        // `in:<folder>` keeps the hits under it (locate has no such filter of its own).
        let (within, text) = match query.regex {
            false => crate::take_within(&query.text),
            true => (None, query.text.clone()),
        };
        command.arg("--").arg(&text);
        command_hits(command, query, within.as_deref().map(std::path::Path::new))
    }

    fn available(&self) -> bool {
        true
    }
}
