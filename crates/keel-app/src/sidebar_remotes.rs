//! Sidebar "Remotes" section: one row per configured host with a status dot, its bookmarks
//! nested below, and a right-click menu (connect, disconnect, edit, ssh, copy address).
//! The "Cloud" section: one row per account with a status dot (reconnect, edit, remove).

use crate::clouds::{has_quota, needs_sign_in, quota_text, root_of, CloudCmd, QuotaSlot};
use crate::keys::Action;
use crate::remotes::{home_of, remote_path, RemoteCmd};
use keel_vfs::{CloudAccount, ConnStatus, RemoteHost, VPath};
use std::collections::HashMap;

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
                    .map(|(label, path)| (label.clone(), remote_path(&h.id, path)))
                    .collect(),
            }
        })
        .collect()
}

fn dot_color(ui: &egui::Ui, s: ConnStatus) -> egui::Color32 {
    match s {
        ConnStatus::Disconnected => ui.visuals().weak_text_color(),
        ConnStatus::Connecting => egui::Color32::from_rgb(230, 180, 40),
        ConnStatus::Connected => egui::Color32::from_rgb(70, 180, 90),
        ConnStatus::Failed => ui.visuals().error_fg_color,
    }
}

pub fn ui(ui: &mut egui::Ui, rows: &[RemoteRow], current: &VPath, out: &mut Vec<Action>) {
    if rows.is_empty() {
        ui.horizontal(|ui| {
            ui.weak("None configured");
            if ui.small_button("Add…").clicked() {
                out.push(Action::Remote {
                    host: String::new(),
                    cmd: RemoteCmd::Add,
                });
            }
        });
        return;
    }
    for row in rows {
        let r = ui
            .horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size([16.0, 16.0].into(), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 4.5, dot_color(ui, row.status));
                let on_host = current.scheme == "sftp" && current.authority == row.id;
                ui.add(
                    egui::Button::new(row.label.as_str())
                        .frame(false)
                        .selected(on_host),
                )
            })
            .inner;
        let tip = match row.status {
            ConnStatus::Disconnected => "Not connected".to_owned(),
            _ => row.detail.clone(),
        };
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
                let icon = if path == current {
                    crate::icons::folder_open()
                } else {
                    crate::icons::folder()
                };
                let r = ui
                    .add(
                        egui::Button::image_and_text(
                            egui::Image::new(icon).fit_to_exact_size([16.0, 16.0].into()),
                            label.as_str(),
                        )
                        .frame(false),
                    )
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

/// Hovering a row asks for its storage quota (cached for `clouds::QUOTA_TTL`). Share
/// links are in the file context menu ("Copy link").
pub fn cloud_ui(ui: &mut egui::Ui, rows: &[CloudRow], current: &VPath, out: &mut Vec<Action>) {
    let cmd = |id: &str, cmd| Action::Cloud {
        id: id.to_owned(),
        cmd,
    };
    if rows.is_empty() {
        ui.horizontal(|ui| {
            ui.weak("None configured");
            if ui.small_button("Add…").clicked() {
                out.push(cmd("", CloudCmd::Add));
            }
        });
        return;
    }
    for row in rows {
        let r = ui
            .horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size([16.0, 16.0].into(), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 4.5, dot_color(ui, row.status));
                let here = current.scheme == "cloud" && current.authority == row.id;
                ui.add(
                    egui::Button::new(row.label.as_str())
                        .frame(false)
                        .selected(here),
                )
            })
            .inner;
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
