//! Тема и акцентный цвет окна (egui). Платформонезависимо.

use eframe::egui::{self, Color32, ThemePreference};

use crate::config::{self, ThemeChoice};

/// Акцент из строки "#RRGGBB"; при ошибке — акцент по умолчанию.
pub fn accent_color(s: &str) -> Color32 {
    let [r, g, b] = config::parse_hex_color(s)
        .or_else(|| config::parse_hex_color(config::DEFAULT_ACCENT))
        .unwrap_or([0x3c, 0xc8, 0x8a]);
    Color32::from_rgb(r, g, b)
}

/// Читаемый цвет текста поверх `bg` (чёрный или белый по яркости).
pub fn contrast_text(bg: Color32) -> Color32 {
    let lum = 0.299 * f32::from(bg.r()) + 0.587 * f32::from(bg.g()) + 0.114 * f32::from(bg.b());
    if lum > 150.0 { Color32::BLACK } else { Color32::WHITE }
}

pub fn preference(t: ThemeChoice) -> ThemePreference {
    match t {
        ThemeChoice::System => ThemePreference::System,
        ThemeChoice::Light => ThemePreference::Light,
        ThemeChoice::Dark => ThemePreference::Dark,
    }
}

/// Применяет тему (system следует теме ОС) и акцент к обеим палитрам egui.
pub fn apply(ctx: &egui::Context, theme: ThemeChoice, accent: Color32) {
    ctx.set_theme(preference(theme));
    let text = contrast_text(accent);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.selection.bg_fill = accent;
        v.selection.stroke.color = text;
        v.hyperlink_color = accent;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_parsing_and_fallback() {
        assert_eq!(accent_color("#FF0000"), Color32::from_rgb(255, 0, 0));
        assert_eq!(accent_color("garbage"), Color32::from_rgb(0x3c, 0xc8, 0x8a));
    }

    #[test]
    fn contrast() {
        assert_eq!(contrast_text(Color32::WHITE), Color32::BLACK);
        assert_eq!(contrast_text(Color32::from_rgb(10, 20, 80)), Color32::WHITE);
    }

    #[test]
    fn theme_mapping() {
        assert_eq!(preference(ThemeChoice::System), ThemePreference::System);
        assert_eq!(preference(ThemeChoice::Dark), ThemePreference::Dark);
        assert_eq!(preference(ThemeChoice::Light), ThemePreference::Light);
    }
}
