//! Settings → Search index: the folders and volumes Keel's own index walks (FAT, exFAT,
//! ReFS, removable and network drives, any folder), how each is kept current, the
//! Exclude rules, the journal, the indexed properties and the run history
//! (`keel_search::IndexSettings`, `[search_index]` in config.toml).
//! Edits are applied with **Apply**, so typing a wildcard does not rebuild an index per
//! keystroke.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use keel_search::{
    split_list, FolderRoot, IndexSettings, IndexedProperty, ScanOptions, Schedule, VolumeRoot,
};

use crate::a11y::Named;
use crate::i18n::tr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Folders,
    Volumes,
    Exclude,
    Journal,
    Properties,
    RunHistory,
}

/// The page's state between frames.
#[derive(Default)]
pub struct SearchPage {
    pub tab: Tab,
    /// The settings being edited, and what they started from.
    draft: Option<(IndexSettings, IndexSettings)>,
    new_folder: String,
    new_volume: String,
    /// Text of the `;`-separated list fields and times as typed, by field id.
    texts: HashMap<String, String>,
    /// Folders and volumes the backend reports offline (set before each frame).
    pub offline: Vec<PathBuf>,
    new_property: String,
    /// What the last **Clear** of the run history said.
    cleared: Option<String>,
}

impl SearchPage {
    fn is_offline(&self, root: &str) -> bool {
        let root = Path::new(root);
        self.offline
            .iter()
            .any(|o| o == root || o.starts_with(root))
    }
}

/// A `;`-separated list edited as text; the list follows the text as typed.
fn list_field(
    ui: &mut egui::Ui,
    texts: &mut HashMap<String, String>,
    id: &str,
    label: &str,
    hint: &str,
    list: &mut Vec<String>,
) {
    ui.label(label);
    let text = texts
        .entry(id.to_owned())
        .or_insert_with(|| list.join("; "));
    let r = ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(hint)
            .desired_width(320.0),
    );
    if r.changed() {
        *list = split_list(text);
    }
    let _ = r.named(label);
    ui.end_row();
}

fn scan_options(
    ui: &mut egui::Ui,
    texts: &mut HashMap<String, String>,
    id: &str,
    scan: &mut ScanOptions,
) {
    egui::Grid::new(format!("search-scan-{id}"))
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            ui.label(tr("search_settings.changes"));
            ui.checkbox(&mut scan.monitor, tr("search_settings.monitor_changes"));
            ui.end_row();
            let label = ui.label(tr("search_settings.change_buffer"));
            ui.add_enabled_ui(scan.monitor, |ui| {
                let r = ui.add(
                    egui::DragValue::new(&mut scan.buffer_kib)
                        .range(4..=4096)
                        .suffix(" KiB"),
                );
                let _ = r
                    .labelled_by(label.id)
                    .on_hover_text(tr("search_settings.change_buffer_tip"));
            });
            ui.end_row();
            ui.label("");
            ui.checkbox(
                &mut scan.rescan_on_overflow,
                tr("search_settings.rescan_on_overflow"),
            );
            ui.end_row();
            ui.label(tr("search_settings.rescan"));
            ui.horizontal(|ui| {
                let shown = match &scan.schedule {
                    Schedule::Never => tr("search_settings.never"),
                    Schedule::EveryHours(_) => tr("search_settings.every_n_hours"),
                    Schedule::Daily(_) => tr("search_settings.daily_at"),
                };
                let combo = egui::ComboBox::from_id_salt(format!("search-schedule-{id}"))
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        let mut pick = |ui: &mut egui::Ui, value: Schedule, text: &str| {
                            let on = std::mem::discriminant(&scan.schedule)
                                == std::mem::discriminant(&value);
                            if ui.selectable_label(on, text).clicked() && !on {
                                scan.schedule = value;
                            }
                        };
                        pick(ui, Schedule::Never, tr("search_settings.never"));
                        pick(
                            ui,
                            Schedule::EveryHours(24),
                            tr("search_settings.every_n_hours"),
                        );
                        pick(
                            ui,
                            Schedule::Daily("03:00".into()),
                            tr("search_settings.daily_at"),
                        );
                    });
                let _ = combo.response.named(tr("search_settings.rescan"));
                match &mut scan.schedule {
                    Schedule::Never => {}
                    Schedule::EveryHours(hours) => {
                        let r = ui.add(egui::DragValue::new(hours).range(1..=720).suffix(" h"));
                        let _ = r.named(tr("search_settings.rescan_every_n_hours"));
                    }
                    Schedule::Daily(time) => {
                        let text = texts
                            .entry(format!("{id}-time"))
                            .or_insert_with(|| time.clone());
                        let r = ui.add(
                            egui::TextEdit::singleline(text)
                                .hint_text("HH:MM")
                                .desired_width(60.0),
                        );
                        if r.changed() {
                            *time = text.trim().to_owned();
                        }
                        let _ = r.named(tr("search_settings.rescan_daily_at"));
                        if keel_search::parse_time_of_day(time).is_none() {
                            ui.colored_label(ui.visuals().warn_fg_color, "HH:MM");
                        }
                    }
                }
            });
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut scan.fast_rescan, tr("search_settings.fast_rescan"))
                .on_hover_text(tr("search_settings.fast_rescan_tip"));
            ui.end_row();
        });
}

