//! Task 64: design tokens. One place for the spacing scale, corner radii, row heights,
//! type scale, tag colors and the semantic colors of the two appearance styles:
//!
//! - **Finder** (the default for new profiles): Finder's spacing, rounder corners, a
//!   13 px body, the Finder palette on the stock dark and light themes and the system
//!   accent color.
//! - **Classic**: Keel's look before Phase 14, value for value (egui's defaults and the
//!   theme's own colors); a test holds it to egui's defaults.
//!
//! A color theme (`theme`) sets colors on top of either style; the style sets shapes,
//! spacing and type. Only the stock dark and light themes take the Finder palette.

use crate::theme::{ratio, rgb, Theme};
use egui::{vec2, Color32, Context, CornerRadius, FontFamily, FontId, Id, Margin, TextStyle, Vec2};
use std::sync::OnceLock;

/// The spacing scale (4 px base); gaps between and around controls come from it. The
/// whole scale is defined; not every step has a user yet.
#[allow(dead_code)]
pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const S: f32 = 6.0;
    pub const M: f32 = 8.0;
    pub const L: f32 = 12.0;
    pub const XL: f32 = 16.0;
    pub const XXL: f32 = 20.0;
    pub const XXXL: f32 = 24.0;
    /// The whole scale, smallest first.
    pub const SCALE: [f32; 8] = [XXS, XS, S, M, L, XL, XXL, XXXL];
}

/// Window chrome only the Finder style has (the unified toolbar and the sidebar of the
/// tasks after 64 read these).
#[allow(dead_code)]
pub mod finder {
    /// A sidebar row.
    pub const SIDEBAR_ROW: f32 = 24.0;
    /// The unified toolbar, icons only / icons with labels.
    pub const TOOLBAR: f32 = 38.0;
    pub const TOOLBAR_LABELS: f32 = 52.0;
    /// Window corners (macOS 11 and later).
    pub const WINDOW_RADIUS: f32 = 10.0;
}

/// Appearance style: shapes, spacing and type (Settings → Appearance).
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    #[default]
    Finder,
    Classic,
}

impl Style {
    pub const ALL: [Style; 2] = [Style::Finder, Style::Classic];

    /// What a config.toml written before the setting existed gets: the look it had.
    pub fn existing() -> Style {
        Style::Classic
    }

    pub fn label(self) -> &'static str {
        match self {
            Style::Finder => crate::i18n::tr("settings.style_finder"),
            Style::Classic => crate::i18n::tr("settings.style_classic"),
        }
    }

    pub fn tokens(self) -> &'static Tokens {
        match self {
            Style::Finder => &FINDER,
            Style::Classic => &CLASSIC,
        }
    }
}

/// Where the accent color comes from (Settings → Appearance → Accent).
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Accent {
    /// Finder style on the stock dark and light themes: the system accent; otherwise
    /// the theme's.
    #[default]
    Auto,
    /// The system accent color (blue where the OS has none).
    System,
    /// The color theme's accent.
    Theme,
    /// A color of the user's.
    Custom([u8; 3]),
}

impl Accent {
    pub fn label(self) -> &'static str {
        match self {
            Accent::Auto => crate::i18n::tr("settings.accent_auto"),
            Accent::System => crate::i18n::tr("settings.accent_system"),
            Accent::Theme => crate::i18n::tr("settings.accent_theme"),
            Accent::Custom(_) => crate::i18n::tr("settings.accent_custom"),
        }
    }
}

/// Corner radii in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Radii {
    /// Buttons, text fields, combo boxes.
    pub control: u8,
    /// A control under the pointer.
    pub control_hovered: u8,
    /// Windows and dialogs.
    pub window: u8,
    /// Menus and popups.
    pub menu: u8,
    /// The selection of a tile (grid and media views).
    pub selection: f32,
    /// The selection of a row drawn by Keel (Columns view).
    pub selection_row: f32,
    /// Filter and tag chips.
    pub chip: f32,
    /// Progress and share bars.
    pub bar: f32,
}

/// Row heights in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rows {
    /// A list row at the default size (Details and Columns views).
    pub list: f32,
    /// A Details row with a content snippet under the name.
    pub snippet: f32,
}

/// Font sizes in points (one family).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeScale {
    pub small: f32,
    pub body: f32,
    pub button: f32,
    pub monospace: f32,
    pub heading: f32,
}

