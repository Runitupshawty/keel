//! Opt-in update check (Settings → General → "Check for updates", off by default; the
//! first start of each profile asks once with a toast). When on: at most once every 24 hours, on a worker
//! thread, one `GET` of GitHub's "latest release" of this repository ([`LATEST_URL`]) with
//! `User-Agent: keel/<version>` and nothing else: no identifiers, no settings, no
//! telemetry. A newer release shows a banner (What's new, Install, Skip this version).
//! Offline, rate limited (HTTP 403) or an unexpected answer: nothing is shown and the
//! check runs again a day later. `keel --check-update` checks once on the command line.

use crossbeam_channel::{Receiver, TryRecvError};
use std::path::{Path, PathBuf};

/// The only request the update check makes.
pub const LATEST_URL: &str = "https://api.github.com/repos/Runitupshawty/keel/releases/latest";
/// Release pages must be under this (the banner opens them in the browser).
const RELEASES: &str = "https://github.com/Runitupshawty/keel/releases/";
/// The README's install one-liner for this platform (it verifies `SHA256SUMS` itself).
pub const ONE_LINER: &str = if cfg!(windows) {
    "irm https://raw.githubusercontent.com/Runitupshawty/keel/main/scripts/install.ps1 | iex"
} else {
    "curl -fsSL https://raw.githubusercontent.com/Runitupshawty/keel/main/scripts/install.sh | bash"
};
/// At most one check per this many seconds.
pub const INTERVAL_SECS: i64 = 24 * 60 * 60;
/// The release JSON is a few KiB; anything larger is not what we asked for.
const MAX_JSON: u64 = 1 << 20;
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `[updates]` in config.toml.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct UpdateSettings {
    /// Check once a day (off until the user turns it on).
    pub check: bool,
    /// The one-time "check for updates?" toast was shown.
    pub asked: bool,
    /// Unix seconds of the last check (tried, whatever the answer).
    pub last_check: i64,
    /// "Skip this version": no banner for this version.
    pub skip: String,
}

/// A release worth offering.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// `X.Y.Z` (no `v`).
    pub version: String,
    /// Its page on GitHub (release notes and downloads).
    pub page: String,
}

#[derive(serde::Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

pub fn user_agent() -> String {
    format!("keel/{VERSION}")
}

/// The release in GitHub's answer `json` when it is newer than `current` and not `skip`.
/// Pre-releases are offered only to a running pre-release. Err: not a release JSON.
pub fn newer(json: &[u8], current: &str, skip: &str) -> anyhow::Result<Option<Release>> {
    let latest: Latest = serde_json::from_slice(json)?;
    let version = semver::Version::parse(latest.tag_name.trim_start_matches('v'))?;
    let current = semver::Version::parse(current)?;
    anyhow::ensure!(
        latest.html_url.starts_with(RELEASES),
        "unexpected release page {}",
        latest.html_url
    );
    let pre = latest.prerelease || !version.pre.is_empty();
    let offer = !latest.draft
        && (!pre || !current.pre.is_empty())
        && version > current
        && version.to_string() != skip.trim_start_matches('v');
    Ok(offer.then(|| Release {
        version: version.to_string(),
        page: latest.html_url,
    }))
}

/// One check against `url` (blocking). Ok(None): nothing newer.
pub fn check(url: &str, current: &str, skip: &str) -> anyhow::Result<Option<Release>> {
    newer(
        &keel_vfs::cloud::get_small(url, &user_agent(), MAX_JSON)?,
        current,
        skip,
    )
}

/// Whether a check is due at `now` (Unix seconds): on, and a day since the last one (or the
/// clock went back).
pub fn due(s: &UpdateSettings, now: i64) -> bool {
    s.check && !(s.last_check..s.last_check.saturating_add(INTERVAL_SECS)).contains(&now)
}

