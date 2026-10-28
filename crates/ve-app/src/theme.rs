//! The look: the "Achromatic ledger" app style (taste skill, seed 547259720) as egui fonts,
//! Visuals and Style, in a light and a matching dark palette. The chrome holds no colour; the
//! one data hue marks On-disk Size, and every figure is set in Geist Mono so sizes and counts
//! line up digit for digit (egui applies no OpenType features, so Geist's own `tnum` figures
//! are not available).

use egui::epaint::text::VariationCoords;
use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Margin,
    Shadow, Stroke, TextStyle, Theme, ThemePreference, Vec2,
};
use std::sync::Arc;

/// Every colour the app paints itself, for one of the two modes.
pub struct Palette {
    pub ground: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub border: Color32,
    pub rule: Color32,
    pub control: Color32,
    pub text: Color32,
    pub muted: Color32,
    /// The secondary control: a grey pill with an ink label.
    pub pill: Color32,
    pub pill_hovered: Color32,
    pub pressed: Color32,
    /// A selected row, tab or text run; its text stays ink.
    pub selected: Color32,
    /// The table row under the pointer.
    pub row_hovered: Color32,
    /// The one chart hue, on the screen's main data element: On-disk Size.
    pub data: Color32,
    pub negative: Color32,
    pub warning: Color32,
    /// A Treemap folder: its inset frame, or the whole block when too small to subdivide.
    pub folder: Color32,
    pub folder_edge: Color32,
    /// The 1px seam between file tiles.
    pub seam: Color32,
    /// The inner edge of the selected tile, against the outer ink edge.
    pub highlight_inner: Color32,
    pub shadow_alpha: u8,
}

pub const LIGHT: Palette = Palette {
    ground: Color32::from_rgb(0xf8, 0xf8, 0xf8),
    surface: Color32::WHITE,
    raised: Color32::from_rgb(0xee, 0xee, 0xee),
    border: Color32::from_rgb(0xe6, 0xe6, 0xe6),
    rule: Color32::from_rgb(0xf0, 0xf0, 0xf0),
    control: Color32::from_rgb(0xd4, 0xd4, 0xd4),
    text: Color32::from_rgb(0x0a, 0x0a, 0x0a),
    muted: Color32::from_rgb(0x6b, 0x6b, 0x6b),
    pill: Color32::from_rgb(0xe5, 0xe5, 0xe5),
    pill_hovered: Color32::from_rgb(0xdb, 0xdb, 0xdb),
    pressed: Color32::from_rgb(0xcf, 0xcf, 0xcf),
    selected: Color32::from_rgb(0xe3, 0xe3, 0xe3),
    row_hovered: Color32::from_rgb(0xf2, 0xf2, 0xf2),
    data: Color32::from_rgb(0x1d, 0x5f, 0xbf),
    negative: Color32::from_rgb(0xc4, 0x24, 0x2a),
    warning: Color32::from_rgb(0x92, 0x40, 0x0e),
    folder: Color32::from_rgb(0xd0, 0xd0, 0xd2),
    folder_edge: Color32::from_rgb(0xb2, 0xb2, 0xb6),
    seam: Color32::from_rgba_premultiplied(170, 170, 170, 170), // white at 2/3 opacity
    highlight_inner: Color32::WHITE,
    shadow_alpha: 24,
};

/// The same structure with the neutrals inverted; the data hue and the alert colours are
/// lightened to keep their contrast on a dark ground.
pub const DARK: Palette = Palette {
    ground: Color32::from_rgb(0x12, 0x12, 0x12),
    surface: Color32::from_rgb(0x1a, 0x1a, 0x1a),
    raised: Color32::from_rgb(0x26, 0x26, 0x26),
    border: Color32::from_rgb(0x2c, 0x2c, 0x2c),
    rule: Color32::from_rgb(0x20, 0x20, 0x20),
    control: Color32::from_rgb(0x44, 0x44, 0x44),
    text: Color32::from_rgb(0xed, 0xed, 0xed),
    muted: Color32::from_rgb(0x9a, 0x9a, 0x9a),
    pill: Color32::from_rgb(0x2c, 0x2c, 0x2c),
    pill_hovered: Color32::from_rgb(0x36, 0x36, 0x36),
    pressed: Color32::from_rgb(0x42, 0x42, 0x42),
    selected: Color32::from_rgb(0x33, 0x33, 0x33),
    row_hovered: Color32::from_rgb(0x1f, 0x1f, 0x1f),
    data: Color32::from_rgb(0x5b, 0x9b, 0xf0),
    negative: Color32::from_rgb(0xf0, 0x6a, 0x6e),
    warning: Color32::from_rgb(0xe8, 0xa8, 0x5c),
    folder: Color32::from_rgb(0x3a, 0x3a, 0x3e),
    folder_edge: Color32::from_rgb(0x58, 0x58, 0x5e),
    seam: Color32::from_rgba_premultiplied(0, 0, 0, 150), // black at 3/5 opacity
    highlight_inner: Color32::BLACK,
    shadow_alpha: 90,
};