/// Settings → Search index.
pub fn settings_page(ui: &mut egui::Ui, s: &mut crate::settings::Settings, page: &mut SearchPage) {
    // Follow changes made elsewhere (a volume included automatically) while nothing
    // is being edited.
    if page
        .draft
        .as_ref()
        .is_some_and(|(draft, base)| draft == base && *base != s.search_index)
    {
        page.draft = None;
        page.texts.clear();
    }
    let (mut draft, base) = page
        .draft
        .take()
        .unwrap_or_else(|| (s.search_index.clone(), s.search_index.clone()));
    ui.label(tr("search_settings.intro"));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.selectable_value(
            &mut page.tab,
            Tab::Folders,
            tr("search_settings.tab_folders"),
        );
        ui.selectable_value(
            &mut page.tab,
            Tab::Volumes,
            tr("search_settings.tab_volumes"),
        );
        ui.selectable_value(
            &mut page.tab,
            Tab::Exclude,
            tr("search_settings.tab_exclude"),
        );
        ui.selectable_value(
            &mut page.tab,
            Tab::Journal,
            tr("search_settings.tab_journal"),
        );
        ui.selectable_value(
            &mut page.tab,
            Tab::Properties,
            tr("search_settings.tab_properties"),
        );
        ui.selectable_value(
            &mut page.tab,
            Tab::RunHistory,
            tr("search_settings.tab_run_history"),
        );
    });
    ui.separator();
    egui::ScrollArea::vertical()
        .max_height(360.0)
        .show(ui, |ui| match page.tab {
            Tab::Folders => folders(ui, &mut draft, page),
            Tab::Volumes => volumes(ui, &mut draft, page),
            Tab::Exclude => exclude(ui, &mut draft, page),
            Tab::Journal => journal(ui, &mut draft),
            Tab::Properties => properties(ui, &mut draft, page),
            Tab::RunHistory => run_history(ui, &mut draft, page),
        });
    ui.separator();
    ui.horizontal(|ui| {
        let edited = draft != s.search_index;
        if ui
            .add_enabled(edited, egui::Button::new(tr("search_settings.apply")))
            .on_hover_text(tr("search_settings.apply_tip"))
            .clicked()
        {
            s.search_index = draft.clone();
        }
        if ui
            .add_enabled(edited, egui::Button::new(tr("search_settings.revert")))
            .clicked()
        {
            draft = s.search_index.clone();
            page.texts.clear();
        }
    });
    let base = if draft == s.search_index {
        s.search_index.clone()
    } else {
        base
    };
    page.draft = Some((draft, base));
}

