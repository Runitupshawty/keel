//! Properties dialog (all OSes): size, item counts, dates and attributes of the targets,
//! gathered on a worker. On Windows it also offers Explorer's own Properties sheet.

use crate::keys::Action;
use crate::view_details::size_text_of;
use keel_vfs::{Kind, Router, VPath};
use std::time::{Instant, SystemTime};

/// A folder walk stops after this many items (the dialog says the totals are partial).
const MAX_ITEMS: u64 = 2_000_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Props {
    pub name: String,
    pub location: String,
    pub kind: String,
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub hidden: bool,
    pub read_only: bool,
    pub link: bool,
    /// Totals stopped early (too many items, or a remote folder that is not walked).
    pub partial: bool,
}

/// Gathers the properties of `paths` (one item, or a summary of several). Walks local
/// folders without following links. Blocks: workers only.
pub fn gather(router: &Router, paths: &[VPath]) -> anyhow::Result<Props> {
    let first = paths
        .first()
        .ok_or_else(|| anyhow::anyhow!("nothing selected"))?;
    let mut p = Props {
        location: first.parent().map(|d| d.display()).unwrap_or_default(),
        ..Props::default()
    };
    let started = Instant::now();
    for path in paths {
        match path.to_local_path() {
            Some(local) => local_props(&local, &mut p)?,
            None => {
                let provider = router
                    .provider_for(path)
                    .ok_or_else(|| anyhow::anyhow!("no provider for {}", path.display()))?;
                let e = provider.stat(path)?;
                if e.kind == Kind::Dir {
                    p.folders += 1;
                    p.partial = true;
                } else {
                    p.files += 1;
                    p.bytes += e.size;
                }
                p.modified = e.modified;
                p.hidden = e.hidden;
                p.link = e.is_link;
            }
        }
    }
    tracing::debug!(
        "properties of {} items in {:?}",
        paths.len(),
        started.elapsed()
    );
    if let [one] = paths {
        p.name = one.name().to_owned();
        p.kind = if p.folders > 0 && !p.link {
            "Folder".into()
        } else if p.link {
            "Link".into()
        } else {
            match one.name().rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{} file", ext.to_uppercase()),
                _ => "File".into(),
            }
        };
        // The item itself is not one of its contents.
        if p.kind == "Folder" {
            p.folders -= 1;
        }
    } else {
        p.name = format!("{} items", paths.len());
        p.kind = "Several items".into();
        p.modified = None;
        p.created = None;
    }
    Ok(p)
}

fn local_props(root: &std::path::Path, p: &mut Props) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(root)?;
    p.modified = meta.modified().ok();
    p.created = meta.created().ok();
    p.read_only = meta.permissions().readonly();
    p.link = meta.file_type().is_symlink();
    p.hidden = is_hidden(root, &meta);
    let mut stack = vec![(root.to_path_buf(), meta)];
    while let Some((path, meta)) = stack.pop() {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            p.folders += 1;
            // Unreadable folders count, their contents do not.
            let Ok(children) = std::fs::read_dir(&path) else {
                continue;
            };
            for child in children.flatten() {
                if p.files + p.folders >= MAX_ITEMS {
                    p.partial = true;
                    return Ok(());
                }
                if let Ok(m) = child.metadata() {
                    stack.push((child.path(), m));
                }
            }
        } else {
            p.files += 1;
            p.bytes += meta.len();
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_hidden(_: &std::path::Path, meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x2 != 0
}

#[cfg(not(windows))]
fn is_hidden(path: &std::path::Path, _: &std::fs::Metadata) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn date(t: Option<SystemTime>) -> String {
    t.map(|t| {
        chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    })
    .unwrap_or_else(|| "—".into())
}

/// "2 files, 1 folder": the Contains row.
fn contains(files: u64, folders: u64) -> String {
    let n = |n: u64, one: &str| match n {
        1 => format!("1 {one}"),
        n => format!("{n} {one}s"),
    };
    format!("{}, {}", n(files, "file"), n(folders, "folder"))
}

/// The dialog body; `info` is None while the worker is still counting.
pub fn ui(
    ui: &mut egui::Ui,
    paths: &[VPath],
    info: &Option<Result<Props, String>>,
    out: &mut Option<Action>,
) {
    let p = match info {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading properties…");
            });
            return;
        }
        Some(Err(e)) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return;
        }
        Some(Ok(p)) => p,
    };
    ui.heading(&p.name);
    ui.add_space(4.0);
    egui::Grid::new("props")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.weak(k);
                ui.add(egui::Label::new(v).truncate());
                ui.end_row();
            };
            row("Type", p.kind.clone());
            row("Location", p.location.clone());
            let more = if p.partial { "at least " } else { "" };
            row(
                "Size",
                format!("{more}{} ({} bytes)", size_text_of(p.bytes), p.bytes),
            );
            if p.folders > 0 || p.kind == "Folder" || paths.len() > 1 {
                row(
                    "Contains",
                    format!("{more}{}", contains(p.files, p.folders)),
                );
            }
            if paths.len() == 1 {
                row("Modified", date(p.modified));
                row("Created", date(p.created));
                let attrs: Vec<&str> = [
                    (p.read_only, "read-only"),
                    (p.hidden, "hidden"),
                    (p.link, "link"),
                ]
                .into_iter()
                .filter_map(|(on, name)| on.then_some(name))
                .collect();
                if !attrs.is_empty() {
                    row("Attributes", attrs.join(", "));
                }
            }
        });
    if cfg!(windows) {
        if let [one] = paths {
            if let Some(local) = one.to_local_path() {
                ui.add_space(6.0);
                if ui.button("Windows Properties…").clicked() {
                    *out = Some(Action::ShellProperties(local));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_row_counts_in_the_singular_too() {
        assert_eq!(contains(2, 1), "2 files, 1 folder");
        assert_eq!(contains(1, 0), "1 file, 0 folders");
        assert_eq!(contains(0, 3), "0 files, 3 folders");
    }

    #[test]
    fn folder_totals_count_files_and_subfolders() {
        let dir = std::env::temp_dir().join(format!("keel-props-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("one.txt"), "12345").unwrap();
        std::fs::write(dir.join("a/two.txt"), "123").unwrap();
        std::fs::write(dir.join("a/b/three.bin"), "1").unwrap();
        let router = Router::new();
        let p = gather(&router, &[VPath::local(&dir)]).unwrap();
        assert_eq!(
            (p.kind.as_str(), p.files, p.folders, p.bytes),
            ("Folder", 3, 2, 9)
        );
        assert!(!p.partial);
        let f = gather(&router, &[VPath::local(dir.join("one.txt"))]).unwrap();
        assert_eq!((f.kind.as_str(), f.bytes, f.files), ("TXT file", 5, 1));
        assert!(f.modified.is_some());
        let two = [
            VPath::local(dir.join("one.txt")),
            VPath::local(dir.join("a")),
        ];
        let s = gather(&router, &two).unwrap();
        assert_eq!(
            (s.name.as_str(), s.files, s.folders, s.bytes),
            ("2 items", 3, 2, 9)
        );
        assert!(gather(&router, &[VPath::local(dir.join("missing"))]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dialog_draws_every_state() {
        let ctx = egui::Context::default();
        let paths = [VPath::local(std::env::temp_dir())];
        for info in [
            None,
            Some(Err("gone".to_owned())),
            Some(Ok(Props {
                name: "x".into(),
                kind: "Folder".into(),
                partial: true,
                ..Props::default()
            })),
        ] {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |p| ui(p, &paths, &info, &mut None));
            });
        }
    }
}
