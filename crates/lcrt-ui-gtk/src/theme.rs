//! The colors of the three themes and the stylesheet that applies them to
//! LILOPOP's own windows. Kept free of GTK types so contrast and output can
//! be tested directly.
//!
//! The palette overrides libadwaita's colors, so every standard widget,
//! popover and dialog follows the theme without per-widget rules.
//! Caption text, font, background and opacity are never themed: they are
//! the user's caption style (see [`crate::presentation::caption_css`]).

use lcrt_core::{Rgb, Theme};

/// The colors of one theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Palette {
    /// Whether the theme is dark, which selects libadwaita's dark style.
    pub(crate) dark: bool,
    /// Window and page background.
    pub(crate) background: Rgb,
    /// Lists, cards, dialogs and the caption window's control bar.
    pub(crate) surface: Rgb,
    /// Popovers and menus.
    pub(crate) field: Rgb,
    /// Text on every surface above.
    pub(crate) ink: Rgb,
    /// The primary action: Start, selected switches and checks.
    pub(crate) action: Rgb,
    /// Text on [`Self::action`].
    pub(crate) action_ink: Rgb,
    /// Accent drawn on the window itself: focus rings and links.
    pub(crate) highlight: Rgb,
    /// Brand accent: the theme preview and, when the overlay is themed, the
    /// caption language badges and outline.
    pub(crate) accent: Rgb,
    /// Text on [`Self::accent`].
    pub(crate) accent_ink: Rgb,
    /// Header bars.
    pub(crate) header: Rgb,
    /// Text on [`Self::header`].
    pub(crate) header_ink: Rgb,
}

/// Opacity of secondary (dimmed) text, chosen so that it still reads at
/// 4.5:1 on every theme's surfaces. libadwaita's own 55% is too faint on
/// Calm Native and on Pop Studio's menus.
const DIM_OPACITY: f64 = 0.7;

const PURPLE: Rgb = Rgb::new(0x2a, 0x1e, 0x5c);
const PINK: Rgb = Rgb::new(0xff, 0x5c, 0x8a);
const YELLOW: Rgb = Rgb::new(0xff, 0xd2, 0x3f);

/// The palette of `theme`.
pub(crate) const fn palette(theme: Theme) -> Palette {
    match theme {
        Theme::PopStudio => Palette {
            dark: true,
            background: PURPLE,
            surface: Rgb::new(0x39, 0x27, 0x6b),
            field: Rgb::new(0x4c, 0x38, 0x7c),
            ink: Rgb::new(0xff, 0xff, 0xff),
            action: PINK,
            action_ink: PURPLE,
            highlight: YELLOW,
            accent: YELLOW,
            accent_ink: PURPLE,
            header: YELLOW,
            header_ink: PURPLE,
        },
        Theme::CalmNative => Palette {
            dark: false,
            background: Rgb::new(0xf6, 0xf7, 0xfa),
            surface: Rgb::new(0xff, 0xff, 0xff),
            field: Rgb::new(0xff, 0xff, 0xff),
            ink: Rgb::new(0x27, 0x2a, 0x3d),
            action: PURPLE,
            action_ink: Rgb::new(0xff, 0xff, 0xff),
            highlight: PURPLE,
            accent: PINK,
            accent_ink: PURPLE,
            header: Rgb::new(0xff, 0xff, 0xff),
            header_ink: Rgb::new(0x27, 0x2a, 0x3d),
        },
        Theme::CinemaFocus => Palette {
            dark: true,
            background: Rgb::new(0x14, 0x17, 0x21),
            surface: Rgb::new(0x21, 0x26, 0x37),
            field: Rgb::new(0x2a, 0x2e, 0x43),
            ink: Rgb::new(0xfa, 0xfa, 0xff),
            action: YELLOW,
            action_ink: PURPLE,
            highlight: YELLOW,
            accent: PURPLE,
            accent_ink: Rgb::new(0xff, 0xff, 0xff),
            header: Rgb::new(0x1a, 0x1e, 0x2b),
            header_ink: Rgb::new(0xfa, 0xfa, 0xff),
        },
    }
}

/// The CSS class naming `theme`, used by its preview in Settings.
pub(crate) const fn css_name(theme: Theme) -> &'static str {
    match theme {
        Theme::PopStudio => "pop-studio",
        Theme::CalmNative => "calm-native",
        Theme::CinemaFocus => "cinema-focus",
    }
}

/// The swatches of a theme's preview, in order.
pub(crate) const PREVIEW_SWATCHES: usize = 4;

