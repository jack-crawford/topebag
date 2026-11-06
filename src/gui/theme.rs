//! Look and feel: a warm "paper" palette, Segoe UI on Windows, and the
//! window icon (drawn in code, so there are no binary assets to ship).

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Stroke, Visuals};

pub const PAPER: Color32 = Color32::from_rgb(0xF7, 0xF3, 0xEC);
pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
pub const CARD_SELECTED: Color32 = Color32::from_rgb(0xFD, 0xEB, 0xDF);
pub const INK: Color32 = Color32::from_rgb(0x2B, 0x26, 0x22);
pub const MUTED: Color32 = Color32::from_rgb(0x7A, 0x70, 0x66);
pub const HAIRLINE: Color32 = Color32::from_rgb(0xE4, 0xDC, 0xD0);
pub const ACCENT: Color32 = Color32::from_rgb(0xC8, 0x4B, 0x12);
pub const ACCENT_DARK: Color32 = Color32::from_rgb(0xA3, 0x3C, 0x0C);
pub const SUCCESS: Color32 = Color32::from_rgb(0x2F, 0x7D, 0x4A);
pub const DANGER: Color32 = Color32::from_rgb(0xB4, 0x23, 0x18);

/// Font family used for bold display text (titles).
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// Apply fonts and visuals to the egui context.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::ThemePreference::Light);

    let mut visuals = Visuals::light();
    visuals.panel_fill = PAPER;
    visuals.window_fill = CARD;
    visuals.extreme_bg_color = CARD;
    visuals.faint_bg_color = Color32::from_rgb(0xF2, 0xEC, 0xE3);
    visuals.override_text_color = Some(INK);
    visuals.hyperlink_color = ACCENT_DARK;
    visuals.selection.bg_fill = Color32::from_rgb(0xF6, 0xC9, 0xAE);
    visuals.selection.stroke = Stroke::new(1.0, ACCENT_DARK);
    visuals.window_stroke = Stroke::new(1.0, HAIRLINE);
    visuals.window_corner_radius = CornerRadius::same(10);

    let radius = CornerRadius::same(6);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    widgets.inactive.weak_bg_fill = Color32::from_rgb(0xEF, 0xE8, 0xDE);
    widgets.inactive.bg_fill = Color32::from_rgb(0xEF, 0xE8, 0xDE);
    widgets.inactive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    widgets.hovered.weak_bg_fill = Color32::from_rgb(0xE8, 0xDE, 0xD1);
    widgets.hovered.bg_fill = Color32::from_rgb(0xE8, 0xDE, 0xD1);
    widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xD3, 0xC6, 0xB6));
    widgets.active.weak_bg_fill = Color32::from_rgb(0xDF, 0xD3, 0xC4);
    widgets.active.bg_fill = Color32::from_rgb(0xDF, 0xD3, 0xC4);
    for w in [&mut widgets.noninteractive, &mut widgets.inactive, &mut widgets.hovered, &mut widgets.active, &mut widgets.open] {
        w.corner_radius = radius;
    }
    ctx.set_visuals_of(egui::Theme::Light, visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(11.0, 5.0);
        style.spacing.interact_size.y = 28.0;
        use egui::{FontId, TextStyle};
        style.text_styles = [
            (TextStyle::Heading, FontId::new(22.0, bold_family())),
            (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();
    });
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut bold_stack = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();

    #[cfg(windows)]
    {
        let dir = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
        if let Ok(regular) = std::fs::read(dir.join("segoeui.ttf")) {
            fonts.font_data.insert("segoe".into(), FontData::from_owned(regular).into());
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
        }
        if let Ok(bold) = std::fs::read(dir.join("segoeuib.ttf")) {
            fonts.font_data.insert("segoe-bold".into(), FontData::from_owned(bold).into());
            bold_stack.insert(0, "segoe-bold".into());
        }
        if let Ok(symbols) = std::fs::read(dir.join("seguisym.ttf")) {
            fonts.font_data.insert("segoe-symbol".into(), FontData::from_owned(symbols).into());
            fonts.families.entry(FontFamily::Proportional).or_default().push("segoe-symbol".into());
            bold_stack.push("segoe-symbol".into());
        }
    }

    if cfg!(target_os = "macos") {
        add_mac_fonts(&mut fonts, &mut bold_stack);
    }

    fonts.families.insert(bold_family(), bold_stack);
    ctx.set_fonts(fonts);
}

