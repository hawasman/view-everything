//! The look of the app: the `--*` tokens of `designs/final/ve.css`, light and dark, as one
//! gpui-kit theme, plus the bundled fonts and the few icons the kit does not ship.

use crate::settings::ThemeChoice;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
use gpui_kit::*;
use std::borrow::Cow;
use std::rc::Rc;
use ve_core::Category;

/// Static instances of the OFL variable Instrument Sans (wght 400, 500, 600): GPUI picks a
/// weight by face, so each one is its own file.
const SANS: [&[u8]; 3] = [
    include_bytes!("../assets/fonts/InstrumentSans-Regular.ttf"),
    include_bytes!("../assets/fonts/InstrumentSans-Medium.ttf"),
    include_bytes!("../assets/fonts/InstrumentSans-SemiBold.ttf"),
];
const SERIF: &[u8] = include_bytes!("../assets/fonts/InstrumentSerif-Regular.ttf");
pub const SANS_FAMILY: &str = "Instrument Sans";
pub const SERIF_FAMILY: &str = "Instrument Serif";

pub const RADIUS_CARD: f32 = 22.;
pub const RADIUS_INNER: f32 = 14.;

fn hx(v: u32) -> Hsla {
    rgb(v).into()
}

/// Every colour token of `ve.css`, ported whole; the positive and negative pills wait for
/// the screens that use them.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Palette {
    pub ground: Hsla,
    pub surface: Hsla,
    pub raised: Hsla,
    pub active: Hsla,
    pub border: Hsla,
    pub rule: Hsla,
    pub control: Hsla,
    pub skeleton: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub positive: Hsla,
    pub negative: Hsla,
    pub warning: Hsla,
    pub primary: Hsla,
    pub on_primary: Hsla,
    pub pill_positive: (Hsla, Hsla),
    pub pill_negative: (Hsla, Hsla),
    pub pill_warning: (Hsla, Hsla),
    pub pill_neutral: (Hsla, Hsla),
    pub pill_accent: (Hsla, Hsla),
    /// `--cat-*`, in `Category` declaration order.
    pub cat: [Hsla; 8],
    pub tileink: Hsla,
    pub free: Hsla,
    pub avatar: (Hsla, Hsla),
}

impl Palette {
    fn light() -> Self {
        Self {
            ground: hx(0xffffff),
            surface: hx(0xf4f6f6),
            raised: hx(0xffffff),
            active: hx(0xececec),
            border: hx(0xeaeaea),
            rule: hx(0xe6e9e9),
            control: hx(0xd0d3d2),
            skeleton: hx(0xe3e6e6),
            text: hx(0x1f201d),
            muted: hx(0x636361),
            accent: hx(0x1b6e66),
            positive: hx(0x2e7d32),
            negative: hx(0xb42318),
            warning: hx(0x92400e),
            primary: hx(0x1f201d),
            on_primary: hx(0xffffff),
            pill_positive: (hx(0xdcfae5), hx(0x04250e)),
            pill_negative: (hx(0xfdecea), hx(0x7a1d14)),
            pill_warning: (hx(0xfffbeb), hx(0x92400e)),
            pill_neutral: (hx(0xe9ecec), hx(0x3d3d3a)),
            pill_accent: (hx(0xdbeeeb), hx(0x0f4a44)),
            cat: [
                hx(0x35609a),
                hx(0xa8661a),
                hx(0xa0456f),
                hx(0x5f7d2a),
                hx(0x6256a0),
                hx(0x2c7f97),
                hx(0x9a5530),
                hx(0x6f6f6b),
            ],
            tileink: hx(0xffffff),
            free: hx(0xe7eaea),
            avatar: (hx(0xdbe7f3), hx(0x1d3a57)),
        }
    }

