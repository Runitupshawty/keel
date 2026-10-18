//! File lists in the app (Task 77): open an EFU, TXT, CSV, TSV or M3U list as a search tab
//! (File list tabs search the list's entries, not the disk: `keel_search::filelist`),
//! export the shown rows of any tab (Export dialog), and `keel filelist create|export`.

use crate::cli::{ExportArgs, FilelistCmd};
use crate::keys::Action;
use crate::state::{AppState, Msg};
use crate::tab::{Tab, TabKind};
use crate::worker;
use keel_search::filelist::{
    self, create, export, filetime_from_system, read_file, split_wildcards, CreateOptions,
    ExportOptions, FileList, Format, Limits, ATTR_DIRECTORY, ATTR_HIDDEN,
};
use keel_search::{FileListSearcher, Query, Searcher};
use keel_vfs::{Entry, Kind, VPath};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// A file list tab answers this many rows per query (a search tab: `search_tab::MAX_HITS`).
pub const LIST_HITS: u32 = 100_000;

/// The extensions File → Open file list offers.
pub const OPEN_EXTENSIONS: &[&str] = &["efu", "txt", "csv", "tsv", "m3u", "m3u8"];

#[derive(Clone, Debug, PartialEq)]
pub enum FileListCmd {
    /// Open this file list as a tab (None: pick one first).
    Open(Option<PathBuf>),
    /// The Export dialog for the active tab's rows.
    Export,
    /// The dialog answered: pick where to save, then write.
    ExportTo {
        opts: ExportOptions,
        selection: bool,
    },
    /// Open the File List Editor window (`filelist_editor`).
    Editor,
    /// Files and folders dropped on the File List Editor: add them to its list.
    EditorAdd(Vec<PathBuf>),
}

pub enum FileListMsg {
    /// A list was read (pane to open it in).
    Loaded {
        pane: usize,
        path: PathBuf,
        result: Result<Arc<FileListSearcher>, String>,
    },
    /// An export finished: the file and how many rows, or why not.
    Exported(Result<(PathBuf, usize), String>),
    /// A File List Editor worker answered.
    Editor(crate::filelist_editor::EditorMsg),
}

/// The Export dialog.
pub struct ExportDialog {
    pub format: Format,
    pub header: bool,
    pub bom: bool,
    /// Only the selected rows (offered when something is selected).
    pub selection: bool,
    pub selected: usize,
    pub shown: usize,
}

/// A row of a tab as a file list entry (attributes: folder, hidden).
fn list_entry(e: &Entry) -> filelist::Entry {
    let is_dir = e.kind == Kind::Dir;
    let mut attributes = if is_dir { ATTR_DIRECTORY } else { 0 };
    if e.hidden {
        attributes |= ATTR_HIDDEN;
    }
    filelist::Entry {
        path: e.path.display(),
        is_dir,
        size: (!is_dir).then_some(e.size),
        modified: e.modified.and_then(filetime_from_system),
        created: None,
        attributes: Some(attributes),
        extra: Vec::new(),
    }
}

/// The rows of `tab` to export: the selected ones (in the order shown) when `selection`,
/// else every shown row.
pub fn rows(tab: &Tab, selection: bool) -> FileList {
    let entries = tab.entries();
    let entries = (tab.visible_cached().iter())
        .map(|&i| &entries[i])
        .filter(|e| !selection || tab.selected.contains(&e.name))
        .map(list_entry)
        .collect();
    FileList {
        columns: Vec::new(),
        entries,
    }
}