fn folders(ui: &mut egui::Ui, d: &mut IndexSettings, page: &mut SearchPage) {
    let mut remove = None;
    for (i, f) in d.folders.iter_mut().enumerate() {
        let offline = page.is_offline(&f.path);
        let title = if offline {
            tr("search_settings.offline").replace("{path}", &f.path)
        } else {
            f.path.clone()
        };
        egui::CollapsingHeader::new(title)
            .id_salt(format!("search-folder-{i}"))
            .show(ui, |ui| {
                scan_options(ui, &mut page.texts, &format!("folder-{i}"), &mut f.scan);
                if ui.button(tr("search_settings.remove_folder")).clicked() {
                    remove = Some(i);
                }
            });
    }
    if let Some(i) = remove {
        d.folders.remove(i);
        page.texts.clear();
    }
    if d.folders.is_empty() {
        ui.weak(tr("search_settings.no_folders_yet"));
    }
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut page.new_folder)
                .hint_text(r"C:\Data, \\server\share or /mnt/data")
                .desired_width(280.0),
        );
        let _ = r.named(tr("search_settings.new_folder"));
        let path = page.new_folder.trim().to_owned();
        if ui
            .add_enabled(
                !path.is_empty(),
                egui::Button::new(tr("search_settings.add_folder")),
            )
            .clicked()
        {
            d.folders.push(FolderRoot {
                path,
                scan: ScanOptions::default(),
            });
            page.new_folder.clear();
        }
    });
}

fn volumes(ui: &mut egui::Ui, d: &mut IndexSettings, page: &mut SearchPage) {
    ui.checkbox(
        &mut d.auto_include_fixed,
        tr("search_settings.auto_include_fixed"),
    );
    ui.checkbox(
        &mut d.auto_include_removable,
        tr("search_settings.auto_include_removable"),
    );
    ui.checkbox(
        &mut d.auto_remove_offline,
        tr("search_settings.auto_remove_offline"),
    )
    .on_hover_text(tr("search_settings.auto_remove_offline_tip"));
    ui.add_space(4.0);
    let mut remove = None;
    for (i, v) in d.volumes.iter_mut().enumerate() {
        let offline = page.is_offline(&v.root);
        let title = match (v.include, offline) {
            (_, true) => tr("search_settings.offline").replace("{path}", &v.root),
            (false, _) => tr("search_settings.not_indexed").replace("{path}", &v.root),
            _ => v.root.clone(),
        };
        egui::CollapsingHeader::new(title)
            .id_salt(format!("search-volume-{i}"))
            .show(ui, |ui| {
                ui.checkbox(&mut v.include, tr("search_settings.include_in_database"));
                egui::Grid::new(format!("search-volume-only-{i}"))
                    .num_columns(2)
                    .show(ui, |ui| {
                        list_field(
                            ui,
                            &mut page.texts,
                            &format!("volume-{i}-only"),
                            tr("search_settings.include_only"),
                            "Photos; Work\\Projects",
                            &mut v.include_only,
                        );
                    });
                scan_options(ui, &mut page.texts, &format!("volume-{i}"), &mut v.scan);
                if ui.button(tr("search_settings.remove_volume")).clicked() {
                    remove = Some(i);
                }
            });
    }
    if let Some(i) = remove {
        d.volumes.remove(i);
        page.texts.clear();
    }
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut page.new_volume)
                .hint_text(tr("search_settings.new_volume_hint"))
                .desired_width(200.0),
        );
        let _ = r.named(tr("search_settings.new_volume"));
        let root = page.new_volume.trim().to_owned();
        if ui
            .add_enabled(
                !root.is_empty(),
                egui::Button::new(tr("search_settings.add_volume")),
            )
            .clicked()
        {
            d.volumes.push(VolumeRoot {
                root,
                ..VolumeRoot::default()
            });
            page.new_volume.clear();
        }
    });
}

