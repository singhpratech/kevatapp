//! The design system's colour tokens, mapped onto `egui::Visuals`.
//!
//! Two rules of the design system are easy to break by accident and are load-bearing
//! here:
//!
//!   * **Every field is set explicitly** rather than mutating `Visuals::default()`.
//!     egui's defaults are grey, and a missed field shows up as unstyled grey against
//!     the teal-biased neutrals — which is exactly what makes an interface look like an
//!     untouched default.
//!   * **The accent has two forms.** `#5BB8A8` is ~2.2:1 on a light ground and fails
//!     text contrast, so it is a *fill* colour only. Accent text on light uses `#2C7568`.
//!     On dark the two coincide.

use egui::epaint::Shadow;
use egui::{Color32, Rounding, Stroke, Visuals};

#[derive(Clone, Copy)]
pub struct Palette {
    pub ground: Color32,
    pub surface: Color32,
    pub sunk: Color32,
    pub line: Color32,
    pub line_soft: Color32,
    pub ink: Color32,
    pub ink_2: Color32,
    pub ink_3: Color32,
    /// The brand constant — identical in both themes. Fills only, never light-mode text.
    pub teal_bright: Color32,
    /// The text form of the accent.
    pub teal: Color32,
    pub teal_wash: Color32,
    pub amber: Color32,
    pub dark: bool,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb(
        ((hex >> 16) & 0xFF) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    )
}

pub const LIGHT: Palette = Palette {
    ground: rgb(0xEEF3F1),
    surface: rgb(0xFFFFFF),
    sunk: rgb(0xE4EBE8),
    line: rgb(0xD2DEDA),
    line_soft: rgb(0xE2EAE7),
    ink: rgb(0x16211F),
    ink_2: rgb(0x475854),
    ink_3: rgb(0x778783),
    teal_bright: rgb(0x5BB8A8),
    teal: rgb(0x2C7568),
    teal_wash: rgb(0xE2F1ED),
    amber: rgb(0x9A6612),
    dark: false,
};

pub const DARK: Palette = Palette {
    ground: rgb(0x0D1615),
    surface: rgb(0x161F1D),
    sunk: rgb(0x101817),
    line: rgb(0x2A3735),
    line_soft: rgb(0x202B29),
    ink: rgb(0xE8F0ED),
    ink_2: rgb(0xA3B4B0),
    ink_3: rgb(0x74857F),
    teal_bright: rgb(0x5BB8A8),
    teal: rgb(0x5BB8A8),
    teal_wash: rgb(0x15302B),
    amber: rgb(0xD9A03C),
    dark: true,
};

/// The text colour to place on top of a `teal_bright` fill. Near-white on a light
/// theme's buttons, and the dark ground colour on dark — matching the website, where
/// a filled teal button flips its label colour rather than dimming the fill.
pub const fn on_accent(p: &Palette) -> Color32 {
    if p.dark {
        rgb(0x0D1615)
    } else {
        Color32::WHITE
    }
}

pub fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.panel_fill = p.ground;
    v.window_fill = p.surface;
    v.faint_bg_color = p.sunk;
    v.extreme_bg_color = p.sunk;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.hyperlink_color = p.teal;
    v.override_text_color = Some(p.ink);

    v.selection.bg_fill = p.teal_bright.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.ink);

    // Small: this is the radius every widget gets, including a ~14px checkbox, and at
    // 8px that one turns into a disc indistinguishable from a radio button. The larger
    // radius the design system asks for on buttons is applied per-button instead.
    let rounding = Rounding::same(5.0);

    // Non-interactive: labels, separators, panel furniture.
    v.widgets.noninteractive.bg_fill = p.surface;
    v.widgets.noninteractive.weak_bg_fill = p.surface;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line_soft);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.ink_2);
    v.widgets.noninteractive.rounding = rounding;

    // Inactive: a control at rest.
    v.widgets.inactive.bg_fill = p.sunk;
    v.widgets.inactive.weak_bg_fill = p.sunk;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.ink);
    v.widgets.inactive.rounding = rounding;

    // Hovered and active both take the accent fill.
    v.widgets.hovered.bg_fill = p.teal_bright;
    v.widgets.hovered.weak_bg_fill = p.teal_wash;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.teal_bright);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.ink);
    v.widgets.hovered.rounding = rounding;

    v.widgets.active.bg_fill = p.teal_bright;
    v.widgets.active.weak_bg_fill = p.teal_bright;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.teal_bright);
    v.widgets.active.fg_stroke = Stroke::new(1.0, on_accent(p));
    v.widgets.active.rounding = rounding;

    v.widgets.open.bg_fill = p.sunk;
    v.widgets.open.weak_bg_fill = p.sunk;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.ink);
    v.widgets.open.rounding = rounding;

    // Large soft shadows are expensive to rasterize on a CPU and read as heavy at this
    // scale — the design system asks for them low. NONE is the cheapest honest answer.
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow::NONE;
    v.window_rounding = Rounding::same(10.0);

    v
}
