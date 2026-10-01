//! Persisted, non-secret user preferences.
//!
//! Credentials are deliberately absent from every type here: the API key lives
//! only in the OS secret store, the process environment, or session memory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CaptionLane, Language, LanguageSelection, ProcessingMode, TranslationTargets};

/// Current on-disk preferences schema version.
pub const PREFERENCES_VERSION: u32 = 1;

/// Supported caption font sizes, in points.
pub const FONT_SIZE_RANGE: (f64, f64) = (16.0, 64.0);
/// Supported caption surface widths, in logical pixels.
pub const WIDTH_RANGE: (i32, i32) = (320, 2_560);
/// Supported caption surface heights, in logical pixels.
pub const HEIGHT_RANGE: (i32, i32) = (160, 1_440);

/// An opaque sRGB color.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Rgb {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl Rgb {
    /// Creates a color from 8-bit channels.
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    /// CSS hex notation, such as `#ffffff`.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
    }
}

/// All persisted preferences.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// Schema version that wrote this value.
    pub version: u32,
    /// Session defaults.
    pub general: GeneralPreferences,
    /// Caption presentation.
    pub appearance: AppearancePreferences,
    /// Vocabulary explanations.
    pub vocabulary: VocabularyPreferences,
}

impl Preferences {
    /// Returns these preferences with every value inside its supported range.
    pub fn normalized(mut self) -> Self {
        self.version = PREFERENCES_VERSION;
        self.appearance = self.appearance.normalized();
        self.general.custom_model = self
            .general
            .custom_model
            .filter(|path| !path.as_os_str().is_empty());
        self.general.default_source_id = self
            .general
            .default_source_id
            .filter(|id| !id.trim().is_empty());
        // A hand-edited or older file may hold targets that repeat each
        // other or the spoken language, or hide every lane.
        let (first, second) = (
            self.general.translation_target,
            self.general.second_translation_target,
        );
        self.general.set_translation_targets(Some(first), second);
        self
    }
}

/// Defaults for starting a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralPreferences {
    /// Mode selected when the application starts.
    pub default_mode: ProcessingMode,
    /// Stable identifier of the preferred audio source, when one was chosen.
    pub default_source_id: Option<String>,
    /// A Whisper model file the user chose in place of the built-in one.
    /// Offline Captions use the built-in multilingual model when this is
    /// `None`. (Earlier versions stored a required `model_path`; it is not
    /// read, so those users move to the built-in model.)
    pub custom_model: Option<PathBuf>,
    /// Spoken-language hint for transcription.
    pub spoken_language: LanguageSelection,
    /// First output language for Translation.
    pub translation_target: Language,
    /// Optional second output language for Translation. It opens a second
    /// translation session.
    pub second_translation_target: Option<Language>,
    /// Whether Translation shows the original speech. Hiding it changes
    /// only what is shown: the source is still transcribed.
    pub show_original: bool,
    /// Translation targets whose lane is hidden. Hiding a lane is not
    /// pausing it: its session keeps translating.
    pub hidden_targets: Vec<Language>,
}

impl GeneralPreferences {
    /// The stored translation targets.
    pub fn translation_targets(&self) -> TranslationTargets {
        TranslationTargets::resolve(
            Some(self.translation_target),
            self.second_translation_target,
            self.spoken_language.language(),
        )
    }

    /// Stores `first` and `second` as chosen by the user, corrected to a
    /// valid combination (see [`TranslationTargets::resolve`]).
    pub fn set_translation_targets(&mut self, first: Option<Language>, second: Option<Language>) {
        let targets = TranslationTargets::resolve(first, second, self.spoken_language.language());
        self.translation_target = targets.first();
        self.second_translation_target = targets.second();
        // Only current targets can be hidden, and at least one lane stays
        // visible, so the captions never disappear altogether.
        self.hidden_targets
            .retain(|language| targets.contains(*language));
        self.hidden_targets.dedup();
        if !self.show_original
            && targets
                .iter()
                .all(|target| self.hidden_targets.contains(&target))
        {
            self.hidden_targets.clear();
        }
    }

