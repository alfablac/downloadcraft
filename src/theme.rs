//! Design tokens (design-system/idmcraft/MASTER.md) mapped to egui.
//! Primitive -> semantic -> component, flat dark, teal primary, dense desktop.
use egui::{Color32, FontId, TextStyle};

// Primitive
pub const SLATE_950: Color32 = Color32::from_rgb(0x02, 0x06, 0x17);
pub const SLATE_900: Color32 = Color32::from_rgb(0x0F, 0x17, 0x2A); // --color-background
pub const SLATE_850: Color32 = Color32::from_rgb(0x11, 0x18, 0x27); // --color-card
pub const SLATE_800: Color32 = Color32::from_rgb(0x1E, 0x29, 0x3B); // --color-muted
pub const SLATE_700: Color32 = Color32::from_rgb(0x33, 0x41, 0x55); // --color-border
pub const SLATE_300: Color32 = Color32::from_rgb(0xCB, 0xD5, 0xE1); // muted fg
pub const SLATE_50: Color32 = Color32::from_rgb(0xF8, 0xFA, 0xFC); // foreground
pub const TEAL_600: Color32 = Color32::from_rgb(0x0D, 0x94, 0x88); // primary / ring
pub const TEAL_500: Color32 = Color32::from_rgb(0x14, 0xB8, 0xA6); // secondary
pub const ORANGE_600: Color32 = Color32::from_rgb(0xEA, 0x58, 0x0C); // accent CTA
pub const RED_600: Color32 = Color32::from_rgb(0xDC, 0x26, 0x26); // destructive
pub const AMBER_500: Color32 = Color32::from_rgb(0xF5, 0x9E, 0x0B);
pub const GREEN_500: Color32 = Color32::from_rgb(0x22, 0xC5, 0x5E);

// Semantic
pub const BG: Color32 = SLATE_900;
pub const SURFACE: Color32 = SLATE_850;
pub const SURFACE_RAISED: Color32 = SLATE_800;
pub const BORDER: Color32 = SLATE_700;
pub const TEXT: Color32 = SLATE_50;
pub const TEXT_MUTED: Color32 = SLATE_300;
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x94, 0xA3, 0xB8);
pub const PRIMARY: Color32 = TEAL_600;
pub const ACCENT: Color32 = ORANGE_600;

// Spacing (4px rhythm)
#[allow(dead_code)]
pub const SP_XS: f32 = 4.0;
pub const SP_SM: f32 = 8.0;
pub const SP_MD: f32 = 16.0;
pub const RADIUS: f32 = 8.0;

pub fn status_color(s: crate::model::TaskStatus) -> Color32 {
    use crate::model::TaskStatus::*;
    match s { Complete => GREEN_500, Downloading => TEAL_500, Error => RED_600, Protected => AMBER_500, Paused => AMBER_500, Queued => TEXT_FAINT }
}

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = SLATE_950;
    v.faint_bg_color = SURFACE;
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0f32, TEXT_MUTED);
    v.widgets.inactive.bg_fill = SURFACE_RAISED;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x2B, 0x3B, 0x55);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0f32, TEXT);
    v.widgets.active.bg_fill = PRIMARY;
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0f32, Color32::WHITE);
    v.selection.bg_fill = PRIMARY;
    v.selection.stroke = egui::Stroke::new(1.0f32, Color32::WHITE);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0f32, BORDER);
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0f32, BORDER);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0f32, TEAL_500);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0f32, TEAL_500);
    v.window_stroke = egui::Stroke::new(1.0f32, BORDER);
    let rounding = egui::Rounding::same(RADIUS);
    v.widgets.noninteractive.rounding = rounding;
    v.widgets.inactive.rounding = rounding;
    v.widgets.hovered.rounding = rounding;
    v.widgets.active.rounding = rounding;
    v.window_rounding = egui::Rounding::same(12.0);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(SP_SM, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.window_margin = egui::Margin::same(SP_MD);
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(20.0)),
        (TextStyle::Body, FontId::proportional(13.5)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Small, FontId::proportional(11.5)),
        (TextStyle::Monospace, FontId::monospace(12.0)),
    ].into();
    ctx.set_style(style);
}

/// Primary CTA button (Add URL / Start).
pub fn primary_btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(label).color(Color32::WHITE).strong()).fill(ACCENT).min_size(egui::vec2(0.0, 30.0)))
}
pub fn ghost_btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(label).color(TEXT)).fill(SURFACE_RAISED).min_size(egui::vec2(0.0, 30.0)))
}
pub fn chip(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::none().fill(SURFACE_RAISED).rounding(egui::Rounding::same(10.0)).inner_margin(egui::Margin::symmetric(8.0, 2.0))
        .show(ui, |ui| { ui.label(egui::RichText::new(label).color(color).small()); });
}