    fn dark() -> Self {
        Self {
            ground: hx(0x111111),
            surface: hx(0x1b1c1c),
            raised: hx(0x242525),
            active: hx(0x2a2b2b),
            border: hx(0x2a2b2b),
            rule: hx(0x2e2f2f),
            control: hx(0x4a4b4a),
            skeleton: hx(0x2c2d2d),
            text: hx(0xf2f2ef),
            muted: hx(0xa7a7a3),
            accent: hx(0x5fc4b8),
            positive: hx(0x4cc27a),
            negative: hx(0xef7a6f),
            warning: hx(0xe3a864),
            primary: hx(0xf2f2ef),
            on_primary: hx(0x111111),
            pill_positive: (hx(0x172a1d), hx(0x4cc27a)),
            pill_negative: (hx(0x2c1a18), hx(0xef7a6f)),
            pill_warning: (hx(0x2d2415), hx(0xe3a864)),
            pill_neutral: (hx(0x2a2b2b), hx(0xcfcfcb)),
            pill_accent: (hx(0x16302d), hx(0x5fc4b8)),
            cat: [
                hx(0x4a7fbf),
                hx(0xc98a3d),
                hx(0xc0638f),
                hx(0x86a44a),
                hx(0x8479c4),
                hx(0x3f9bb8),
                hx(0xbf7448),
                hx(0x8a8a85),
            ],
            tileink: hx(0x0d0d0d),
            free: hx(0x2a2b2b),
            avatar: (hx(0x25384a), hx(0xbcd6ee)),
        }
    }

    pub fn category(&self, category: Category) -> Hsla {
        self.cat[category as usize]
    }

    /// The tokens as the kit's theme file expresses them.
    fn config(&self, name: &'static str, mode: ThemeMode) -> ThemeConfig {
        let s = |c: Hsla| Some(hex(c).into());
        let mut colors = ThemeConfigColors::default();
        colors.background = s(self.ground);
        colors.foreground = s(self.text);
        colors.border = s(self.border);
        colors.input = s(self.control);
        colors.muted = s(self.surface);
        colors.muted_foreground = s(self.muted);
        colors.accent = s(self.active);
        colors.accent_foreground = s(self.text);
        colors.ring = s(self.accent);
        colors.caret = s(self.text);
        colors.selection = s(self.pill_accent.0);
        colors.link = s(self.accent);
        colors.link_hover = s(self.accent);
        colors.link_active = s(self.accent);
        colors.primary = s(self.primary);
        colors.primary_hover = s(self.primary);
        colors.primary_active = s(self.primary);
        colors.primary_foreground = s(self.on_primary);
        colors.secondary = s(self.surface);
        colors.secondary_hover = s(self.active);
        colors.secondary_active = s(self.active);
        colors.secondary_foreground = s(self.text);
        colors.popover = s(self.raised);
        colors.popover_foreground = s(self.text);
        colors.danger = s(self.negative);
        colors.danger_hover = s(self.negative);
        colors.danger_active = s(self.negative);
        colors.danger_foreground = s(self.on_primary);
        colors.warning = s(self.warning);
        colors.warning_foreground = s(self.on_primary);
        colors.success = s(self.positive);
        colors.success_foreground = s(self.on_primary);
        colors.info = s(self.accent);
        colors.info_foreground = s(self.on_primary);
        // Buttons: pill outlines on the ground, as `.btn` in ve.css.
        colors.button = s(self.ground);
        colors.button_hover = s(self.surface);
        colors.button_active = s(self.active);
        colors.button_foreground = s(self.text);
        colors.button_primary = s(self.primary);
        colors.button_primary_hover = s(self.primary);
        colors.button_primary_active = s(self.primary);
        colors.button_primary_foreground = s(self.on_primary);
        colors.button_secondary = s(self.ground);
        colors.button_secondary_hover = s(self.surface);
        colors.button_secondary_active = s(self.active);
        colors.button_secondary_foreground = s(self.text);
        colors.button_danger = s(self.ground);
        colors.button_danger_hover = s(self.surface);
        colors.button_danger_active = s(self.active);
        colors.button_danger_foreground = s(self.text);
        colors.switch = s(self.control);
        colors.switch_thumb = s(self.ground);
        colors.skeleton = s(self.skeleton);
        colors.sidebar = s(self.ground);
        colors.sidebar_border = s(self.border);
        colors.sidebar_foreground = s(self.muted);
        colors.sidebar_accent = s(self.active);
        colors.sidebar_accent_foreground = s(self.text);
        colors.sidebar_primary = s(self.primary);
        colors.sidebar_primary_foreground = s(self.on_primary);
        colors.table = s(self.surface);
        colors.table_even = s(self.surface);
        colors.table_head = s(self.surface);
        colors.table_head_foreground = s(self.muted);
        colors.table_hover = s(self.active);
        colors.table_active = s(self.active);
        colors.table_active_border = s(self.active);
        colors.table_row_border = s(self.rule);
        colors.table_foot = s(self.surface);
        colors.table_foot_foreground = s(self.muted);
        colors.list = s(self.ground);
        colors.list_even = s(self.ground);
        colors.list_head = s(self.ground);
        colors.list_hover = s(self.active);
        colors.list_active = s(self.active);
        colors.list_active_border = s(self.active);
        colors.tab = s(self.ground);
        colors.tab_active = s(self.ground);
        colors.tab_active_foreground = s(self.text);
        colors.tab_foreground = s(self.muted);
        colors.tab_bar = s(self.ground);
        colors.tab_bar_segmented = s(self.surface);
        colors.title_bar = s(self.ground);
        colors.title_bar_border = s(self.border);
        colors.status_bar = s(self.ground);
        colors.status_bar_border = s(self.border);
        colors.progress_bar = s(self.accent);
        colors.scrollbar = s(self.surface.opacity(0.));
        colors.scrollbar_thumb = s(self.control);
        colors.scrollbar_thumb_hover = s(self.muted);
        colors.window_border = s(self.border);
        // The dim behind a dialog.
        colors.overlay = Some("#0000006b".into());
        ThemeConfig {
            name: name.into(),
            mode,
            font_size: Some(14.),
            font_family: Some(SANS_FAMILY.into()),
            mono_font_family: Some(SANS_FAMILY.into()),
            mono_font_size: Some(13.),
            radius: Some(RADIUS_INNER as usize),
            radius_lg: Some(RADIUS_CARD as usize),
            shadow: Some(false),
            colors,
            ..Default::default()
        }
    }
}

