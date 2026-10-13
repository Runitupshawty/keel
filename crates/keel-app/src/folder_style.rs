//! Folder colors and icons (Task 56), and file icons: Folder color and Folder icon in a
//! folder's context menu (File icon in a file's), the "Type an emoji…" dialog's check, and
//! drawing a styled folder or file in every view, the sidebar's library section and
//! Ctrl+P. Styles are the library's (`LibraryUi::styles`, by real path); only folders and
//! files in a library source take one, and a file only an icon.

use crate::a11y::Named;
use crate::keys::Action;
use crate::library::LibCmd;
use egui::{Color32, Id, Rect};
use keel_core::{FolderStyle, SourceSummary};
use keel_vfs::{Entry, Kind, VPath};
use std::collections::HashMap;
use std::sync::Arc;

/// Folder color: the tag palette (`library_ui::TAG_COLORS`).
pub use crate::library_ui::TAG_COLORS as COLORS;

/// Folder icon: common emoji (all in the window's emoji font, `theme` tests).
pub const EMOJI: [&str; 40] = [
    "📁", "📂", "⭐", "❤", "🔥", "✅", "❗", "📌", "🏠", "💼", "📷", "🎵", "🎬", "🎮", "📚", "📝",
    "💻", "🎓", "🔧", "📦", "📒", "💰", "📈", "✈", "🌍", "🌳", "🐱", "🐶", "🎨", "🎁", "🔒", "☁",
    "⚙", "📊", "📅", "👪", "🌴", "🚗", "🍕", "⚽",
];

/// Why a folder outside the library cannot be styled.
pub const NOT_IN_LIBRARY: &str = "Add the folder's drive to the library to style it";
/// The library is off.
pub const LIBRARY_OFF: &str = "Turn the library on (Settings → Library) to style folders";

/// Styles by `key` of the folder's real path.
pub type Styles = Arc<HashMap<String, FolderStyle>>;

/// A path as a style key (Windows folders: either case).
pub fn key(p: &VPath) -> String {
    let shown = p.display();
    let shown = shown.trim_end_matches(['/', '\\']);
    if cfg!(windows) && p.scheme == "file" {
        shown.to_lowercase()
    } else {
        shown.to_owned()
    }
}

/// What the context menu and Ctrl+P need of the library, published every frame.
#[derive(Clone, Default)]
struct Shared {
    open: bool,
    sources: Vec<SourceSummary>,
    styles: Styles,
}

fn shared_id() -> Id {
    Id::new("keel-folder-styles")
}

pub fn publish(ctx: &egui::Context, lib: &crate::library::LibraryUi) {
    let shared = Arc::new(Shared {
        open: lib.is_open() && !lib.lost,
        sources: lib.sources.clone(),
        styles: lib.styles.clone(),
    });
    ctx.data_mut(|d| d.insert_temp(shared_id(), shared));
}

fn shared(ctx: &egui::Context) -> Arc<Shared> {
    ctx.data(|d| d.get_temp(shared_id())).unwrap_or_default()
}

/// The style of the folder at `real` (a real path), as last published.
pub fn published(ctx: &egui::Context, real: &VPath) -> Option<FolderStyle> {
    let s = shared(ctx);
    (!s.styles.is_empty())
        .then(|| s.styles.get(&key(real)).cloned())
        .flatten()
}

/// The icon theme's names a folder's (`dir`) or a file's `theme:<name>` icon can take.
pub fn theme_names(dir: bool) -> Vec<String> {
    match dir {
        true => crate::icons::theme_folder_names(),
        false => crate::icons::theme_file_names(),
    }
}

/// Why the icon `icon` (typed by the user) cannot be set on a folder (`dir`) or file,
/// given its `theme_names` (`theme:<name>` must be one of them).
pub fn icon_error(icon: &str, theme_names: &[String], dir: bool) -> Option<String> {
    if let Some(name) = icon.strip_prefix(keel_core::THEME_ICON) {
        if !keel_core::valid_theme_icon(icon) || !theme_names.iter().any(|n| n == name) {
            let what = if dir { "folder" } else { "file" };
            return Some(match theme_names.is_empty() {
                true => format!("No icon theme with {what} icons is installed"),
                false => format!("The icon theme has no {what} icon named {name:?}"),
            });
        }
        return None;
    }
    (!keel_core::valid_emoji(icon)).then(|| "Type one emoji".into())
}

fn color32(hex: &str) -> Color32 {
    crate::library_ui::color_of(Some(hex))
}

fn cmd(path: &VPath, color: Option<Option<&str>>, icon: Option<Option<&str>>) -> Action {
    Action::Library(LibCmd::FolderStyle {
        path: path.clone(),
        color: color.map(|c| c.map(str::to_owned)),
        icon: icon.map(|i| i.map(str::to_owned)),
    })
}

