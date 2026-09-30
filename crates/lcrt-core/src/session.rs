//! Processing modes, languages, and caption-session identity.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How captured audio is turned into captions. Exactly one mode owns a session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingMode {
    /// Local Whisper transcription; audio never leaves the device.
    #[default]
    OfflineCaptions,
    /// Streaming transcription by the online service.
    OnlineCaptions,
    /// Streaming speech translation by the online service.
    Translation,
}

impl ProcessingMode {
    /// All modes in presentation order.
    pub const ALL: [Self; 3] = [
        Self::OfflineCaptions,
        Self::OnlineCaptions,
        Self::Translation,
    ];

    /// Whether this mode streams audio to the online service.
    pub fn streams_audio_online(self) -> bool {
        !matches!(self, Self::OfflineCaptions)
    }

    /// Short user-facing name.
    pub fn label(self) -> &'static str {
        match self {
            Self::OfflineCaptions => "Offline Captions",
            Self::OnlineCaptions => "Online Captions",
            Self::Translation => "Translation",
        }
    }
}

/// A language offered for speech hints, translation targets, and vocabulary.
///
/// Codes are ISO 639-1, the format the online services accept.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// English.
    English,
    /// Japanese.
    Japanese,
    /// Vietnamese.
    Vietnamese,
    /// Mandarin Chinese.
    Chinese,
    /// Korean.
    Korean,
    /// Spanish.
    Spanish,
    /// French.
    French,
    /// German.
    German,
}

impl Language {
    /// All languages in presentation order; English, Japanese, and Vietnamese
    /// are first-class and always listed first.
    pub const ALL: [Self; 8] = [
        Self::English,
        Self::Japanese,
        Self::Vietnamese,
        Self::Chinese,
        Self::Korean,
        Self::Spanish,
        Self::French,
        Self::German,
    ];

    /// ISO 639-1 code.
    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Japanese => "ja",
            Self::Vietnamese => "vi",
            Self::Chinese => "zh",
            Self::Korean => "ko",
            Self::Spanish => "es",
            Self::French => "fr",
            Self::German => "de",
        }
    }

    /// Parses an ISO 639-1 code such as `ja`.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|language| language.code().eq_ignore_ascii_case(code.trim()))
    }

    /// User-facing name, in English.
    pub fn label(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Japanese => "Japanese",
            Self::Vietnamese => "Vietnamese",
            Self::Chinese => "Chinese",
            Self::Korean => "Korean",
            Self::Spanish => "Spanish",
            Self::French => "French",
            Self::German => "German",
        }
    }
}

/// The spoken language: detected automatically or hinted explicitly.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageSelection {
    /// Let the engine detect the language; no hint is sent.
    #[default]
    Auto,
    /// Hint a specific language.
    Language(Language),
}

impl LanguageSelection {
    /// The hinted language, or `None` for automatic detection.
    pub fn language(self) -> Option<Language> {
        match self {
            Self::Auto => None,
            Self::Language(language) => Some(language),
        }
    }

    /// User-facing name.
    pub fn label(self) -> &'static str {
        self.language().map_or("Auto", Language::label)
    }
}

/// Most translation targets one session shows. Each target is its own
/// online translation session, so this also bounds cost.
pub const MAX_TRANSLATION_TARGETS: usize = 2;
/// Most caption lanes visible at once: the source plus every target.
pub const MAX_CAPTION_LANES: usize = 1 + MAX_TRANSLATION_TARGETS;

/// The target languages of a translation session.
///
/// There is always a first target, the second is optional, the two differ,
/// and neither repeats a source language that is shown in its own lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranslationTargets {
    first: Language,
    second: Option<Language>,
}