/// One style's shapes, spacing and type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub item_spacing: Vec2,
    pub button_padding: Vec2,
    /// Height of an interactive control.
    pub interact_height: f32,
    pub window_margin: i8,
    pub menu_margin: i8,
    pub radius: Radii,
    pub rows: Rows,
    pub text: TypeScale,
}

/// Classic: egui's defaults and Keel's constants before Phase 14.
pub const CLASSIC: Tokens = Tokens {
    item_spacing: vec2(space::M, 3.0),
    button_padding: vec2(space::XS, 1.0),
    interact_height: 18.0,
    window_margin: 6,
    menu_margin: 6,
    radius: Radii {
        control: 2,
        control_hovered: 3,
        window: 6,
        menu: 6,
        selection: 4.0,
        selection_row: 2.0,
        chip: 8.0,
        bar: 2.0,
    },
    rows: Rows {
        list: 22.0,
        snippet: 40.0,
    },
    text: TypeScale {
        small: 9.0,
        body: 12.5,
        button: 12.5,
        monospace: 12.0,
        heading: 18.0,
    },
};

/// Finder: 24 px sidebar rows (20 + 4), rounder controls, the 11 / 12 / 13 / 15 scale.
pub const FINDER: Tokens = Tokens {
    item_spacing: vec2(space::M, space::XS),
    button_padding: vec2(space::M, 3.0),
    interact_height: 20.0,
    window_margin: 12,
    menu_margin: 6,
    radius: Radii {
        control: 6,
        control_hovered: 6,
        window: 10,
        menu: 8,
        selection: 5.0,
        selection_row: 5.0,
        chip: 8.0,
        bar: 2.0,
    },
    rows: Rows {
        list: 22.0,
        snippet: 40.0,
    },
    text: TypeScale {
        small: 11.0,
        body: 13.0,
        button: 13.0,
        monospace: 12.0,
        heading: 15.0,
    },
};

impl Tokens {
    /// Spacing and type onto an egui style (both of egui's themes get the same).
    pub fn apply_spacing(&self, style: &mut egui::Style) {
        let s = &mut style.spacing;
        s.item_spacing = self.item_spacing;
        s.button_padding = self.button_padding;
        s.interact_size.y = self.interact_height;
        s.window_margin = Margin::same(self.window_margin);
        s.menu_margin = Margin::same(self.menu_margin);
        let t = &self.text;
        for (style_name, size, family) in [
            (TextStyle::Small, t.small, FontFamily::Proportional),
            (TextStyle::Body, t.body, FontFamily::Proportional),
            (TextStyle::Button, t.button, FontFamily::Proportional),
            (TextStyle::Monospace, t.monospace, FontFamily::Monospace),
            (TextStyle::Heading, t.heading, FontFamily::Proportional),
        ] {
            style
                .text_styles
                .insert(style_name, FontId::new(size, family));
        }
    }

    /// Corner radii onto egui's visuals.
    pub fn apply_radii(&self, v: &mut egui::Visuals) {
        let control = CornerRadius::same(self.radius.control);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = control;
        }
        v.widgets.hovered.corner_radius = CornerRadius::same(self.radius.control_hovered);
        v.window_corner_radius = CornerRadius::same(self.radius.window);
        v.menu_corner_radius = CornerRadius::same(self.radius.menu);
    }
}

/// Finder's tag colors (none is a tag without a color), for the sidebar's Tags section
/// and tag dots (Phase 14 tasks after this one).
#[allow(dead_code)]
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TagColor {
    None,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Gray,
}

#[allow(dead_code)]
impl TagColor {
    pub const ALL: [TagColor; 8] = [
        TagColor::Red,
        TagColor::Orange,
        TagColor::Yellow,
        TagColor::Green,
        TagColor::Blue,
        TagColor::Purple,
        TagColor::Gray,
        TagColor::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TagColor::None => "None",
            TagColor::Red => "Red",
            TagColor::Orange => "Orange",
            TagColor::Yellow => "Yellow",
            TagColor::Green => "Green",
            TagColor::Blue => "Blue",
            TagColor::Purple => "Purple",
            TagColor::Gray => "Gray",
        }
    }

    /// The dot's fill; `None` has none (an outline only).
    pub fn fill(self) -> Option<Color32> {
        let [r, g, b] = match self {
            TagColor::None => return None,
            TagColor::Red => [0xff, 0x3b, 0x30],
            TagColor::Orange => [0xff, 0x95, 0x00],
            TagColor::Yellow => [0xff, 0xcc, 0x00],
            TagColor::Green => [0x28, 0xcd, 0x41],
            TagColor::Blue => [0x00, 0x7a, 0xff],
            TagColor::Purple => [0xaf, 0x52, 0xde],
            TagColor::Gray => [0x8e, 0x8e, 0x93],
        };
        Some(Color32::from_rgb(r, g, b))
    }
}