fn hex(c: Hsla) -> String {
    let c: Rgba = c.into();
    let byte = |v: f32| (v * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

#[derive(Clone, Copy)]
struct Palettes {
    light: Palette,
    dark: Palette,
}

impl Global for Palettes {}

/// The tokens of the theme in use.
pub fn palette(cx: &App) -> Palette {
    let both = cx.global::<Palettes>();
    if cx.theme().is_dark() { both.dark } else { both.light }
}

/// Embedded icons the kit does not carry (a Lucide subset, ISC).
const ICONS: [(&str, &str); 2] = [
    (
        "icons/filters.svg",
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M2 3.5h12M4.5 8h7M7 12.5h2"/></svg>"#,
    ),
    (
        "icons/trash.svg",
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 4h11M6 4V2.5h4V4M4 4l.6 9.5h6.8L12 4"/></svg>"#,
    ),
];

/// The kit's assets plus the embedded icons above.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, svg)) = ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(svg.as_bytes())));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

/// Loads the fonts and the light and dark themes, and applies the user's choice. Call once
/// before opening the window.
pub fn init(choice: ThemeChoice, cx: &mut App) {
    cx.text_system()
        .add_fonts(SANS.iter().chain([&SERIF]).map(|f| Cow::Borrowed(*f)).collect())
        .expect("embedded fonts are valid");
    let (light, dark) = (Palette::light(), Palette::dark());
    let configs = (
        Rc::new(light.config("View Everything Light", ThemeMode::Light)),
        Rc::new(dark.config("View Everything Dark", ThemeMode::Dark)),
    );
    cx.set_global(Palettes { light, dark });
    let theme = Theme::global_mut(cx);
    theme.light_theme = configs.0;
    theme.dark_theme = configs.1;
    apply(choice, cx);
}

/// Applies the theme choice; System follows the operating system.
pub fn apply(choice: ThemeChoice, cx: &mut App) {
    match choice {
        ThemeChoice::System => Theme::sync_system_appearance(None, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

/// Digits of equal width (OpenType `tnum`), so figures line up.
pub fn tabular() -> FontFeatures {
    FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]))
}