impl TranslationTargets {
    /// Builds valid targets from what the user chose, correcting invalid
    /// combinations instead of rejecting them:
    ///
    /// - a target equal to `shown_source` is dropped;
    /// - a repeated target is dropped;
    /// - a second target without a first becomes the first;
    /// - with no target left, the first language other than `shown_source`
    ///   is used, because a translation session needs one.
    pub fn resolve(
        first: Option<Language>,
        second: Option<Language>,
        shown_source: Option<Language>,
    ) -> Self {
        let mut chosen = [first, second]
            .into_iter()
            .flatten()
            .filter(|target| Some(*target) != shown_source);
        let first = chosen.next().unwrap_or_else(|| {
            Language::ALL
                .into_iter()
                .find(|language| Some(*language) != shown_source)
                .unwrap_or(Language::English)
        });
        let second = chosen.find(|target| *target != first);
        Self { first, second }
    }

    /// A single target.
    pub fn single(target: Language) -> Self {
        Self {
            first: target,
            second: None,
        }
    }

    /// The first target; it always exists.
    pub fn first(self) -> Language {
        self.first
    }

    /// The optional second target.
    pub fn second(self) -> Option<Language> {
        self.second
    }

    /// The targets in lane order.
    pub fn iter(self) -> impl Iterator<Item = Language> {
        std::iter::once(self.first).chain(self.second)
    }
}

impl Default for TranslationTargets {
    fn default() -> Self {
        Self::single(Language::English)
    }
}

/// One visible caption row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptionLane {
    /// The original speech, in the spoken language.
    Source(LanguageSelection),
    /// A translation into this language.
    Target(Language),
}

/// Monotonic identity of one caption session.
///
/// Events stamped with an older generation belong to a session that has
/// ended and must never change what the current session presents.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionGeneration(u64);

impl SessionGeneration {
    /// The generation that follows this one.
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

impl fmt::Display for SessionGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Everything needed to start one caption session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    /// The processing backend that owns the session.
    pub mode: ProcessingMode,
    /// Stable platform identifier of the audio source.
    pub source_id: String,
    /// The spoken language. Online Captions send it as a hint. Translation
    /// detects the language itself and uses this only to label the source
    /// lane and to keep targets from repeating it.
    pub spoken_language: LanguageSelection,
    /// Output languages for [`ProcessingMode::Translation`].
    pub translation_targets: TranslationTargets,
    /// Whether Translation also transcribes and shows the original speech.
    pub show_original: bool,
}

impl SessionOptions {
    /// The source language when Translation shows it in its own lane and the
    /// user named it; targets must not repeat it.
    pub fn shown_source(show_original: bool, spoken: LanguageSelection) -> Option<Language> {
        spoken.language().filter(|_| show_original)
    }

    /// The caption lanes of a translation session, in their fixed order:
    /// the source when shown, then each target. Other modes have one
    /// unlabeled lane and return nothing here.
    pub fn translation_lanes(&self) -> Vec<CaptionLane> {
        if self.mode != ProcessingMode::Translation {
            return Vec::new();
        }
        self.show_original
            .then_some(CaptionLane::Source(self.spoken_language))
            .into_iter()
            .chain(self.translation_targets.iter().map(CaptionLane::Target))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CaptionLane, Language, LanguageSelection, MAX_CAPTION_LANES, ProcessingMode,
        SessionGeneration, SessionOptions, TranslationTargets,
    };

    fn targets(
        first: Option<Language>,
        second: Option<Language>,
        source: Option<Language>,
    ) -> Vec<Language> {
        TranslationTargets::resolve(first, second, source)
            .iter()
            .collect()
    }

    #[test]
    fn valid_targets_are_kept_in_order() {
        use Language::{English, Japanese, Vietnamese};
        assert_eq!(
            targets(Some(English), Some(Vietnamese), Some(Japanese)),
            [English, Vietnamese]
        );
        assert_eq!(targets(Some(Japanese), None, Some(English)), [Japanese]);
        assert_eq!(
            targets(Some(English), Some(Japanese), None),
            [English, Japanese]
        );
    }

    #[test]
    fn duplicate_targets_collapse_to_one() {
        use Language::{English, Vietnamese};
        assert_eq!(targets(Some(English), Some(English), None), [English]);
        assert_eq!(
            targets(Some(Vietnamese), Some(Vietnamese), Some(English)),
            [Vietnamese]
        );
    }