/// macOS: Helvetica Neue for text (the regular and bold faces picked out of
/// the system's font collection) and Apple Symbols for arrows, ticks and ⌘.
/// Compiled on every platform so it's type-checked and tested everywhere;
/// elsewhere the files simply don't exist.
fn add_mac_fonts(fonts: &mut FontDefinitions, bold_stack: &mut Vec<String>) {
    if let Ok(data) = std::fs::read("/System/Library/Fonts/HelveticaNeue.ttc") {
        if let Some(index) = face_index(&data, 700) {
            let mut bold = FontData::from_owned(data.clone());
            bold.index = index;
            fonts.font_data.insert("helvetica-bold".into(), bold.into());
            bold_stack.insert(0, "helvetica-bold".into());
        }
        if let Some(index) = face_index(&data, 400) {
            let mut regular = FontData::from_owned(data);
            regular.index = index;
            fonts.font_data.insert("helvetica".into(), regular.into());
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "helvetica".into());
        }
    }
    if let Ok(symbols) = std::fs::read("/System/Library/Fonts/Apple Symbols.ttf") {
        fonts.font_data.insert("apple-symbols".into(), FontData::from_owned(symbols).into());
        fonts.families.entry(FontFamily::Proportional).or_default().push("apple-symbols".into());
        bold_stack.push("apple-symbols".into());
    }
}

/// Index of the upright, normal-width face of `weight` (400 regular, 700
/// bold) in a font file or collection.
fn face_index(data: &[u8], weight: u16) -> Option<u32> {
    use typst::text::{FontInfo, FontStretch, FontStyle};
    FontInfo::iter(data)
        .position(|info| {
            info.variant.weight.to_number() == weight
                && info.variant.style == FontStyle::Normal
                && info.variant.stretch == FontStretch::NORMAL
        })
        .and_then(|i| u32::try_from(i).ok())
}

/// A 64×64 window icon: an orange tote with a white printed page.
pub fn icon() -> egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];
    let mut put = |x: usize, y: usize, c: [u8; 4]| {
        let i = (y * S + x) * 4;
        rgba[i..i + 4].copy_from_slice(&c);
    };
    let orange = [0xC8, 0x4B, 0x12, 0xFF];
    let white = [0xFF, 0xFF, 0xFF, 0xFF];
    let line = [0xB9, 0xAE, 0xA2, 0xFF];
    for y in 0..S {
        for x in 0..S {
            // Rounded square body (bag) from y=16..62 with radius 8.
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let inside_body = {
                let (l, r, t, b, rad) = (4.0, 60.0, 18.0, 62.0, 9.0);
                let cx = fx.clamp(l + rad, r - rad);
                let cy = fy.clamp(t + rad, b - rad);
                (fx - cx).powi(2) + (fy - cy).powi(2) <= rad * rad
            };
            // Handle: ring between radii 10 and 15 centred at (32, 18), top half.
            let d = ((fx - 32.0).powi(2) + (fy - 18.0).powi(2)).sqrt();
            let handle = fy < 18.0 && (10.0..=14.5).contains(&d);
            if inside_body || handle {
                put(x, y, orange);
            }
            // Page sticking out of the bag.
            if (20..44).contains(&x) && (8..50).contains(&y) {
                put(x, y, white);
                if (24..40).contains(&x) && [14, 19, 24, 29].contains(&y) {
                    put(x, y, line);
                }
            }
            // Front pocket over the page.
            if inside_body && y >= 34 {
                put(x, y, orange);
            }
        }
    }
    egui::IconData { rgba, width: S as u32, height: S as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faces_are_found_by_weight() {
        let regular = typst_assets::fonts()
            .find(|data| {
                typst::text::FontInfo::iter(data)
                    .next()
                    .is_some_and(|i| i.family == "Libertinus Serif" && i.variant.weight.to_number() == 400 && i.variant.style == typst::text::FontStyle::Normal)
            })
            .expect("Libertinus Serif Regular is bundled");
        assert_eq!(face_index(regular, 400), Some(0));
        assert_eq!(face_index(regular, 700), None);
    }

    #[test]
    fn mac_fonts_are_skipped_where_missing() {
        let mut fonts = FontDefinitions::default();
        let mut bold = Vec::new();
        add_mac_fonts(&mut fonts, &mut bold);
        if cfg!(target_os = "macos") {
            // On a Mac (CI runs one) the system fonts are really picked up.
            assert!(fonts.font_data.contains_key("helvetica") && fonts.font_data.contains_key("helvetica-bold"));
            assert_eq!(bold.first().map(String::as_str), Some("helvetica-bold"));
        } else {
            assert!(bold.is_empty() && !fonts.font_data.contains_key("helvetica"));
        }
    }
}