/// Folder color and Folder icon for the folder at `path` (a context menu's item); File
/// icon for a file (`!dir`).
pub fn menu(ui: &mut egui::Ui, path: &VPath, dir: bool, out: &mut Vec<Action>) {
    let s = shared(ui.ctx());
    let located = s.open.then(|| crate::library::locate(&s.sources, path));
    let why = match located {
        None => Some(LIBRARY_OFF),
        Some(None) => Some(NOT_IN_LIBRARY),
        Some(Some(_)) => None,
    };
    let mut push = |ui: &mut egui::Ui, action: Action| {
        out.push(action);
        ui.close_menu();
    };
    if dir {
        ui.menu_button("Folder color", |ui| {
            if let Some(why) = why {
                ui.weak(why);
                return;
            }
            for (name, hex) in COLORS {
                let text = egui::RichText::new(format!("●  {name}"));
                let r = ui.add(egui::Button::new(text.color(color32(hex))));
                if r.named(name).clicked() {
                    push(ui, cmd(path, Some(Some(hex)), None));
                }
            }
            ui.separator();
            if ui.button("Clear").named("Clear color").clicked() {
                push(ui, cmd(path, Some(None), None));
            }
        });
    }
    let title = if dir { "Folder icon" } else { "File icon" };
    ui.menu_button(title, |ui| {
        if let Some(why) = why {
            ui.weak(why);
            return;
        }
        egui::Grid::new("keel-folder-emoji").show(ui, |ui| {
            for (i, emoji) in EMOJI.iter().enumerate() {
                let r = ui.add(egui::Button::new(egui::RichText::new(*emoji).size(18.0)));
                if r.named(emoji).clicked() {
                    push(ui, cmd(path, None, Some(Some(emoji))));
                }
                if i % 8 == 7 {
                    ui.end_row();
                }
            }
        });
        if ui.button("Type an emoji…").clicked() {
            let path = path.clone();
            push(ui, Action::Library(LibCmd::TypeFolderIcon { path, dir }));
        }
        let names = theme_names(dir);
        if !names.is_empty() {
            ui.menu_button("Icon theme", |ui| {
                // A theme maps thousands of file names: only the rows in view are drawn.
                let row = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
                egui::ScrollArea::vertical().max_height(360.0).show_rows(
                    ui,
                    row,
                    names.len(),
                    |ui, rows| {
                        for name in &names[rows] {
                            let src = match dir {
                                true => crate::icons::named_folder(name),
                                false => Some(crate::icons::file_icon(name)),
                            };
                            let Some(src) = src else {
                                continue;
                            };
                            let img = egui::Image::new(src).fit_to_exact_size([16.0, 16.0].into());
                            let r = ui.add(egui::Button::image_and_text(img, name.as_str()));
                            if r.clicked() {
                                let icon = format!("{}{name}", keel_core::THEME_ICON);
                                push(ui, cmd(path, None, Some(Some(&icon))));
                            }
                        }
                    },
                );
            });
        }
        ui.separator();
        if ui.button("Clear").named("Clear icon").clicked() {
            push(ui, cmd(path, None, Some(None)));
        }
    });
}

/// Draws an icon in `rect`: a styled folder's or file's emoji (in its color), else `base`
/// (or the `theme:<name>` icon when set: the icon theme's folder icon when installed, or
/// for a file (`!dir`) the file icon of a file named `<name>`) tinted with its color.
/// `large`: a tile (the icon rasterized at tile size).
pub fn paint(
    ui: &egui::Ui,
    rect: Rect,
    base: egui::ImageSource<'static>,
    style: Option<&FolderStyle>,
    dir: bool,
    large: bool,
) {
    let color = (style.and_then(FolderStyle::rgb)).map(|[r, g, b]| Color32::from_rgb(r, g, b));
    if let Some(emoji) = style.and_then(FolderStyle::emoji) {
        let ink = color.unwrap_or_else(|| ui.visuals().text_color());
        // The emoji font has no variation selectors (they would draw as boxes).
        let emoji: String = (emoji.chars())
            .filter(|c| !matches!(c, '\u{fe0e}' | '\u{fe0f}'))
            .collect();
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            emoji,
            egui::FontId::proportional(rect.height() * 0.85),
            ink,
        );
        return;
    }
    let src = (style.and_then(FolderStyle::theme_icon))
        .and_then(|name| match dir {
            true => crate::icons::named_folder(name),
            false => Some(crate::icons::file_icon(name)),
        })
        .unwrap_or(base);
    // Keel Finder icons are drawn for the slot's size and in the folder's color; other
    // themes' icons are tinted.
    let px = rect.size().max_elem() * ui.ctx().pixels_per_point();
    let rgb = color.map(|c| [c.r(), c.g(), c.b()]);
    let (src, tint) = match crate::icons::sized(&src, px, rgb) {
        Some(sized) => (sized, None),
        None if large => (crate::icons::large(src), color),
        None => (src, color),
    };
    let mut img = egui::Image::new(src).fit_to_exact_size(rect.size());
    if let Some(c) = tint {
        // Multiplied onto the icon's own (mid-grey) colour: lightened first, so the folder
        // shows the color rather than a dark shade of it.
        let up = |v: u8| (v as f32 * 1.6).min(255.0) as u8;
        img = img.tint(Color32::from_rgb(up(c.r()), up(c.g()), up(c.b())));
    }
    img.paint_at(ui, rect);
}

