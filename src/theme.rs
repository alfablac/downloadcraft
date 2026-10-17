//! Desktop theme: graphite neutrals, square corners, hard 1px borders.
//! Native-tool feel (Win32/Qt-like), not web. No rounded cards, no gradients,
//! no blue-tinted surfaces. Single muted steel accent for selection/progress.
use egui::{Color32, FontId, TextStyle};

// Primitive — graphite ramp (neutral gray, no blue cast)
pub const G_950: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
pub const G_900: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x1C);
pub const G_850: Color32 = Color32::from_rgb(0x22, 0x22, 0x22);
pub const G_800: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2A);
pub const G_750: Color32 = Color32::from_rgb(0x33, 0x33, 0x33);
pub const G_700: Color32 = Color32::from_rgb(0x3D, 0x3D, 0x3D);
pub const G_600: Color32 = Color32::from_rgb(0x55, 0x55, 0x55);
pub const G_400: Color32 = Color32::from_rgb(0x9A, 0x9A, 0x9A);
pub const G_300: Color32 = Color32::from_rgb(0xBB, 0xBB, 0xBB);
pub const G_100: Color32 = Color32::from_rgb(0xE8, 0xE8, 0xE8);

// Accent: desaturated steel teal, used sparingly (selection, progress, active nav)
pub const STEEL: Color32 = Color32::from_rgb(0x4E, 0x7A, 0x74);
pub const STEEL_DIM: Color32 = Color32::from_rgb(0x3A, 0x4A, 0x47);
pub const OK: Color32 = Color32::from_rgb(0x5F, 0x9E, 0x6E);
pub const WARN: Color32 = Color32::from_rgb(0xB8, 0x8A, 0x3D);
pub const ERR: Color32 = Color32::from_rgb(0xB0, 0x4A, 0x44);

// Semantic
pub const BG: Color32 = G_900;              // central list background
pub const SURFACE: Color32 = G_850;         // panels (menubar, rail, statusbar)
pub const SURFACE_RAISED: Color32 = G_800;  // toolbars, headers, inputs
pub const SURFACE_HOVER: Color32 = G_750;
pub const BORDER: Color32 = G_700;
pub const BORDER_STRONG: Color32 = G_600;
pub const TEXT: Color32 = G_100;
pub const TEXT_MUTED: Color32 = G_300;
pub const TEXT_FAINT: Color32 = G_400;
pub const PRIMARY: Color32 = STEEL;
#[allow(dead_code)]
pub const ACCENT: Color32 = STEEL;
pub const TEAL_500: Color32 = STEEL; // legacy alias (app.rs sparkline/status)
pub const RED_600: Color32 = ERR;

pub const SP_SM: f32 = 6.0;
#[allow(dead_code)]
pub const SP_MD: f32 = 10.0;

pub fn status_color(s: crate::model::TaskStatus) -> Color32 {
    use crate::model::TaskStatus::*;
    match s { Complete => OK, Downloading => STEEL, Error => ERR, Protected => WARN, Paused => WARN, Queued => TEXT_FAINT }
}

pub fn apply(ctx: &egui::Context) {
    let square = egui::Rounding::ZERO;
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = G_850;
    v.extreme_bg_color = G_950;
    v.faint_bg_color = G_850;
    v.window_rounding = square;
    v.window_stroke = egui::Stroke::new(1.0f32, BORDER_STRONG);
    v.widgets.noninteractive.rounding = square;
    v.widgets.inactive.rounding = square;
    v.widgets.hovered.rounding = square;
    v.widgets.active.rounding = square;
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0f32, BORDER);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0f32, TEXT_MUTED);
    v.widgets.inactive.bg_fill = SURFACE_RAISED;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0f32, BORDER_STRONG);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.widgets.hovered.bg_fill = SURFACE_HOVER;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0f32, BORDER_STRONG);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.widgets.active.bg_fill = STEEL_DIM;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0f32, BORDER_STRONG);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.selection.bg_fill = STEEL_DIM;
    v.selection.stroke = egui::Stroke::new(1.0f32, TEXT);
    v.hyperlink_color = STEEL;
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(SP_SM, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    style.spacing.window_margin = egui::Margin::same(12.0);
    style.spacing.menu_margin = egui::Margin::same(6.0);
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(15.0)),
        (TextStyle::Body, FontId::proportional(12.5)),
        (TextStyle::Button, FontId::proportional(12.5)),
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Monospace, FontId::monospace(11.5)),
    ].into();
    ctx.set_style(style);
}

/// Flat bordered toolbar button — primary action.
pub fn primary_btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(label).color(TEXT)).fill(STEEL_DIM).min_size(egui::vec2(0.0, 24.0)))
}
/// Flat bordered toolbar button.
pub fn ghost_btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(label).color(TEXT)).fill(SURFACE_RAISED).min_size(egui::vec2(0.0, 24.0)))
}
/// Square status label — bordered box, no pill.
pub fn chip(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::none().fill(SURFACE_RAISED).stroke(egui::Stroke::new(1.0f32, BORDER)).inner_margin(egui::Margin::symmetric(6.0, 1.0))
        .show(ui, |ui| { ui.label(egui::RichText::new(label).color(color).small()); });
}
/// Bordered panel frame with hard separation (desktop group box).
#[allow(dead_code)]
pub fn panel() -> egui::Frame {
    egui::Frame::none().fill(SURFACE).stroke(egui::Stroke::new(1.0f32, BORDER)).inner_margin(egui::Margin::same(8.0))
}
/// Sunken list/table frame.
#[allow(dead_code)]
pub fn list_frame() -> egui::Frame {
    egui::Frame::none().fill(BG).stroke(egui::Stroke::new(1.0f32, BORDER_STRONG)).inner_margin(egui::Margin::same(0.0))
}
