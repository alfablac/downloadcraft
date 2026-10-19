//! Desktop theme: graphite neutrals, 4pt spacing grid, single 6px radius
//! token (4px chips only), 1px borders (width constant in every widget
//! state — no layout shift). Native-tool feel, not web. No gradients, no
//! shadows-as-hierarchy. Steel accent, sparingly.
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
pub const SELECTION_BG: Color32 = Color32::from_rgb(0x2A, 0x3C, 0x38);
pub const STEEL_DIM: Color32 = Color32::from_rgb(0x3A, 0x4A, 0x47);
pub const OK: Color32 = Color32::from_rgb(0x5F, 0x9E, 0x6E);
pub const WARN: Color32 = Color32::from_rgb(0xB8, 0x8A, 0x3D);
pub const ERR: Color32 = Color32::from_rgb(0xD9, 0x7A, 0x6E); // lightened: #B04A44 was 2.7-3.2:1 as small text

// Semantic
pub const BG: Color32 = G_900; // central list background
pub const SURFACE: Color32 = G_850; // panels (menubar, rail, statusbar)
pub const SURFACE_RAISED: Color32 = G_800; // toolbars, headers, inputs
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
#[allow(dead_code)]
pub const RED_600: Color32 = ERR;

pub const SP_XS: f32 = 4.0;
pub const SP_SM: f32 = 8.0;
#[allow(dead_code)]
pub const SP_MD: f32 = 12.0;
#[allow(dead_code)]
pub const SP_LG: f32 = 16.0;
// Radius: single 6px token for controls/panels/windows; 4px chips only.
pub const R_CTRL: f32 = 6.0;
pub const R_CHIP: f32 = 4.0;
pub const R_PANEL: f32 = R_CTRL;
/// Uniform clickable-control height (buttons, TextEdit, ComboBox, DragValue).
/// 24 = 6*4pt-grid.
pub const BTN_H: f32 = 24.0;
/// Sidebar nav row height (IDM-compact density).
pub const ROW_H_NAV: f32 = 20.0;
/// STEEL as text (links, status, accents): STEEL fill color is 3.0-3.5:1 as text (fail); this is 6.1-7.3:1.
pub const STEEL_TEXT: Color32 = Color32::from_rgb(0x7F, 0xB3, 0xAA);

pub fn status_color(s: crate::model::TaskStatus) -> Color32 {
    use crate::model::TaskStatus::*;
    match s {
        Complete => OK,
        Downloading => STEEL,
        Error => ERR,
        Protected => WARN,
        Paused => WARN,
        Queued => TEXT_FAINT,
    }
}

/// Table header text: 11px semibold. Named body size, so call sites drop `.small()`.
pub fn table_header_text(s: &str) -> egui::RichText {
    egui::RichText::new(s).color(TEXT_MUTED).size(11.0).strong()
}

pub fn apply(ctx: &egui::Context) {
    // Inter (variable, bundled) first for Proportional; Monospace fallback last.
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Inter".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/fonts/InterVariable.ttf")),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("Inter".to_owned());
    ctx.set_fonts(fonts);

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
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = r_ctrl;
        w.expansion = 0.0;
    }
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
    v.selection.bg_fill = SELECTION_BG;
    v.selection.stroke = egui::Stroke::new(1.0f32, STEEL_TEXT);
    v.hyperlink_color = STEEL_TEXT;
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    // Download-manager UI: labels are never text-selection targets (was I-beam cursor + stolen row clicks).
    style.interaction.selectable_labels = false;
    style.spacing.item_spacing = egui::vec2(SP_SM, SP_XS);
    style.spacing.button_padding = egui::vec2(SP_SM, SP_XS);
    style.spacing.interact_size = egui::vec2(32.0, BTN_H);
    style.spacing.window_margin = egui::Margin::same(SP_MD);
    style.spacing.menu_margin = egui::Margin::same(SP_SM);
    style.spacing.slider_width = 160.0;
    style.spacing.indent = SP_LG;
    // Floating thin scrollbars: no layout width, transparent track (all bg opacities 0).
    // foreground_color=false: handle = widget bg_fill (G ramp), not fg text color.
    style.spacing.scroll = egui::style::ScrollStyle {
        floating: true,
        bar_width: 8.0,
        handle_min_length: 24.0,
        bar_inner_margin: SP_XS,
        bar_outer_margin: 0.0,
        floating_width: SP_XS,
        floating_allocated_width: 0.0,
        foreground_color: false,
        dormant_background_opacity: 0.0,
        active_background_opacity: 0.0,
        interact_background_opacity: 0.0,
        // 0.35 not 0: invisible dormant handle hides h-scroll overflow (reads as clipped text).
        dormant_handle_opacity: 0.35,
        active_handle_opacity: 0.6,
        interact_handle_opacity: 1.0,
    };
    style.visuals.slider_trailing_fill = true;
    style.animation_time = 0.15;
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(13.0)),
        (TextStyle::Body, FontId::proportional(11.0)),
        (TextStyle::Button, FontId::proportional(11.0)),
        (TextStyle::Small, FontId::proportional(10.0)),
        (TextStyle::Monospace, FontId::monospace(11.0)),
    ]
    .into();
    ctx.set_style(style);
}

