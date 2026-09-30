//! Pure presentation rules for the caption window, kept free of GTK types so
//! they can be tested directly.

use lcrt_core::{
    AppearancePreferences, AudioSourceDescriptor, AudioSourceKind, CaptionLane, Language,
    ProcessingMode, SessionOptions,
};

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

/// The badges of the three caption rows. A row without a badge is hidden,
/// except the first: captions that are not translations use it unlabeled.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LaneLayout {
    /// Badge of the original-speech row, shown above the translations.
    pub(crate) source: Option<String>,
    /// Badge of the first row of captions.
    pub(crate) first: Option<String>,
    /// Badge of the second translation's row.
    pub(crate) second: Option<String>,
}

/// The short uppercase code shown in a language badge.
pub(crate) fn language_badge(language: Language) -> String {
    language.code().to_uppercase()
}

/// Rows for a session: the source when shown, then each target, always in
/// that order. The source badge names the spoken language when the user
/// chose one, and is a neutral "SRC" while it is detected automatically.
pub(crate) fn lane_layout(options: &SessionOptions) -> LaneLayout {
    let mut layout = LaneLayout::default();
    for lane in options.translation_lanes() {
        match lane {
            CaptionLane::Source(spoken) => {
                layout.source = Some(
                    spoken
                        .language()
                        .map_or_else(|| "SRC".to_owned(), language_badge),
                );
            }
            CaptionLane::Target(target) if layout.first.is_none() => {
                layout.first = Some(language_badge(target));
            }
            CaptionLane::Target(target) => layout.second = Some(language_badge(target)),
        }
    }
    layout
}

/// Status shown while a session is actively producing captions.
pub(crate) fn active_status(mode: ProcessingMode) -> &'static str {
    match mode {
        ProcessingMode::Translation => "Translating…",
        ProcessingMode::OfflineCaptions | ProcessingMode::OnlineCaptions => "Listening…",
    }
}

/// Short inline notice about where audio is processed.
pub(crate) fn privacy_notice(mode: ProcessingMode, translation_sessions: usize) -> &'static str {
    match mode {
        ProcessingMode::OfflineCaptions => "Audio is processed on this device.",
        ProcessingMode::Translation if translation_sessions > 1 => {
            "Audio is streamed to OpenAI in two translation sessions, one per target language. \
             API charges apply for each."
        }
        ProcessingMode::OnlineCaptions | ProcessingMode::Translation => {
            "Audio is streamed to OpenAI for processing. API charges may apply to your OpenAI account."
        }
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

/// How a labeled row's text fits `available` pixels of height, given the
/// height of one `line`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RowFit {
    /// One unwrapped line that follows the newest words, because there is
    /// no room for two lines.
    pub(crate) single_line: bool,
    /// Pixels kept empty above the text so that only whole lines show.
    pub(crate) top_gap: i32,
}

/// An unlabeled row (every mode but Translation) wraps and scrolls freely.
pub(crate) fn row_fit(labeled: bool, available: i32, line: i32) -> RowFit {
    if !labeled || line <= 0 {
        return RowFit {
            single_line: false,
            top_gap: 0,
        };
    }
    RowFit {
        single_line: available < 2 * line,
        top_gap: if available >= line {
            available % line
        } else {
            0
        },
    }
}

/// Stylesheet for the caption surface. Fallback families always follow the
/// chosen one so Japanese and Vietnamese glyphs keep rendering.
pub(crate) fn caption_css(appearance: &AppearancePreferences) -> String {
    let family = css_font_family(appearance.font_family.as_deref())
        .map(|family| format!("font-family: \"{family}\", sans-serif;"))
        .unwrap_or_default();
    let background = appearance.background_color;
    // The badge sits level with the first line of its row's text: half the
    // difference between the two line heights (points to pixels is 4/3, and
    // a line is about 1.3 times its font size).
    let badge_size = (appearance.font_size_points * 0.42).clamp(10.0, 20.0);
    let badge_offset =
        ((appearance.font_size_points - badge_size) * 4.0 / 3.0 * 1.3 / 2.0 - 2.0).max(0.0);
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
             background-color: transparent; color: {text}; opacity: 0.82; {family} \
             font-size: {size:.1}pt; }}\n\
         .lane-badge {{ color: {text}; background-color: alpha({text}, 0.16); \
             border-radius: 9px; padding: 2px 9px; font-weight: 700; \
             font-size: {badge_size:.1}pt; letter-spacing: 1px; min-width: 2.3em; \
             margin-top: {badge_offset:.0}px; }}\n\
         scrolledwindow.caption-lane undershoot, scrolledwindow.caption-lane overshoot, \
         scrolledwindow.lane-badge-holder undershoot, \
         scrolledwindow.lane-badge-holder overshoot {{ \
             background: none; box-shadow: none; }}\n\
         .caption-notice {{ font-size: smaller; }}\n\
         .error {{ color: @error_color; padding: 6px; }}",
        r = background.red,
        g = background.green,
        b = background.blue,
        opacity = appearance.background_opacity,
        text = appearance.text_color.to_hex(),
        size = appearance.font_size_points,
        badge_size = badge_size,
        badge_offset = badge_offset,
    )
}