/// `paint` for a listed entry: its icon, styled.
pub fn paint_entry(ui: &egui::Ui, rect: Rect, e: &Entry, style: Option<&FolderStyle>, large: bool) {
    let dir = e.kind == Kind::Dir;
    paint(ui, rect, crate::icons::icon_for(e), style, dir, large);
}

/// The icon in words ("Folder icon: 📷", a file's "File icon: 📷").
fn icon_words(style: &FolderStyle, dir: bool) -> Option<String> {
    let what = if dir { "Folder" } else { "File" };
    (style.emoji().or(style.theme_icon())).map(|i| format!("{what} icon: {i}"))
}

/// A style in words, for screen readers ("Folder icon: 📷, Folder color: Red").
pub fn describe(style: &FolderStyle, dir: bool) -> String {
    let icon = icon_words(style, dir);
    let color = style.color.as_deref().map(|hex| {
        let name = (COLORS.iter()).find(|(_, h)| h.eq_ignore_ascii_case(hex));
        format!("Folder color: {}", name.map_or(hex, |(n, _)| n))
    });
    [icon, color]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
}

/// `paint` in a 16 px slot of a layout (Details rows, the sidebar); a custom icon is
/// named for screen readers ("Folder icon: 📷", "File icon: 📷").
pub fn slot(
    ui: &mut egui::Ui,
    base: egui::ImageSource<'static>,
    style: Option<&FolderStyle>,
    dir: bool,
) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    paint(ui, rect, base, style, dir, false);
    if let Some(words) = style.and_then(|s| icon_words(s, dir)) {
        resp.named(&words);
    }
}

/// The Details view's dot after a colored folder's name, outlined in the text color so
/// light colors (Yellow on a light theme is 1.5:1) still stand out from the background.
pub fn dot(ui: &mut egui::Ui, style: Option<&FolderStyle>) {
    let Some([r, g, b]) = style.and_then(FolderStyle::rgb) else {
        return;
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let outline = egui::Stroke::new(1.0_f32, ui.visuals().text_color());
    ui.painter()
        .circle(rect.center(), 4.0, Color32::from_rgb(r, g, b), outline);
    let color = FolderStyle {
        color: style.and_then(|s| s.color.clone()),
        icon: None,
    };
    resp.named(&describe(&color, true));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_icons_are_checked() {
        let theme = vec!["docs".to_owned(), "src".to_owned()];
        assert_eq!(icon_error("📁", &theme, true), None);
        assert_eq!(icon_error("👨‍👩‍👧", &[], true), None);
        assert_eq!(icon_error("theme:src", &theme, true), None);
        // A theme name the installed theme does not have, or no theme at all.
        assert!(icon_error("theme:music", &theme, true)
            .unwrap()
            .contains("no folder icon named"));
        assert!(icon_error("theme:src", &[], true)
            .unwrap()
            .contains("No icon theme"));
        assert!(icon_error("theme:", &theme, true).is_some());
        // A file's: its theme's file names and `.ext`.
        let files = vec![".rs".to_owned(), "package.json".to_owned()];
        assert_eq!(icon_error("theme:.rs", &files, false), None);
        assert_eq!(icon_error("📌", &[], false), None);
        assert!(icon_error("theme:.py", &files, false)
            .unwrap()
            .contains("no file icon named"));
        assert!(icon_error("theme:.rs", &[], false)
            .unwrap()
            .contains("No icon theme with file icons"));
        // Not one emoji.
        for bad in ["a", "📁📁", "", "folder", "#"] {
            assert_eq!(
                icon_error(bad, &theme, true).as_deref(),
                Some("Type one emoji"),
                "{bad}"
            );
        }
        for emoji in EMOJI {
            assert_eq!(icon_error(emoji, &[], false), None, "{emoji}");
        }
        for (_, hex) in COLORS {
            assert!(keel_core::valid_color(hex), "{hex}");
        }
    }

    #[test]
    fn keys_ignore_a_trailing_separator_and_windows_case() {
        let a = VPath::parse("mem://t/Work/").unwrap();
        let b = VPath::parse("mem://t/Work").unwrap();
        assert_eq!(key(&a), key(&b));
        if cfg!(windows) {
            assert_eq!(
                key(&VPath::local(r"C:\Work")),
                key(&VPath::local(r"c:\work"))
            );
        }
    }
}
