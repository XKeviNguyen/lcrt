use lcrt_core::TranscriptUpdate;

use crate::{WhisperBackendError, window::InferenceKind};

const TRUNCATION_MARKER: &str = "…";
/// A rolled window starts mid-utterance, so Whisper may garble its first words.
const MAX_GARBLED_LEADING_WORDS: usize = 2;
/// Matching words required before garbled leading words may be dropped.
const MIN_ANCHORED_OVERLAP_WORDS: usize = 3;

/// Bounded transcript state for one utterance.
///
/// `committed` contains words that disappeared from the front of an advancing
/// rolling audio window. `partial` is the current window hypothesis and remains
/// replaceable. Both strings share one explicit byte budget.
pub(crate) struct TranscriptAssembler {
    committed: String,
    partial: String,
    max_bytes: usize,
}

impl TranscriptAssembler {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            committed: String::new(),
            partial: String::new(),
            max_bytes,
        }
    }

    pub(crate) fn apply(
        &mut self,
        kind: InferenceKind,
        text: String,
        window_rolled: bool,
    ) -> Result<Option<TranscriptUpdate>, WhisperBackendError> {
        let text = text.trim();
        match kind {
            InferenceKind::Partial if text.is_empty() => Ok(None),
            InferenceKind::Partial => {
                let previous = self.joined();
                let previous_stable = self.committed.clone();
                let text = if window_rolled {
                    self.commit_prefix_not_in(text)
                } else {
                    text
                };
                self.partial.clear();
                self.partial.push_str(text);
                self.enforce_bound();
                if self.joined() == previous && self.committed == previous_stable {
                    return Ok(None);
                }
                TranscriptUpdate::incremental(&self.committed, &self.partial)
                    .map(Some)
                    .map_err(|error| WhisperBackendError::Whisper(error.to_string()))
            }
            InferenceKind::Final => {
                if !text.is_empty() {
                    let text = if window_rolled {
                        self.commit_prefix_not_in(text)
                    } else {
                        text
                    };
                    self.partial.clear();
                    self.partial.push_str(text);
                }
                self.enforce_bound();
                let finalized = self.joined();
                self.committed.clear();
                self.partial.clear();
                if finalized.is_empty() {
                    Ok(None)
                } else {
                    TranscriptUpdate::finalized(finalized)
                        .map(Some)
                        .map_err(|error| WhisperBackendError::Whisper(error.to_string()))
                }
            }
        }
    }

    /// Commits the part of the previous partial that the rolled window no
    /// longer covers and returns the part of `next_partial` to keep.
    fn commit_prefix_not_in<'a>(&mut self, next_partial: &'a str) -> &'a str {
        let committed_words = self
            .committed
            .split_whitespace()
            .take(MAX_GARBLED_LEADING_WORDS)
            .count();
        let (prefix, partial_start) =
            non_overlapping_prefix(&self.partial, next_partial, committed_words);
        self.committed.push_str(&prefix);
        &next_partial[partial_start..]
    }

    fn enforce_bound(&mut self) {
        if self.partial.len() >= self.max_bytes {
            self.committed.clear();
            self.partial = truncate_front(&self.partial, self.max_bytes);
            return;
        }

        let committed_budget = self.max_bytes.saturating_sub(self.partial.len());
        self.committed = truncate_front(&self.committed, committed_budget);
    }

    fn joined(&self) -> String {
        match (self.committed.is_empty(), self.partial.is_empty()) {
            (true, true) => String::new(),
            (true, false) => self.partial.clone(),
            (false, true) => self.committed.clone(),
            (false, false) => format!("{}{}", self.committed, self.partial),
        }
    }
}