/// True once: the first start with the check off shows the question (marked asked here,
/// so a toast that times out counts too).
pub fn ask_once(s: &mut UpdateSettings) -> bool {
    let ask = !s.check && !s.asked;
    s.asked = true;
    ask
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `keel --check-update`: prints the result; exit 0 when it worked, 1 when not.
pub fn cli() -> i32 {
    match check(LATEST_URL, VERSION, "") {
        Ok(Some(r)) => {
            println!(
                "Keel {} is available (this is {VERSION}): {}",
                r.version, r.page
            );
            println!("Install: {ONE_LINER}");
            0
        }
        Ok(None) => {
            println!("Keel {VERSION} is up to date");
            0
        }
        Err(e) => {
            eprintln!("Could not check for updates: {e:#}");
            1
        }
    }
}

/// How the banner's Install installs.
#[derive(Debug, PartialEq)]
pub enum Install {
    /// Windows, installed by `install.ps1` (which keeps a copy next to keel.exe): run that
    /// local copy again, as the README says to update. It downloads the latest zip and
    /// checks it against the release's `SHA256SUMS` before it replaces anything.
    Script { script: PathBuf, dir: PathBuf },
    /// Anything else: open the release page and show the install one-liner.
    Page,
}

/// How the installer registers the install it updates.
#[derive(Debug, PartialEq)]
pub enum Registration {
    /// The Settings → Apps entry (and Start menu shortcut) of this name is this install's.
    Entry(String),
    /// No entry is this install's (it was installed with `-NoRegister`).
    NoRegister,
}

/// This install's registration from the Settings → Apps entries `(key name,
/// InstallLocation)`. None: no entry points at `dir` but the default one (`Keel`) belongs
/// to another install, whose shortcut and entry the installer would take over; the banner
/// opens the page instead.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn registration(entries: &[(String, String)], dir: &Path) -> Option<Registration> {
    let norm = |p: &str| {
        p.trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_lowercase()
    };
    let here = norm(&dir.to_string_lossy());
    if let Some((name, _)) = entries.iter().find(|(_, at)| norm(at) == here) {
        return Some(Registration::Entry(name.clone()));
    }
    let taken = entries
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("Keel"));
    (!taken).then_some(Registration::NoRegister)
}

/// `(key name, InstallLocation)` from `reg query <Uninstall> /s /v InstallLocation`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_reg_query(out: &str) -> Vec<(String, String)> {
    let mut key = None;
    let mut found = Vec::new();
    for line in out.lines() {
        if line.starts_with("HKEY_") {
            key = line.trim_end().rsplit('\\').next().map(str::to_owned);
        } else if let Some((name, rest)) = line.trim_start().split_once(" REG_") {
            // `REG_SZ` or `REG_EXPAND_SZ`, then the value.
            let value = rest.trim_start_matches(|c: char| !c.is_whitespace()).trim();
            if let (Some(key), "InstallLocation") = (&key, name.trim()) {
                found.push((key.clone(), value.to_owned()));
            }
        }
    }
    found
}

/// The Settings → Apps entries of this user that have an install folder.
fn uninstall_entries() -> std::io::Result<Vec<(String, String)>> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("reg.exe")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall",
                "/s",
                "/v",
                "InstallLocation",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        Ok(parse_reg_query(&String::from_utf8_lossy(&out.stdout)))
    }
    #[cfg(not(windows))]
    Ok(Vec::new())
}

/// `powershell.exe` arguments that run the installed `script` for `dir` with this install's
/// registration, keep the console open on an error until Enter is pressed, and start
/// keel.exe again afterwards (updated or not).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn install_args(script: &Path, dir: &Path, reg: &Registration) -> Vec<String> {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let path = |p: &Path| q(&p.to_string_lossy());
    let reg = match reg {
        Registration::Entry(name) => format!("-RegistryKeyName {}", q(name)),
        Registration::NoRegister => "-NoRegister".to_owned(),
    };
    let command = format!(
        "try {{ & {} -InstallDir {} {reg} }} catch {{ Write-Host $_ -ForegroundColor Red; \
         Read-Host 'The update failed. Press Enter to start Keel again' | Out-Null }}; \
         try {{ Start-Process -FilePath {} }} catch {{ Write-Host $_ -ForegroundColor Red; \
         Read-Host 'Press Enter to close' | Out-Null }}",
        path(script),
        path(dir),
        path(&dir.join("keel.exe")),
    );
    [
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &command,
    ]
    .map(str::to_owned)
    .to_vec()
}

pub fn install_plan(exe_dir: &Path) -> Install {
    let script = exe_dir.join("install.ps1");
    match cfg!(windows) && script.is_file() {
        true => Install::Script {
            script,
            dir: exe_dir.to_owned(),
        },
        false => Install::Page,
    }
}

/// The update check's state in the window.
#[derive(Default)]
pub struct Updates {
    rx: Option<Receiver<Option<Release>>>,
    pub banner: Option<Release>,
    /// Install opened the page: the banner shows the one-liner.
    hint: bool,
}

