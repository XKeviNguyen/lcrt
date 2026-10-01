//! Caption domain state.

use std::{
    error::Error,
    fmt,
    time::{Duration, Instant},
};

use crate::transcription::{TranscriptUpdate, TranslationLanes};

/// Whether a caption may still change or has been finalized by transcription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptionStatus {
    /// The caption contains an incremental transcription result.
    Partial,
    /// The caption contains a completed transcription result.
    Final,
}

/// A platform-independent caption produced by the transcription pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Caption {
    text: String,
    status: CaptionStatus,
    lanes: Option<TranslationLanes>,
}

impl Caption {
    /// Creates a caption from an incremental transcription result.
    pub fn partial(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            status: CaptionStatus::Partial,
            lanes: None,
        }
    }

    /// Returns every lane's text when this caption is a translation.
    pub fn translation_lanes(&self) -> Option<&TranslationLanes> {
        self.lanes.as_ref()
    }

    /// Returns the caption text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns whether the caption is partial or final.
    pub fn status(&self) -> CaptionStatus {
        self.status
    }

    /// Marks the caption as final while preserving its text.
    pub fn finalize(&mut self) {
        self.status = CaptionStatus::Final;
    }
}

/// Immutable caption state sent to a UI adapter.
#[derive(Clone, Debug)]
pub struct CaptionSnapshot {
    revision: u64,
    caption: Caption,
    created_at: Instant,
}

impl PartialEq for CaptionSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision && self.caption == other.caption
    }
}

impl Eq for CaptionSnapshot {}

impl CaptionSnapshot {
    /// Returns a monotonically increasing state revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the current caption.
    pub fn caption(&self) -> &Caption {
        &self.caption
    }

    /// Returns elapsed time since the pipeline created this snapshot.
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }
}

/// Mutable caption state owned by the application pipeline.
#[derive(Clone, Debug, Default)]
pub struct CaptionState {
    revision: u64,
    current: Option<Caption>,
}

impl CaptionState {
    /// Creates empty caption state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one transcription update and returns the new UI snapshot.
    pub fn apply(
        &mut self,
        update: TranscriptUpdate,
    ) -> Result<CaptionSnapshot, CaptionStateError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(CaptionStateError::RevisionOverflow)?;
        let status = update.status();
        let (text, lanes) = update.into_parts();
        let caption = Caption {
            text,
            status,
            lanes,
        };
        self.current = Some(caption.clone());
        Ok(CaptionSnapshot {
            revision: self.revision,
            caption,
            created_at: Instant::now(),
        })
    }

    /// Returns the current state without changing its revision.
    pub fn snapshot(&self) -> Option<CaptionSnapshot> {
        self.current.clone().map(|caption| CaptionSnapshot {
            revision: self.revision,
            caption,
            created_at: Instant::now(),
        })
    }
}

/// Failure while updating caption state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptionStateError {
    /// The monotonically increasing revision counter was exhausted.
    RevisionOverflow,
}

impl fmt::Display for CaptionStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("caption revision counter overflowed")
    }
}

impl Error for CaptionStateError {}

#[cfg(test)]
mod tests {
    use super::{Caption, CaptionState, CaptionStatus};
    use crate::{Language, TargetText, TranscriptUpdate, TranslationLanes};

    #[test]
    fn every_translation_lane_reaches_the_caption() {
        let mut state = CaptionState::new();
        let lanes = TranslationLanes {
            original: "こんにちは".to_owned(),
            targets: vec![
                TargetText {
                    language: Language::English,
                    text: "Hello".to_owned(),
                },
                TargetText {
                    language: Language::Vietnamese,
                    text: "Xin chào".to_owned(),
                },
            ],
        };
        let update = TranscriptUpdate::lanes(lanes.clone(), CaptionStatus::Partial).unwrap();
        let snapshot = state.apply(update).unwrap();
        assert_eq!(snapshot.caption().translation_lanes(), Some(&lanes));
        // A caption that is not a translation has no lanes.
        let plain = state
            .apply(TranscriptUpdate::partial("captions").unwrap())
            .unwrap();
        assert_eq!(plain.caption().translation_lanes(), None);
        assert_eq!(plain.caption().text(), "captions");
    }

    #[test]
    fn finalizing_a_partial_caption_preserves_its_text() {
        let mut caption = Caption::partial("low-latency captions");
        assert_eq!(caption.status(), CaptionStatus::Partial);
        caption.finalize();
        assert_eq!(caption.text(), "low-latency captions");
        assert_eq!(caption.status(), CaptionStatus::Final);
    }

    #[test]
    fn applying_updates_advances_revision_and_replaces_current_caption() {
        let mut state = CaptionState::new();
        let partial = state
            .apply(TranscriptUpdate::partial("hello").unwrap())
            .unwrap();
        let final_caption = state
            .apply(TranscriptUpdate::finalized("hello world").unwrap())
            .unwrap();
        assert_eq!(partial.revision(), 1);
        assert_eq!(partial.caption().status(), CaptionStatus::Partial);
        assert_eq!(final_caption.revision(), 2);
        assert_eq!(final_caption.caption().text(), "hello world");
        assert_eq!(final_caption.caption().status(), CaptionStatus::Final);
        assert_eq!(state.snapshot(), Some(final_caption));
    }
}