/// Returns the prefix of `previous` not covered by `current`, and the byte
/// offset where `current` starts to overlap `previous`.
///
/// An exact overlap at the start of `current` always wins. Only when none
/// exists are up to [`MAX_GARBLED_LEADING_WORDS`] leading words of `current`
/// treated as a re-recognition of already recognized audio, and only when the
/// remainder anchors on at least [`MIN_ANCHORED_OVERLAP_WORDS`] matching words.
/// Skipped words are dropped only when at least as many recognized words
/// (`committed_words` plus the previous words before the anchor) precede the
/// anchor; otherwise they are retained as genuine leading speech.
fn non_overlapping_prefix(
    previous: &str,
    current: &str,
    committed_words: usize,
) -> (String, usize) {
    if contains_unsegmented_script(previous) || contains_unsegmented_script(current) {
        return (non_overlapping_character_prefix(previous, current), 0);
    }

    let previous_words = word_spans(previous);
    let current_words = word_spans(current);
    let overlap = (0..=MAX_GARBLED_LEADING_WORDS.min(current_words.len())).find_map(|skipped| {
        let minimum = if skipped == 0 {
            1
        } else {
            MIN_ANCHORED_OVERLAP_WORDS
        };
        let maximum = previous_words.len().min(current_words.len() - skipped);
        (minimum..=maximum)
            .rev()
            .find(|&count| {
                previous_words[previous_words.len() - count..]
                    .iter()
                    .zip(&current_words[skipped..skipped + count])
                    .all(|(left, right)| {
                        let word = normalized_word(left.2);
                        // Punctuation or noise markers such as `♪` cannot
                        // justify dropping recognized leading words.
                        word == normalized_word(right.2) && (skipped == 0 || !word.is_empty())
                    })
            })
            .map(|count| (count, skipped))
    });
    if let Some((count, skipped)) = overlap {
        let overlap_start = previous_words[previous_words.len() - count].0;
        let words_before_anchor = previous_words.len() - count + committed_words;
        let partial_start = if words_before_anchor >= skipped {
            current_words[skipped].0
        } else {
            0
        };
        return (previous[..overlap_start].to_owned(), partial_start);
    }

    if previous.is_empty() {
        (String::new(), 0)
    } else {
        (format!("{} ", previous.trim_end()), 0)
    }
}

fn word_spans(text: &str) -> Vec<(usize, usize, &str)> {
    let mut words = Vec::new();
    let mut start = None;
    for (index, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(word_start) = start.take() {
                words.push((word_start, index, &text[word_start..index]));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(word_start) = start {
        words.push((word_start, text.len(), &text[word_start..]));
    }
    words
}

fn contains_unsegmented_script(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            character as u32,
            0x3040..=0x30ff
                | 0x3400..=0x4dbf
                | 0x4e00..=0x9fff
                | 0xac00..=0xd7af
                | 0xf900..=0xfaff
                | 0x20000..=0x323af
        )
    })
}

fn non_overlapping_character_prefix(previous: &str, current: &str) -> String {
    let previous_characters = previous.char_indices().collect::<Vec<_>>();
    let current_characters = current.chars().collect::<Vec<_>>();
    let maximum = previous_characters.len().min(current_characters.len());
    let overlap = (1..=maximum)
        .rev()
        .find(|&count| {
            previous_characters[previous_characters.len() - count..]
                .iter()
                .map(|(_, character)| character)
                .eq(current_characters[..count].iter())
        })
        .unwrap_or(0);
    let prefix_characters = previous_characters.len() - overlap;
    let prefix_end = previous_characters
        .get(prefix_characters)
        .map_or(previous.len(), |(byte_index, _)| *byte_index);
    previous[..prefix_end].to_owned()
}

fn normalized_word(word: &str) -> String {
    word.trim_matches(|character: char| !character.is_alphanumeric())
        .to_lowercase()
}

