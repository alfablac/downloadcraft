//! Desktop theme: graphite neutrals, 6px control / 8px panel radius, 1px borders
//! (width constant in every widget state — no layout shift). Native-tool feel,
//! not web. No gradients, no shadows-as-hierarchy. Steel accent, sparingly.
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
pub const ERR: Color32 = Color32::from_rgb(0xD9, 0x7A, 0x6E); // lightened: #B04A44 was 2.7-3.2:1 as small text

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

#[allow(dead_code)]
pub const SP_XS: f32 = 4.0;
#[allow(dead_code)]
pub const SP_SM: f32 = 8.0;
#[allow(dead_code)]
pub const SP_MD: f32 = 16.0;
// Radius: 6px controls (buttons/inputs/rows), 4px chips/strips, 8px panels/windows.
pub const R_CTRL: f32 = 6.0;
pub const R_CHIP: f32 = 4.0;
pub const R_PANEL: f32 = 8.0;
/// Uniform clickable-control height (buttons, TextEdit, ComboBox, DragValue).
pub const BTN_H: f32 = 26.0;
/// Sidebar nav row height (IDM-compact density).
pub const ROW_H_NAV: f32 = 22.0;
/// STEEL as text (links, status, accents): STEEL fill color is 3.0-3.5:1 as text (fail); this is 6.1-7.3:1.
pub const STEEL_TEXT: Color32 = Color32::from_rgb(0x7F, 0xB3, 0xAA);

pub fn status_color(s: crate::model::TaskStatus) -> Color32 {
    use crate::model::TaskStatus::*;
    match s { Complete => OK, Downloading => STEEL, Error => ERR, Protected => WARN, Paused => WARN, Queued => TEXT_FAINT }
}

pub fn apply(ctx: &egui::Context) {
    let r_ctrl = egui::Rounding::same(R_CTRL);
    let r_panel = egui::Rounding::same(R_PANEL);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = G_950;
    v.faint_bg_color = G_850;
    v.code_bg_color = G_950;
    v.warn_fg_color = WARN;
    v.error_fg_color = ERR;
    v.window_rounding = r_panel;
    v.menu_rounding = r_ctrl;
    v.window_stroke = egui::Stroke::new(1.0f32, BORDER);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] { w.rounding = r_ctrl; w.expansion = 0.0; }
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.weak_bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0f32, BORDER);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.widgets.inactive.bg_fill = SURFACE_RAISED;
    v.widgets.inactive.weak_bg_fill = SURFACE_RAISED;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0f32, BORDER);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.widgets.hovered.bg_fill = SURFACE_HOVER;
    v.widgets.hovered.weak_bg_fill = SURFACE_HOVER;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0f32, BORDER_STRONG);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.widgets.active.bg_fill = STEEL_DIM;
    v.widgets.active.weak_bg_fill = STEEL_DIM;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0f32, STEEL);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.widgets.open = v.widgets.hovered;
    v.selection.bg_fill = STEEL_DIM;
    v.selection.stroke = egui::Stroke::new(1.0f32, STEEL_TEXT);
    v.hyperlink_color = STEEL_TEXT;
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    // Download-manager UI: labels are never text-selection targets (was I-beam cursor + stolen row clicks).
    style.interaction.selectable_labels = false;
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.interact_size = egui::vec2(36.0, BTN_H);
    style.spacing.window_margin = egui::Margin::same(12.0);
    style.spacing.menu_margin = egui::Margin::same(6.0);
    style.spacing.slider_width = 160.0;
    style.spacing.scroll.floating = true;
    style.spacing.scroll.bar_width = 10.0;
    style.spacing.scroll.handle_min_length = 24.0;
    style.visuals.slider_trailing_fill = true;
    style.animation_time = 0.15;
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(15.0)),
        (TextStyle::Body, FontId::proportional(12.5)),
        (TextStyle::Button, FontId::proportional(12.0)),
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Monospace, FontId::monospace(12.0)),
    ].into();
    ctx.set_style(style);
}

/// Primary action button: STEEL fill (white text 9.3:1 on it), global hover/pressed.
pub fn primary_btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(label).color(Color32::WHITE).strong()).fill(STEEL).rounding(R_CTRL).min_size(egui::vec2(0.0, BTN_H)))
}
/// Secondary button: no .fill() — global inactive/hovered/active visuals paint it.
pub fn ghost_btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(label).color(TEXT)).rounding(R_CTRL).min_size(egui::vec2(0.0, BTN_H)))
}
/// Small status label — bordered box, 4px radius.
pub fn chip(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::none().fill(SURFACE_RAISED).stroke(egui::Stroke::new(1.0f32, BORDER)).rounding(R_CHIP).inner_margin(egui::Margin::symmetric(6.0, 2.0))
        .show(ui, |ui| { ui.label(egui::RichText::new(label).color(color).small()); });
}
/// Panel frame (desktop group box), 8px radius.
#[allow(dead_code)]
pub fn panel() -> egui::Frame {
    egui::Frame::none().fill(SURFACE).stroke(egui::Stroke::new(1.0f32, BORDER)).rounding(R_PANEL).inner_margin(egui::Margin::same(12.0))
}
/// Sunken list/table frame.
#[allow(dead_code)]
pub fn list_frame() -> egui::Frame {
    egui::Frame::none().fill(BG).stroke(egui::Stroke::new(1.0f32, BORDER_STRONG)).rounding(R_PANEL).inner_margin(egui::Margin::same(0.0))
}