fn preview_colors(palette: &Palette) -> [Rgb; PREVIEW_SWATCHES] {
    [
        palette.background,
        palette.surface,
        palette.action,
        palette.accent,
    ]
}

/// Stylesheet for LILOPOP's windows in `theme`. With `high_contrast` the
/// palette is left out, so the desktop's high-contrast style applies.
///
/// libadwaita 1.6 and later read colors from CSS variables, which a
/// desktop style (such as Ubuntu's Yaru) may set directly, so with
/// `variables` the palette sets those as well. Older versions only have
/// named colors, and GTK before 4.16 would reject the variables.
pub(crate) fn theme_css(theme: Theme, high_contrast: bool, variables: bool) -> String {
    let mut css = String::new();
    if !high_contrast {
        let palette = palette(theme);
        let colors = [
            ("window_bg_color", palette.background),
            ("window_fg_color", palette.ink),
            ("view_bg_color", palette.surface),
            ("view_fg_color", palette.ink),
            ("card_bg_color", palette.surface),
            ("card_fg_color", palette.ink),
            ("dialog_bg_color", palette.surface),
            ("dialog_fg_color", palette.ink),
            ("popover_bg_color", palette.field),
            ("popover_fg_color", palette.ink),
            ("headerbar_bg_color", palette.header),
            ("headerbar_backdrop_color", palette.header),
            ("headerbar_fg_color", palette.header_ink),
            ("accent_bg_color", palette.action),
            ("accent_fg_color", palette.action_ink),
            ("accent_color", palette.highlight),
        ];
        for (name, color) in colors {
            css.push_str(&format!("@define-color {name} {};\n", color.to_hex()));
        }
        if variables {
            css.push_str(":root {");
            for (name, color) in colors {
                css.push_str(&format!(
                    " --{}: {};",
                    name.replace('_', "-"),
                    color.to_hex()
                ));
            }
            css.push_str(&format!(
                " --dim-opacity: {:.0}%; }}\n",
                DIM_OPACITY * 100.0
            ));
        }
        css.push_str(&format!(
            ".dim-label, row label.subtitle {{ opacity: {DIM_OPACITY:.2}; }}\n"
        ));
    }
    // The caption window's controls sit on a bar of their own, so they read
    // the same over any caption background. libadwaita tints Stop (a
    // destructive action) over that bar, too faintly on Pop Studio, so it
    // gets a solid fill.
    css.push_str(
        ".session-controls { background-color: @view_bg_color; color: @view_fg_color; \
             border-radius: 12px; padding: 6px; }\n\
         .session-controls button.destructive-action { \
             background-color: @destructive_bg_color; color: @destructive_fg_color; }\n\
         .session-controls button.destructive-action:hover { \
             background-image: image(alpha(currentColor, 0.1)); }\n\
         .session-controls button.destructive-action:active { \
             background-image: image(alpha(black, 0.2)); }\n\
         .error-banner { background-color: @error_bg_color; color: @error_fg_color; \
             border-radius: 10px; padding: 4px 4px 4px 10px; margin-top: 6px; }\n\
         .theme-default-tag { background-color: @accent_bg_color; color: @accent_fg_color; \
             border-radius: 999px; padding: 2px 8px; font-weight: bold; font-size: smaller; }\n\
         .theme-swatch { min-width: 12px; min-height: 26px; border-radius: 4px; \
             box-shadow: inset 0 0 0 1px alpha(currentColor, 0.15); }\n",
    );
    for theme in Theme::ALL {
        let colors = preview_colors(&palette(theme));
        for (index, color) in colors.iter().enumerate() {
            css.push_str(&format!(
                ".theme-swatch.{}-{index} {{ background-color: {}; }}\n",
                css_name(theme),
                color.to_hex()
            ));
        }
    }
    css
}

#[cfg(test)]
mod tests {
    use lcrt_core::{Rgb, Theme};

    use super::{DIM_OPACITY, palette, theme_css};

