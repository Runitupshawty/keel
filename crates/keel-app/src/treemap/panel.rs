//! The chart options panel: style preset, layout, cushion shading, light source (a point
//! to drag), grid, folder frames, extension highlight and the extension palette.

use super::layout::Algorithm;
use super::shade::{Options, Preset, PALETTE};
use egui::{Color32, Sense, Stroke, Ui};

/// Edits `o`; true when anything changed.
pub fn options_ui(ui: &mut Ui, o: &mut Options) -> bool {
    let before = o.clone();
    ui.horizontal(|ui| {
        ui.label("Style");
        let current = o.preset().map_or("Custom", Preset::name);
        egui::ComboBox::from_id_salt("keel-treemap-preset")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for p in Preset::ALL {
                    if ui
                        .selectable_label(o.preset() == Some(p), p.name())
                        .clicked()
                    {
                        *o = p.options();
                    }
                }
            });
        if ui
            .button("Reset")
            .on_hover_text("Back to the WinDirStat style")
            .clicked()
        {
            *o = Options::default();
        }
    });
    ui.horizontal(|ui| {
        ui.label("Layout");
        egui::ComboBox::from_id_salt("keel-treemap-layout")
            .selected_text(o.algorithm.name())
            .show_ui(ui, |ui| {
                for a in Algorithm::ALL {
                    ui.selectable_value(&mut o.algorithm, a, a.name());
                }
            });
    });
    ui.checkbox(&mut o.cushion, "Cushion shading")
        .on_hover_text("Off draws flat tiles");
    ui.add_enabled_ui(o.cushion, |ui| {
        ui.add(egui::Slider::new(&mut o.height, 0.0..=1.0).text("Cushion height"));
        ui.add(egui::Slider::new(&mut o.scale, 0.0..=1.0).text("Scale factor"))
            .on_hover_text("Each level's cushion height relative to its folder's");
        ui.add(egui::Slider::new(&mut o.ambient, 0.0..=1.0).text("Ambient light"));
        ui.horizontal(|ui| {
            light_pad(ui, &mut o.light);
            ui.vertical(|ui| {
                ui.label("Light source");
                ui.add(egui::Slider::new(&mut o.light[0], -2.0..=2.0).text("x"));
                ui.add(egui::Slider::new(&mut o.light[1], -2.0..=2.0).text("y"));
            });
        });
    });
    ui.add(egui::Slider::new(&mut o.brightness, 0.0..=1.0).text("Brightness"));
    ui.horizontal(|ui| {
        ui.checkbox(&mut o.grid, "Grid");
        ui.color_edit_button_srgb(&mut o.grid_color)
            .on_hover_text("Grid color");
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut o.frames, "Folder frames");
        ui.color_edit_button_srgb(&mut o.frame_color)
            .on_hover_text("Frame color");
    });
    ui.add_enabled(
        o.frames,
        egui::Slider::new(&mut o.frame_min, 4.0..=400.0)
            .logarithmic(true)
            .text("Frame folders larger than (points)"),
    );
    ui.checkbox(
        &mut o.highlight_ext,
        "Outline every file of the selected type",
    );
    ui.label("Colors by size rank of the file types");
    ui.horizontal_wrapped(|ui| {
        for c in &mut o.palette {
            ui.color_edit_button_srgb(c);
        }
        if ui
            .small_button("Default colors")
            .on_hover_text("WinDirStat's 13 colors")
            .clicked()
        {
            o.palette = PALETTE.to_vec();
        }
    });
    *o != before
}

/// A square to drag the light source in: the dot is where the light comes from.
fn light_pad(ui: &mut Ui, light: &mut [f32; 2]) {
    let (rect, r) = ui.allocate_exact_size(egui::vec2(72.0, 72.0), Sense::click_and_drag());
    let r = crate::a11y::Named::named(r, "Light source position");
    if let Some(p) = r.interact_pointer_pos() {
        let f = (p - rect.center()) / (rect.width() / 2.0);
        light[0] = (f.x * 2.0).clamp(-2.0, 2.0);
        light[1] = (f.y * 2.0).clamp(-2.0, 2.0);
    }
    let v = ui.visuals();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, v.extreme_bg_color);
    let c = rect.center();
    let line = Stroke::new(1.0_f32, v.weak_text_color());
    painter.hline(rect.x_range(), c.y, line);
    painter.vline(c.x, rect.y_range(), line);
    let dot = c + egui::vec2(light[0], light[1]) * (rect.width() / 4.0);
    painter.circle_filled(dot, 5.0, Color32::from_rgb(255, 210, 60));
    painter.circle_stroke(dot, 5.0, Stroke::new(1.0_f32, v.text_color()));
}