/// The palette of the mode `ui` is drawn in.
pub fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// Geist at weight 500, for section headings.
pub const MEDIUM: &str = "Geist Medium";

const GEIST: &[u8] = include_bytes!("../assets/fonts/Geist-Variable.ttf");
const GEIST_MONO: &[u8] = include_bytes!("../assets/fonts/GeistMono-Variable.ttf");

/// Install the fonts and both styles on `ctx`, showing the one `preference` selects.
pub fn apply(ctx: &egui::Context, preference: ThemePreference) {
    ctx.set_fonts(fonts());
    ctx.style_mut_of(Theme::Light, |s| style(s, &LIGHT));
    ctx.style_mut_of(Theme::Dark, |s| style(s, &DARK));
    ctx.set_theme(preference);
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let weight = |w: f32| FontTweak {
        coords: VariationCoords::new([(b"wght", w)]),
        ..Default::default()
    };
    fonts.font_data.insert(
        "Geist".into(),
        Arc::new(FontData::from_static(GEIST).tweak(weight(400.0))),
    );
    fonts.font_data.insert(
        MEDIUM.into(),
        Arc::new(FontData::from_static(GEIST).tweak(weight(500.0))),
    );
    fonts.font_data.insert(
        "Geist Mono".into(),
        Arc::new(FontData::from_static(GEIST_MONO).tweak(weight(400.0))),
    );
    // egui's own fonts stay behind Geist as fallbacks for the arrows and icons it lacks.
    let fallbacks = fonts.families[&FontFamily::Proportional].clone();
    let family = |first: &str| {
        std::iter::once(first.to_owned())
            .chain(fallbacks.iter().cloned())
            .collect::<Vec<_>>()
    };
    fonts.families.insert(FontFamily::Proportional, family("Geist"));
    fonts.families.insert(FontFamily::Name(MEDIUM.into()), family(MEDIUM));
    fonts.families.insert(FontFamily::Monospace, family("Geist Mono"));
    fonts
}

fn style(style: &mut egui::Style, p: &Palette) {
    use FontFamily::{Monospace, Name, Proportional};
    style.text_styles = [
        (TextStyle::Heading, FontId::new(16.0, Name(MEDIUM.into()))),
        (TextStyle::Body, FontId::new(14.0, Proportional)),
        (TextStyle::Button, FontId::new(14.0, Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, Monospace)),
        (TextStyle::Small, FontId::new(12.0, Proportional)),
    ]
    .into();

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 6.0);
    s.button_padding = Vec2::new(12.0, 6.0);
    s.interact_size.y = 32.0;
    s.window_margin = Margin::same(24);
    s.menu_margin = Margin::same(6);
    s.icon_width = 16.0;
    s.icon_width_inner = 9.0;
    s.icon_spacing = 8.0;

    let v = &mut style.visuals;
    v.panel_fill = p.ground;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.surface;
    v.text_edit_bg_color = Some(p.raised);
    v.faint_bg_color = p.rule;
    v.code_bg_color = p.raised;
    v.weak_text_color = Some(p.muted);
    v.hyperlink_color = p.text;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.negative;
    v.striped = false;
    v.disabled_alpha = 0.6;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    // Only what floats casts a shadow, and a faint one.
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(p.shadow_alpha),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(p.shadow_alpha * 5 / 6),
    };
    v.text_cursor.stroke = Stroke::new(1.5, p.text);
    v.selection.bg_fill = p.selected;
    v.selection.stroke = Stroke::new(1.0, p.text);

    let w = &mut v.widgets;
    let control = CornerRadius::same(6);
    w.noninteractive.bg_fill = p.ground;
    w.noninteractive.weak_bg_fill = p.ground;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border); // separators and panel edges
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text); // body text
    w.noninteractive.corner_radius = CornerRadius::same(8);
    w.inactive.weak_bg_fill = p.pill; // buttons
    w.inactive.bg_fill = p.pill; // checkboxes, drag values
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = control;
    w.hovered.weak_bg_fill = p.pill_hovered;
    w.hovered.bg_fill = p.row_hovered;
    w.hovered.bg_stroke = Stroke::new(1.0, p.control);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = control;
    w.hovered.expansion = 0.0;
    w.active.weak_bg_fill = p.pressed;
    w.active.bg_fill = p.pressed;
    w.active.bg_stroke = Stroke::new(1.0, p.text);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = control;
    w.active.expansion = 0.0;
    w.open.weak_bg_fill = p.pill;
    w.open.bg_fill = p.pill;
    w.open.bg_stroke = Stroke::new(1.0, p.control);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = control;
}
