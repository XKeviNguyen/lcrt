//! Pure presentation rules for the caption window, kept free of GTK types so
//! they can be tested directly.

use lcrt_core::{
    AppearancePreferences, AudioSourceDescriptor, AudioSourceKind, CaptionLane, GeneralPreferences,
    Language, LanguageSelection, ProcessingMode, TargetStatus, TranslationTargets,
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

/// One caption row of a translation session, and its control chip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaneRow {
    /// The uppercase language code on its badge and chip.
    pub(crate) badge: String,
    /// Whether the row is shown. A hidden row keeps its text.
    pub(crate) visible: bool,
    /// What the target's session is doing; `None` for the source row and
    /// before a session reports.
    pub(crate) status: Option<TargetStatus>,
}

/// The caption rows. Outside Translation there is one unlabeled row.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LaneLayout {
    /// The original-speech row, above the translations.
    pub(crate) source: Option<LaneRow>,
    /// Target rows in lane order.
    pub(crate) targets: Vec<(Language, LaneRow)>,
}

/// The short uppercase code shown in a language badge.
pub(crate) fn language_badge(language: Language) -> String {
    language.code().to_uppercase()
}

/// Rows for a translation session: the source, then each target, always in
/// that order, each shown or hidden as `visibility` says. The source badge
/// names the spoken language when the user chose one, and is a neutral
/// "SRC" while it is detected automatically.
pub(crate) fn lane_layout(
    mode: ProcessingMode,
    spoken: LanguageSelection,
    targets: TranslationTargets,
    visibility: &GeneralPreferences,
    statuses: &[(Language, TargetStatus)],
) -> LaneLayout {
    if mode != ProcessingMode::Translation {
        return LaneLayout::default();
    }
    LaneLayout {
        source: Some(LaneRow {
            badge: spoken
                .language()
                .map_or_else(|| "SRC".to_owned(), language_badge),
            visible: visibility.lane_visible(CaptionLane::Source),
            status: None,
        }),
        targets: targets
            .iter()
            .map(|target| {
                let row = LaneRow {
                    badge: language_badge(target),
                    visible: visibility.lane_visible(CaptionLane::Target(target)),
                    status: statuses
                        .iter()
                        .find(|(language, _)| *language == target)
                        .map(|(_, status)| *status),
                };
                (target, row)
            })
            .collect(),
    }
}

/// What a target's row says while it has no live translation to show.
pub(crate) fn lane_status_text(status: Option<TargetStatus>) -> Option<&'static str> {
    match status? {
        TargetStatus::Connecting => Some("Connecting…"),
        TargetStatus::Reconnecting => Some("Reconnecting…"),
        TargetStatus::Paused => Some("Paused"),
        TargetStatus::Failed => Some("Stopped"),
        TargetStatus::Active => None,
    }
}

/// How many of `targets` have a session: those not paused or failed.
pub(crate) fn open_sessions(
    targets: TranslationTargets,
    statuses: &[(Language, TargetStatus)],
) -> usize {
    targets
        .iter()
        .filter(|target| {
            !statuses.iter().any(|(language, status)| {
                language == target && matches!(status, TargetStatus::Paused | TargetStatus::Failed)
            })
        })
        .count()
}

/// Adds the `newer` target statuses to `statuses`, one per language, and
/// keeps only those of `current` targets: a report from a removed target may
/// still arrive after it was removed.
pub(crate) fn merge_target_statuses(
    statuses: &mut Vec<(Language, TargetStatus)>,
    newer: &[(Language, TargetStatus)],
    current: Option<TranslationTargets>,
) {
    for (language, status) in newer {
        statuses.retain(|(target, _)| target != language);
        statuses.push((*language, *status));
    }
    statuses.retain(|(language, _)| current.is_none_or(|targets| targets.contains(*language)));
}

