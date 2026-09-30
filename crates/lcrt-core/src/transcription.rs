//! Speech-to-text adapter boundary and incremental update types.

use std::{error::Error, fmt};

use crate::{AudioChunk, CaptionStatus};

/// An incremental or final speech-to-text result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptUpdate {
    text: String,
    status: CaptionStatus,
    stable_prefix_len: usize,
    original: Option<String>,
    second_translation: Option<String>,
}

/// The texts of a translation session's caption lanes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TranslationLanes {
    /// The spoken-language text, when the source lane is shown.
    pub original: Option<String>,
    /// The first target's text.
    pub first: String,
    /// The second target's text, when there is a second target.
    pub second: Option<String>,
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
            original: None,
            second_translation: None,
        })
    }

    /// Creates a translation result: `translation` in the target language and
    /// `original` in the spoken language, from the same translation session.
    ///
    /// Either lane may still be empty, because the original usually arrives
    /// before its translation, but not both.
    pub fn translated(
        translation: impl Into<String>,
        original: impl Into<String>,
        status: CaptionStatus,
    ) -> Result<Self, TranscriptUpdateError> {
        Self::lanes(
            TranslationLanes {
                original: Some(original.into()),
                first: translation.into(),
                second: None,
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
        let TranslationLanes {
            original,
            first: text,
            second,
        } = lanes;
        let is_empty =
            |lane: &Option<String>| lane.as_deref().is_none_or(|text| text.trim().is_empty());
        if text.trim().is_empty() && is_empty(&original) && is_empty(&second) {
            return Err(TranscriptUpdateError::EmptyText);
        }
        let stable_prefix_len = if status == CaptionStatus::Final {
            text.len()
        } else {
            0
        };
        Ok(Self {
            text,
            status,
            stable_prefix_len,
            original,
            second_translation: second,
        })
    }

    /// Returns the spoken-language text for translation results.
    pub fn original(&self) -> Option<&str> {
        self.original.as_deref()
    }

    /// Returns the second target's text for two-target translation results.
    pub fn second_translation(&self) -> Option<&str> {
        self.second_translation.as_deref()
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

    /// Consumes the update and returns its lanes.
    pub(crate) fn into_lanes(self) -> TranslationLanes {
        TranslationLanes {
            original: self.original,
            first: self.text,
            second: self.second_translation,
        }
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
    use super::{TranscriptUpdate, TranscriptUpdateError, TranslationLanes};
    use crate::CaptionStatus;

    #[test]
    fn a_lane_update_needs_text_in_at_least_one_lane() {
        let empty = TranslationLanes {
            original: Some("  ".to_owned()),
            first: String::new(),
            second: Some(String::new()),
        };
        assert_eq!(
            TranscriptUpdate::lanes(empty, CaptionStatus::Partial).unwrap_err(),
            TranscriptUpdateError::EmptyText
        );
        // The second target can be the only lane with text so far.
        let second_only = TranslationLanes {
            original: None,
            first: String::new(),
            second: Some("Xin chào".to_owned()),
        };
        let update = TranscriptUpdate::lanes(second_only, CaptionStatus::Partial).unwrap();
        assert_eq!(update.text(), "");
        assert_eq!(update.original(), None);
        assert_eq!(update.second_translation(), Some("Xin chào"));
    }

    #[test]
    fn transcript_update_rejects_whitespace_only_text() {
        assert_eq!(
            TranscriptUpdate::partial("   ").unwrap_err(),
            TranscriptUpdateError::EmptyText
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
        let pending = TranscriptUpdate::translated("", "今日は", CaptionStatus::Partial).unwrap();
        assert_eq!(pending.text(), "");
        assert_eq!(pending.original(), Some("今日は"));
        assert_eq!(
            TranscriptUpdate::translated(" ", "", CaptionStatus::Partial).unwrap_err(),
            TranscriptUpdateError::EmptyText
        );
    }
}