/// Black or white, whichever reads better on `fill`.
#[allow(dead_code)]
pub fn on_color(fill: Color32) -> Color32 {
    if ratio(Color32::BLACK, fill) >= ratio(Color32::WHITE, fill) {
        Color32::BLACK
    } else {
        Color32::WHITE
    }
}

/// Status dots: connected / online, and connecting / indexing.
pub const STATUS_OK: Color32 = Color32::from_rgb(70, 180, 90);
pub const STATUS_BUSY: Color32 = Color32::from_rgb(230, 180, 40);

/// The accent where nothing else gives one.
pub const BLUE: [u8; 3] = [0x00, 0x7a, 0xff];

/// The semantic colors the window is drawn with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window background: toolbar, status bar, side panels.
    pub window: Color32,
    /// The sidebar's material.
    pub sidebar: Color32,
    /// File lists and other content.
    pub content: Color32,
    pub separator: Color32,
    /// Every other list row.
    pub alternating_row: Color32,
    pub label: Color32,
    pub secondary_label: Color32,
    /// Placeholders and disabled text.
    pub tertiary_label: Color32,
    /// Links and accented text (legible on every background above).
    pub accent: Color32,
    /// The keyboard focus ring.
    pub focus_ring: Color32,
    /// The selection egui's widgets and the current views draw, under `label` text.
    pub selection: Color32,
    /// A solid selection in a focused list, under `on_selected` text.
    pub selected_focused: Color32,
    /// A selection in a list without the keyboard focus, under `label` text.
    pub selected_unfocused: Color32,
    pub on_selected: Color32,
}

impl Palette {
    /// The colors `theme` is drawn with in `look`.
    pub fn of(theme: &Theme, look: &Look) -> Palette {
        let stock = is_stock(theme);
        let system = || look.os.unwrap_or(BLUE);
        let accent = match look.accent {
            Accent::Theme => None,
            Accent::Custom(c) => Some(c),
            Accent::System => Some(system()),
            Accent::Auto => (look.style == Style::Finder && stock).then(system),
        };
        if look.style == Style::Finder && stock {
            Palette::finder(theme.dark, rgb(accent.unwrap_or(BLUE)))
        } else {
            Palette::classic(theme, accent.map(rgb))
        }
    }

    /// A theme's own colors, as Keel drew them before Phase 14; an accent other than the
    /// theme's is moved toward legibility.
    pub fn classic(theme: &Theme, accent: Option<Color32>) -> Palette {
        let defaults = if theme.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        let (bg, panel, text) = (rgb(theme.bg), rgb(theme.panel), rgb(theme.text));
        let stripe = panel.lerp_to_gamma(text, 0.04);
        let selection = rgb(theme.selection);
        Palette {
            dark: theme.dark,
            window: panel,
            sidebar: panel,
            content: bg,
            separator: defaults.widgets.noninteractive.bg_stroke.color,
            alternating_row: stripe,
            label: text,
            secondary_label: theme.muted(),
            tertiary_label: theme.muted(),
            accent: accent.map_or(theme.accent(), |a| legible(a, &[bg, panel, stripe], 4.5)),
            focus_ring: defaults.selection.stroke.color,
            selection,
            selected_focused: selection,
            selected_unfocused: selection,
            on_selected: text,
        }
    }

