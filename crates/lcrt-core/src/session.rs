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
/// and neither is the spoken language when the user named it: translating
/// speech into its own language is never useful.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranslationTargets {
    first: Language,
    second: Option<Language>,
}

impl TranslationTargets {
    /// Builds valid targets from what the user chose, correcting invalid
    /// combinations instead of rejecting them:
    ///
    /// - a target equal to `source` is dropped;
    /// - a repeated target is dropped;
    /// - a second target without a first becomes the first;
    /// - with no target left, the first language other than `source` is
    ///   used, because a translation session needs one.
    pub fn resolve(
        first: Option<Language>,
        second: Option<Language>,
        source: Option<Language>,
    ) -> Self {
        let mut chosen = [first, second]
            .into_iter()
            .flatten()
            .filter(|target| Some(*target) != source);
        let first = chosen.next().unwrap_or_else(|| {
            Language::ALL
                .into_iter()
                .find(|language| Some(*language) != source)
                .unwrap_or(Language::English)
        });
        let second = chosen.find(|target| *target != first);
        Self { first, second }
    }

    /// These targets with `language` added last, or `None` when there are
    /// already [`MAX_TRANSLATION_TARGETS`], it already is a target, or it
    /// is the spoken language.
    pub fn with_added(self, language: Language, source: Option<Language>) -> Option<Self> {
        (self.second.is_none() && language != self.first && Some(language) != source).then_some(
            Self {
                first: self.first,
                second: Some(language),
            },
        )
    }

    /// These targets without `language`, or `None` when it is not a target
    /// or is the only one: a translation session always has a target.
    pub fn without(self, language: Language) -> Option<Self> {
        let second = self.second?;
        if language == self.first {
            Some(Self {
                first: second,
                second: None,
            })
        } else {
            (language == second).then_some(Self {
                first: self.first,
                second: None,
            })
        }
    }

    /// Whether `language` is one of these targets.
    pub fn contains(self, language: Language) -> bool {
        self.iter().any(|target| target == language)
    }

    /// The languages [`Self::with_added`] would accept, in presentation
    /// order; none once the targets are full.
    pub fn addable(self, source: Option<Language>) -> Vec<Language> {
        Language::ALL
            .into_iter()
            .filter(|language| self.with_added(*language, source).is_some())
            .collect()
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

/// One caption row of a translation session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptionLane {
    /// The original speech, in the spoken language.
    Source,
    /// A translation into this language.
    Target(Language),
}

/// What one translation target's session is doing, as shown on its lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetStatus {
    /// Opening its session; no translation yet.
    Connecting,
    /// Translating.
    Active,
    /// Recovering a lost connection.
    Reconnecting,
    /// Paused by the user: its session is closed, its text kept.
    Paused,
    /// Its session failed; the other targets keep translating.
    Failed,
}

/// A change to the targets of a running translation session. Only the
/// named target is affected: audio capture and the other targets go on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetChange {
    /// Start translating into another language.
    Add(Language),
    /// Stop translating into a language and drop its lane.
    Remove(Language),
    /// Close a target's session, keeping its lane and text.
    Pause(Language),
    /// Open a paused or failed target's session again.
    Resume(Language),
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
    /// The spoken language. Offline Captions pass it to Whisper and Online
    /// Captions send it as a hint; Auto detects it. Translation detects the
    /// language itself and uses this only to label the source lane and to
    /// keep targets from repeating it.
    pub spoken_language: LanguageSelection,
    /// Output languages for [`ProcessingMode::Translation`]. A running
    /// session changes them with [`TargetChange`], without a restart.
    pub translation_targets: TranslationTargets,
}