    #[test]
    fn targets_never_repeat_the_shown_source_language() {
        use Language::{English, Japanese, Vietnamese};
        // The repeated target is dropped and the other one moves up.
        assert_eq!(
            targets(Some(Japanese), Some(English), Some(Japanese)),
            [English]
        );
        assert_eq!(
            targets(Some(English), Some(Japanese), Some(Japanese)),
            [English]
        );
        // A hidden source restricts nothing.
        let hidden = SessionOptions::shown_source(false, LanguageSelection::Language(Japanese));
        assert_eq!(
            targets(Some(Japanese), Some(Vietnamese), hidden),
            [Japanese, Vietnamese]
        );
        // Neither does an automatically detected one.
        let auto = SessionOptions::shown_source(true, LanguageSelection::Auto);
        assert_eq!(targets(Some(Japanese), None, auto), [Japanese]);
    }

    #[test]
    fn a_second_target_alone_becomes_the_first() {
        use Language::Vietnamese;
        assert_eq!(targets(None, Some(Vietnamese), None), [Vietnamese]);
    }

    #[test]
    fn with_every_target_off_one_is_chosen_that_differs_from_the_source() {
        use Language::{English, Japanese};
        assert_eq!(targets(None, None, None), [English]);
        assert_eq!(targets(None, None, Some(English)), [Japanese]);
        assert_eq!(
            targets(Some(English), Some(English), Some(English)),
            [Japanese]
        );
    }

    fn options(
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

    #[test]
    fn lanes_keep_a_fixed_order_and_never_exceed_three() {
        use Language::{English, Japanese, Vietnamese};
        let japanese = LanguageSelection::Language(Japanese);
        assert_eq!(
            options(true, japanese, English, Some(Vietnamese)).translation_lanes(),
            [
                CaptionLane::Source(japanese),
                CaptionLane::Target(English),
                CaptionLane::Target(Vietnamese),
            ]
        );
        assert_eq!(
            options(false, LanguageSelection::Language(English), Japanese, None)
                .translation_lanes(),
            [CaptionLane::Target(Japanese)]
        );
        assert_eq!(
            options(true, LanguageSelection::Auto, English, None).translation_lanes(),
            [
                CaptionLane::Source(LanguageSelection::Auto),
                CaptionLane::Target(English)
            ]
        );
        for show in [false, true] {
            for second in [None, Some(Vietnamese), Some(English)] {
                let lanes = options(show, japanese, English, second).translation_lanes();
                assert!((1..=MAX_CAPTION_LANES).contains(&lanes.len()));
            }
        }
    }

    #[test]
    fn other_modes_have_no_labeled_lanes() {
        let mut offline = options(true, LanguageSelection::Auto, Language::English, None);
        offline.mode = ProcessingMode::OnlineCaptions;
        assert!(offline.translation_lanes().is_empty());
    }

    #[test]
    fn only_online_modes_stream_audio() {
        assert!(!ProcessingMode::OfflineCaptions.streams_audio_online());
        assert!(ProcessingMode::OnlineCaptions.streams_audio_online());
        assert!(ProcessingMode::Translation.streams_audio_online());
    }

    #[test]
    fn first_class_languages_lead_with_iso_codes() {
        assert_eq!(
            Language::ALL[..3]
                .iter()
                .map(|l| l.code())
                .collect::<Vec<_>>(),
            ["en", "ja", "vi"]
        );
    }

    #[test]
    fn auto_selection_sends_no_language() {
        assert_eq!(LanguageSelection::Auto.language(), None);
        assert_eq!(
            LanguageSelection::Language(Language::Vietnamese).language(),
            Some(Language::Vietnamese)
        );
        assert_eq!(LanguageSelection::Auto.label(), "Auto");
    }

    #[test]
    fn generations_advance_monotonically() {
        let first = SessionGeneration::default();
        assert!(first.next() > first);
        assert_ne!(first.next(), first.next().next());
    }
}
