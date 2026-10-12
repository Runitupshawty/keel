//! Settings → Search index: the folders and volumes Keel's own index walks (FAT, exFAT,
//! ReFS, removable and network drives, any folder), how each is kept current, and the
//! Exclude rules (`keel_search::IndexSettings`, `[search_index]` in config.toml).
//! Edits are applied with **Apply**, so typing a wildcard does not rebuild an index per
//! keystroke.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use keel_search::{split_list, FolderRoot, IndexSettings, ScanOptions, Schedule, VolumeRoot};

use crate::a11y::Named;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Folders,
    Volumes,
    Exclude,
    Journal,
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
            ui.label("Changes");
            ui.checkbox(&mut scan.monitor, "Monitor changes");
            ui.end_row();
            let label = ui.label("Change buffer");
            ui.add_enabled_ui(scan.monitor, |ui| {
                let r = ui.add(
                    egui::DragValue::new(&mut scan.buffer_kib)
                        .range(4..=4096)
                        .suffix(" KiB"),
                );
                let _ = r
                    .labelled_by(label.id)
                    .on_hover_text("Windows; a network share takes at most 64 KiB");
            });
            ui.end_row();
            ui.label("");
            ui.checkbox(
                &mut scan.rescan_on_overflow,
                "Rescan when the change buffer overflows",
            );
            ui.end_row();
            ui.label("Rescan");
            ui.horizontal(|ui| {
                let shown = match &scan.schedule {
                    Schedule::Never => "Never",
                    Schedule::EveryHours(_) => "Every N hours",
                    Schedule::Daily(_) => "Daily at",
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
                        pick(ui, Schedule::Never, "Never");
                        pick(ui, Schedule::EveryHours(24), "Every N hours");
                        pick(ui, Schedule::Daily("03:00".into()), "Daily at");
                    });
                let _ = combo.response.named("Rescan");
                match &mut scan.schedule {
                    Schedule::Never => {}
                    Schedule::EveryHours(hours) => {
                        let r = ui.add(egui::DragValue::new(hours).range(1..=720).suffix(" h"));
                        let _ = r.named("Rescan every N hours");
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
                        let _ = r.named("Rescan daily at");
                        if keel_search::parse_time_of_day(time).is_none() {
                            ui.colored_label(ui.visuals().warn_fg_color, "HH:MM");
                        }
                    }
                }
            });
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut scan.fast_rescan, "Fast rescan")
                .on_hover_text("A rescan lists only the folders whose modified time changed");
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
    ui.label(
        "Keel's own index reads NTFS drives directly; these folders and volumes \
         (FAT, exFAT, ReFS, removable and network drives) are indexed by walking them.",
    );
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut page.tab, Tab::Folders, "Folders");
        ui.selectable_value(&mut page.tab, Tab::Volumes, "Volumes");
        ui.selectable_value(&mut page.tab, Tab::Exclude, "Exclude");
        ui.selectable_value(&mut page.tab, Tab::Journal, "Journal");
    });
    ui.separator();
    egui::ScrollArea::vertical()
        .max_height(360.0)
        .show(ui, |ui| match page.tab {
            Tab::Folders => folders(ui, &mut draft, page),
            Tab::Volumes => volumes(ui, &mut draft, page),
            Tab::Exclude => exclude(ui, &mut draft, page),
            Tab::Journal => journal(ui, &mut draft),
        });
    ui.separator();
    ui.horizontal(|ui| {
        let edited = draft != s.search_index;
        if ui
            .add_enabled(edited, egui::Button::new("Apply"))
            .on_hover_text("Indexes what changed; an index whose rules changed is walked again")
            .clicked()
        {
            s.search_index = draft.clone();
        }
        if ui
            .add_enabled(edited, egui::Button::new("Revert"))
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
            format!("{} (offline)", f.path)
        } else {
            f.path.clone()
        };
        egui::CollapsingHeader::new(title)
            .id_salt(format!("search-folder-{i}"))
            .show(ui, |ui| {
                scan_options(ui, &mut page.texts, &format!("folder-{i}"), &mut f.scan);
                if ui.button("Remove folder").clicked() {
                    remove = Some(i);
                }
            });
    }
    if let Some(i) = remove {
        d.folders.remove(i);
        page.texts.clear();
    }
    if d.folders.is_empty() {
        ui.weak("No folders yet.");
    }
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut page.new_folder)
                .hint_text(r"C:\Data, \\server\share or /mnt/data")
                .desired_width(280.0),
        );
        let _ = r.named("New folder");
        let path = page.new_folder.trim().to_owned();
        if ui
            .add_enabled(!path.is_empty(), egui::Button::new("Add folder"))
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
        "Automatically include new fixed volumes",
    );
    ui.checkbox(
        &mut d.auto_include_removable,
        "Automatically include new removable volumes",
    );
    ui.checkbox(
        &mut d.auto_remove_offline,
        "Automatically remove offline volumes",
    )
    .on_hover_text("Off: an offline volume stays searchable from its last scan, marked offline");
    ui.add_space(4.0);
    let mut remove = None;
    for (i, v) in d.volumes.iter_mut().enumerate() {
        let offline = page.is_offline(&v.root);
        let title = match (v.include, offline) {
            (_, true) => format!("{} (offline)", v.root),
            (false, _) => format!("{} (not indexed)", v.root),
            _ => v.root.clone(),
        };
        egui::CollapsingHeader::new(title)
            .id_salt(format!("search-volume-{i}"))
            .show(ui, |ui| {
                ui.checkbox(&mut v.include, "Include in database");
                egui::Grid::new(format!("search-volume-only-{i}"))
                    .num_columns(2)
                    .show(ui, |ui| {
                        list_field(
                            ui,
                            &mut page.texts,
                            &format!("volume-{i}-only"),
                            "Include only",
                            "Photos; Work\\Projects",
                            &mut v.include_only,
                        );
                    });
                scan_options(ui, &mut page.texts, &format!("volume-{i}"), &mut v.scan);
                if ui.button("Remove volume").clicked() {
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
                .hint_text(r"E:\ or a mapped network drive")
                .desired_width(200.0),
        );
        let _ = r.named("New volume");
        let root = page.new_volume.trim().to_owned();
        if ui
            .add_enabled(!root.is_empty(), egui::Button::new("Add volume"))
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
    ui.checkbox(&mut x.hidden, "Exclude hidden files and folders");
    ui.checkbox(&mut x.system, "Exclude system files and folders");
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
                "Exclude folders",
                r"node_modules; C:\Temp; *\cache",
                &mut x.folders,
            );
            list_field(
                ui,
                t,
                "include-only-folders",
                "Include only folders",
                r"C:\Users; D:\Work",
                &mut x.include_only_folders,
            );
            list_field(
                ui,
                t,
                "include-only-files",
                "Include only files",
                "*.pdf; *.docx",
                &mut x.include_only_files,
            );
            list_field(
                ui,
                t,
                "exclude-files",
                "Exclude files",
                "*.tmp; ~$*",
                &mut x.files,
            );
        });
    ui.weak("Lists are separated by ';'. Rules apply while indexing: an index walks again when they change.");
}