impl Updates {
    /// Starts a due check of `url` ([`LATEST_URL`]) on a worker and takes its answer (errors
    /// are logged only).
    pub fn tick(&mut self, s: &mut UpdateSettings, ctx: &egui::Context, url: &str) {
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(found) => {
                    self.rx = None;
                    self.banner = found.filter(|r| r.version != s.skip);
                }
                Err(TryRecvError::Disconnected) => self.rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let now = now();
        if self.rx.is_none() && due(s, now) {
            s.last_check = now;
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (ctx, skip, url) = (ctx.clone(), s.skip.clone(), url.to_owned());
            crate::worker::spawn("keel-update", move || {
                let found = check(&url, VERSION, &skip)
                    .map_err(|e| tracing::info!("update check: {e:#}"))
                    .ok()
                    .flatten();
                let _ = tx.send(found);
                ctx.request_repaint();
            });
            self.rx = Some(rx);
        }
    }

    /// The banner above the panes while a newer release is known.
    pub fn banner(&mut self, ctx: &egui::Context, s: &mut UpdateSettings) {
        let Some(r) = self.banner.clone() else {
            return;
        };
        egui::TopBottomPanel::top("keel-update").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong(format!("Keel {} is available", r.version));
                if ui.button("What's new").clicked() {
                    open_page(&r.page);
                }
                if ui
                    .button("Install")
                    .on_hover_text("Download and install it the way the README describes")
                    .clicked()
                {
                    self.install(ctx, &r);
                }
                if ui.button("Skip this version").clicked() {
                    s.skip = r.version.clone();
                    self.banner = None;
                }
                if ui
                    .small_button("×")
                    .on_hover_text("Hide until the next start")
                    .clicked()
                {
                    self.banner = None;
                }
            });
            if self.hint {
                ui.horizontal_wrapped(|ui| {
                    ui.label("To install, run in a terminal:");
                    ui.code(ONE_LINER);
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(ONE_LINER.to_owned());
                    }
                });
            }
        });
    }

    fn install(&mut self, ctx: &egui::Context, r: &Release) {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_owned));
        if let Some(Install::Script { script, dir }) = exe_dir.as_deref().map(install_plan) {
            let entries = uninstall_entries().map_err(|e| tracing::warn!("reg query: {e}"));
            match entries.ok().and_then(|e| registration(&e, &dir)) {
                Some(reg) => {
                    // A console window shows the installer's progress; it stops Keel before
                    // it copies, so the window closes first (settings are saved on close).
                    let started = std::process::Command::new("powershell.exe")
                        .args(install_args(&script, &dir, &reg))
                        .spawn();
                    match started {
                        Ok(_) => return ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(e) => tracing::warn!("run {}: {e}", script.display()),
                    }
                }
                None => tracing::info!("the default Start menu entry is another install's"),
            }
        }
        open_page(&r.page);
        self.hint = true;
    }
}