    /// Finder's palette for a light or dark window with `accent`.
    pub fn finder(dark: bool, accent: Color32) -> Palette {
        let hex = |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        let (window, sidebar, content, alternating_row, separator) = if dark {
            (
                hex(0x323232),
                hex(0x282828),
                hex(0x1e1e1e),
                hex(0x262626),
                hex(0x3d3d3d),
            )
        } else {
            (
                hex(0xececec),
                hex(0xe6e6e6),
                hex(0xffffff),
                hex(0xf4f5f5),
                hex(0xd5d5d5),
            )
        };
        let (label, secondary_label, tertiary_label, selected_unfocused) = if dark {
            (hex(0xe5e5e5), hex(0xa5a5aa), hex(0x86868b), hex(0x464649))
        } else {
            (hex(0x1f1f1f), hex(0x646469), hex(0x7a7a7f), hex(0xdcdcdc))
        };
        let backs = [window, sidebar, content, alternating_row];
        // A tint of the accent over the content, as light as it must be for the label.
        let mut t: f32 = if dark { 0.4 } else { 0.22 };
        let mut selection = content.lerp_to_gamma(accent, t);
        while ratio(label, selection) < 4.5 && t > 0.0 {
            t = (t - 0.02).max(0.0);
            selection = content.lerp_to_gamma(accent, t);
        }
        let ring_backs = [window, sidebar, content, alternating_row, selection];
        Palette {
            dark,
            window,
            sidebar,
            content,
            separator,
            alternating_row,
            label,
            secondary_label,
            tertiary_label,
            accent: legible(accent, &backs, 4.5),
            focus_ring: legible(accent, &ring_backs, 3.0),
            selection,
            selected_focused: legible(accent, &[Color32::WHITE], 4.5),
            selected_unfocused,
            on_selected: Color32::WHITE,
        }
    }

    /// `theme` with this palette's colors, for code that reads a theme's fields.
    pub fn onto(&self, theme: &Theme) -> Theme {
        let c = |c: Color32| [c.r(), c.g(), c.b()];
        Theme {
            bg: c(self.content),
            panel: c(self.window),
            text: c(self.label),
            accent: c(self.accent),
            selection: c(self.selection),
            muted: c(self.secondary_label),
            ..theme.clone()
        }
    }
}

/// `fg`, moved toward white (on dark backgrounds) or black until it reaches `min`
/// against every one of `backs`.
pub fn legible(fg: Color32, backs: &[Color32], min: f64) -> Color32 {
    let worst = |c: Color32| {
        backs
            .iter()
            .map(|b| ratio(c, *b))
            .fold(f64::INFINITY, f64::min)
    };
    if worst(fg) >= min {
        return fg;
    }
    let mean = backs
        .iter()
        .map(|b| crate::theme::luminance(*b))
        .sum::<f64>()
        / (backs.len().max(1) as f64);
    let dark_backs = mean < 0.18;
    let toward = if dark_backs {
        Color32::WHITE
    } else {
        Color32::BLACK
    };
    let mut best = fg;
    for step in 1..=50 {
        best = fg.lerp_to_gamma(toward, step as f32 / 50.0);
        if worst(best) >= min {
            break;
        }
    }
    best
}

/// The stock dark or light theme, unchanged (a user file of that name is not).
pub fn is_stock(theme: &Theme) -> bool {
    static STOCK: OnceLock<Vec<Theme>> = OnceLock::new();
    let stock = STOCK.get_or_init(|| {
        crate::theme::BUILTIN[..2]
            .iter()
            .filter_map(|(_, s)| toml::from_str(s).ok())
            .collect()
    });
    stock.contains(theme)
}

/// What the window is drawn in: the style, the accent setting and the system accent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    pub style: Style,
    pub accent: Accent,
    /// The system accent, once read (`os_accent`).
    pub os: Option<[u8; 3]>,
}

impl Look {
    /// Classic with the theme's colors: how a context nobody set a look on is drawn.
    pub const CLASSIC: Look = Look {
        style: Style::Classic,
        accent: Accent::Auto,
        os: None,
    };
}

#[derive(Clone, Copy)]
struct Applied {
    look: Look,
    palette: Option<Palette>,
}

fn id() -> Id {
    Id::new("keel-design")
}

fn applied(ctx: &Context) -> Applied {
    ctx.data(|d| d.get_temp::<Applied>(id()))
        .unwrap_or(Applied {
            look: Look::CLASSIC,
            palette: None,
        })
}

/// The look the window is drawn in (Classic until one is set).
pub fn look(ctx: &Context) -> Look {
    applied(ctx).look
}