impl SessionOptions {
    /// Whether moving from these options to `next` needs a new session.
    /// Only what the backend itself depends on does: the mode, the audio
    /// source, and the spoken language where it is sent to the backend.
    /// Translation targets change live, and which lanes are shown is
    /// presentation only.
    pub fn needs_restart_for(&self, next: &SessionOptions) -> bool {
        self.mode != next.mode
            || self.source_id != next.source_id
            || (self.mode != ProcessingMode::Translation
                && self.spoken_language != next.spoken_language)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Language, LanguageSelection, MAX_TRANSLATION_TARGETS, ProcessingMode, SessionGeneration,
        SessionOptions, TranslationTargets,
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
    fn targets_never_repeat_the_named_spoken_language() {
        use Language::{English, Japanese};
        // The repeated target is dropped and the other one moves up.
        assert_eq!(
            targets(Some(Japanese), Some(English), Some(Japanese)),
            [English]
        );
        assert_eq!(
            targets(Some(English), Some(Japanese), Some(Japanese)),
            [English]
        );
        // An automatically detected language restricts nothing.
        assert_eq!(targets(Some(Japanese), None, None), [Japanese]);
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

    #[test]
    fn a_target_is_added_only_while_there_is_room_and_it_is_new() {
        use Language::{English, Japanese, Vietnamese};
        let one = TranslationTargets::resolve(Some(English), None, Some(Japanese));
        let two = one.with_added(Vietnamese, Some(Japanese)).unwrap();
        assert_eq!(two.iter().collect::<Vec<_>>(), [English, Vietnamese]);
        // Full, a duplicate, and the spoken language are all refused.
        assert_eq!(two.with_added(Language::German, Some(Japanese)), None);
        assert_eq!(one.with_added(English, Some(Japanese)), None);
        assert_eq!(one.with_added(Japanese, Some(Japanese)), None);
        assert_eq!(two.iter().count(), MAX_TRANSLATION_TARGETS);
        // The menu offers exactly what would be accepted.
        let addable = one.addable(Some(Japanese));
        assert!(!addable.contains(&English) && !addable.contains(&Japanese));
        assert!(addable.contains(&Vietnamese));
        assert!(two.addable(Some(Japanese)).is_empty());
    }

    #[test]
    fn removing_a_target_keeps_the_other_and_never_the_last() {
        use Language::{English, Vietnamese};
        let two = TranslationTargets::resolve(Some(English), Some(Vietnamese), None);
        // Removing the first moves the second up.
        let rest = two.without(English).unwrap();
        assert_eq!(rest.iter().collect::<Vec<_>>(), [Vietnamese]);
        assert_eq!(
            two.without(Vietnamese).unwrap().iter().collect::<Vec<_>>(),
            [English]
        );
        // The only target, or a language that isn't one, can't be removed.
        assert_eq!(rest.without(Vietnamese), None);
        assert_eq!(two.without(Language::German), None);
        assert!(two.contains(Vietnamese) && !rest.contains(English));
    }

    fn options(mode: ProcessingMode, source: &str, spoken: LanguageSelection) -> SessionOptions {
        SessionOptions {
            mode,
            source_id: source.to_owned(),
            spoken_language: spoken,
            translation_targets: TranslationTargets::resolve(Some(Language::English), None, None),
        }
    }

    #[test]
    fn only_backend_changes_need_a_new_session() {
        use ProcessingMode::{OfflineCaptions, OnlineCaptions, Translation};
        let japanese = LanguageSelection::Language(Language::Japanese);
        let auto = LanguageSelection::Auto;
        let running = options(Translation, "monitor", auto);
        // Targets change live in a running translation session.
        let mut more_targets = running.clone();
        more_targets.translation_targets = running
            .translation_targets
            .with_added(Language::Vietnamese, None)
            .unwrap();
        assert!(!running.needs_restart_for(&more_targets));
        // Translation detects the language; naming it only relabels a lane.
        assert!(!running.needs_restart_for(&options(Translation, "monitor", japanese)));
        // Captions send the spoken language to the backend.
        for mode in [OfflineCaptions, OnlineCaptions] {
            assert!(
                options(mode, "monitor", auto)
                    .needs_restart_for(&options(mode, "monitor", japanese))
            );
        }
        assert!(running.needs_restart_for(&options(OnlineCaptions, "monitor", auto)));
        assert!(running.needs_restart_for(&options(Translation, "microphone", auto)));
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