fn open_page(url: &str) {
    if let Err(e) = open::that_detached(url) {
        tracing::warn!("open {url}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-shot HTTP server on 127.0.0.1: answers `status` + `body` and hands back the
    /// request head it received.
    fn serve(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/repos/Runitupshawty/keel/releases/latest",
            listener.local_addr().unwrap()
        );
        let (status, body) = (status.to_owned(), body.to_owned());
        let thread = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.ends_with(b"\r\n\r\n") {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0, "request cut short");
                head.extend_from_slice(&buf[..n]);
            }
            let answer = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(answer.as_bytes()).unwrap();
            String::from_utf8(head).unwrap()
        });
        (url, thread)
    }

    fn release(tag: &str, prerelease: bool) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/Runitupshawty/keel/releases/tag/{tag}","prerelease":{prerelease},"draft":false,"assets":[],"body":"notes"}}"#
        )
    }

    fn check_against(status: &str, body: &str, current: &str) -> anyhow::Result<Option<Release>> {
        let (url, server) = serve(status, body);
        let found = check(&url, current, "");
        server.join().unwrap();
        found
    }

    #[test]
    fn sends_one_get_with_only_a_user_agent() {
        let (url, server) = serve("200 OK", &release("v9.0.0", false));
        let found = check(&url, "0.16.0", "").unwrap().unwrap();
        assert_eq!(found.version, "9.0.0");
        assert_eq!(
            found.page,
            "https://github.com/Runitupshawty/keel/releases/tag/v9.0.0"
        );
        let head = server.join().unwrap().to_ascii_lowercase();
        let mut lines = head.lines();
        assert_eq!(
            lines.next(),
            Some("get /repos/runitupshawty/keel/releases/latest http/1.1")
        );
        let mut names: Vec<&str> = lines
            .filter(|l| !l.is_empty())
            .map(|l| l.split(':').next().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["accept", "host", "user-agent"], "{head}");
        assert!(
            head.contains(&format!("user-agent: keel/{VERSION}\r\n")),
            "{head}"
        );
        assert!(head.contains("accept: */*\r\n"), "{head}");
    }

    #[test]
    fn only_a_newer_release_is_offered() {
        let newer = check_against("200 OK", &release("v0.17.0", false), "0.16.0").unwrap();
        assert_eq!(newer.unwrap().version, "0.17.0");
        for (tag, why) in [("v0.16.0", "same"), ("v0.15.2", "older")] {
            assert_eq!(
                check_against("200 OK", &release(tag, false), "0.16.0").unwrap(),
                None,
                "{why}"
            );
        }
    }

    #[test]
    fn pre_releases_only_for_a_running_pre_release() {
        for (tag, flag) in [
            ("v0.17.0-rc1", true),
            ("v0.17.0-rc1", false),
            ("v0.17.0", true),
        ] {
            assert_eq!(
                check_against("200 OK", &release(tag, flag), "0.16.0").unwrap(),
                None,
                "{tag} prerelease={flag}"
            );
        }
        let rc2 = newer(release("v0.17.0-rc2", true).as_bytes(), "0.17.0-rc1", "").unwrap();
        assert_eq!(rc2.unwrap().version, "0.17.0-rc2");
        let done = newer(release("v0.17.0", false).as_bytes(), "0.17.0-rc1", "").unwrap();
        assert_eq!(done.unwrap().version, "0.17.0");
    }

    #[test]
    fn malformed_and_rate_limited_answers_offer_nothing() {
        let foreign = r#"{"tag_name":"v9.0.0","html_url":"https://example.com/x"}"#;
        let limited = r#"{"message":"API rate limit exceeded"}"#;
        for (status, body) in [
            ("200 OK", "<html>not json</html>"),
            ("200 OK", r#"{"message":"Not Found"}"#),
            ("200 OK", release("latest", false).as_str()),
            ("200 OK", foreign),
            ("403 Forbidden", limited),
            ("500 Internal Server Error", ""),
        ] {
            assert!(
                check_against(status, body, "0.16.0").is_err(),
                "{status} {body}"
            );
        }
        // Nothing listening: an error too (the window stays silent on every error).
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(check(&format!("http://127.0.0.1:{port}/x"), "0.16.0", "").is_err());
        // Only HTTPS leaves the machine.
        assert!(check("http://example.com/x", "0.16.0", "").is_err());
    }

    #[test]
    fn skipped_version_is_not_offered_and_persists() {
        let json = release("v0.17.0", false);
        assert_eq!(newer(json.as_bytes(), "0.16.0", "0.17.0").unwrap(), None);
        assert!(newer(json.as_bytes(), "0.16.0", "0.16.5")
            .unwrap()
            .is_some());
        let s = crate::settings::Settings {
            updates: UpdateSettings {
                check: true,
                asked: true,
                last_check: 1_700_000_000,
                skip: "0.17.0".into(),
            },
            ..Default::default()
        };
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(text.contains("[updates]"), "{text}");
        let back: crate::settings::Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.updates, s.updates);
    }

    #[test]
    fn off_by_default_and_at_most_daily() {
        let mut s = crate::settings::Settings::default().updates;
        assert!(!s.check, "opt-in");
        assert!(!due(&s, 1_700_000_000), "never while off");
        s.check = true;
        assert!(due(&s, 1_700_000_000), "first check");
        s.last_check = 1_700_000_000;
        assert!(!due(&s, 1_700_000_000 + 60));
        assert!(!due(&s, 1_700_000_000 + INTERVAL_SECS - 1));
        assert!(due(&s, 1_700_000_000 + INTERVAL_SECS));
        assert!(due(&s, 1_700_000_000 - 60), "clock went back");
        // Old config files without the table: off, not asked.
        let old: crate::settings::Settings = toml::from_str("theme = 'dark'").unwrap();
        assert_eq!(old.updates, UpdateSettings::default());
    }

    #[test]
    fn first_start_asks_exactly_once() {
        let mut s = UpdateSettings::default();
        assert!(ask_once(&mut s));
        assert!(!ask_once(&mut s));
        assert!(!ask_once(&mut s));
        let mut on = UpdateSettings {
            check: true,
            ..Default::default()
        };
        assert!(!ask_once(&mut on), "already on: no question");
    }

    #[test]
    fn tick_makes_no_request_while_off_and_checks_the_given_url_when_on() {
        let idle = TcpListener::bind("127.0.0.1:0").unwrap();
        idle.set_nonblocking(true).unwrap();
        let idle_url = format!("http://{}/x", idle.local_addr().unwrap());
        let ctx = egui::Context::default();
        let mut updates = Updates::default();
        let mut s = UpdateSettings::default();
        for _ in 0..3 {
            updates.tick(&mut s, &ctx, &idle_url);
        }
        assert!(updates.rx.is_none(), "no check started");
        assert_eq!(s.last_check, 0);
        let nothing = idle.accept().map(|_| ()).unwrap_err();
        assert_eq!(
            nothing.kind(),
            std::io::ErrorKind::WouldBlock,
            "no request arrived"
        );
        // On: the one request goes to the URL given.
        let (url, server) = serve("200 OK", &release("v9.0.0", false));
        s.check = true;
        updates.tick(&mut s, &ctx, &url);
        assert!(server.join().unwrap().starts_with("GET /repos/"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while updates.banner.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            updates.tick(&mut s, &ctx, &url);
        }
        assert_eq!(updates.banner.map(|r| r.version).as_deref(), Some("9.0.0"));
        assert!(idle.accept().is_err(), "still nothing at the first address");
    }

    #[test]
    fn install_keeps_the_registration_of_the_install_it_updates() {
        let dir = Path::new(r"C:\Apps\Keel");
        let entry = |name: &str, at: &str| (name.to_owned(), at.to_owned());
        assert_eq!(
            registration(&[entry("Keel", r"c:\apps\keel\")], dir),
            Some(Registration::Entry("Keel".into()))
        );
        assert_eq!(
            registration(
                &[
                    entry("Keel", r"C:\Other"),
                    entry("KeelTest", r"C:\Apps\Keel")
                ],
                dir
            ),
            Some(Registration::Entry("KeelTest".into())),
            "a test install keeps its own name"
        );
        assert_eq!(
            registration(&[entry("Other", r"C:\Other")], dir),
            Some(Registration::NoRegister),
            "installed with -NoRegister"
        );
        assert_eq!(
            registration(&[entry("keel", r"C:\Other")], dir),
            None,
            "the default entry and shortcut are another install's"
        );
        let out = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Keel\r\n    \
                   InstallLocation    REG_SZ    C:\\Apps\\Keel\r\n\r\n\
                   HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Other App\r\n    \
                   InstallLocation    REG_EXPAND_SZ    C:\\Program Files\\Other App\r\n\r\n\
                   End of search: 2 match(es) found.\r\n";
        assert_eq!(
            parse_reg_query(out),
            [
                entry("Keel", r"C:\Apps\Keel"),
                entry("Other App", r"C:\Program Files\Other App")
            ]
        );
        assert_eq!(parse_reg_query(""), []);
    }

    #[test]
    fn install_args_pause_on_error_and_start_keel_again() {
        let dir = Path::new(r"C:\Apps\Keel's copy");
        let script = dir.join("install.ps1");
        let args = install_args(&script, dir, &Registration::Entry("Keel".into()));
        assert_eq!(
            args[..4],
            ["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]
        );
        assert_eq!(args.len(), 5);
        let command = &args[4];
        let sep = std::path::MAIN_SEPARATOR;
        assert!(
            command.starts_with(&format!(
                "try {{ & 'C:\\Apps\\Keel''s copy{sep}install.ps1' -InstallDir 'C:\\Apps\\Keel''s copy' \
                 -RegistryKeyName 'Keel' }} catch {{ Write-Host $_"
            )),
            "{command}"
        );
        assert!(
            command.contains("Read-Host 'The update failed."),
            "{command}"
        );
        assert!(
            command.contains(&format!(
                "Start-Process -FilePath 'C:\\Apps\\Keel''s copy{sep}keel.exe'"
            )),
            "{command}"
        );
        assert!(
            !command.contains('"'),
            "no double quotes to escape: {command}"
        );
        let bare = install_args(&script, dir, &Registration::NoRegister);
        assert!(bare[4].contains("' -NoRegister }"), "{}", bare[4]);
        assert!(!bare[4].contains("RegistryKeyName"));
    }

    #[test]
    fn install_runs_the_local_installer_only_where_one_is_installed() {
        let dir = std::env::temp_dir().join(format!("keel-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("install.ps1"));
        assert_eq!(install_plan(&dir), Install::Page);
        std::fs::write(dir.join("install.ps1"), "").unwrap();
        let want = match cfg!(windows) {
            true => Install::Script {
                script: dir.join("install.ps1"),
                dir: dir.clone(),
            },
            false => Install::Page,
        };
        assert_eq!(install_plan(&dir), want);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