    /// Whether `lane` is shown in Translation.
    pub fn lane_visible(&self, lane: CaptionLane) -> bool {
        match lane {
            CaptionLane::Source => self.show_original,
            CaptionLane::Target(language) => !self.hidden_targets.contains(&language),
        }
    }

    /// Shows or hides `lane`. Returns false, changing nothing, when that
    /// would hide the last visible lane or `lane` is not a current lane.
    pub fn set_lane_visible(&mut self, lane: CaptionLane, visible: bool) -> bool {
        let targets = self.translation_targets();
        let visible_lanes = usize::from(self.show_original)
            + targets
                .iter()
                .filter(|target| !self.hidden_targets.contains(target))
                .count();
        if !visible && self.lane_visible(lane) && visible_lanes == 1 {
            return false;
        }
        match lane {
            CaptionLane::Source => self.show_original = visible,
            CaptionLane::Target(language) if targets.contains(language) => {
                self.hidden_targets.retain(|hidden| *hidden != language);
                if !visible {
                    self.hidden_targets.push(language);
                }
            }
            CaptionLane::Target(_) => return false,
        }
        true
    }
}

impl Default for GeneralPreferences {
    fn default() -> Self {
        Self {
            default_mode: ProcessingMode::OfflineCaptions,
            default_source_id: None,
            custom_model: None,
            spoken_language: LanguageSelection::Auto,
            translation_target: Language::English,
            second_translation_target: None,
            show_original: true,
            hidden_targets: Vec::new(),
        }
    }
}

/// Caption surface presentation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearancePreferences {
    /// Font family, or `None` for the system default.
    pub font_family: Option<String>,
    /// Caption font size in points.
    pub font_size_points: f64,
    /// Caption text color.
    pub text_color: Rgb,
    /// Caption background color.
    pub background_color: Rgb,
    /// Background opacity from 0 (transparent) to 1 (opaque).
    pub background_opacity: f64,
    /// Caption surface width in logical pixels.
    pub width: i32,
    /// Caption surface height in logical pixels.
    pub height: i32,
}

impl Default for AppearancePreferences {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size_points: 32.0,
            text_color: Rgb::new(0xff, 0xff, 0xff),
            background_color: Rgb::new(0x1e, 0x1e, 0x1e),
            background_opacity: 0.86,
            width: 760,
            height: 320,
        }
    }
}

impl AppearancePreferences {
    /// Returns these values clamped to their supported ranges; non-finite
    /// numbers fall back to the defaults.
    pub fn normalized(self) -> Self {
        let defaults = Self::default();
        let finite_or = |value: f64, fallback: f64| {
            if value.is_finite() { value } else { fallback }
        };
        Self {
            font_family: self
                .font_family
                .map(|family| family.trim().to_owned())
                .filter(|family| !family.is_empty() && family.len() <= 256),
            font_size_points: finite_or(self.font_size_points, defaults.font_size_points)
                .clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1),
            text_color: self.text_color,
            background_color: self.background_color,
            background_opacity: finite_or(self.background_opacity, defaults.background_opacity)
                .clamp(0.0, 1.0),
            width: self.width.clamp(WIDTH_RANGE.0, WIDTH_RANGE.1),
            height: self.height.clamp(HEIGHT_RANGE.0, HEIGHT_RANGE.1),
        }
    }
}

/// Vocabulary explanation behavior.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VocabularyPreferences {
    /// Whether selecting caption text may request an online explanation.
    pub enabled: bool,
    /// Language the explanation is written in.
    pub explanation_language: Language,
}

