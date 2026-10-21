//! Shared UI components (bin crate): buttons, badges, modal dialogs and
//! dialog layout. Colors and metrics come from `theme` tokens / the global
//! `Style` set in `theme::apply`; call sites never pass colors or sizes.
//!
//! egui 0.29 has no `egui::Modal` (added in 0.31), so modals are a
//! `Window` above a full-screen backdrop `Area` that dims and swallows input.
use crate::icons::{icon, Icon};
use crate::theme;
use egui::{Align2, Color32, Id, Key, Rect, Response, Sense, Stroke, Ui, Vec2};

// ── Dialog size tiers (max width) ──────────────────────────────────────────

/// Dialog width tiers, reused by every modal and native dialog viewport.
pub mod size {
    pub const SM: f32 = 400.0;
    pub const MD: f32 = 560.0;
    pub const LG: f32 = 760.0;
    pub const XL: f32 = 960.0;
    /// Smallest width a resizable dialog may shrink to.
    pub const MIN_W: f32 = 400.0;
    /// Smallest height a resizable dialog may shrink to.
    pub const MIN_H: f32 = 240.0;
}
// Tiers ascend; resizable minimum never exceeds the smallest tier.
const _: () = assert!(size::SM < size::MD && size::MD < size::LG && size::LG < size::XL);
const _: () = assert!(size::MIN_W <= size::SM);

/// Gap kept between any dialog and the viewport edge.
pub const VIEWPORT_MARGIN: f32 = theme::SP_2XL;
/// Pinned dialog footer height: gap + separator + MD button row.
pub const FOOTER_H: f32 = theme::SP_SM + theme::SP_SM + theme::BTN_MD + theme::SP_SM;

// ── Buttons ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonKind {
    /// Filled accent. At most one per view / dialog.
    Primary,
    /// Bordered neutral (global widget visuals).
    Secondary,
    /// Borderless until hovered: tertiary / low-emphasis actions.
    Ghost,
    /// Filled red: irreversible actions (only inside confirmations).
    Destructive,
}

/// Button builder. Height = current `interact_size.y` (BTN_SM globally,
/// BTN_MD inside `footer_row`, BTN_LG via `with_button_height`).
pub struct Btn<'a> {
    label: &'a str,
    kind: ButtonKind,
    icon: Option<Icon>,
    min_w: f32,
    enabled: bool,
}

impl<'a> Btn<'a> {
    pub fn new(kind: ButtonKind, label: &'a str) -> Self {
        Self {
            label,
            kind,
            icon: None,
            min_w: if label.is_empty() {
                0.0
            } else {
                theme::BTN_MIN_W
            },
            enabled: true,
        }
    }
    pub fn primary(label: &'a str) -> Self {
        Self::new(ButtonKind::Primary, label)
    }
    pub fn secondary(label: &'a str) -> Self {
        Self::new(ButtonKind::Secondary, label)
    }
    pub fn icon(mut self, ic: Icon) -> Self {
        self.icon = Some(ic);
        self
    }
    /// Stable width for buttons whose label toggles (Pause/Resume).
    pub fn min_width(mut self, w: f32) -> Self {
        self.min_w = w;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let enabled = self.enabled && ui.is_enabled();
        let h = ui.spacing().interact_size.y;
        let font = egui::TextStyle::Button.resolve(ui.style());
        let galley = ui
            .painter()
            .layout_no_wrap(self.label.to_owned(), font, Color32::PLACEHOLDER);
        let pad = ui.spacing().button_padding.x + theme::SP_XS;
        // 16pt icon at control heights; shrinks for dense row buttons.
        let icon_w = if self.icon.is_some() {
            (h - 8.0).clamp(8.0, 16.0)
        } else {
            0.0
        };
        let gap = if self.icon.is_some() && !self.label.is_empty() {
            6.0
        } else {
            0.0
        };
        let content_w = icon_w + gap + galley.size().x;
        let w = if self.label.is_empty() {
            h.max(self.min_w)
        } else {
            self.min_w.max(pad + content_w + pad)
        };
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), sense);
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, self.label)
        });
        if !ui.is_rect_visible(rect) {
            return resp;
        }
        let (fill, stroke, fg) = button_colors(ui, self.kind, &resp, enabled);
        let rounding = ui.visuals().widgets.inactive.rounding;
        ui.painter()
            .rect(rect, rounding, fill, Stroke::new(1.0_f32, stroke));
        if enabled && resp.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(2.0),
                rounding.at_least(2.0),
                Stroke::new(2.0_f32, ui.visuals().selection.stroke.color),
            );
        }
        if enabled && resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let x0 = rect.center().x - content_w / 2.0;
        if let Some(ic) = self.icon {
            let ir = Rect::from_min_size(
                egui::pos2(x0, rect.center().y - icon_w / 2.0),
                egui::vec2(icon_w, icon_w),
            );
            icon(ui.painter(), &ic, ir, fg);
        }
        if !self.label.is_empty() {
            let pos = egui::pos2(x0 + icon_w + gap, rect.center().y - galley.size().y / 2.0);
            ui.painter().galley(pos, galley, fg);
        }
        resp
    }
}

