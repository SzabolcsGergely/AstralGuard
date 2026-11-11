//! Seven colour tokens per preset (background, surfaces, borders, primary text, accent,
//! danger, pending) plus three inline value tints, and the fonts. Flat and dark by default.

use eframe::egui::{
    self, epaint::Shadow, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, Style,
    TextStyle, Visuals,
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub background: Color32,
    pub surface: Color32,
    pub border: Color32,
    pub text: Color32,
    pub accent: Color32,
    pub danger: Color32,
    pub pending: Color32,
    // inline value tints
    pub blue: Color32,
    pub green: Color32,
    pub red: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

const DARK_TINTS: (Color32, Color32, Color32) = (hex(0x4D94FF), hex(0x63C787), hex(0xEF6B73));

const fn dark_preset(bg: u32, surface: u32, border: u32, text: u32, accent: u32) -> Theme {
    Theme {
        dark: true,
        background: hex(bg),
        surface: hex(surface),
        border: hex(border),
        text: hex(text),
        accent: hex(accent),
        danger: hex(0xEF6B73),
        pending: hex(0xE5A84B),
        blue: DARK_TINTS.0,
        green: DARK_TINTS.1,
        red: DARK_TINTS.2,
    }
}

pub const DARK: Theme = dark_preset(0x111317, 0x1A1D23, 0x2A2E36, 0xE6E9EE, 0x4D94FF);
pub const GRAPHITE: Theme = dark_preset(0x161616, 0x202020, 0x313131, 0xE8E8E8, 0x9AA6B8);
pub const MIDNIGHT: Theme = dark_preset(0x0A0F1C, 0x111A2C, 0x1F2B45, 0xE2E8F5, 0x5B9DFF);
pub const VIOLET: Theme = dark_preset(0x120F1A, 0x1B1726, 0x2D2642, 0xECE8F5, 0xA07CFF);
pub const LIGHT: Theme = Theme {
    dark: false,
    background: hex(0xF2F3F5),
    surface: hex(0xFFFFFF),
    border: hex(0xDADDE3),
    text: hex(0x1C1F24),
    accent: hex(0x2F6FE0),
    danger: hex(0xD9434F),
    pending: hex(0xB7791F),
    blue: hex(0x2F6FE0),
    green: hex(0x2E9E5B),
    red: hex(0xD9434F),
};

/// (key stored in ui.json, label in the picker)
pub const PRESETS: [(&str, &str); 6] = [
    ("system", "System"),
    ("dark", "Dark"),
    ("graphite", "Graphite"),
    ("midnight", "Midnight"),
    ("violet", "Violet"),
    ("light", "Light"),
];

pub fn preset(key: &str) -> Theme {
    match key {
        "graphite" => GRAPHITE,
        "midnight" => MIDNIGHT,
        "violet" => VIOLET,
        "light" => LIGHT,
        "system" => {
            if system_prefers_light() {
                LIGHT
            } else {
                DARK
            }
        }
        _ => DARK,
    }
}

/// Windows: the "apps use light theme" switch. Elsewhere: dark.
pub fn system_prefers_light() -> bool {
    #[cfg(windows)]
    {
        super::win::apps_use_light_theme().unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

impl Theme {
    /// Captions and secondary text.
    pub fn muted(&self) -> Color32 {
        lerp(self.text, self.surface, 0.45)
    }
    /// Axis labels, grid lines.
    pub fn faint(&self) -> Color32 {
        lerp(self.text, self.surface, 0.72)
    }
    pub fn grid(&self) -> Color32 {
        lerp(self.border, self.surface, 0.25)
    }
    /// Slightly raised surface: hovered buttons, input fields.
    pub fn raised(&self) -> Color32 {
        if self.dark {
            lerp(self.surface, Color32::WHITE, 0.05)
        } else {
            lerp(self.surface, self.background, 0.6)
        }
    }
    pub fn field(&self) -> Color32 {
        if self.dark {
            self.background
        } else {
            Color32::WHITE
        }
    }
    pub fn tint_bg(&self, c: Color32) -> Color32 {
        lerp(self.surface, c, if self.dark { 0.16 } else { 0.12 })
    }
}

pub const FAMILY_SEMIBOLD: &str = "semibold";
/// egui's built-in monospace font, the last resort for glyphs the UI fonts lack.
const FALLBACK_FONT: &str = "Hack";

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_SEMIBOLD.into()))
}
pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}
pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// Segoe UI (and Consolas for the log) from the system when present; egui's own fonts
/// otherwise and as fallbacks for any glyph the system font lacks.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let base_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let base_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let mut semibold = base_prop.clone();

    let dir = system_font_dir();
    let mut load = |name: &str, file: &str| -> bool {
        let Some(dir) = &dir else { return false };
        match std::fs::read(dir.join(file)) {
            Ok(bytes) => {
                fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            Err(_) => false,
        }
    };
    let have_regular = load("ui-regular", "segoeui.ttf");
    let have_semibold = load("ui-semibold", "seguisb.ttf");
    let have_mono = load("ui-mono", "consola.ttf");

    if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional) {
        if have_regular {
            prop.insert(0, "ui-regular".into());
        }
    }
    if have_semibold {
        semibold.insert(0, "ui-semibold".into());
    } else if have_regular {
        semibold.insert(0, "ui-regular".into());
    }
    if let Some(m) = fonts.families.get_mut(&FontFamily::Monospace) {
        if have_mono {
            m.insert(0, "ui-mono".into());
        }
    }
    let _ = base_mono;
    // egui's own UI font (Ubuntu Light) has no arrows: "→" (the Sag hints) drew as an empty
    // box wherever Segoe UI is missing, i.e. on Linux. Its monospace font, Hack, has them, so it
    // goes last in the text families as the fallback for any glyph the others lack.
    for family in [fonts.families.get_mut(&FontFamily::Proportional), Some(&mut semibold)].into_iter().flatten() {
        if !family.iter().any(|f| f == FALLBACK_FONT) {
            family.push(FALLBACK_FONT.into());
        }
    }
    fonts.families.insert(FontFamily::Name(FAMILY_SEMIBOLD.into()), semibold);
    ctx.set_fonts(fonts);
}

