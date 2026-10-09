use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::{command_hits, walk, Hit, Query, Searcher};

/// Linux search: `plocate`, then `locate`, else a walk of `$HOME`.
pub(crate) enum Locate {
    Tool(&'static str),
    Walk(PathBuf),
}

impl Locate {
    /// `None` when no locate database is readable and `$HOME` is missing.
    pub(crate) fn new() -> Option<Self> {
        for tool in ["plocate", "locate"] {
            // "/" matches every indexed path, so success means the tool runs
            // and its database is readable.
            let works = Command::new(tool)
                .args(["-l", "1", "--", "/"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if works {
                return Some(Self::Tool(tool));
            }
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_dir())
            .map(Self::Walk)
    }
}

impl Searcher for Locate {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        match self {
            Self::Tool(tool) => {
                let mut command = Command::new(tool);
                if !query.match_case {
                    command.arg("-i");
                }
                if query.regex {
                    command.arg("--regex");
                }
                command.arg("--").arg(&query.text);
                command_hits(command, query)
            }
            Self::Walk(home) => Ok(walk(home, query)),
        }
    }

    fn available(&self) -> bool {
        true
    }
}