/// Fill / border / text per kind + interaction state. Secondary and Ghost
/// read the global widget `Visuals`; filled kinds use theme tokens.
fn button_colors(
    ui: &Ui,
    kind: ButtonKind,
    resp: &Response,
    enabled: bool,
) -> (Color32, Color32, Color32) {
    let w = &ui.visuals().widgets;
    if !enabled {
        // Flat (panel bg), quiet border, dim label: unmistakably inert.
        let nw = &w.noninteractive;
        return match kind {
            ButtonKind::Ghost => (
                Color32::TRANSPARENT,
                Color32::TRANSPARENT,
                theme::TEXT_DISABLED,
            ),
            _ => (theme::BG, nw.bg_stroke.color, theme::TEXT_DISABLED),
        };
    }
    let pressed = resp.is_pointer_button_down_on();
    let hovered = resp.hovered();
    let state = if pressed {
        &w.active
    } else if hovered {
        &w.hovered
    } else {
        &w.inactive
    };
    match kind {
        ButtonKind::Primary => {
            let (fill, stroke) = if pressed {
                (theme::STEEL_DIM, theme::STEEL)
            } else if hovered {
                (theme::STEEL_DIM, theme::STEEL_TEXT)
            } else {
                (theme::STEEL, theme::STEEL)
            };
            (fill, stroke, Color32::WHITE)
        }
        ButtonKind::Destructive => {
            let fill = if pressed {
                theme::DANGER_PRESSED
            } else if hovered {
                theme::DANGER_HOVER
            } else {
                theme::DANGER
            };
            (fill, fill, Color32::WHITE)
        }
        ButtonKind::Secondary => (
            state.weak_bg_fill,
            state.bg_stroke.color,
            state.fg_stroke.color,
        ),
        ButtonKind::Ghost => {
            if pressed || hovered {
                (
                    state.weak_bg_fill,
                    state.bg_stroke.color,
                    state.fg_stroke.color,
                )
            } else {
                (
                    Color32::TRANSPARENT,
                    Color32::TRANSPARENT,
                    theme::TEXT_MUTED,
                )
            }
        }
    }
}

pub fn primary_button(ui: &mut Ui, label: &str) -> Response {
    Btn::primary(label).show(ui)
}
pub fn secondary_button(ui: &mut Ui, label: &str) -> Response {
    Btn::secondary(label).show(ui)
}
pub fn ghost_button(ui: &mut Ui, label: &str) -> Response {
    Btn::new(ButtonKind::Ghost, label).show(ui)
}
pub fn destructive_button(ui: &mut Ui, label: &str) -> Response {
    Btn::new(ButtonKind::Destructive, label).show(ui)
}

/// Icon-only ghost button at control height (toolbars, pane actions).
pub fn icon_button(ui: &mut Ui, ic: Icon, tip: &str) -> Response {
    Btn::new(ButtonKind::Ghost, "")
        .icon(ic)
        .show(ui)
        .on_hover_text(tip)
}

/// Compact icon-only button for dense table rows (fits ROW_BTN_H rows).
pub fn row_icon_button(ui: &mut Ui, ic: Icon, tip: &str) -> Response {
    with_button_height(ui, theme::ROW_BTN_H, |ui| {
        Btn::secondary("").icon(ic).show(ui)
    })
    .on_hover_text(tip)
}

/// Run `add` with a different button/control height (e.g. BTN_LG CTA).
pub fn with_button_height<R>(ui: &mut Ui, h: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = h;
        add(ui)
    })
    .inner
}

// ── Badges ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BadgeKind {
    Neutral,
    Success,
    Warning,
    Error,
    Info,
    /// Data-driven hue (category / protocol / user color overrides).
    Hue(Color32),
}