pub fn set_look(ctx: &Context, look: Look) {
    let palette = applied(ctx).palette;
    ctx.data_mut(|d| d.insert_temp(id(), Applied { look, palette }));
}

/// The active style's tokens.
pub fn tokens(ctx: &Context) -> &'static Tokens {
    look(ctx).style.tokens()
}

/// The palette the theme was last applied with (`Theme::apply`).
pub fn palette(ctx: &Context) -> Option<Palette> {
    applied(ctx).palette
}

pub(crate) fn set_palette(ctx: &Context, palette: Palette) {
    let look = look(ctx);
    ctx.data_mut(|d| {
        d.insert_temp(
            id(),
            Applied {
                look,
                palette: Some(palette),
            },
        )
    });
}

/// `theme` as the window draws it: with the colors it was last applied with.
pub fn shown(ctx: &Context, theme: &Theme) -> Theme {
    palette(ctx).map_or_else(|| theme.clone(), |p| p.onto(theme))
}

/// Every frame: the look from the settings; on a change the theme is applied again. The
/// system accent is read on a worker thread the first time it is needed.
pub fn sync(ctx: &Context, settings: &crate::settings::Settings, theme: &Theme) {
    let needs_os = match settings.accent {
        Accent::System => true,
        Accent::Auto => settings.style == Style::Finder,
        Accent::Theme | Accent::Custom(_) => false,
    };
    let want = Look {
        style: settings.style,
        accent: settings.accent,
        os: if needs_os { os_accent(ctx) } else { None },
    };
    if look(ctx) != want {
        set_look(ctx, want);
        theme.apply(ctx);
    }
}

static OS_ACCENT: OnceLock<Option<[u8; 3]>> = OnceLock::new();

/// The system accent color once read (None before that, or when the OS has none). The
/// first call starts the read on a worker thread and repaints when it is done.
pub fn os_accent(ctx: &Context) -> Option<[u8; 3]> {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("os-accent".into())
            .spawn(move || {
                let _ = OS_ACCENT.set(read_os_accent());
                ctx.request_repaint();
            });
        if spawned.is_err() {
            let _ = OS_ACCENT.set(None);
        }
    });
    OS_ACCENT.get().copied().flatten()
}

/// Windows: the accent of Settings → Personalization → Colors (the DWM's `AccentColor`,
/// 0xAABBGGRR).
#[cfg(windows)]
fn read_os_accent() -> Option<[u8; 3]> {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `value` holds `size` bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\DWM"),
            w!("AccentColor"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    rc.is_ok().then(|| abgr(value))
}

/// macOS: `AppleAccentColor` (absent: multicolor, which is blue).
#[cfg(target_os = "macos")]
fn read_os_accent() -> Option<[u8; 3]> {
    let out = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleAccentColor"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return Some(BLUE);
    }
    let n = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    mac_accent(n)
}

/// Linux: GNOME's `accent-color` (GNOME 47 and later); elsewhere none.
#[cfg(not(any(windows, target_os = "macos")))]
fn read_os_accent() -> Option<[u8; 3]> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "accent-color"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    gnome_accent(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(any(windows, test))]
fn abgr(v: u32) -> [u8; 3] {
    [v as u8, (v >> 8) as u8, (v >> 16) as u8]
}

/// macOS accent numbers (System Settings → Appearance).
#[cfg(any(target_os = "macos", test))]
fn mac_accent(n: i32) -> Option<[u8; 3]> {
    Some(match n {
        -1 => [0x8e, 0x8e, 0x93], // graphite
        0 => [0xff, 0x3b, 0x30],
        1 => [0xff, 0x95, 0x00],
        2 => [0xff, 0xcc, 0x00],
        3 => [0x28, 0xcd, 0x41],
        4 => BLUE,
        5 => [0xaf, 0x52, 0xde],
        6 => [0xff, 0x2d, 0x55], // pink
        _ => return None,
    })
}