fn truncate_front(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    if max_bytes < TRUNCATION_MARKER.len() {
        return String::new();
    }
    if max_bytes == TRUNCATION_MARKER.len() {
        return TRUNCATION_MARKER.to_owned();
    }

    let suffix_budget = max_bytes - TRUNCATION_MARKER.len() - 1;
    let mut start = text.len().saturating_sub(suffix_budget);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let suffix = text[start..].trim_start();
    format!("{TRUNCATION_MARKER} {suffix}")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lcrt_core::CaptionStatus;

    use super::{TRUNCATION_MARKER, TranscriptAssembler};
    use crate::{
        WhisperConfig,
        window::{InferenceKind, StreamingWindow},
    };

    fn short_window_config() -> WhisperConfig {
        let mut config = WhisperConfig::new(std::env::temp_dir().join("unused-test-model.bin"));
        config.window_duration = Duration::from_secs(2);
        config.partial_step = Duration::from_millis(500);
        config.minimum_speech = Duration::from_millis(250);
        config.final_silence = Duration::from_millis(300);
        config.speech_rms_threshold = 0.01;
        config
    }

    #[test]
    fn rolling_windows_commit_disappearing_prefix_without_duplicates() {
        let mut transcript = TranscriptAssembler::new(256);

        transcript
            .apply(
                InferenceKind::Partial,
                "alpha beta gamma delta".to_owned(),
                false,
            )
            .unwrap();
        let second = transcript
            .apply(
                InferenceKind::Partial,
                "gamma delta epsilon zeta".to_owned(),
                true,
            )
            .unwrap()
            .unwrap();
        let third = transcript
            .apply(
                InferenceKind::Partial,
                "epsilon zeta eta theta".to_owned(),
                true,
            )
            .unwrap()
            .unwrap();

        assert_eq!(second.text(), "alpha beta gamma delta epsilon zeta");
        assert_eq!(second.stable_text(), "alpha beta ");
        assert_eq!(
            third.text(),
            "alpha beta gamma delta epsilon zeta eta theta"
        );
        assert_eq!(third.stable_text(), "alpha beta gamma delta ");
    }

    #[test]
    fn rolled_window_with_garbled_leading_words_does_not_duplicate_sentence() {
        let mut transcript = TranscriptAssembler::new(256);
        transcript
            .apply(
                InferenceKind::Partial,
                "Great time, Captain. Route time caption, have me follow every conversation."
                    .to_owned(),
                false,
            )
            .unwrap();

        let update = transcript
            .apply(
                InferenceKind::Partial,
                "Roo-time caption, have me follow every conversation.".to_owned(),
                true,
            )
            .unwrap()
            .unwrap();
        let unchanged = transcript
            .apply(
                InferenceKind::Partial,
                "Root time caption, have me follow every conversation.".to_owned(),
                true,
            )
            .unwrap();

        assert_eq!(
            update.text(),
            "Great time, Captain. Route time caption, have me follow every conversation."
        );
        assert_eq!(update.stable_text(), "Great time, Captain. Route time ");
        assert!(unchanged.is_none());
    }

    #[test]
    fn exact_leading_overlap_wins_over_longer_match_after_skipped_words() {
        let mut transcript = TranscriptAssembler::new(256);
        transcript
            .apply(
                InferenceKind::Partial,
                "we need to go home".to_owned(),
                false,
            )
            .unwrap();

        let update = transcript
            .apply(
                InferenceKind::Partial,
                "go home we need to go home now".to_owned(),
                true,
            )
            .unwrap()
            .unwrap();

        assert_eq!(update.text(), "we need to go home we need to go home now");
        assert_eq!(update.stable_text(), "we need to ");
    }

    #[test]
    fn leading_words_without_earlier_recognized_text_are_retained() {
        let mut transcript = TranscriptAssembler::new(256);
        transcript
            .apply(InferenceKind::Partial, "I want to go".to_owned(), false)
            .unwrap();

        let update = transcript
            .apply(
                InferenceKind::Partial,
                "but I want to go home".to_owned(),
                true,
            )
            .unwrap()
            .unwrap();

        assert_eq!(update.text(), "but I want to go home");
    }

    #[test]
    fn punctuation_only_anchor_does_not_drop_leading_words() {
        let mut transcript = TranscriptAssembler::new(256);
        transcript
            .apply(InferenceKind::Partial, "music ♪ ♪ ♪".to_owned(), false)
            .unwrap();

        let update = transcript
            .apply(InferenceKind::Partial, "hello ♪ ♪ ♪".to_owned(), true)
            .unwrap()
            .unwrap();

        assert_eq!(update.text(), "music ♪ ♪ ♪ hello ♪ ♪ ♪");
    }

    #[test]
    fn short_overlap_after_leading_words_is_not_treated_as_repetition() {
        let mut transcript = TranscriptAssembler::new(256);
        transcript
            .apply(InferenceKind::Partial, "alpha beta gamma".to_owned(), false)
            .unwrap();

        let update = transcript
            .apply(
                InferenceKind::Partial,
                "delta gamma epsilon".to_owned(),
                true,
            )
            .unwrap()
            .unwrap();

        assert_eq!(update.text(), "alpha beta gamma delta gamma epsilon");
    }

    #[test]
    fn continuous_speech_beyond_window_preserves_accepted_beginning() {
        let config = short_window_config();
        let mut window = StreamingWindow::new(&config).unwrap();
        let mut transcript = TranscriptAssembler::new(config.max_transcript_bytes);
        let hypotheses = [
            "zero",
            "zero one",
            "zero one two",
            "zero one two three",
            "one two three four",
            "two three four five",
        ];
        let mut last = None;

        for hypothesis in hypotheses {
            let kind = window.push(&vec![0.1; 8_000]).unwrap();
            let rolled = window.rolled_since_inference();
            window.mark_inferred(kind);
            last = transcript
                .apply(kind, hypothesis.to_owned(), rolled)
                .unwrap()
                .or(last);
        }

        let last = last.unwrap();
        assert_eq!(window.samples().len(), 32_000);
        assert_eq!(last.text(), "zero one two three four five");
        assert_eq!(last.stable_text(), "zero one ");
    }

    #[test]
    fn empty_final_preserves_and_finalizes_last_valid_partial() {
        let mut transcript = TranscriptAssembler::new(256);
        transcript
            .apply(
                InferenceKind::Partial,
                "keep this caption".to_owned(),
                false,
            )
            .unwrap();

        let final_update = transcript
            .apply(InferenceKind::Final, String::new(), false)
            .unwrap()
            .unwrap();

        assert_eq!(final_update.text(), "keep this caption");
        assert_eq!(final_update.status(), CaptionStatus::Final);
    }

    #[test]
    fn rolling_unsegmented_script_uses_character_overlap() {
        let mut transcript = TranscriptAssembler::new(256);
        transcript
            .apply(InferenceKind::Partial, "你好世界".to_owned(), false)
            .unwrap();

        let update = transcript
            .apply(InferenceKind::Partial, "世界和平".to_owned(), true)
            .unwrap()
            .unwrap();

        assert_eq!(update.text(), "你好世界和平");
        assert_eq!(update.stable_text(), "你好");
    }

    #[test]
    fn stable_boundary_advancement_emits_unchanged_visible_text() {
        let mut transcript = TranscriptAssembler::new(256);
        transcript
            .apply(InferenceKind::Partial, "alpha beta gamma".to_owned(), false)
            .unwrap();

        let update = transcript
            .apply(InferenceKind::Partial, "beta gamma".to_owned(), true)
            .unwrap()
            .unwrap();

        assert_eq!(update.text(), "alpha beta gamma");
        assert_eq!(update.stable_text(), "alpha ");
        assert_eq!(update.partial_text(), "beta gamma");
    }

    #[test]
    fn transcript_memory_and_visible_text_stay_bounded() {
        let mut transcript = TranscriptAssembler::new(48);
        transcript
            .apply(
                InferenceKind::Partial,
                "one two three four five six seven eight".to_owned(),
                false,
            )
            .unwrap();
        let update = transcript
            .apply(
                InferenceKind::Partial,
                "seven eight nine ten eleven twelve thirteen".to_owned(),
                true,
            )
            .unwrap()
            .unwrap();

        assert!(update.text().len() <= 48);
        assert!(update.text().starts_with(TRUNCATION_MARKER));
    }
}