#[cfg(test)]
mod tests {
    use lcrt_core::{
        AppearancePreferences, AudioSourceDescriptor, AudioSourceKind, ProcessingMode, Rgb,
    };

    use super::{
        LaneLayout, LanguageControl, SessionPhase, caption_css, css_font_family, lane_layout,
        language_control, preferred_source_index, privacy_notice,
    };
    use lcrt_core::{Language, LanguageSelection, SessionOptions, TranslationTargets};

    fn translation(
        show_original: bool,
        spoken: LanguageSelection,
        first: Language,
        second: Option<Language>,
    ) -> SessionOptions {
        SessionOptions {
            mode: ProcessingMode::Translation,
            source_id: "monitor".to_owned(),
            spoken_language: spoken,
            translation_targets: TranslationTargets::resolve(
                Some(first),
                second,
                SessionOptions::shown_source(show_original, spoken),
            ),
            show_original,
        }
    }

    fn badges(layout: &LaneLayout) -> [Option<&str>; 3] {
        [
            layout.source.as_deref(),
            layout.first.as_deref(),
            layout.second.as_deref(),
        ]
    }

    #[test]
    fn translation_rows_are_source_then_targets_with_uppercase_badges() {
        use Language::{English, Japanese, Vietnamese};
        let japanese = LanguageSelection::Language(Japanese);
        let layout = lane_layout(&translation(true, japanese, English, Some(Vietnamese)));
        assert_eq!(badges(&layout), [Some("JA"), Some("EN"), Some("VI")]);

        let layout = lane_layout(&translation(
            false,
            LanguageSelection::Language(English),
            Japanese,
            None,
        ));
        assert_eq!(badges(&layout), [None, Some("JA"), None]);

        // A detected source has no language to name.
        let layout = lane_layout(&translation(true, LanguageSelection::Auto, English, None));
        assert_eq!(badges(&layout), [Some("SRC"), Some("EN"), None]);
    }

    #[test]
    fn a_labeled_row_shows_whole_lines_and_one_line_when_short() {
        use super::{RowFit, row_fit};
        // Room for one line: a single line, with the spare pixels above it.
        assert_eq!(
            row_fit(true, 60, 56),
            RowFit {
                single_line: true,
                top_gap: 4
            }
        );
        // Just under two lines is still one line.
        assert_eq!(
            row_fit(true, 108, 56),
            RowFit {
                single_line: true,
                top_gap: 52
            }
        );
        // Two lines and more wrap, never showing part of a line.
        assert_eq!(
            row_fit(true, 112, 56),
            RowFit {
                single_line: false,
                top_gap: 0
            }
        );
        assert_eq!(
            row_fit(true, 155, 56),
            RowFit {
                single_line: false,
                top_gap: 43
            }
        );
        // Shorter than a line: the row is clipped, with nothing to align.
        for available in [40, 0] {
            assert_eq!(
                row_fit(true, available, 56),
                RowFit {
                    single_line: true,
                    top_gap: 0
                }
            );
        }
        // An unlabeled row is left alone.
        assert_eq!(
            row_fit(false, 60, 56),
            RowFit {
                single_line: false,
                top_gap: 0
            }
        );
    }

    #[test]
    fn other_modes_have_one_unlabeled_row() {
        for mode in [
            ProcessingMode::OfflineCaptions,
            ProcessingMode::OnlineCaptions,
        ] {
            let mut options = translation(true, LanguageSelection::Auto, Language::English, None);
            options.mode = mode;
            let layout = lane_layout(&options);
            assert_eq!(badges(&layout), [None, None, None]);
        }
    }

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
            privacy_notice(ProcessingMode::OfflineCaptions, 0),
            "Audio is processed on this device."
        );
        assert!(privacy_notice(ProcessingMode::Translation, 1).contains("streamed to OpenAI"));
        // Two targets are two paid sessions, and the notice says so.
        assert!(
            privacy_notice(ProcessingMode::Translation, 2).contains("two translation sessions")
        );
        assert!(!privacy_notice(ProcessingMode::OnlineCaptions, 2).contains("two"));
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
        // The badge follows the text color and stays readable at any size.
        assert!(css.contains(".lane-badge { color: #ffff00;"));
        assert!(css.contains("font-size: 16.8pt"));
    }
}