impl BadgeKind {
    /// Task status -> semantic badge (Downloading uses STEEL_TEXT for contrast).
    pub fn status(s: crate::model::TaskStatus) -> Self {
        use crate::model::TaskStatus::*;
        match s {
            Complete => BadgeKind::Success,
            Downloading => BadgeKind::Info,
            Error => BadgeKind::Error,
            Paused | Protected => BadgeKind::Warning,
            Queued => BadgeKind::Neutral,
        }
    }
    fn color(self) -> Color32 {
        match self {
            BadgeKind::Neutral => theme::TEXT_MUTED,
            BadgeKind::Success => theme::OK,
            BadgeKind::Warning => theme::WARN,
            BadgeKind::Error => theme::ERR,
            BadgeKind::Info => theme::STEEL_TEXT,
            BadgeKind::Hue(c) => c,
        }
    }
}

const BADGE_PAD_X: f32 = 6.0;

/// Small non-interactive badge: tinted fill + colored border/text, chip
/// radius, Small text style, one line. Painted at its exact size (a Frame
/// would stretch to the row's interact height); truncates with a tooltip
/// when the row is narrower than the text.
pub fn badge(ui: &mut Ui, text: &str, kind: BadgeKind) -> Response {
    let color = kind.color();
    let (fill, border) = theme::badge_tint(color);
    let fg = theme::badge_text(color);
    let font = egui::TextStyle::Small.resolve(ui.style());
    let max_text_w = (ui.available_width() - 2.0 * BADGE_PAD_X - 2.0).max(0.0);
    let mut job =
        egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, fg));
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_text_w);
    let galley = ui.fonts(|f| f.layout_job(job));
    let elided = galley.elided;
    let size = galley.size() + egui::vec2(2.0 * BADGE_PAD_X + 2.0, 2.0 + 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect(rect, theme::R_CHIP, fill, Stroke::new(1.0_f32, border));
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, fg);
    }
    if elided {
        resp.on_hover_text(text)
    } else {
        resp
    }
}

/// Natural badge width for `text` (fixed table cells).
pub fn badge_width(ui: &Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Small.resolve(ui.style());
    ui.fonts(|f| f.layout_no_wrap(text.into(), font, Color32::WHITE).size().x)
        + 2.0 * BADGE_PAD_X
        + 2.0
}

// ── Modal ──────────────────────────────────────────────────────────────────

/// Per-pass list of shown modals; previous pass decides the topmost (ESC
/// target) and whether global shortcuts are blocked.
#[derive(Clone, Default)]
struct ModalStack {
    pass: u64,
    cur: Vec<Id>,
    prev: Vec<Id>,
    /// Open modals whose default button has not taken focus yet.
    focus_pending: Vec<Id>,
}

fn stack_id() -> Id {
    Id::new("ui::modal_stack")
}

/// Register `id` as shown this pass. Returns true when it was the topmost
/// modal last pass (or the only one opening now).
fn register_modal(ctx: &egui::Context, id: Id) -> bool {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| {
        let s = d.get_temp_mut_or_default::<ModalStack>(stack_id());
        if s.pass != pass {
            s.prev = std::mem::take(&mut s.cur);
            s.pass = pass;
        }
        s.cur.push(id);
        let top = match s.prev.last() {
            Some(top) => *top == id,
            None => s.cur.len() == 1,
        };
        if !s.prev.contains(&id) && !s.focus_pending.contains(&id) {
            s.focus_pending.push(id);
        }
        let live: Vec<Id> = s.prev.iter().chain(s.cur.iter()).copied().collect();
        s.focus_pending.retain(|o| live.contains(o));
        top
    })
}

fn current_modal_key() -> Id {
    Id::new("ui::current_modal")
}

/// Give `resp` keyboard focus when its modal opens. Use on the safe default
/// (Cancel in destructive confirmations, the primary otherwise): Enter /
/// Space then activate exactly that button. Re-requested until the button
/// actually holds focus once (the click that opened the modal, or egui's
/// first-show sizing pass, would otherwise drop it), then left to the user.
pub fn focus_on_open(ui: &Ui, resp: &Response) {
    let ctx = ui.ctx();
    let Some(modal) = ctx.data(|d| d.get_temp::<Id>(current_modal_key())) else {
        return;
    };
    let pending = ctx.data(|d| {
        d.get_temp::<ModalStack>(stack_id())
            .is_some_and(|s| s.focus_pending.contains(&modal))
    });
    if !pending {
        return;
    }
    if resp.has_focus() {
        ctx.data_mut(|d| {
            d.get_temp_mut_or_default::<ModalStack>(stack_id())
                .focus_pending
                .retain(|m| *m != modal)
        });
    } else {
        resp.request_focus();
    }
}