/// The Journal tab: the index journal on or off, and its size.
fn journal(ui: &mut egui::Ui, d: &mut IndexSettings) {
    let j = &mut d.journal;
    ui.checkbox(&mut j.enabled, "Keep an index journal")
        .on_hover_text(
            "Every create, delete, rename, modify and attribute change the index applies",
        );
    ui.add_enabled_ui(j.enabled, |ui| {
        ui.horizontal(|ui| {
            ui.label("Size");
            ui.add(
                egui::DragValue::new(&mut j.max_mb)
                    .range(1..=4096)
                    .suffix(" MB"),
            )
            .named("Journal size in MB");
        });
    });
    ui.weak(
        "When the journal reaches its size the oldest entries go first. Open it with \
         Index journal in the command palette, or `keel journal` on the command line.",
    );
}

/// Hands Settings → Search index to the search backend on a worker (it may list volumes and
/// open or close indexes), and the journal settings to the index journal.
pub fn push(searcher: Option<std::sync::Arc<dyn keel_search::Searcher>>, settings: &IndexSettings) {
    keel_search::journal::configure(&settings.journal);
    let Some(searcher) = searcher else {
        return;
    };
    let settings = settings.clone();
    let _ = std::thread::Builder::new()
        .name("keel-index-settings".into())
        .spawn(move || searcher.set_index_settings(&settings));
}
