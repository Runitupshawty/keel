//! The sidebar's hosts and cloud accounts (Phase 14: rows of its Locations section). A
//! host: a row with a status dot, its bookmarks nested below, and a right-click menu
//! (connect, disconnect, edit, ssh, copy address). An account: a row with a status dot
//! (reconnect, edit, remove).

use crate::clouds::{has_quota, needs_sign_in, quota_text, root_of, CloudCmd, QuotaSlot};
use crate::keys::Action;
use crate::remotes::{home_of, remote_path, RemoteCmd};
use crate::sidebar;
use crate::sidebar_glyphs::Glyph;
use egui::{Id, Sense};
use keel_vfs::{CloudAccount, ConnStatus, RemoteHost, RemoteProtocol, VPath};
use std::collections::HashMap;

/// A row that clicks and takes the keyboard focus (`Sense::click`).
const CLICK: Sense = Sense::CLICK.union(Sense::FOCUSABLE);

/// What the sidebar shows for one host (rebuilt from config + status every frame).
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteRow {
    pub id: String,
    pub label: String,
    pub status: ConnStatus,
    /// Last status detail (hover text).
    pub detail: String,
    pub home: VPath,
    pub bookmarks: Vec<(String, VPath)>,
    /// Plain FTP: the clear-text warning, repeated in the hover text.
    pub warning: Option<&'static str>,
    /// SFTP: "Open terminal here" runs ssh.
    pub ssh: bool,
}

pub fn rows(
    hosts: &[RemoteHost],
    status: &HashMap<String, (ConnStatus, String)>,
) -> Vec<RemoteRow> {
    hosts
        .iter()
        .map(|h| {
            let (status, detail) = status
                .get(&h.id)
                .cloned()
                .unwrap_or((ConnStatus::Disconnected, String::new()));
            RemoteRow {
                id: h.id.clone(),
                label: h.label.clone(),
                status,
                detail,
                home: home_of(h),
                bookmarks: h
                    .bookmarks
                    .iter()
                    .map(|(label, path)| (label.clone(), remote_path(h.protocol, &h.id, path)))
                    .collect(),
                warning: (h.protocol == RemoteProtocol::Ftp).then_some(keel_vfs::FTP_CLEAR_TEXT),
                ssh: h.protocol == RemoteProtocol::Sftp,
            }
        })
        .collect()
}

fn dot_color(ui: &egui::Ui, s: ConnStatus) -> egui::Color32 {
    match s {
        ConnStatus::Disconnected => ui.visuals().weak_text_color(),
        ConnStatus::Connecting => crate::design::STATUS_BUSY,
        ConnStatus::Connected => crate::design::STATUS_OK,
        ConnStatus::Failed => ui.visuals().error_fg_color,
    }
}

/// The hosts in the sidebar's Locations: a row per host with its status dot, its bookmarks
/// below it. Nothing while none is configured (Locations offers "Add server or cloud…").
pub fn ui(ui: &mut egui::Ui, rows: &[RemoteRow], current: &VPath, out: &mut Vec<Action>) {
    for row in rows {
        let on_host = current.scheme == row.home.scheme && current.authority == row.id;
        let on_bookmark = row.bookmarks.iter().any(|(_, p)| p == current);
        let dot = dot_color(ui, row.status);
        let id = Id::new(("sidebar-remote", &row.id));
        let r = sidebar::row(
            ui,
            id,
            Glyph::Network,
            &row.label,
            on_host && !on_bookmark,
            CLICK,
            |ui| status_dot(ui, dot),
        );
        let mut tip = match row.status {
            ConnStatus::Disconnected => "Not connected".to_owned(),
            _ => row.detail.clone(),
        };
        if let Some(warning) = row.warning {
            tip = format!("{tip}\n{warning}");
        }
        let r = r.on_hover_text(tip);
        if r.clicked() {
            out.push(Action::Navigate(row.home.clone()));
        } else if r.middle_clicked() {
            out.push(Action::NewTabAt(row.home.clone()));
        }
        r.context_menu(|ui| {
            for (text, cmd) in [
                ("Connect", RemoteCmd::Connect),
                ("Disconnect", RemoteCmd::Disconnect),
                ("Edit…", RemoteCmd::Edit),
                ("Open terminal here", RemoteCmd::Terminal),
                ("Copy address", RemoteCmd::CopyAddress),
            ] {
                if cmd == RemoteCmd::Terminal && !row.ssh {
                    continue;
                }
                if ui.button(text).clicked() {
                    out.push(Action::Remote {
                        host: row.id.clone(),
                        cmd,
                    });
                    ui.close_menu();
                }
            }
        });
        ui.indent(("remote-bookmarks", &row.id), |ui| {
            for (label, path) in &row.bookmarks {
                let id = Id::new(("sidebar-bookmark", &row.id, label));
                let here = path == current;
                let r = sidebar::row(ui, id, Glyph::Folder, label, here, CLICK, |_| ())
                    .on_hover_text(&path.path);
                if r.clicked() {
                    out.push(Action::Navigate(path.clone()));
                } else if r.middle_clicked() {
                    out.push(Action::NewTabAt(path.clone()));
                }
            }
        });
    }
}

