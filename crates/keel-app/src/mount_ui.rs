//! Mount… (Phase 9 follow-up): a library source, or a folder inside one, as a drive letter
//! or mount folder through `mounts.add` on keel-daemon. The app is not the mounting
//! process: it talks to the daemon (preview, its own confirmation, execute) and shows what
//! is mounted as a badge in the sidebar. Attached to the daemon, the calls go through the
//! window's connection (`LibraryBackend::Daemon`); with the library open in-process they
//! use a connection of their own, for a daemon that may still run separately.

use crate::backend::Remote;
use crate::keys::Action;
use crate::state::{AppState, Msg};
use crate::worker;
use keel_api::client::Client;
use keel_api::types::{MountInfo, PlanPreview};
use keel_api::ApiError;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Shown whenever no daemon answers.
pub const NOT_RUNNING: &str = "keel-daemon is not running: start it with `keel daemon start`";
const START_HINT: &str = "start it with `keel daemon start`";
const REFRESH: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq)]
pub enum MountCmd {
    /// Open the dialog for `source` (and a folder inside it).
    Open {
        source: String,
        subtree: String,
    },
    /// Preview `method` on the daemon; the app confirms, then executes.
    Plan {
        method: String,
        params: Value,
    },
    Execute {
        plan_id: String,
        input_hash: String,
    },
}

pub enum MountMsg {
    List(Vec<MountInfo>),
    Preview {
        method: String,
        plan: PlanPreview,
    },
    Done(String),
    Failed(String),
    /// The Browse… folder picker's answer.
    Picked(String),
}

#[derive(Default)]
pub struct MountUi {
    pub mounts: Vec<MountInfo>,
    asked: Option<Instant>,
    busy: bool,
}

/// A daemon error as the user should read it.
pub fn error_text(e: &ApiError) -> String {
    if e.code == ApiError::MOUNTS_UNAVAILABLE {
        if e.message.contains("keel daemon start") {
            return e.message.clone();
        }
        return format!("{} ({START_HINT})", e.message);
    }
    e.message.clone()
}

/// Free drive letters K..=Z: not a drive now (`in_use`), not a mount's target.
pub fn free_letters(mounts: &[MountInfo], in_use: impl Fn(char) -> bool) -> Vec<char> {
    ('K'..='Z')
        .filter(|&c| {
            !in_use(c)
                && !mounts
                    .iter()
                    .any(|m| m.target.to_ascii_uppercase().starts_with(c))
        })
        .collect()
}

/// `mounted K:` (several: `mounted K:, L:`) for a source's sidebar row.
pub fn badge(mounts: &[MountInfo], source: &str) -> Option<String> {
    let t: Vec<String> = mounts
        .iter()
        .filter(|m| m.source == source)
        .map(|m| short_target(&m.target))
        .collect();
    (!t.is_empty()).then(|| format!("mounted {}", t.join(", ")))
}