/// True while any modal is open: callers gate global shortcuts on it so keys
/// never reach the view behind a dialog.
pub fn any_modal_open(ctx: &egui::Context) -> bool {
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|d| {
        d.get_temp::<ModalStack>(stack_id()).is_some_and(|s| {
            if s.pass == pass {
                !s.cur.is_empty() || !s.prev.is_empty()
            } else if s.pass + 1 == pass {
                !s.cur.is_empty()
            } else {
                false
            }
        })
    })
}

/// Full-screen dim layer under the modal: paints SCRIM and swallows clicks,
/// drags, hover and scroll aimed at the view behind.
fn backdrop(ctx: &egui::Context, id: Id) {
    let screen = ctx.screen_rect();
    let resp = egui::Area::new(id.with("backdrop"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .constrain(false)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, _) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
            ui.painter().rect_filled(rect, 0.0, theme::SCRIM);
        });
    ctx.move_to_top(resp.response.layer_id);
}

/// Dialog frame: window fill + border, SP_LG margins, panel radius.
pub fn modal_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::window(&ctx.style())
        .inner_margin(egui::Margin::same(theme::SP_LG))
        .shadow(egui::epaint::Shadow::NONE)
}

/// Native dialog viewport frame (OS window owns the title bar).
pub fn dialog_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(theme::SURFACE)
        .inner_margin(egui::Margin::same(theme::SP_LG))
}

pub struct ModalResponse<R> {
    pub inner: Option<R>,
    /// Close button, ESC (topmost modal, no popup open) or OS close.
    pub close: bool,
    /// Enter with nothing focused: dialogs opt in only for safe defaults.
    pub enter: bool,
}

/// Centered, non-collapsible modal: header (title + close), body, backdrop.
pub struct Modal {
    id: Id,
    title: String,
    width: f32,
    height: Option<f32>,
    resizable: bool,
}

impl Modal {
    pub fn new(id_source: impl std::hash::Hash, title: impl Into<String>) -> Self {
        Self {
            id: Id::new(("modal", id_source)),
            title: title.into(),
            width: size::MD,
            height: None,
            resizable: false,
        }
    }
    /// Width tier (`size::SM/MD/LG/XL`). Clamped to viewport minus margin.
    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }
    /// Default height for resizable dialogs.
    pub fn height(mut self, h: f32) -> Self {
        self.height = Some(h);
        self
    }
    pub fn resizable(mut self, r: bool) -> Self {
        self.resizable = r;
        self
    }

    pub fn show<R>(self, ctx: &egui::Context, add: impl FnOnce(&mut Ui) -> R) -> ModalResponse<R> {
        let is_top = register_modal(ctx, self.id);
        backdrop(ctx, self.id);
        let bounds = ctx.screen_rect().shrink(VIEWPORT_MARGIN);
        let w = self.width.min(bounds.width()).max(1.0);
        let max_h = bounds.height().max(1.0);
        let mut open = true;
        let mut win = egui::Window::new(
            egui::RichText::new(&self.title).text_style(egui::TextStyle::Heading),
        )
        .id(self.id)
        .open(&mut open)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .constrain_to(bounds)
        .frame(modal_frame(ctx));
        win = if self.resizable {
            let h = self.height.unwrap_or(max_h).min(max_h);
            win.resizable(true)
                .default_size([w, h])
                .min_size([size::MIN_W.min(w), size::MIN_H.min(h)])
                .max_size(bounds.size())
        } else {
            win.resizable(false)
                .min_width(w)
                .max_width(w)
                .max_height(max_h)
        };
        ctx.data_mut(|d| d.insert_temp(current_modal_key(), self.id));
        let shown = win.show(ctx, add);
        ctx.data_mut(|d| d.remove::<Id>(current_modal_key()));
        if let Some(r) = &shown {
            ctx.move_to_top(r.response.layer_id);
        }
        let popup = ctx.memory(|m| m.any_popup_open());
        let esc = is_top
            && !popup
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
        let enter = is_top
            && !popup
            && ctx.memory(|m| m.focused().is_none())
            && ctx.input(|i| i.key_pressed(Key::Enter));
        ModalResponse {
            inner: shown.and_then(|r| r.inner),
            close: !open || esc,
            enter,
        }
    }

    /// Native dialog viewport content: embedded fallback renders as a modal;
    /// a real OS window gets a CentralPanel (title bar + X are the OS's,
    /// callers handle `close_requested`).
    pub fn show_in_viewport<R>(
        self,
        ctx: &egui::Context,
        class: egui::ViewportClass,
        add: impl FnOnce(&mut Ui) -> R,
    ) -> ModalResponse<R> {
        if matches!(class, egui::ViewportClass::Embedded) {
            return self.show(ctx, add);
        }
        let inner = egui::CentralPanel::default()
            .frame(dialog_frame())
            .show(ctx, add)
            .inner;
        ModalResponse {
            inner: Some(inner),
            close: false,
            enter: false,
        }
    }
}

