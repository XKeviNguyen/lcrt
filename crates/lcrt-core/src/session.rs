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
    /// Spoken-language hint for transcription.
    pub spoken_language: LanguageSelection,
    /// Output language for [`ProcessingMode::Translation`].
    pub translation_target: Language,
    /// Whether Translation also transcribes and shows the original speech.
    pub show_original: bool,
}

#[cfg(test)]
mod tests {
    use super::{Language, LanguageSelection, ProcessingMode, SessionGeneration};

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