fn short_target(t: &str) -> String {
    let is_letter = t.trim_end_matches(['\\', '/', ':']).len() == 1;
    if is_letter {
        return format!("{}:", t.chars().next().unwrap_or('?').to_ascii_uppercase());
    }
    std::path::Path::new(t)
        .file_name()
        .map_or_else(|| t.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// `~/Keel Mounts/<label>`, the label made safe as one folder name.
pub fn default_folder(home: &std::path::Path, label: &str) -> String {
    let name: String = label
        .chars()
        .map(|c| {
            if "/\\:*?\"<>|".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim().trim_matches('.');
    home.join("Keel Mounts")
        .join(if name.is_empty() { "Library" } else { name })
        .display()
        .to_string()
}

pub struct MountDialog {
    pub source: String,
    pub label: String,
    /// Read-only: relative to the source, "" = all of it.
    pub subtree: String,
    windows: bool,
    pub letters: Vec<char>,
    pub letter: Option<char>,
    pub folder: String,
}

impl MountDialog {
    pub fn new(
        source: String,
        label: String,
        subtree: String,
        windows: bool,
        letters: Vec<char>,
        folder: String,
    ) -> Self {
        Self {
            source,
            label,
            subtree,
            windows,
            letter: letters.first().copied(),
            letters,
            folder,
        }
    }

    /// The `mounts.add` params, or why the dialog cannot continue.
    pub fn params(&self) -> Result<Value, String> {
        let target = if self.windows {
            let c = self.letter.ok_or("No drive letter is free (K: to Z:)")?;
            format!("{c}:")
        } else {
            let f = self.folder.trim();
            if f.is_empty() {
                return Err("Choose a folder".into());
            }
            if !std::path::Path::new(f).is_absolute() {
                return Err("The folder must be an absolute path".into());
            }
            f.to_owned()
        };
        Ok(json!({"source": self.source, "subtree": self.subtree, "target": target}))
    }
}

/// The dialog's body; answers with `Plan` on OK.
pub fn dialog_ui(ui: &mut egui::Ui, d: &mut MountDialog, cancel: &mut bool) -> Option<Action> {
    ui.strong(format!("Mount {}", d.label));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Shows:");
        let mut shown = if d.subtree.is_empty() {
            format!("{} (everything)", d.label)
        } else {
            format!("{}/{}", d.label, d.subtree)
        };
        ui.add_enabled(false, egui::TextEdit::singleline(&mut shown));
    });
    ui.horizontal(|ui| {
        if d.windows {
            ui.label("Drive:");
            let text = d.letter.map_or("none free".into(), |c| format!("{c}:"));
            egui::ComboBox::from_id_salt("keel-mount-letter")
                .selected_text(text)
                .show_ui(ui, |ui| {
                    for &c in &d.letters {
                        ui.selectable_value(&mut d.letter, Some(c), format!("{c}:"));
                    }
                });
        } else {
            ui.label("Folder:");
            ui.text_edit_singleline(&mut d.folder);
            if ui.button("Browse…").clicked() {
                ui.ctx()
                    .data_mut(|m| m.insert_temp(egui::Id::new("keel-mount-browse"), true));
            }
        }
    });
    ui.add_space(4.0);
    ui.weak(
        "Needs keel-daemon running with a mount backend (`keel daemon start`); \
         the mount lasts until the daemon stops.",
    );
    let params = d.params();
    if let Err(why) = &params {
        ui.colored_label(ui.visuals().error_fg_color, why);
    }
    ui.add_space(8.0);
    let mut out = None;
    ui.horizontal(|ui| {
        let ok = ui.add_enabled(params.is_ok(), egui::Button::new("Mount"));
        if let (true, Ok(params)) = (ok.clicked(), &params) {
            out = Some(Action::Mount(MountCmd::Plan {
                method: "mounts.add".into(),
                params: params.clone(),
            }));
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

/// The source row's context-menu entries: Mount… and one Unmount per mount.
pub fn source_menu(ui: &mut egui::Ui, source: &str, out: &mut Vec<Action>) {
    if ui.button("Mount…").clicked() {
        out.push(Action::Mount(MountCmd::Open {
            source: source.to_owned(),
            subtree: String::new(),
        }));
        ui.close_menu();
    }
    for m in published(ui.ctx()).iter().filter(|m| m.source == source) {
        if ui
            .button(format!("Unmount {}", short_target(&m.target)))
            .clicked()
        {
            out.push(Action::Mount(MountCmd::Plan {
                method: "mounts.remove".into(),
                params: json!({ "target": m.target }),
            }));
            ui.close_menu();
        }
    }
}

/// "Mount this folder…" for a folder inside a `library://` tab.
pub fn folder_action(dir: &keel_vfs::VPath) -> Option<Action> {
    let (source, sub) = keel_vfs::library::split(dir)?;
    Some(Action::Mount(MountCmd::Open {
        source: source.to_owned(),
        subtree: sub.trim_matches('/').to_owned(),
    }))
}

/// The sidebar badge for `source`.
pub fn badge_for(ctx: &egui::Context, source: &str) -> Option<String> {
    badge(&published(ctx), source)
}

fn published(ctx: &egui::Context) -> Vec<MountInfo> {
    ctx.data(|d| d.get_temp(egui::Id::new("keel-mounts")))
        .unwrap_or_default()
}

/// One call to keel-daemon on a worker thread (never the UI thread): through the window's
/// daemon connection when attached, else a connection of its own.
fn call(remote: Option<&Remote>, method: &str, params: Value) -> Result<Value, String> {
    if let Some(r) = remote {
        return r.call_raw(method, params).map_err(|e| error_text(&e));
    }
    let cfg = crate::backend::host_config(&crate::cli::profile()).map_err(|e| format!("{e:#}"))?;
    let mut client = Client::connect(&cfg.socket_name()).map_err(|_| NOT_RUNNING.to_string())?;
    client.call(method, params).map_err(|e| error_text(&e))
}

impl AppState {
    /// The window's daemon connection, when attached.
    fn mount_remote(&self) -> Option<Arc<Remote>> {
        self.library.remote().cloned()
    }

    pub fn mount_cmd(&mut self, cmd: MountCmd) {
        // The daemon is gone: nothing changes until the user reconnects or opens it here.
        if self.library.lost && !matches!(cmd, MountCmd::Open { .. }) {
            return self.toasts.error(crate::library::LOST);
        }
        match cmd {
            MountCmd::Open { source, subtree } => {
                let label = crate::library::label_of(&source).unwrap_or_else(|| source.clone());
                let used: Vec<char> = (self.sidebar.drives.iter())
                    .filter_map(|(n, ..)| n.chars().next())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                let letters = free_letters(&self.mount.mounts, |c| used.contains(&c));
                let home = directories::UserDirs::new()
                    .map(|u| u.home_dir().to_path_buf())
                    .unwrap_or_default();
                self.dialog = Some(crate::dialogs::Dialog::Mount(Box::new(MountDialog::new(
                    source,
                    label.clone(),
                    subtree,
                    cfg!(windows),
                    letters,
                    default_folder(&home, &label),
                ))));
            }
            MountCmd::Plan { method, params } => {
                let (tx, ctx, remote) = (self.tx.clone(), self.ctx.clone(), self.mount_remote());
                worker::spawn("keel-mount-plan", move || {
                    let msg = (|| {
                        // Linux and macOS mount on an existing empty folder.
                        if method == "mounts.add" && !cfg!(windows) {
                            if let Some(t) = params["target"].as_str() {
                                std::fs::create_dir_all(t).map_err(|e| format!("{t}: {e}"))?;
                            }
                        }
                        let v = call(remote.as_deref(), &method, params)?;
                        let plan: PlanPreview =
                            serde_json::from_value(v).map_err(|e| e.to_string())?;
                        Ok::<_, String>(MountMsg::Preview { method, plan })
                    })()
                    .unwrap_or_else(MountMsg::Failed);
                    worker::send(&tx, &ctx, Msg::Mount(msg));
                });
            }
            MountCmd::Execute {
                plan_id,
                input_hash,
            } => {
                let (tx, ctx, remote) = (self.tx.clone(), self.ctx.clone(), self.mount_remote());
                worker::spawn("keel-mount-run", move || {
                    let params = json!({"plan_id": plan_id, "input_hash": input_hash});
                    let msg = match call(remote.as_deref(), "execute", params) {
                        Ok(v) => {
                            MountMsg::Done(v["result"]["target"].as_str().unwrap_or("").to_owned())
                        }
                        Err(e) => MountMsg::Failed(e),
                    };
                    worker::send(&tx, &ctx, Msg::Mount(msg));
                });
            }
        }
    }

    pub fn mount_msg(&mut self, msg: MountMsg) {
        match msg {
            MountMsg::List(list) => {
                self.mount.busy = false;
                self.mount.mounts = list;
                let shared = self.mount.mounts.clone();
                self.ctx
                    .data_mut(|d| d.insert_temp(egui::Id::new("keel-mounts"), shared));
            }
            MountMsg::Preview { method, plan } => {
                let mut text = plan.summary.clone();
                for w in &plan.warnings {
                    text.push_str(&format!("\n! {}", w.message));
                }
                if method == "mounts.remove" {
                    text.push_str("\nUnmount now?");
                }
                self.dialog = Some(crate::dialogs::Dialog::Confirm {
                    text,
                    on_yes: Action::Mount(MountCmd::Execute {
                        plan_id: plan.plan_id,
                        input_hash: plan.input_hash,
                    }),
                });
            }
            MountMsg::Done(target) => {
                self.toasts.info(format!("Mount list updated: {target}"));
                // Re-read the list at the next tick.
                self.mount.asked = None;
            }
            MountMsg::Failed(text) => self.toasts.error(text),
            MountMsg::Picked(path) => {
                if let Some(crate::dialogs::Dialog::Mount(d)) = &mut self.dialog {
                    d.folder = path;
                }
            }
        }
    }

    /// Per frame: the Browse… button, and a refresh of `mounts.list` every few seconds
    /// while the library is open (a stopped daemon just means no mounts).
    pub fn mount_tick(&mut self) {
        if self
            .ctx
            .data_mut(|d| d.remove_temp::<bool>(egui::Id::new("keel-mount-browse")))
            .is_some()
        {
            let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
            worker::spawn("keel-mount-pick", move || {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Mount folder")
                    .pick_folder()
                {
                    let msg = MountMsg::Picked(dir.display().to_string());
                    worker::send(&tx, &ctx, Msg::Mount(msg));
                }
            });
        }
        let due = self.mount.asked.is_none_or(|t| t.elapsed() > REFRESH);
        if !self.library.is_open() || self.mount.busy || !due {
            return;
        }
        self.mount.asked = Some(Instant::now());
        self.mount.busy = true;
        let (tx, ctx, remote) = (self.tx.clone(), self.ctx.clone(), self.mount_remote());
        worker::spawn("keel-mount-list", move || {
            let list = call(remote.as_deref(), "mounts.list", Value::Null)
                .ok()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            worker::send(&tx, &ctx, Msg::Mount(MountMsg::List(list)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_api::config::HostConfig;

    fn mount(target: &str, source: &str) -> MountInfo {
        MountInfo {
            target: target.into(),
            source: source.into(),
            source_label: "Photos".into(),
            subtree: String::new(),
            root: String::new(),
            backend: "winfsp".into(),
        }
    }

    fn dialog(windows: bool, letters: Vec<char>, folder: &str) -> MountDialog {
        MountDialog::new(
            "a".into(),
            "A".into(),
            "x/y".into(),
            windows,
            letters,
            folder.into(),
        )
    }

    #[test]
    fn first_free_letter_is_selected() {
        let letters = free_letters(&[mount("K:", "a")], |c| c == 'L');
        assert_eq!(letters[0], 'M');
        assert!(!letters.contains(&'K') && !letters.contains(&'L'));
        let d = dialog(true, letters, "");
        assert_eq!(d.letter, Some('M'));
        assert_eq!(
            d.params().unwrap(),
            json!({"source": "a", "subtree": "x/y", "target": "M:"})
        );
    }

    #[test]
    fn validation() {
        assert!(dialog(true, vec![], "").params().is_err());
        let mut f = dialog(false, vec![], "  ");
        assert!(f.params().is_err());
        f.folder = "relative/dir".into();
        assert!(f.params().is_err());
        f.folder = std::env::temp_dir().display().to_string();
        assert_eq!(f.params().unwrap()["target"], f.folder);
    }

    #[test]
    fn default_folder_is_safe() {
        let home = std::path::Path::new("h");
        assert!(default_folder(home, "Photos/2026: x").ends_with("Photos_2026_ x"));
        assert!(default_folder(home, "  ").ends_with("Library"));
    }

    #[test]
    fn badges() {
        let m = [
            mount("K:", "a"),
            mount("l:\\", "a"),
            mount("/m/Keel Mounts/b", "b"),
        ];
        assert_eq!(badge(&m, "a").unwrap(), "mounted K:, L:");
        assert_eq!(badge(&m, "b").unwrap(), "mounted b");
        assert_eq!(badge(&m, "c"), None);
    }

    #[test]
    fn attached_calls_go_through_the_windows_daemon_connection() {
        let (config, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let profile = format!("mount-test-{}", std::process::id());
        let cfg = HostConfig::read(&profile, config.path().into(), data.path().into());
        let name = cfg.socket_name();
        let _daemon = keel_daemon::server::Daemon::start(keel_daemon::server::Options {
            cfg,
            ws: None,
            web: None,
            ws_allow_remote: false,
            web_hosts: Vec::new(),
            net: None,
        })
        .unwrap();
        let remote = Remote::connect(&name).unwrap();
        assert_eq!(
            call(Some(&remote), "mounts.list", Value::Null).unwrap(),
            json!([])
        );
        let e = call(
            Some(&remote),
            "mounts.add",
            json!({"source": "none", "target": "/nonexistent"}),
        )
        .unwrap_err();
        // Without a mount backend built in, the error carries the start hint.
        if e.contains("--features") {
            assert!(e.contains("keel daemon start"), "{e}");
        }
    }

    #[test]
    fn errors() {
        let e = ApiError::new(ApiError::MOUNTS_UNAVAILABLE, "no mount backend");
        assert!(error_text(&e).contains("keel daemon start"));
        let e = ApiError::new(
            ApiError::MOUNTS_UNAVAILABLE,
            "mounts are served by keel-daemon: start it with `keel daemon start`",
        );
        assert_eq!(error_text(&e), e.message);
        assert_eq!(error_text(&ApiError::failed("boom")), "boom");
    }
}
