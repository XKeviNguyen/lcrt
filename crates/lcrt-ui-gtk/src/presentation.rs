//! Pure presentation rules for the caption window, kept free of GTK types so
//! they can be tested directly.

use lcrt_core::{AppearancePreferences, AudioSourceDescriptor, AudioSourceKind, ProcessingMode};

/// Where the current caption session is in its lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionPhase {
    /// No session.
    Idle,
    /// Start was requested and the session is loading or connecting.
    Starting,
    /// The session is capturing and captioning.
    Running,
    /// Stop was requested and the session is finishing.
    Stopping,
}

/// Enabled state of the primary controls for one phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ControlState {
    /// Label of the Start/Stop button.
    pub(crate) start_label: &'static str,
    /// Whether the Start/Stop button accepts a click.
    pub(crate) start_sensitive: bool,
}

impl SessionPhase {
    /// Controls for this phase. Transitional phases ignore further clicks so
    /// each Start and Stop takes effect exactly once.
    pub(crate) fn controls(self, can_start: bool) -> ControlState {
        match self {
            Self::Idle => ControlState {
                start_label: "Start",
                start_sensitive: can_start,
            },
            Self::Starting | Self::Running => ControlState {
                start_label: "Stop",
                start_sensitive: true,
            },
            Self::Stopping => ControlState {
                start_label: "Stop",
                start_sensitive: false,
            },
        }
    }
}

/// Which language choice the control row shows for a mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LanguageControl {
    /// Offline language follows the chosen model; nothing to choose here.
    None,
    /// Spoken-language hint for online transcription (Auto or a language).
    Spoken,
    /// Output language for translation; the source is detected automatically.
    Target,
}

pub(crate) fn language_control(mode: ProcessingMode) -> LanguageControl {
    match mode {
        ProcessingMode::OfflineCaptions => LanguageControl::None,
        ProcessingMode::OnlineCaptions => LanguageControl::Spoken,
        ProcessingMode::Translation => LanguageControl::Target,
    }
}

/// Status shown while a session is actively producing captions.
pub(crate) fn active_status(mode: ProcessingMode) -> &'static str {
    match mode {
        ProcessingMode::Translation => "Translating…",
        ProcessingMode::OfflineCaptions | ProcessingMode::OnlineCaptions => "Listening…",
    }
}

/// Short inline notice about where audio is processed.
pub(crate) fn privacy_notice(mode: ProcessingMode) -> &'static str {
    if mode.streams_audio_online() {
        "Audio is streamed to OpenAI for processing. API charges may apply to your OpenAI account."
    } else {
        "Audio is processed on this device."
    }
}

pub(crate) fn source_label(source: &AudioSourceDescriptor) -> String {
    let kind = match source.kind() {
        AudioSourceKind::SystemOutput => "System audio",
        AudioSourceKind::Microphone => "Microphone",
    };
    format!("{kind} — {}", source.name())
}

/// The source to select initially: the remembered one when still present,
/// otherwise system audio, which is LCRT's primary input.
pub(crate) fn preferred_source_index(
    sources: &[AudioSourceDescriptor],
    remembered: Option<&str>,
) -> Option<usize> {
    remembered
        .and_then(|id| sources.iter().position(|source| source.id() == id))
        .or_else(|| {
            sources
                .iter()
                .position(|source| source.kind() == AudioSourceKind::SystemOutput)
        })
        .or(if sources.is_empty() { None } else { Some(0) })
}

/// A font family safe to place inside a CSS string, or `None` for the
/// system default.
pub(crate) fn css_font_family(family: Option<&str>) -> Option<String> {
    let family = family?.trim();
    let safe = !family.is_empty()
        && family.chars().all(|character| {
            !matches!(character, '"' | '\'' | '\\' | ';' | '{' | '}' | '<' | '>')
                && !character.is_control()
        });
    safe.then(|| family.to_owned())
}

