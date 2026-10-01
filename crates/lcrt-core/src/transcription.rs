//! Speech-to-text adapter boundary and incremental update types.

use std::{error::Error, fmt};

use crate::{AudioChunk, CaptionStatus, Language};

/// An incremental or final speech-to-text result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptUpdate {
    text: String,
    status: CaptionStatus,
    stable_prefix_len: usize,
    lanes: Option<TranslationLanes>,
}

/// The texts of a translation session's caption lanes.
///
/// Each translation names its language, so a lane added or removed while
/// the session runs can never show another language's text.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TranslationLanes {
    /// The spoken-language text.
    pub original: String,
    /// Each target's text, in lane order.
    pub targets: Vec<TargetText>,
}

/// One target language's translated text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetText {
    /// The target language.
    pub language: Language,
    /// The translation so far.
    pub text: String,
}

impl TranslationLanes {
    /// The text translated into `language`, if it is one of the targets.
    pub fn target(&self, language: Language) -> Option<&str> {
        self.targets
            .iter()
            .find(|target| target.language == language)
            .map(|target| target.text.as_str())
    }

    fn is_empty(&self) -> bool {
        self.original.trim().is_empty()
            && self
                .targets
                .iter()
                .all(|target| target.text.trim().is_empty())
    }
}

impl TranscriptUpdate {
    /// Creates an incremental result that may be superseded.
    pub fn partial(text: impl Into<String>) -> Result<Self, TranscriptUpdateError> {
        Self::new(text, CaptionStatus::Partial, 0)
    }

    /// Creates a finalized result.
    pub fn finalized(text: impl Into<String>) -> Result<Self, TranscriptUpdateError> {
        let text = text.into();
        let stable_prefix_len = text.len();
        Self::new(text, CaptionStatus::Final, stable_prefix_len)
    }

    /// Creates a partial result with a stable prefix and replaceable tail.
    ///
    /// This lets rolling-window transcribers retain accepted text while still
    /// replacing only the newest hypothesis on later inference passes.
    pub fn incremental(
        stable_text: impl Into<String>,
        partial_text: impl Into<String>,
    ) -> Result<Self, TranscriptUpdateError> {
        let mut text = stable_text.into();
        let partial_text = partial_text.into();
        if partial_text.trim().is_empty() {
            return Err(TranscriptUpdateError::EmptyText);
        }
        let stable_prefix_len = text.len();
        text.push_str(&partial_text);
        Self::new(text, CaptionStatus::Partial, stable_prefix_len)
    }

    fn new(
        text: impl Into<String>,
        status: CaptionStatus,
        stable_prefix_len: usize,
    ) -> Result<Self, TranscriptUpdateError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(TranscriptUpdateError::EmptyText);
        }
        debug_assert!(text.is_char_boundary(stable_prefix_len));
        Ok(Self {
            text,
            status,
            stable_prefix_len,
            lanes: None,
        })
    }

    /// Creates a translation result: `translation` into `target` and
    /// `original` in the spoken language, from the same translation session.
    ///
    /// Either lane may still be empty, because the original usually arrives
    /// before its translation, but not both.
    pub fn translated(
        target: Language,
        translation: impl Into<String>,
        original: impl Into<String>,
        status: CaptionStatus,
    ) -> Result<Self, TranscriptUpdateError> {
        Self::lanes(
            TranslationLanes {
                original: original.into(),
                targets: vec![TargetText {
                    language: target,
                    text: translation.into(),
                }],
            },
            status,
        )
    }

    /// Creates a translation result from every lane's current text.
    ///
    /// Lanes fill at different times, so any of them may be empty, but not
    /// all of them.
    pub fn lanes(
        lanes: TranslationLanes,
        status: CaptionStatus,
    ) -> Result<Self, TranscriptUpdateError> {
        if lanes.is_empty() {
            return Err(TranscriptUpdateError::EmptyText);
        }
        // The first target's text stands for the whole update where one
        // text is needed, such as logging its length.
        let text = lanes
            .targets
            .first()
            .map(|target| target.text.clone())
            .unwrap_or_default();
        let stable_prefix_len = if status == CaptionStatus::Final {
            text.len()
        } else {
            0
        };
        Ok(Self {
            text,
            status,
            stable_prefix_len,
            lanes: Some(lanes),
        })
    }

    /// Returns every lane's text for translation results.
    pub fn translation_lanes(&self) -> Option<&TranslationLanes> {
        self.lanes.as_ref()
    }

    /// Returns the update text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the accepted prefix that later partial updates must preserve.
    pub fn stable_text(&self) -> &str {
        &self.text[..self.stable_prefix_len]
    }

    /// Returns the replaceable tail of a partial update.
    pub fn partial_text(&self) -> &str {
        self.text[self.stable_prefix_len..].trim_start()
    }

    /// Consumes the update and returns its text and translation lanes.
    pub(crate) fn into_parts(self) -> (String, Option<TranslationLanes>) {
        (self.text, self.lanes)
    }

    /// Returns whether this update is partial or final.
    pub fn status(&self) -> CaptionStatus {
        self.status
    }
}