/// The overall status of a translation session, from its targets' own:
/// translating while any target is, so a target that is still connecting
/// or reconnecting shows that on its own row only.
pub(crate) fn translation_status(statuses: &[(Language, TargetStatus)]) -> Option<&'static str> {
    let any = |wanted: TargetStatus| statuses.iter().any(|(_, status)| *status == wanted);
    if statuses.is_empty() {
        None
    } else if any(TargetStatus::Active) {
        Some("Translating…")
    } else if any(TargetStatus::Reconnecting) {
        Some("Reconnecting…")
    } else if any(TargetStatus::Connecting) {
        Some("Connecting…")
    } else {
        Some("Paused")
    }
}

/// Status shown while a session is actively producing captions.
pub(crate) fn active_status(mode: ProcessingMode) -> &'static str {
    match mode {
        ProcessingMode::Translation => "Translating…",
        ProcessingMode::OfflineCaptions | ProcessingMode::OnlineCaptions => "Listening…",
    }
}

/// Short inline notice about where audio is processed. For Translation,
/// `translation_sessions` counts the targets that are not paused or failed:
/// each is a session that audio is sent to.
pub(crate) fn privacy_notice(mode: ProcessingMode, translation_sessions: usize) -> &'static str {
    match mode {
        ProcessingMode::OfflineCaptions => "Audio is processed on this device.",
        ProcessingMode::Translation if translation_sessions == 0 => {
            "Every translation language is paused: no audio is being sent."
        }
        ProcessingMode::Translation if translation_sessions > 1 => {
            "Audio is streamed to OpenAI for processing, in one session per translation \
             language. API charges apply for each."
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
         splitbutton.hidden-lane > button:first-child {{ opacity: 0.45; }}\n\
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
        LaneLayout, SessionPhase, caption_css, css_font_family, lane_layout, lane_status_text,
        merge_target_statuses, open_sessions, preferred_source_index, privacy_notice,
        translation_status,
    };
    use lcrt_core::{
        CaptionLane, GeneralPreferences, Language, LanguageSelection, TargetStatus,
        TranslationTargets,
    };

    /// Every row's badge, with `-` marking a hidden one.
    fn badges(layout: &LaneLayout) -> Vec<String> {
        layout
            .source
            .iter()
            .chain(layout.targets.iter().map(|(_, row)| row))
            .map(|row| {
                if row.visible {
                    row.badge.clone()
                } else {
                    format!("-{}", row.badge)
                }
            })
            .collect()
    }

    #[test]
    fn translation_rows_are_source_then_targets_with_uppercase_badges() {
        use Language::{English, Japanese, Vietnamese};
        let japanese = LanguageSelection::Language(Japanese);
        let targets = TranslationTargets::resolve(Some(English), Some(Vietnamese), None);
        let mut shown = GeneralPreferences::default();
        shown.set_translation_targets(Some(English), Some(Vietnamese));
        let layout = lane_layout(ProcessingMode::Translation, japanese, targets, &shown, &[]);
        assert_eq!(badges(&layout), ["JA", "EN", "VI"]);
        // Hidden rows stay in the layout, so showing them again is instant.
        let mut hidden = shown.clone();
        assert!(hidden.set_lane_visible(CaptionLane::Source, false));
        assert!(hidden.set_lane_visible(CaptionLane::Target(English), false));
        let layout = lane_layout(ProcessingMode::Translation, japanese, targets, &hidden, &[]);
        assert_eq!(badges(&layout), ["-JA", "-EN", "VI"]);
        // A detected source has no language to name.
        let layout = lane_layout(
            ProcessingMode::Translation,
            LanguageSelection::Auto,
            targets,
            &shown,
            &[],
        );
        assert_eq!(badges(&layout)[0], "SRC");
        // Other modes have one unlabeled row.
        let captions = lane_layout(
            ProcessingMode::OnlineCaptions,
            japanese,
            targets,
            &shown,
            &[],
        );
        assert_eq!(captions, LaneLayout::default());
    }

    #[test]
    fn each_target_row_carries_its_own_status() {
        use Language::{English, Vietnamese};
        let targets = TranslationTargets::resolve(Some(English), Some(Vietnamese), None);
        let statuses = [
            (English, TargetStatus::Active),
            (Vietnamese, TargetStatus::Connecting),
        ];
        let layout = lane_layout(
            ProcessingMode::Translation,
            LanguageSelection::Auto,
            targets,
            &GeneralPreferences::default(),
            &statuses,
        );
        assert_eq!(layout.targets[0].1.status, Some(TargetStatus::Active));
        assert_eq!(
            lane_status_text(layout.targets[1].1.status),
            Some("Connecting…")
        );
        assert_eq!(lane_status_text(Some(TargetStatus::Active)), None);
        assert_eq!(lane_status_text(Some(TargetStatus::Paused)), Some("Paused"));
    }

    #[test]
    fn paused_and_failed_targets_have_no_session() {
        use Language::{English, Vietnamese};
        let targets = TranslationTargets::resolve(Some(English), Some(Vietnamese), None);
        assert_eq!(open_sessions(targets, &[]), 2);
        assert_eq!(
            open_sessions(targets, &[(English, TargetStatus::Paused)]),
            1
        );
        let both = [
            (English, TargetStatus::Paused),
            (Vietnamese, TargetStatus::Failed),
        ];
        assert_eq!(open_sessions(targets, &both), 0);
    }

    #[test]
    fn a_removed_target_s_late_status_is_dropped() {
        use Language::{English, Vietnamese};
        use TargetStatus::{Active, Paused};
        let current = Some(TranslationTargets::resolve(Some(Vietnamese), None, None));
        let mut statuses = vec![(Vietnamese, Active)];
        // English was removed, but its last report arrives with Vietnamese's.
        merge_target_statuses(
            &mut statuses,
            &[(English, Active), (Vietnamese, Paused)],
            current,
        );
        assert_eq!(statuses, [(Vietnamese, Paused)]);
        // So the session reads as paused, not translating.
        assert_eq!(translation_status(&statuses), Some("Paused"));
    }

    #[test]
    fn the_session_is_translating_while_any_target_is() {
        use Language::{English, Vietnamese};
        use TargetStatus::{Active, Connecting, Failed, Paused, Reconnecting};
        assert_eq!(translation_status(&[]), None);
        // A target added live connects on its own row only.
        assert_eq!(
            translation_status(&[(English, Active), (Vietnamese, Connecting)]),
            Some("Translating…")
        );
        assert_eq!(
            translation_status(&[(English, Reconnecting), (Vietnamese, Paused)]),
            Some("Reconnecting…")
        );
        assert_eq!(
            translation_status(&[(English, Connecting)]),
            Some("Connecting…")
        );
        assert_eq!(
            translation_status(&[(English, Paused), (Vietnamese, Failed)]),
            Some("Paused")
        );
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
    fn each_mode_says_where_audio_is_processed() {
        assert_eq!(
            privacy_notice(ProcessingMode::OfflineCaptions, 0),
            "Audio is processed on this device."
        );
        assert!(
            privacy_notice(ProcessingMode::OnlineCaptions, 0)
                .starts_with("Audio is streamed to OpenAI for processing.")
        );
        assert!(privacy_notice(ProcessingMode::Translation, 1).contains("streamed to OpenAI"));
        // Two targets are two paid sessions, and the notice says so.
        assert!(
            privacy_notice(ProcessingMode::Translation, 2)
                .contains("one session per translation language")
        );
        assert!(!privacy_notice(ProcessingMode::OnlineCaptions, 2).contains("session per"));
        // With every target paused, nothing is sent and the notice says so.
        assert_eq!(
            privacy_notice(ProcessingMode::Translation, 0),
            "Every translation language is paused: no audio is being sent."
        );
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
        // A hidden lane's chip stays, muted, so one click restores it.
        assert!(css.contains("splitbutton.hidden-lane > button:first-child { opacity: 0.45; }"));
    }
}