/// What the sidebar shows for one cloud account (rebuilt from config + status every frame).
#[derive(Clone, Debug, PartialEq)]
pub struct CloudRow {
    pub id: String,
    pub label: String,
    pub status: ConnStatus,
    pub detail: String,
    pub root: VPath,
    /// "12.3 GB of 15 GB used" (hover), for accounts that report storage.
    pub quota: Option<String>,
}

pub fn cloud_rows(
    accounts: &[CloudAccount],
    status: &HashMap<String, (ConnStatus, String)>,
    quota: &HashMap<String, QuotaSlot>,
) -> Vec<CloudRow> {
    accounts
        .iter()
        .map(|a| {
            let (status, detail) = status
                .get(&a.id)
                .cloned()
                .unwrap_or((ConnStatus::Disconnected, String::new()));
            CloudRow {
                id: a.id.clone(),
                label: a.label.clone(),
                status,
                detail,
                root: root_of(&a.id),
                quota: has_quota(a.kind).then(|| quota_text(quota.get(&a.id))),
            }
        })
        .collect()
}

/// The cloud accounts in the sidebar's Locations, a row each with its status dot. Hovering
/// a row asks for its storage quota (cached for `clouds::QUOTA_TTL`). Share links are in
/// the file context menu ("Copy link").
pub fn cloud_ui(ui: &mut egui::Ui, rows: &[CloudRow], current: &VPath, out: &mut Vec<Action>) {
    let cmd = |id: &str, cmd| Action::Cloud {
        id: id.to_owned(),
        cmd,
    };
    for row in rows {
        let here = current.scheme == "cloud" && current.authority == row.id;
        let dot = dot_color(ui, row.status);
        let id = Id::new(("sidebar-cloud", &row.id));
        let r = sidebar::row(ui, id, Glyph::Cloud, &row.label, here, CLICK, |ui| {
            status_dot(ui, dot)
        });
        let mut tip = match row.status {
            ConnStatus::Disconnected => "Not connected yet".to_owned(),
            _ => row.detail.clone(),
        };
        if let Some(quota) = &row.quota {
            tip = format!("{tip}\n{quota}");
            if r.hovered() {
                out.push(cmd(&row.id, CloudCmd::Quota));
            }
        }
        let r = r.on_hover_text(tip);
        if r.clicked() {
            out.push(Action::Navigate(row.root.clone()));
        } else if r.middle_clicked() {
            out.push(Action::NewTabAt(row.root.clone()));
        }
        let reauth = row.status == ConnStatus::Failed && needs_sign_in(&row.detail);
        r.context_menu(|ui| {
            let sign_in = reauth.then_some(("Sign in again…", CloudCmd::SignIn));
            for (text, c) in sign_in.into_iter().chain([
                ("Reconnect", CloudCmd::Reconnect),
                ("Edit…", CloudCmd::Edit),
                ("Remove…", CloudCmd::Remove),
            ]) {
                if ui.button(text).clicked() {
                    out.push(cmd(&row.id, c));
                    ui.close_menu();
                }
            }
        });
    }
}

/// A connection's status dot at the right end of its row.
fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}