// ── Dialog layout ──────────────────────────────────────────────────────────

/// Scrolling body + pinned footer. Body never pushes the footer off-screen;
/// returns body result and measured body content height.
pub fn dialog_body_footer<R>(
    ui: &mut Ui,
    id_salt: &str,
    body: impl FnOnce(&mut Ui) -> R,
    footer: impl FnOnce(&mut Ui),
) -> (R, f32) {
    let max_h = (ui.available_height() - FOOTER_H).max(48.0);
    let out = egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink([false, true])
        .max_height(max_h)
        .show(ui, body);
    ui.add_space(theme::SP_SM);
    ui.separator();
    footer_row(ui, footer);
    (out.inner, out.content_size.y)
}

/// Right-aligned footer action row at BTN_MD. RTL: first added = rightmost,
/// so add the primary (or destructive) action first, Cancel after it.
pub fn footer_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = theme::BTN_MD;
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), theme::BTN_MD),
            egui::Layout::right_to_left(egui::Align::Center),
            add,
        );
    });
}

/// Body paragraph in a dialog: wraps, muted for secondary text.
pub fn dialog_text(ui: &mut Ui, text: impl Into<String>, muted: bool) {
    let rt = egui::RichText::new(text.into());
    let rt = if muted {
        rt.color(theme::TEXT_MUTED)
    } else {
        rt.color(theme::TEXT)
    };
    ui.add(egui::Label::new(rt).wrap());
}

/// Single-line text field at control height (lines up with buttons and
/// combo boxes in the same row; egui sizes TextEdit to the font otherwise).
pub fn text_field(buf: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(buf)
        .min_size(egui::vec2(0.0, theme::BTN_H))
        .vertical_align(egui::Align::Center)
}

/// Form label column width (all forms: labels right-aligned on one edge).
pub const LABEL_W: f32 = 120.0;

/// 2-col form grid: label col fixed LABEL_W, field fills.
/// No max_col_width: egui caps the LAST col at max_cell_size, so any finite cap
/// froze fields once the window grew past it.
pub fn form_grid(id: &str) -> egui::Grid {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([theme::SP_MD, theme::SP_SM])
        .min_col_width(LABEL_W)
}

/// Right-aligned muted form label in a fixed LABEL_W cell (grid col 0 or
/// a horizontal row). Fixed cell: RTL in a non-last Grid col could paint
/// off-screen and widen the parent.
pub fn form_label(ui: &mut Ui, text: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(LABEL_W, ui.spacing().interact_size.y),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.add(egui::Label::new(egui::RichText::new(text).color(theme::TEXT_MUTED)).truncate())
                .on_hover_text(text);
        },
    );
}

/// Native dialog viewport inner size for a width tier, clamped to the
/// monitor minus margin (falls back to the requested size).
pub fn viewport_size(ctx: &egui::Context, width: f32, height: f32) -> [f32; 2] {
    let mon = ctx
        .input(|i| i.viewport().monitor_size)
        .filter(|m| m.x > 0.0 && m.y > 0.0);
    match mon {
        Some(m) => [
            width.min(m.x - 2.0 * VIEWPORT_MARGIN),
            height.min(m.y - 2.0 * VIEWPORT_MARGIN),
        ],
        None => [width, height],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_on_open_focuses_default_button() {
        let ctx = egui::Context::default();
        let mut btn_id = None;
        for _ in 0..4 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                Modal::new("t", "T").width(size::SM).show(ctx, |ui| {
                    let r = secondary_button(ui, "Cancel");
                    focus_on_open(ui, &r);
                    btn_id = Some(r.id);
                });
            });
            eprintln!("focused={:?} btn={:?}", ctx.memory(|m| m.focused()), btn_id);
        }
        assert_eq!(ctx.memory(|m| m.focused()), btn_id);
    }

    #[test]
    fn modal_stack_tracks_topmost() {
        let ctx = egui::Context::default();
        let a = Id::new("a");
        let b = Id::new("b");
        let mut tops = Vec::new();
        for _ in 0..3 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                tops.push((register_modal(ctx, a), register_modal(ctx, b)));
            });
        }
        // First pass: only the first registrant counts as top; afterwards b.
        assert_eq!(tops[0], (true, false));
        assert_eq!(tops[2], (false, true));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(any_modal_open(ctx));
        });
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(!any_modal_open(ctx));
        });
    }
}