/// Primary action button: STEEL fill (white text 4.8:1 on it), global hover/pressed.
pub fn primary_btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(Color32::WHITE).strong())
            .fill(STEEL)
            .rounding(R_CTRL)
            .min_size(egui::vec2(0.0, BTN_H)),
    )
}
/// Secondary button: no .fill() — global inactive/hovered/active visuals paint it.
pub fn ghost_btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(TEXT))
            .rounding(R_CTRL)
            .min_size(egui::vec2(0.0, BTN_H)),
    )
}
/// Darken base toward BG for badge fill. Text-safe hues stay legible on tint:
/// fill ~20% base over BG, border ~55% base. No new palette entries.
fn mixc(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

pub fn badge_tint(base: Color32) -> (Color32, Color32) {
    (mixc(BG, base, 0.16), mixc(BG, base, 0.55))
}

/// Badge label color: base lightened toward white so 9px text clears
/// 4.5:1 on the tinted fill (raw hues measured 2.8-4.4:1).
pub fn badge_text(base: Color32) -> Color32 {
    mixc(base, Color32::WHITE, 0.38)
}

/// Small non-interactive badge: tinted fill + colored border/text, 4px radius.
/// Replaces button-like chips for tags (category, type, status, queue meta).
/// Fixed small font keeps badge <= ROW_H; proportional would overflow 22px rows.
const BADGE_FONT: f32 = 9.0;

pub fn badge(ui: &mut egui::Ui, label: &str, color: Color32) {
    let (fill, border) = badge_tint(color);
    egui::Frame::none()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0f32, border))
        .rounding(R_CHIP)
        .inner_margin(egui::Margin::symmetric(6.0, 1.0))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(label)
                        .color(badge_text(color))
                        .size(BADGE_FONT)
                        .strong(),
                )
                .truncate()
                .selectable(false),
            );
        });
}

/// Natural badge width for label at BADGE_FONT (for fixed table cells).
pub fn badge_width(ui: &egui::Ui, label: &str) -> f32 {
    ui.fonts(|f| {
        f.layout_no_wrap(label.into(), egui::FontId::proportional(BADGE_FONT), Color32::WHITE)
            .size()
            .x
    }) + 14.0 // 6+6 inner margin + 1+1 border
}

/// File category -> badge hue. Distinct hues per category; General neutral.
pub fn category_color(category: &str) -> Color32 {
    match category {
        "Video" => Color32::from_rgb(0xC4, 0x8A, 0xD8), // violet
        "Audio" => Color32::from_rgb(0xD8, 0x9A, 0x5F), // orange
        "Documents" => Color32::from_rgb(0x6E, 0xA8, 0xD8), // blue
        "Compressed" => Color32::from_rgb(0xC9, 0xB4, 0x58), // gold
        "Programs" => OK,
        "Unfinished" => WARN,
        "Finished" => OK,
        _ => TEXT_MUTED, // General / All Downloads / unknown
    }
}

/// Protocol type -> badge hue. Avoid ERR (means Error status).
pub fn proto_color(k: crate::stream::ProtoKind) -> Color32 {
    use crate::stream::ProtoKind::*;
    match k {
        Torrent => WARN,
        Dash | Hls => Color32::from_rgb(0xC4, 0x8A, 0xD8),
        YtDlp => Color32::from_rgb(0xD8, 0x7A, 0x6E),
        Sftp => OK,
        Ftp | Ftps => STEEL_TEXT,
        Nntp => Color32::from_rgb(0x6E, 0xA8, 0xD8),
        Metalink => Color32::from_rgb(0xC9, 0xB4, 0x58),
        Http => TEXT_MUTED,
        File => TEXT_FAINT,
    }
}
/// Stored [r,g,b] override lookup.
pub fn color_override(overrides: &[(String, [u8; 3])], key: &str) -> Option<Color32> {
    overrides
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, [r, g, b])| Color32::from_rgb(*r, *g, *b))
}

/// Category badge hue: user override falling back to category_color.
pub fn category_color_or(category: &str, overrides: &[(String, [u8; 3])]) -> Color32 {
    color_override(overrides, category).unwrap_or_else(|| category_color(category))
}

/// Rail TYPE bucket default hue, keyed by TypeSel key string.
pub fn type_key_color(key: &str) -> Color32 {
    match key {
        "Torrent" => WARN,
        "Nntp" => Color32::from_rgb(0x6E, 0xA8, 0xD8),
        "Ftp" => STEEL_TEXT,
        "Stream" => Color32::from_rgb(0xC4, 0x8A, 0xD8),
        "YtDlp" => Color32::from_rgb(0xD8, 0x7A, 0x6E),
        "Metalink" => Color32::from_rgb(0xC9, 0xB4, 0x58),
        "Http" => TEXT_MUTED,
        _ => TEXT_FAINT, // File/unknown
    }
}

/// TypeSel-bucket badge hue: user override falling back to type_key_color.
pub fn type_color_or(key: &str, overrides: &[(String, [u8; 3])]) -> Color32 {
    color_override(overrides, key).unwrap_or_else(|| type_key_color(key))
}

/// Panel frame (desktop group box), 8px radius.
#[allow(dead_code)]
pub fn panel() -> egui::Frame {
    egui::Frame::none()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0f32, BORDER))
        .rounding(R_PANEL)
        .inner_margin(egui::Margin::same(SP_MD))
}
/// Sunken list/table frame.
#[allow(dead_code)]
pub fn list_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(BG)
        .stroke(egui::Stroke::new(1.0f32, BORDER))
        .rounding(R_PANEL)
        .inner_margin(egui::Margin::same(0.0))
}