/// GNOME accent names (`'blue'`, quoted as gsettings prints them).
#[cfg(any(not(any(windows, target_os = "macos")), test))]
fn gnome_accent(name: &str) -> Option<[u8; 3]> {
    Some(match name.trim().trim_matches('\'') {
        "blue" => [0x35, 0x84, 0xe4],
        "teal" => [0x21, 0x90, 0xa4],
        "green" => [0x3a, 0x94, 0x4a],
        "yellow" => [0xc8, 0x88, 0x00],
        "orange" => [0xed, 0x5b, 0x00],
        "red" => [0xe6, 0x2d, 0x42],
        "pink" => [0xd5, 0x61, 0x99],
        "purple" => [0x91, 0x41, 0xac],
        "slate" => [0x6f, 0x83, 0x96],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classic is the look before Phase 14: egui's own defaults.
    #[test]
    fn classic_tokens_are_egui_defaults() {
        let mut style = egui::Style::default();
        let before = style.clone();
        CLASSIC.apply_spacing(&mut style);
        assert_eq!(style.spacing.item_spacing, before.spacing.item_spacing);
        assert_eq!(style.spacing.button_padding, before.spacing.button_padding);
        assert_eq!(style.spacing.interact_size, before.spacing.interact_size);
        assert_eq!(style.spacing.window_margin, before.spacing.window_margin);
        assert_eq!(style.spacing.menu_margin, before.spacing.menu_margin);
        assert_eq!(style.text_styles, before.text_styles);
        for dark in [true, false] {
            let fresh = if dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            };
            let mut v = fresh.clone();
            CLASSIC.apply_radii(&mut v);
            assert_eq!(v, fresh, "dark {dark}");
        }
    }

    #[test]
    fn finder_tokens_follow_the_plan() {
        assert_eq!(space::SCALE, [2.0, 4.0, 6.0, 8.0, 12.0, 16.0, 20.0, 24.0]);
        let t = &FINDER;
        assert_eq!(t.radius.selection, 5.0);
        assert_eq!(t.radius.control, 6);
        assert_eq!(t.radius.window as f32, finder::WINDOW_RADIUS);
        assert_eq!(t.rows.list, 22.0);
        assert_eq!(t.interact_height + t.item_spacing.y, finder::SIDEBAR_ROW);
        let s = &t.text;
        assert_eq!(
            (s.small, s.monospace, s.body, s.heading),
            (11.0, 12.0, 13.0, 15.0)
        );
        let mut style = egui::Style::default();
        t.apply_spacing(&mut style);
        assert_eq!(style.text_styles[&TextStyle::Body].size, 13.0);
        assert_eq!(style.spacing.interact_size.y, 20.0);
    }

    fn stock(name: &str) -> Theme {
        let (_, text) = crate::theme::BUILTIN
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap();
        toml::from_str(text).unwrap()
    }

    /// Accents a user may have: every macOS and GNOME one, a pale and a dark custom.
    fn accents() -> Vec<[u8; 3]> {
        let mut all: Vec<[u8; 3]> = (-1..=6).filter_map(mac_accent).collect();
        for n in [
            "blue", "teal", "green", "yellow", "orange", "red", "pink", "purple", "slate",
        ] {
            all.push(gnome_accent(n).unwrap());
        }
        all.extend([
            [0xff, 0xff, 0xff],
            [0, 0, 0],
            [0xff, 0xf0, 0x80],
            [0x10, 0x20, 0x40],
        ]);
        all
    }

    /// The four token sets (Finder and Classic, light and dark) reach WCAG AA: text 4.5:1,
    /// secondary text 3:1 (Keel's own also 4.5:1), the focus ring 3:1; Finder with any
    /// accent.
    #[test]
    fn token_sets_meet_wcag_contrast() {
        let mut sets = Vec::new();
        for name in ["dark", "light"] {
            let theme = stock(name);
            for accent in [Accent::Theme, Accent::Custom([0xff, 0xcc, 0x00])] {
                let look = Look {
                    style: Style::Classic,
                    accent,
                    os: None,
                };
                sets.push((
                    format!("classic {name} {accent:?}"),
                    Palette::of(&theme, &look),
                ));
            }
            for a in accents() {
                let look = Look {
                    style: Style::Finder,
                    accent: Accent::Custom(a),
                    os: None,
                };
                sets.push((format!("finder {name} {a:?}"), Palette::of(&theme, &look)));
            }
        }
        let mut failures = Vec::new();
        for (set, p) in &sets {
            let mut check = |what: &str, fg: Color32, on: &str, bg: Color32, min: f64| {
                let r = ratio(fg, bg);
                if r < min {
                    failures.push(format!("{set}: {what} on {on} {r:.2} < {min}"));
                }
            };
            for (on, bg) in [
                ("window", p.window),
                ("sidebar", p.sidebar),
                ("content", p.content),
                ("alternating row", p.alternating_row),
            ] {
                check("label", p.label, on, bg, 4.5);
                check("secondary", p.secondary_label, on, bg, 4.5);
                check("tertiary", p.tertiary_label, on, bg, 3.0);
                check("accent", p.accent, on, bg, 4.5);
                check("focus ring", p.focus_ring, on, bg, 3.0);
            }
            check("label", p.label, "selection", p.selection, 4.5);
            check(
                "on selected",
                p.on_selected,
                "selected",
                p.selected_focused,
                4.5,
            );
            check(
                "label",
                p.label,
                "unfocused selection",
                p.selected_unfocused,
                4.5,
            );
            if set.starts_with("finder") {
                check("focus ring", p.focus_ring, "selection", p.selection, 3.0);
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// The Finder palette is for the stock themes; any other keeps its own colors, and
    /// Classic with the theme's accent draws exactly the theme.
    #[test]
    fn palette_follows_style_theme_and_accent() {
        let dark = stock("dark");
        let finder = Look {
            style: Style::Finder,
            accent: Accent::Auto,
            os: Some([0xff, 0x95, 0x00]),
        };
        let p = Palette::of(&dark, &finder);
        assert_eq!(p.content, Color32::from_rgb(0x1e, 0x1e, 0x1e));
        assert_ne!(p.accent, dark.accent(), "the system accent");
        let classic = Palette::of(&dark, &Look::CLASSIC);
        assert_eq!(classic.onto(&dark), dark, "Classic is the theme itself");
        let mut own = stock("dark");
        own.bg = [1, 2, 3];
        assert!(!is_stock(&own));
        let p = Palette::of(&own, &finder);
        assert_eq!(p.content, Color32::from_rgb(1, 2, 3));
        assert_eq!(p.accent, own.accent(), "Automatic keeps a theme's accent");
        let system = Look {
            accent: Accent::System,
            ..finder
        };
        assert_ne!(Palette::of(&own, &system).accent, own.accent());
        let none = Look { os: None, ..finder };
        assert_eq!(
            Palette::of(&stock("light"), &none).selected_focused,
            legible(rgb(BLUE), &[Color32::WHITE], 4.5),
            "blue without a system accent"
        );
    }

    #[test]
    fn os_accent_values_decode() {
        assert_eq!(abgr(0xff_d7_78_00), [0x00, 0x78, 0xd7]);
        assert_eq!(mac_accent(4), Some(BLUE));
        assert_eq!(mac_accent(9), None);
        assert_eq!(gnome_accent("'teal'\n"), Some([0x21, 0x90, 0xa4]));
        assert_eq!(gnome_accent("'unknown'"), None);
    }

    #[test]
    fn tag_colors_are_distinct_and_labelled() {
        let fills: Vec<_> = TagColor::ALL.iter().filter_map(|t| t.fill()).collect();
        assert_eq!(fills.len(), 7, "none has no fill");
        for (i, a) in fills.iter().enumerate() {
            assert!(!fills[i + 1..].contains(a));
            assert!(ratio(on_color(*a), *a) >= 4.5, "{a:?}");
        }
        let names: Vec<_> = TagColor::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(
            names,
            ["Red", "Orange", "Yellow", "Green", "Blue", "Purple", "Gray", "None"]
        );
    }

    #[test]
    fn look_lives_in_the_context() {
        let ctx = Context::default();
        assert_eq!(look(&ctx), Look::CLASSIC);
        assert_eq!(tokens(&ctx), &CLASSIC);
        let finder = Look {
            style: Style::Finder,
            ..Look::CLASSIC
        };
        set_look(&ctx, finder);
        assert_eq!(tokens(&ctx), &FINDER);
        stock("light").apply(&ctx);
        assert_eq!(look(&ctx), finder, "applying a theme keeps the look");
        let p = palette(&ctx).unwrap();
        assert_eq!(p.content, Color32::WHITE);
        assert_eq!(ctx.style().spacing.interact_size.y, FINDER.interact_height);
        assert_eq!(ctx.style().visuals.extreme_bg_color, Color32::WHITE);
    }
}