/// Stylesheet for the caption surface. Fallback families always follow the
/// chosen one so Japanese and Vietnamese glyphs keep rendering.
pub(crate) fn caption_css(appearance: &AppearancePreferences) -> String {
    let family = css_font_family(appearance.font_family.as_deref())
        .map(|family| format!("font-family: \"{family}\", sans-serif;"))
        .unwrap_or_default();
    let background = appearance.background_color;
    format!(
        "window.caption-overlay, window.caption-overlay > contents, \
         window.caption-overlay toolbarview {{ background-color: transparent; }}\n\
         window.caption-overlay headerbar {{ \
             background-color: rgba({r}, {g}, {b}, {opacity:.3}); box-shadow: none; }}\n\
         .caption-panel {{ background-color: rgba({r}, {g}, {b}, {opacity:.3}); \
             border-radius: 16px; }}\n\
         textview.caption-text, textview.caption-text text {{ background-color: transparent; \
             color: {text}; {family} font-size: {size:.1}pt; }}\n\
         textview.caption-original, textview.caption-original text {{ \
             background-color: transparent; color: {text}; opacity: 0.8; {family} \
             font-size: {original_size:.1}pt; }}\n\
         .caption-notice {{ font-size: smaller; }}\n\
         .error {{ color: @error_color; padding: 6px; }}",
        r = background.red,
        g = background.green,
        b = background.blue,
        opacity = appearance.background_opacity,
        text = appearance.text_color.to_hex(),
        size = appearance.font_size_points,
        original_size = (appearance.font_size_points * 0.62).max(12.0),
    )
}

#[cfg(test)]
mod tests {
    use lcrt_core::{
        AppearancePreferences, AudioSourceDescriptor, AudioSourceKind, ProcessingMode, Rgb,
    };

    use super::{
        LanguageControl, SessionPhase, caption_css, css_font_family, language_control,
        preferred_source_index, privacy_notice,
    };

    fn sources() -> Vec<AudioSourceDescriptor> {
        vec![
            AudioSourceDescriptor::new("mic", "Built-in", AudioSourceKind::Microphone),
            AudioSourceDescriptor::new("monitor", "Built-in", AudioSourceKind::SystemOutput),
        ]
    }

    #[test]
    fn system_audio_is_the_default_source_unless_one_was_remembered() {
        assert_eq!(preferred_source_index(&sources(), None), Some(1));
        assert_eq!(preferred_source_index(&sources(), Some("mic")), Some(0));
        assert_eq!(
            preferred_source_index(&sources(), Some("unplugged")),
            Some(1)
        );
        assert_eq!(preferred_source_index(&[], None), None);
    }

    #[test]
    fn transitional_phases_accept_each_click_exactly_once() {
        assert!(SessionPhase::Idle.controls(true).start_sensitive);
        assert!(!SessionPhase::Idle.controls(false).start_sensitive);
        assert_eq!(SessionPhase::Starting.controls(true).start_label, "Stop");
        assert!(!SessionPhase::Stopping.controls(true).start_sensitive);
    }

    #[test]
    fn each_mode_shows_the_right_language_choice_and_privacy_notice() {
        assert_eq!(
            language_control(ProcessingMode::OfflineCaptions),
            LanguageControl::None
        );
        assert_eq!(
            language_control(ProcessingMode::OnlineCaptions),
            LanguageControl::Spoken
        );
        assert_eq!(
            language_control(ProcessingMode::Translation),
            LanguageControl::Target
        );
        assert_eq!(
            privacy_notice(ProcessingMode::OfflineCaptions),
            "Audio is processed on this device."
        );
        assert!(privacy_notice(ProcessingMode::Translation).contains("streamed to OpenAI"));
    }

    #[test]
    fn font_families_are_sanitized_before_reaching_css() {
        assert_eq!(
            css_font_family(Some("Noto Sans CJK JP")).as_deref(),
            Some("Noto Sans CJK JP")
        );
        assert_eq!(css_font_family(Some("Evil\"; } * { color: red")), None);
        assert_eq!(css_font_family(None), None);
    }

    #[test]
    fn css_reflects_colors_opacity_size_and_keeps_a_fallback_family() {
        let css = caption_css(&AppearancePreferences {
            font_family: Some("Ubuntu".to_owned()),
            font_size_points: 40.0,
            text_color: Rgb::new(255, 255, 0),
            background_color: Rgb::new(0, 0, 0),
            background_opacity: 0.0,
            ..AppearancePreferences::default()
        });
        assert!(css.contains("color: #ffff00"));
        assert!(css.contains("rgba(0, 0, 0, 0.000)"));
        assert!(css.contains("font-size: 40.0pt"));
        assert!(css.contains("font-family: \"Ubuntu\", sans-serif;"));
    }
}