fn exclude(ui: &mut egui::Ui, d: &mut IndexSettings, page: &mut SearchPage) {
    let x = &mut d.exclude;
    ui.checkbox(&mut x.hidden, tr("search_settings.exclude_hidden"));
    ui.checkbox(&mut x.system, tr("search_settings.exclude_system"));
    ui.add_space(4.0);
    egui::Grid::new("search-exclude")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            let t = &mut page.texts;
            list_field(
                ui,
                t,
                "exclude-folders",
                tr("search_settings.exclude_folders"),
                r"node_modules; C:\Temp; *\cache",
                &mut x.folders,
            );
            list_field(
                ui,
                t,
                "include-only-folders",
                tr("search_settings.include_only_folders"),
                r"C:\Users; D:\Work",
                &mut x.include_only_folders,
            );
            list_field(
                ui,
                t,
                "include-only-files",
                tr("search_settings.include_only_files"),
                "*.pdf; *.docx",
                &mut x.include_only_files,
            );
            list_field(
                ui,
                t,
                "exclude-files",
                tr("search_settings.exclude_files"),
                "*.tmp; ~$*",
                &mut x.files,
            );
        });
    ui.weak(tr("search_settings.lists_note"));
}

/// The Journal tab: the index journal on or off, and its size.
fn journal(ui: &mut egui::Ui, d: &mut IndexSettings) {
    let j = &mut d.journal;
    ui.checkbox(&mut j.enabled, tr("search_settings.keep_journal"))
        .on_hover_text(tr("search_settings.keep_journal_tip"));
    ui.add_enabled_ui(j.enabled, |ui| {
        ui.horizontal(|ui| {
            ui.label(tr("search_settings.size"));
            ui.add(
                egui::DragValue::new(&mut j.max_mb)
                    .range(1..=4096)
                    .suffix(" MB"),
            )
            .named(tr("search_settings.journal_size"));
        });
    });
    ui.weak(tr("search_settings.journal_note"));
}

/// The Properties tab (Everything 1.5's): which properties the idle job reads into the
/// property store, for which files.
fn properties(ui: &mut egui::Ui, d: &mut IndexSettings, page: &mut SearchPage) {
    ui.label(tr("search_settings.properties_intro"));
    ui.add_space(4.0);
    let p = &mut d.properties;
    let mut remove = None;
    egui::Grid::new("search-properties")
        .num_columns(3)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            for (i, prop) in p.properties.iter_mut().enumerate() {
                ui.label(egui::RichText::new(&prop.name).monospace());
                ui.checkbox(&mut prop.fast_sort, tr("search_settings.fast_sort"))
                    .on_hover_text(tr("search_settings.fast_sort_tip"));
                if ui.button(tr("search_settings.remove_property")).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some(i) = remove {
        p.properties.remove(i);
    }
    if p.properties.is_empty() {
        ui.weak(tr("search_settings.no_properties_yet"));
    }
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut page.new_property)
                .hint_text("artist, width, sha256, System.Photo.FNumber")
                .desired_width(280.0),
        );
        let _ = r.named(tr("search_settings.new_property"));
        let name = page.new_property.trim().to_owned();
        let known = keel_search::propindex::is_property_name(&name);
        let dupe = (p.properties.iter()).any(|x| x.name.eq_ignore_ascii_case(&name));
        if ui
            .add_enabled(
                known && !dupe,
                egui::Button::new(tr("search_settings.add_property")),
            )
            .clicked()
        {
            p.properties.push(IndexedProperty {
                name,
                fast_sort: false,
            });
            page.new_property.clear();
        } else if !name.is_empty() && !known {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                tr("search_settings.unknown_property"),
            );
        }
    });
    ui.add_space(4.0);
    egui::Grid::new("search-properties-rules")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            let t = &mut page.texts;
            list_field(
                ui,
                t,
                "props-include-only-folders",
                tr("search_settings.include_only_folders"),
                r"C:\Music; D:\Photos",
                &mut p.include_only_folders,
            );
            list_field(
                ui,
                t,
                "props-exclude-folders",
                tr("search_settings.exclude_folders"),
                r"node_modules; *\cache",
                &mut p.exclude_folders,
            );
            list_field(
                ui,
                t,
                "props-include-only-files",
                tr("search_settings.include_only_files"),
                "*.mp3; *.flac; *.jpg",
                &mut p.include_only_files,
            );
            list_field(
                ui,
                t,
                "props-exclude-files",
                tr("search_settings.exclude_files"),
                "*.tmp; ~$*",
                &mut p.exclude_files,
            );
            let label = ui.label(tr("search_settings.max_file_size"));
            let r = ui.add(
                egui::DragValue::new(&mut p.max_file_mb)
                    .range(0..=1_048_576)
                    .suffix(" MB"),
            );
            let _ = r
                .labelled_by(label.id)
                .on_hover_text(tr("search_settings.max_file_size_tip"));
            ui.end_row();
        });
    ui.checkbox(
        &mut p.exclude_recall_on_data_access,
        tr("search_settings.exclude_recall"),
    )
    .on_hover_text(tr("search_settings.exclude_recall_tip"));
    ui.checkbox(
        &mut p.exclude_not_content_indexed,
        tr("search_settings.exclude_not_content_indexed"),
    );
    ui.add_space(4.0);
    let st = keel_search::propindex::status();
    let state = if !st.running {
        tr("search_settings.properties_off")
    } else if st.paused {
        tr("search_settings.properties_paused")
    } else if st.walking || st.pending > 0 {
        tr("search_settings.properties_reading")
    } else {
        tr("search_settings.properties_current")
    };
    ui.weak(
        tr("search_settings.properties_status")
            .replace("{state}", state)
            .replace("{stored}", &st.stored.to_string())
            .replace("{read}", &st.read.to_string()),
    );
    if st.running {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
    }
}