fn system_font_dir() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        let root = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        Some(std::path::PathBuf::from(root).join("Fonts"))
    } else {
        None
    }
}

pub fn apply(ctx: &egui::Context, t: &Theme) {
    let mut v = if t.dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = t.background;
    v.window_fill = t.surface;
    v.window_stroke = Stroke::new(1.0_f32, t.border);
    v.window_shadow = Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(90) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(70) };
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(6);
    v.extreme_bg_color = t.field();
    v.faint_bg_color = t.raised();
    v.code_bg_color = t.field();
    v.hyperlink_color = t.accent;
    v.selection.bg_fill = t.accent.gamma_multiply(if t.dark { 0.45 } else { 0.3 });
    v.selection.stroke = Stroke::new(1.0_f32, t.text);
    v.override_text_color = None;
    v.warn_fg_color = t.pending;
    v.error_fg_color = t.danger;
    v.striped = false;
    v.slider_trailing_fill = true;

    let r = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = t.surface;
    w.noninteractive.weak_bg_fill = t.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, t.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, t.text);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = t.raised();
    w.inactive.weak_bg_fill = t.raised();
    w.inactive.bg_stroke = Stroke::new(1.0_f32, lerp(t.border, t.text, 0.12));
    w.inactive.fg_stroke = Stroke::new(1.0_f32, t.text);
    w.inactive.corner_radius = r;

    let hover = if t.dark { lerp(t.raised(), Color32::WHITE, 0.06) } else { lerp(t.raised(), t.border, 0.5) };
    w.hovered.bg_fill = hover;
    w.hovered.weak_bg_fill = hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, lerp(t.border, t.accent, 0.5));
    w.hovered.fg_stroke = Stroke::new(1.0_f32, t.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = t.accent.gamma_multiply(0.5);
    w.active.weak_bg_fill = lerp(hover, t.accent, 0.25);
    w.active.bg_stroke = Stroke::new(1.0_f32, t.accent);
    w.active.fg_stroke = Stroke::new(1.0_f32, t.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open = w.hovered;

    let mut style = Style { visuals: v, ..Style::default() };
    style.text_styles = [
        (TextStyle::Small, regular(11.0)),
        (TextStyle::Body, regular(13.5)),
        (TextStyle::Button, regular(13.5)),
        (TextStyle::Heading, semibold(18.0)),
        (TextStyle::Monospace, mono(12.5)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 4.0);
    style.spacing.interact_size = egui::vec2(40.0, 26.0);
    style.spacing.combo_width = 150.0;
    style.spacing.icon_width = 16.0;
    style.spacing.icon_width_inner = 9.0;
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    style.interaction.selectable_labels = false;
    // the same style whatever egui thinks the OS theme is
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-ASCII character the program shows (outside comments) has a glyph in the text
    /// fonts as installed here, so none draws as an empty box. On Linux that is egui's own fonts
    /// alone: Segoe UI is Windows-only, so this checks the weakest set.
    #[test]
    fn every_symbol_in_the_ui_has_a_glyph() {
        let mut used = std::collections::BTreeSet::new();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut dirs = vec![src];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    for line in std::fs::read_to_string(&path).unwrap().lines() {
                        let code = line.trim_start();
                        if code.starts_with("//") {
                            continue;
                        }
                        let code = code.split(" // ").next().unwrap_or(code);
                        used.extend(code.chars().filter(|c| !c.is_ascii()));
                    }
                }
            }
        }
        assert!(used.contains(&'°'), "the scan found the UI's symbols");

        let ctx = egui::Context::default();
        install_fonts(&ctx);
        // One frame applies the fonts; its texture upload is not needed here.
        ctx.run_ui(Default::default(), |_| {}).textures_delta.clear();
        let missing: Vec<String> = ctx.fonts_mut(|f| {
            used.iter()
                .filter(|&&c| !f.has_glyph(&FontId::proportional(14.0), c) || !f.has_glyph(&semibold(14.0), c))
                .map(|c| format!("U+{:04X} {c}", *c as u32))
                .collect()
        });
        assert!(missing.is_empty(), "no glyph for {missing:?}");
    }
}