    fn channel(value: u8) -> f64 {
        let value = f64::from(value) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(color: Rgb) -> f64 {
        0.2126 * channel(color.red) + 0.7152 * channel(color.green) + 0.0722 * channel(color.blue)
    }

    /// WCAG 2 contrast ratio.
    fn contrast(first: Rgb, second: Rgb) -> f64 {
        let (first, second) = (luminance(first), luminance(second));
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    /// `foreground` drawn at `opacity` over `background`.
    fn blend(foreground: Rgb, background: Rgb, opacity: f64) -> Rgb {
        let mix = |front: u8, back: u8| {
            (opacity * f64::from(front) + (1.0 - opacity) * f64::from(back)).round() as u8
        };
        Rgb::new(
            mix(foreground.red, background.red),
            mix(foreground.green, background.green),
            mix(foreground.blue, background.blue),
        )
    }

    #[test]
    fn every_theme_s_text_reads_at_wcag_aa_on_every_surface() {
        for theme in Theme::ALL {
            let palette = palette(theme);
            for surface in [palette.background, palette.surface, palette.field] {
                let text = contrast(palette.ink, surface);
                assert!(text >= 4.5, "{theme:?} text {text:.2}");
                let dimmed = blend(palette.ink, surface, DIM_OPACITY);
                let dimmed = contrast(dimmed, surface);
                assert!(dimmed >= 4.5, "{theme:?} dimmed text {dimmed:.2}");
            }
            for (name, text, fill) in [
                ("action", palette.action_ink, palette.action),
                ("accent", palette.accent_ink, palette.accent),
                ("header", palette.header_ink, palette.header),
            ] {
                let ratio = contrast(text, fill);
                assert!(ratio >= 4.5, "{theme:?} {name} {ratio:.2}");
            }
            // The primary action stands out from the window as a control.
            let action = contrast(palette.action, palette.background);
            assert!(action >= 3.0, "{theme:?} action edge {action:.2}");
            // Links read as text on every surface. Focus rings, drawn at
            // half strength, stand out at 3:1 where rows and buttons sit
            // (menus mark focus with a fill instead).
            for surface in [palette.background, palette.surface, palette.field] {
                let link = contrast(palette.highlight, surface);
                assert!(link >= 4.5, "{theme:?} highlight {link:.2}");
            }
            for surface in [palette.background, palette.surface] {
                let ring = contrast(blend(palette.highlight, surface, 0.5), surface);
                assert!(ring >= 3.0, "{theme:?} focus ring {ring:.2}");
            }
        }
    }

    #[test]
    fn dark_themes_select_the_dark_style() {
        assert!(palette(Theme::PopStudio).dark);
        assert!(palette(Theme::CinemaFocus).dark);
        assert!(!palette(Theme::CalmNative).dark);
    }

    #[test]
    fn each_theme_overrides_the_shared_palette_with_its_own_colors() {
        let pop = theme_css(Theme::PopStudio, false, false);
        assert!(pop.contains("@define-color window_bg_color #2a1e5c;"));
        assert!(pop.contains("@define-color accent_bg_color #ff5c8a;"));
        assert!(pop.contains("@define-color headerbar_bg_color #ffd23f;"));
        let calm = theme_css(Theme::CalmNative, false, false);
        assert!(calm.contains("@define-color window_bg_color #f6f7fa;"));
        assert!(calm.contains("@define-color accent_bg_color #2a1e5c;"));
        let cinema = theme_css(Theme::CinemaFocus, false, false);
        assert!(cinema.contains("@define-color accent_bg_color #ffd23f;"));
        assert_ne!(pop, calm);
        assert_ne!(calm, cinema);
        // Older libadwaita gets no variables, newer gets both.
        assert!(!pop.contains("--"));
        let pop = theme_css(Theme::PopStudio, false, true);
        assert!(pop.contains("@define-color window_bg_color #2a1e5c;"));
        assert!(pop.contains(" --window-bg-color: #2a1e5c;"));
        assert!(pop.contains(" --accent-bg-color: #ff5c8a;"));
        assert!(pop.contains(" --dim-opacity: 70%; }"));
    }

    #[test]
    fn high_contrast_keeps_the_desktop_s_colors_and_the_previews() {
        let css = theme_css(Theme::PopStudio, true, true);
        assert!(!css.contains("@define-color"));
        assert!(!css.contains("--window-bg-color"));
        assert!(!css.contains("opacity: 0.70"));
        // Every theme's preview still shows its own colors.
        assert!(css.contains(".theme-swatch.pop-studio-2 { background-color: #ff5c8a; }"));
        assert!(css.contains(".theme-swatch.calm-native-0 { background-color: #f6f7fa; }"));
        assert!(css.contains(".theme-swatch.cinema-focus-2 { background-color: #ffd23f; }"));
    }

    #[test]
    fn the_stylesheet_is_bounded_and_the_same_for_the_same_theme() {
        // Applying a theme again replaces the stylesheet with an identical
        // one; nothing accumulates.
        for theme in Theme::ALL {
            let css = theme_css(theme, false, true);
            assert_eq!(css, theme_css(theme, false, true));
            assert!(css.len() < 4_096, "{theme:?} {}", css.len());
        }
    }
}