impl Default for VocabularyPreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            explanation_language: Language::English,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppearancePreferences, PREFERENCES_VERSION, Preferences, Rgb};
    use crate::{CaptionLane, Language, LanguageSelection};

    #[test]
    fn stored_translation_targets_are_corrected_when_loaded() {
        let mut preferences = Preferences::default();
        // Hiding the source doesn't allow a target in its language.
        preferences.general.show_original = false;
        preferences.general.spoken_language = LanguageSelection::Language(Language::Japanese);
        preferences.general.translation_target = Language::Japanese;
        preferences.general.second_translation_target = Some(Language::Vietnamese);
        let general = preferences.normalized().general;
        // The first target repeated the spoken language, so the second moved up.
        assert_eq!(general.translation_target, Language::Vietnamese);
        assert_eq!(general.second_translation_target, None);
    }

    #[test]
    fn choosing_targets_keeps_valid_pairs_and_fixes_invalid_ones() {
        let mut general = Preferences::default().general;
        general.show_original = false;
        general.set_translation_targets(Some(Language::English), Some(Language::Vietnamese));
        assert_eq!(general.translation_target, Language::English);
        assert_eq!(
            general.second_translation_target,
            Some(Language::Vietnamese)
        );
        general.set_translation_targets(Some(Language::English), Some(Language::English));
        assert_eq!(general.second_translation_target, None);
        general.set_translation_targets(None, None);
        assert_eq!(general.translation_target, Language::English);
    }

    #[test]
    fn hiding_a_lane_is_remembered_but_never_hides_the_last_one() {
        let mut general = Preferences::default().general;
        general.set_translation_targets(Some(Language::English), Some(Language::Vietnamese));
        let english = CaptionLane::Target(Language::English);
        let vietnamese = CaptionLane::Target(Language::Vietnamese);
        assert!(general.set_lane_visible(english, false));
        assert!(general.set_lane_visible(CaptionLane::Source, false));
        assert!(!general.lane_visible(english));
        // Vietnamese is the last visible lane.
        assert!(!general.set_lane_visible(vietnamese, false));
        assert!(general.lane_visible(vietnamese));
        // Showing a lane again restores it.
        assert!(general.set_lane_visible(english, true));
        assert!(general.lane_visible(english));
        // A language that is not a target has no lane to hide.
        assert!(!general.set_lane_visible(CaptionLane::Target(Language::German), false));
    }

    #[test]
    fn a_removed_target_forgets_that_it_was_hidden() {
        let mut general = Preferences::default().general;
        general.set_translation_targets(Some(Language::English), Some(Language::Vietnamese));
        assert!(general.set_lane_visible(CaptionLane::Target(Language::Vietnamese), false));
        general.set_translation_targets(Some(Language::English), None);
        assert!(general.hidden_targets.is_empty());
        // A stored file that hides every lane shows the targets again.
        let mut preferences = Preferences::default();
        preferences.general.show_original = false;
        preferences.general.hidden_targets = vec![Language::English];
        assert!(preferences.normalized().general.hidden_targets.is_empty());
    }

    #[test]
    fn appearance_values_are_clamped_and_non_finite_values_reset() {
        let appearance = AppearancePreferences {
            font_family: Some("   ".to_owned()),
            font_size_points: f64::NAN,
            background_opacity: 4.0,
            width: 10,
            height: 99_999,
            ..AppearancePreferences::default()
        }
        .normalized();

        assert_eq!(appearance.font_family, None);
        assert_eq!(appearance.font_size_points, 32.0);
        assert_eq!(appearance.background_opacity, 1.0);
        assert_eq!(appearance.width, 320);
        assert_eq!(appearance.height, 1_440);
    }

    #[test]
    fn fully_transparent_background_is_allowed() {
        let appearance = AppearancePreferences {
            background_opacity: 0.0,
            ..AppearancePreferences::default()
        }
        .normalized();
        assert_eq!(appearance.background_opacity, 0.0);
    }

    #[test]
    fn unicode_font_family_names_survive_normalization() {
        let appearance = AppearancePreferences {
            font_family: Some(" Noto Sans CJK JP 日本語 ".to_owned()),
            ..AppearancePreferences::default()
        }
        .normalized();
        assert_eq!(
            appearance.font_family.as_deref(),
            Some("Noto Sans CJK JP 日本語")
        );
    }

    #[test]
    fn normalization_stamps_the_current_schema_version() {
        assert_eq!(
            Preferences::default().normalized().version,
            PREFERENCES_VERSION
        );
    }

    #[test]
    fn colors_render_as_css_hex() {
        assert_eq!(Rgb::new(0x1e, 0xff, 0x00).to_hex(), "#1eff00");
    }
}