/// The Export dialog's body; answers `ExportTo`.
pub fn dialog_ui(ui: &mut egui::Ui, d: &mut ExportDialog, cancel: &mut bool) -> Option<Action> {
    ui.strong("Export results");
    ui.add_space(4.0);
    egui::ComboBox::from_label("Format")
        .selected_text(d.format.label())
        .show_ui(ui, |ui| {
            for f in Format::ALL {
                ui.selectable_value(&mut d.format, f, f.label());
            }
        });
    let header = match d.format {
        Format::Csv | Format::Tsv => Some("Column names on the first line"),
        Format::M3u | Format::M3u8 => Some("#EXTM3U on the first line"),
        _ => None,
    };
    if let Some(text) = header {
        ui.checkbox(&mut d.header, text);
    }
    ui.checkbox(&mut d.bom, "UTF-8 byte order mark (BOM)");
    ui.add_enabled_ui(d.selected > 0, |ui| {
        ui.checkbox(
            &mut d.selection,
            format!("Selected rows only ({} of {})", d.selected, d.shown),
        );
    });
    ui.add_space(8.0);
    let mut out = None;
    ui.horizontal(|ui| {
        let save = ui.button("Save as…");
        crate::a11y::first_focus(&save);
        if save.clicked() {
            out = Some(Action::FileList(FileListCmd::ExportTo {
                opts: ExportOptions {
                    format: d.format,
                    header: d.header,
                    bom: d.bom,
                },
                selection: d.selection && d.selected > 0,
            }));
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

impl AppState {
    pub fn filelist_cmd(&mut self, p: usize, cmd: FileListCmd) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        match cmd {
            FileListCmd::Open(None) => {
                worker::spawn("keel-filelist-pick", move || {
                    let picked = crate::platform::file_dialog("Open file list", |d| {
                        d.add_filter("File lists", OPEN_EXTENSIONS)
                            .add_filter("All files", &["*"])
                            .pick_file()
                    });
                    if let Some(path) = picked {
                        load(p, path, &tx, &ctx);
                    }
                });
            }
            FileListCmd::Open(Some(path)) => {
                worker::spawn("keel-filelist-read", move || load(p, path, &tx, &ctx));
            }
            FileListCmd::Editor => self.filelist_editor.show(),
            FileListCmd::EditorAdd(paths) => self.filelist_editor.add(paths, &tx, &ctx),
            FileListCmd::Export => {
                let tab = self.tab(p);
                let shown = tab.visible_cached().len();
                let selected = rows(tab, true).entries.len();
                self.dialog = Some(crate::dialogs::Dialog::ExportList(Box::new(ExportDialog {
                    format: Format::Efu,
                    header: true,
                    bom: false,
                    selection: selected > 0 && selected < shown,
                    selected,
                    shown,
                })));
            }
            FileListCmd::ExportTo { opts, selection } => {
                let list = rows(self.tab(p), selection);
                let name = match self.tab(p).filelist.as_ref() {
                    Some(s) => Path::new(s.label()).with_extension(opts.format.extension()),
                    None => PathBuf::from(format!("Results.{}", opts.format.extension())),
                };
                let dir = self.tab(p).dir.to_local_path();
                worker::spawn("keel-filelist-export", move || {
                    let picked = crate::platform::file_dialog("Export results", |mut dialog| {
                        dialog = dialog
                            .set_file_name(name.to_string_lossy())
                            .add_filter(opts.format.label(), &[opts.format.extension()]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file()
                    });
                    let Some(path) = picked else {
                        return;
                    };
                    let result = write_list(&list, &opts, &path)
                        .map(|()| (path, list.entries.len()))
                        .map_err(|e| format!("{e:#}"));
                    worker::send(&tx, &ctx, Msg::FileList(FileListMsg::Exported(result)));
                });
            }
        }
    }

    pub fn filelist_msg(&mut self, msg: FileListMsg) {
        match msg {
            FileListMsg::Loaded { pane, path, result } => match result {
                Ok(searcher) => self.open_list_tab(pane, &path, searcher),
                Err(e) => self.toasts.error(format!("File list: {e}")),
            },
            FileListMsg::Exported(Ok((path, n))) => self.toasts.info(format!(
                "Exported {n} {} to {}",
                if n == 1 { "row" } else { "rows" },
                path.display()
            )),
            FileListMsg::Exported(Err(e)) => self.toasts.error(format!("Export: {e}")),
            FileListMsg::Editor(msg) => self.filelist_editor.answered(msg),
        }
    }

    /// A search tab over `searcher`'s entries, listing all of them at once.
    pub fn open_list_tab(&mut self, p: usize, path: &Path, searcher: Arc<FileListSearcher>) {
        let dir = (path.parent())
            .filter(|d| d.is_absolute())
            .map_or_else(|| self.tab(p).dir.clone(), VPath::local);
        let mut tab = Tab::search(dir);
        tab.filelist = Some(searcher);
        if let TabKind::Search { due, .. } = &mut tab.kind {
            *due = Some(Instant::now());
        }
        let pane = &mut self.panes[p];
        pane.tabs.push(tab);
        pane.active = pane.tabs.len() - 1;
        pane.focus_filter = true;
        self.ctx.request_repaint();
    }
}

/// A file list tab's searcher over `list`, read from `path`.
pub fn searcher(path: &Path, list: FileList) -> Arc<FileListSearcher> {
    let label = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let searcher = FileListSearcher::new(label, list)
        .with_cap(LIST_HITS)
        .with_source(path.display().to_string());
    Arc::new(searcher)
}

/// Reads `path` on this (worker) thread and sends the tab's searcher.
fn load(pane: usize, path: PathBuf, tx: &crossbeam_channel::Sender<Msg>, ctx: &egui::Context) {
    let result = read_file(&path, &Limits::default())
        .map(|list| searcher(&path, list))
        .map_err(|e| format!("{e:#}"));
    worker::send(
        tx,
        ctx,
        Msg::FileList(FileListMsg::Loaded { pane, path, result }),
    );
}

/// Writes `list` to `path` as EFU (the File List Editor's Save).
pub fn save_efu(list: &FileList, path: &Path) -> anyhow::Result<()> {
    write_list(list, &ExportOptions::new(Format::Efu), path)
}

/// Writes `list` to `path` (through a temporary file next to it, so a failed write leaves
/// no half file under the name).
fn write_list(list: &FileList, opts: &ExportOptions, path: &Path) -> anyhow::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    let written = (|| -> anyhow::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        export(list, opts, &mut file)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written.and_then(|()| Ok(std::fs::rename(&tmp, path)?)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// `keel filelist create|export`

fn options(format: &ExportArgs, output: &Path) -> Result<ExportOptions, String> {
    let named = match &format.format {
        Some(f) => Some(Format::parse(f).ok_or_else(|| {
            format!("unknown format {f:?}: efu, csv, tsv, txt, m3u, m3u8 or json")
        })?),
        None => None,
    };
    let by_ext = output
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Format::parse);
    Ok(ExportOptions {
        format: named.or(by_ext).unwrap_or(Format::Efu),
        header: !format.no_header,
        bom: format.bom,
    })
}

/// Runs `keel filelist …`: 0 ok, 1 failed, 2 usage. `-o -` writes to stdout.
pub fn command(cmd: FilelistCmd, json: bool) -> i32 {
    use crate::commands::{FAILED, OK, USAGE};
    let (list, output, format) = match cmd {
        FilelistCmd::Create {
            folder,
            output,
            include_files,
            exclude_files,
            include_folders,
            exclude_folders,
            no_subfolders,
            no_folders,
            no_files,
            format,
        } => {
            let wild = |w: Option<String>| w.as_deref().map(split_wildcards).unwrap_or_default();
            let opts = CreateOptions {
                include_files: wild(include_files),
                exclude_files: wild(exclude_files),
                include_folders: wild(include_folders),
                exclude_folders: wild(exclude_folders),
                no_subfolders,
                no_folders,
                no_files,
            };
            match create(&folder, &opts, &Limits::default()) {
                Ok(list) => (list, output, format),
                Err(e) => return fail(json, &format!("{e:#}"), FAILED),
            }
        }
        FilelistCmd::Export {
            list,
            output,
            query,
            format,
        } => {
            let read = match read_file(&list, &Limits::default()) {
                Ok(read) => read,
                Err(e) => return fail(json, &format!("{e:#}"), FAILED),
            };
            match query {
                None => (read, output, format),
                Some(text) => match filter(read, &text) {
                    Ok(list) => (list, output, format),
                    Err(e) => return fail(json, &e, USAGE),
                },
            }
        }
    };
    let opts = match options(&format, &output) {
        Ok(opts) => opts,
        Err(e) => return fail(json, &e, USAGE),
    };
    let written = if output.as_os_str() == "-" {
        export(&list, &opts, &mut std::io::stdout().lock()).map_err(anyhow::Error::from)
    } else {
        write_list(&list, &opts, &output)
    };
    if let Err(e) = written {
        return fail(json, &format!("{}: {e:#}", output.display()), FAILED);
    }
    if output.as_os_str() != "-" {
        let n = list.entries.len();
        if json {
            let v = serde_json::json!({
                "file": output.display().to_string(),
                "format": opts.format.extension(),
                "entries": n,
            });
            println!("{v}");
        } else {
            println!("{n} entries written to {}", output.display());
        }
    }
    OK
}

/// The entries of `list` matching `text` (the search tab's syntax), in list order.
fn filter(list: FileList, text: &str) -> Result<FileList, String> {
    let columns = list.columns.clone();
    let searcher = FileListSearcher::new("", list).with_cap(u32::MAX);
    let q = Query {
        text: text.to_owned(),
        ..Query::default()
    };
    let hits = searcher.query(&q).map_err(|e| format!("query: {e:#}"))?;
    let wanted: std::collections::HashSet<String> = hits.iter().map(|h| h.path.display()).collect();
    let entries = (searcher.list().entries.iter())
        .filter(|e| wanted.contains(&VPath::local(&e.path).display()))
        .cloned()
        .collect();
    Ok(FileList { columns, entries })
}

fn fail(json: bool, text: &str, code: i32) -> i32 {
    if json {
        println!("{}", serde_json::json!({ "error": text }));
    } else {
        eprintln!("keel: {text}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir()
                .join(format!("keel-app-filelist-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn export_args(format: Option<&str>) -> ExportArgs {
        ExportArgs {
            format: format.map(str::to_owned),
            no_header: false,
            bom: false,
        }
    }

    #[test]
    fn create_and_export_commands() {
        let d = Dir::new("cmd");
        let src = d.0.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.mp3"), b"123").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), b"1").unwrap();
        let efu = d.0.join("out.efu");
        let code = command(
            FilelistCmd::Create {
                folder: src.clone(),
                output: efu.clone(),
                include_files: None,
                exclude_files: Some("*.txt".into()),
                include_folders: None,
                exclude_folders: None,
                no_subfolders: false,
                no_folders: false,
                no_files: false,
                format: export_args(None),
            },
            false,
        );
        assert_eq!(code, 0);
        let list = read_file(&efu, &Limits::default()).unwrap();
        let names: Vec<&str> = list.entries.iter().map(|e| e.name()).collect();
        assert_eq!(names, ["a.mp3", "sub"]);
        assert!(!d.0.join("out.efu.part").exists());

        // Export with a query, format from the name.
        let csv = d.0.join("mp3.csv");
        let code = command(
            FilelistCmd::Export {
                list: efu.clone(),
                output: csv.clone(),
                query: Some("*.mp3".into()),
                format: export_args(None),
            },
            true,
        );
        assert_eq!(code, 0);
        let text = std::fs::read_to_string(&csv).unwrap();
        assert!(text.starts_with("Filename,Size,Date Modified"), "{text}");
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("a.mp3\",3,"));

        // An unknown format is a usage error; a missing list fails.
        let bad = FilelistCmd::Export {
            list: efu.clone(),
            output: d.0.join("x.out"),
            query: None,
            format: export_args(Some("xls")),
        };
        assert_eq!(command(bad, true), 2);
        let missing = FilelistCmd::Export {
            list: d.0.join("missing.efu"),
            output: d.0.join("y.efu"),
            query: None,
            format: export_args(None),
        };
        assert_eq!(command(missing, false), 1);
        // No extension, no --format: EFU.
        let opts = options(&export_args(None), Path::new("list")).unwrap();
        assert_eq!(opts.format, Format::Efu);
        let opts = options(&export_args(Some("JSON")), Path::new("list.csv")).unwrap();
        assert_eq!(opts.format, Format::Json);
    }

    fn state() -> AppState {
        let dir = VPath::local(std::env::temp_dir());
        AppState::new(
            egui::Context::default(),
            Arc::new(keel_vfs::Router::new()),
            dir,
        )
    }

    fn loaded(state: &mut AppState) {
        let msg = loop {
            match state.rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                m @ Msg::FileList(_) => break m,
                other => state.apply(other),
            }
        };
        state.apply(msg);
    }

    fn searched(state: &mut AppState) {
        state.tick();
        let msg = loop {
            match state.rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                m @ Msg::Search { .. } => break m,
                other => state.apply(other),
            }
        };
        state.apply(msg);
    }

    #[test]
    fn a_file_list_opens_as_a_search_tab_and_exports() {
        let d = Dir::new("tab");
        let efu = d.0.join("music.efu");
        std::fs::write(
            &efu,
            "Filename,Size,Attributes\n/nowhere/Music,,16\n/nowhere/Music/song.mp3,100,32\n/nowhere/notes.txt,5,32\n",
        )
        .unwrap();
        let mut state = state();
        state.run(0, Action::FileList(FileListCmd::Open(Some(efu.clone()))));
        loaded(&mut state);
        let tab = state.tab(0);
        assert!(tab.is_search());
        assert_eq!(tab.title(), "music.efu");
        assert_eq!(tab.dir, VPath::local(&d.0));
        assert!(tab.error.is_none());
        // Everything at first (the paths need not exist: nothing is read from disk).
        searched(&mut state);
        assert_eq!(state.tab_mut(0).visible(false).len(), 3);
        // Then a query over the list.
        if let TabKind::Search { query, due, .. } = &mut state.tab_mut(0).kind {
            *query = "*.mp3".into();
            *due = Some(Instant::now());
        }
        searched(&mut state);
        assert_eq!(state.tab(0).title(), "music.efu: *.mp3");
        let tab = state.tab_mut(0);
        assert_eq!(tab.visible(false).len(), 1);
        let row = tab.entries()[tab.visible_cached()[0]].clone();
        assert_eq!((row.path.name(), row.size), ("song.mp3", 100));

        // Export the shown rows.
        let list = rows(state.tab(0), false);
        assert_eq!(list.entries.len(), 1);
        let out = d.0.join("out.m3u8");
        let opts = ExportOptions {
            format: Format::M3u8,
            header: true,
            bom: false,
        };
        write_list(&list, &opts, &out).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.starts_with("#EXTM3U"));
        assert!(text.contains("song.mp3"));

        // The dialog offers "selection only" once something is selected.
        state.run(0, Action::FileList(FileListCmd::Export));
        let Some(crate::dialogs::Dialog::ExportList(dialog)) = &state.dialog else {
            panic!("export dialog");
        };
        assert_eq!((dialog.shown, dialog.selected), (1, 0));
    }

    #[test]
    fn a_bad_list_says_why() {
        let d = Dir::new("bad");
        let efu = d.0.join("bad.efu");
        std::fs::write(&efu, "Path,Size\n/a,1\n").unwrap();
        let mut state = state();
        let tabs = state.panes[0].tabs.len();
        state.run(0, Action::FileList(FileListCmd::Open(Some(efu))));
        loaded(&mut state);
        assert_eq!(state.panes[0].tabs.len(), tabs, "no tab");
    }

    #[test]
    fn rows_become_entries() {
        let e = Entry {
            path: VPath::local(std::env::temp_dir().join("x.txt")),
            name: "x.txt".into(),
            kind: Kind::File,
            size: 9,
            modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(1)),
            hidden: true,
            is_link: false,
            encrypted: false,
            ext: "txt".into(),
        };
        let l = list_entry(&e);
        assert_eq!(l.size, Some(9));
        assert_eq!(l.attributes, Some(ATTR_HIDDEN));
        assert_eq!(l.modified, Some(116_444_736_010_000_000));
        assert!(l.path.ends_with("x.txt"));
    }
}