/// Invalid speech-to-text update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptUpdateError {
    /// Empty updates are not meaningful caption state.
    EmptyText,
}

impl fmt::Display for TranscriptUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("transcript update text must not be empty")
    }
}

impl Error for TranscriptUpdateError {}

/// Actionable error reported by a speech-to-text adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptionError {
    message: String,
    credential_rejected: bool,
}

impl TranscriptionError {
    /// Creates a backend error with user-actionable context.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            credential_rejected: false,
        }
    }

    /// Creates an error meaning the service rejected the configured
    /// credential, which only the user can fix in settings.
    pub fn credential_rejected(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            credential_rejected: true,
        }
    }

    /// Whether the user must change the credential before retrying.
    pub fn is_credential_rejected(&self) -> bool {
        self.credential_rejected
    }
}

impl fmt::Display for TranscriptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TranscriptionError {}

/// A local or remote-replaceable speech-to-text backend.
pub trait Transcriber: Send {
    /// Consumes one bounded audio chunk and returns zero or more caption updates.
    fn push_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError>;
    /// Flushes any buffered speech when capture stops.
    fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError>;
}

/// Lets the application choose a backend at runtime.
impl<T: Transcriber + ?Sized> Transcriber for Box<T> {
    fn push_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        (**self).push_audio(chunk)
    }

    fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        (**self).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{TargetText, TranscriptUpdate, TranscriptUpdateError, TranslationLanes};
    use crate::{CaptionStatus, Language};

    fn target(language: Language, text: &str) -> TargetText {
        TargetText {
            language,
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_lane_update_needs_text_in_at_least_one_lane() {
        let empty = TranslationLanes {
            original: "  ".to_owned(),
            targets: vec![
                target(Language::English, ""),
                target(Language::Vietnamese, ""),
            ],
        };
        assert_eq!(
            TranscriptUpdate::lanes(empty, CaptionStatus::Partial).unwrap_err(),
            TranscriptUpdateError::EmptyText
        );
        // The second target can be the only lane with text so far.
        let second_only = TranslationLanes {
            original: String::new(),
            targets: vec![
                target(Language::English, ""),
                target(Language::Vietnamese, "Xin chào"),
            ],
        };
        let update = TranscriptUpdate::lanes(second_only, CaptionStatus::Partial).unwrap();
        let lanes = update.translation_lanes().unwrap();
        assert_eq!(lanes.target(Language::Vietnamese), Some("Xin chào"));
        assert_eq!(lanes.target(Language::English), Some(""));
        // A language that is not a target has no lane.
        assert_eq!(lanes.target(Language::German), None);
    }

    #[test]
    fn transcript_update_rejects_whitespace_only_text() {
        assert_eq!(
            TranscriptUpdate::partial("   ").unwrap_err(),
            TranscriptUpdateError::EmptyText
        );
        assert_eq!(
            TranscriptUpdate::partial("words")
                .unwrap()
                .translation_lanes(),
            None
        );
    }

    #[test]
    fn incremental_update_exposes_stable_and_replaceable_text() {
        let update = TranscriptUpdate::incremental("accepted words ", "new hypothesis").unwrap();

        assert_eq!(update.text(), "accepted words new hypothesis");
        assert_eq!(update.stable_text(), "accepted words ");
        assert_eq!(update.partial_text(), "new hypothesis");
    }

    #[test]
    fn translated_update_allows_one_empty_lane_but_not_both() {
        let pending =
            TranscriptUpdate::translated(Language::English, "", "今日は", CaptionStatus::Partial)
                .unwrap();
        let lanes = pending.translation_lanes().unwrap();
        assert_eq!(lanes.original, "今日は");
        assert_eq!(lanes.target(Language::English), Some(""));
        assert_eq!(
            TranscriptUpdate::translated(Language::English, " ", "", CaptionStatus::Partial)
                .unwrap_err(),
            TranscriptUpdateError::EmptyText
        );
    }
}
