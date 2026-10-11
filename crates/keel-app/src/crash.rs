//! The crash report dialog. Reports are written by `keel_api::crash` (a panic on any thread
//! writes `<data dir>/crashes/<time>.txt`, scrubbed of user, server and profile names). The dialog
//! comes up at start for a crash file not offered yet, and when a frame panicked (the app
//! catches it in `App::update`, resets the panes and keeps running).

use std::path::{Path, PathBuf};

/// GitHub's new-issue page; the dialog fills in the title, a template and the version.
const NEW_ISSUE: &str = "https://github.com/Runitupshawty/keel/issues/new";

/// Installs the crash report hook. Called once from `main` before the window opens.
pub fn install_panic_hook() {
    keel_api::crash::install_panic_hook("keel", dir());
}

/// `<data dir>/crashes` (`KEEL_DATA_DIR` moves it; a temp folder in tests).
pub fn dir() -> Option<PathBuf> {
    crate::settings::data_dir().map(|d| d.join("crashes"))
}

/// What the dialog shows.
#[derive(Debug, PartialEq)]
pub struct Prompt {
    pub title: &'static str,
    /// None when the report could not be written.
    pub file: Option<PathBuf>,
    /// The scrubbed report.
    pub text: String,
}

impl Prompt {
    /// The newest crash file in `dir` when it was not offered before (each file once), and
    /// `ask` ("Don't ask again" was not chosen).
    pub fn at_start(dir: &Path, ask: bool) -> Option<Self> {
        let file = keel_api::crash::unseen(dir).filter(|_| ask)?;
        keel_api::crash::mark_seen(dir, &file);
        let text = std::fs::read_to_string(&file)
            .map_err(|e| tracing::warn!("{}: {e}", file.display()))
            .ok()?;
        Some(Self {
            title: "Keel crashed last time",
            file: Some(file),
            text,
        })
    }

    /// After a frame panicked: this run's report (offered now, so not again at start).
    pub fn this_run() -> Self {
        let (file, text) = keel_api::crash::last()
            .unwrap_or_else(|| (None, "No crash report could be written.".to_owned()));
        if let Some((file, dir)) = file.as_deref().and_then(|f| Some((f, f.parent()?))) {
            keel_api::crash::mark_seen(dir, file);
        }
        Self {
            title: "Keel hit a bug and reset the panes",
            file,
            text,
        }
    }
}

/// The new-issue page with a title, the bug template and `version` filled in: nothing
/// from the report (the user pastes it after reading it).
pub fn issue_url(version: &str) -> String {
    let body = format!(
        "## What happened\n\n\n## Steps to reproduce\n\n1.\n\n## Keel version\n\n{version}\n\n\
         ## Crash report\n\n<!-- Paste the report from \"Copy for a bug report\" here. -->\n"
    );
    format!(
        "{NEW_ISSUE}?labels=bug&title={}&body={}",
        encode("[Crash] "),
        encode(&body)
    )
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn open(target: impl AsRef<std::ffi::OsStr>) {
    if let Err(e) = open::that_detached(target.as_ref()) {
        tracing::warn!("open {}: {e}", target.as_ref().to_string_lossy());
    }
}

/// The dialog while `prompt` is Some; it goes None when closed. "Don't ask again" clears
/// `ask` (Settings → General → Crash reports turns it back on).
pub fn modal(ctx: &egui::Context, prompt: &mut Option<Prompt>, ask: &mut bool) {
    let Some(p) = prompt.as_ref() else {
        return;
    };
    let mut close = false;
    let r = egui::Modal::new(egui::Id::new("keel-crash")).show(ctx, |ui| {
        ui.set_max_width(480.0);
        ui.heading(p.title);
        ui.label(
            "A report was saved on this computer. It has the Keel version, the system, where \
             the error happened and the last log lines. Your user name, server names and \
             e-mail addresses are taken out, but file and folder names can appear: read it \
             before you post it. Nothing is sent unless you send it.",
        );
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if let Some(file) = &p.file {
                if ui
                    .button("Show report")
                    .on_hover_text(file.display().to_string())
                    .clicked()
                {
                    open(file);
                }
            }
            if ui.button("Copy for a bug report").clicked() {
                ui.ctx().copy_text(p.text.clone());
            }
            if ui
                .button("Open an issue")
                .on_hover_text("GitHub's new-issue page with the Keel version filled in")
                .clicked()
            {
                open(issue_url(env!("CARGO_PKG_VERSION")));
            }
            if ui
                .button("Don't ask again")
                .on_hover_text("No dialog at start for later crashes")
                .clicked()
            {
                *ask = false;
                close = true;
            }
            if ui.button("Close").clicked() {
                close = true;
            }
        });
    });
    if close || r.should_close() {
        *prompt = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_dialog_offers_each_crash_file_once() {
        let dir = std::env::temp_dir().join(format!("keel-crash-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Prompt::at_start(&dir, true), None, "no crashes");
        let file = keel_api::crash::write(&dir, "report one").unwrap();
        assert_eq!(Prompt::at_start(&dir, false), None, "don't ask again");
        let p = Prompt::at_start(&dir, true).expect("a new crash file");
        assert_eq!(p.file.as_deref(), Some(file.as_path()));
        assert_eq!(p.text, "report one");
        assert_eq!(Prompt::at_start(&dir, true), None, "once per file");
        keel_api::crash::write(&dir, "report two").unwrap();
        assert_eq!(Prompt::at_start(&dir, true).unwrap().text, "report two");
        assert_eq!(Prompt::at_start(&dir, true), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_issue_url_has_the_version_and_no_path() {
        let url = issue_url("0.16.0");
        assert!(url.starts_with("https://github.com/Runitupshawty/keel/issues/new?"));
        assert!(url.contains("0.16.0"), "{url}");
        assert!(url.contains("title=%5BCrash%5D%20"), "{url}");
        let query = &url[url.find('?').unwrap()..];
        for bad in ["/", "\\", "%2F", "%5C", "~", "Users", "home"] {
            assert!(!query.contains(bad), "{bad} in {query}");
        }
        assert_eq!(encode("a b/ü"), "a%20b%2F%C3%BC");
    }
}