/// The Run history tab: counting opens on or off, and clearing them.
fn run_history(ui: &mut egui::Ui, d: &mut IndexSettings, page: &mut SearchPage) {
    ui.checkbox(
        &mut d.run_history.enabled,
        tr("search_settings.keep_run_history"),
    )
    .on_hover_text(tr("search_settings.keep_run_history_tip"));
    if let Some(store) = keel_search::runs::global() {
        ui.weak(tr("search_settings.run_history_count").replace("{n}", &store.len().to_string()));
    }
    ui.horizontal(|ui| {
        if ui
            .button(tr("search_settings.clear_run_history"))
            .on_hover_text(tr("search_settings.clear_run_history_tip"))
            .clicked()
        {
            page.cleared = Some(match keel_search::runs::clear() {
                Ok(()) => tr("search_settings.run_history_cleared").to_owned(),
                Err(e) => format!("{e:#}"),
            });
        }
        if let Some(text) = &page.cleared {
            ui.weak(text.as_str());
        }
    });
    ui.weak(tr("search_settings.run_history_note"));
}

/// What the property job walks when the Properties tab names no include-only folder: the
/// home folder and the Folders and Volumes of this page.
pub fn property_roots(settings: &IndexSettings) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = keel_search::home_root().into_iter().collect();
    roots.extend(settings.folders.iter().map(|f| PathBuf::from(&f.path)));
    roots.extend(
        (settings.volumes.iter())
            .filter(|v| v.include)
            .map(|v| PathBuf::from(&v.root)),
    );
    roots
}

/// Hands Settings → Search index to the search backend on a worker (it may list volumes and
/// open or close indexes), the journal settings to the index journal, the run history's
/// to the run history and the Properties tab to the property job.
pub fn push(searcher: Option<std::sync::Arc<dyn keel_search::Searcher>>, settings: &IndexSettings) {
    keel_search::journal::configure(&settings.journal);
    keel_search::runs::configure(&settings.run_history);
    // The property job pauses on battery like the library's background jobs.
    keel_search::propindex::set_battery_probe(Some(keel_core::on_battery));
    {
        let props = settings.properties.clone();
        let roots = property_roots(settings);
        // Starting or stopping the job waits for the old one: off the UI thread.
        let _ = std::thread::Builder::new()
            .name("keel-props-settings".into())
            .spawn(move || keel_search::propindex::configure(&props, &roots, true));
    }
    let Some(searcher) = searcher else {
        return;
    };
    let settings = settings.clone();
    let _ = std::thread::Builder::new()
        .name("keel-index-settings".into())
        .spawn(move || searcher.set_index_settings(&settings));
}
