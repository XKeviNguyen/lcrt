//! Audio preparation for the online services: PCM16 encoding and client-side
//! turn detection.

use std::collections::VecDeque;

/// Sample rate the online services expect for PCM input.
pub const ONLINE_SAMPLE_RATE: u32 = 24_000;

const FRAME_SAMPLES: usize = ONLINE_SAMPLE_RATE as usize / 50;

/// Appends little-endian signed 16-bit PCM for `samples`, clipping to range.
pub fn encode_pcm16(samples: &[f32], output: &mut Vec<u8>) {
    output.reserve(samples.len() * 2);
    for &sample in samples {
        let clipped = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let value = (clipped * f32::from(i16::MAX)).round() as i16;
        output.extend_from_slice(&value.to_le_bytes());
    }
}

/// What to do with the audio seen so far.
#[derive(Debug, PartialEq)]
pub enum TurnAction {
    /// Send these samples as part of the current turn.
    Append(Vec<f32>),
    /// The turn is complete; ask for its final transcript.
    Commit,
}

/// Thresholds for splitting continuous audio into turns.
#[derive(Clone, Copy, Debug)]
pub struct TurnConfig {
    /// RMS level, in normalized units, above which a frame counts as speech.
    pub speech_rms: f32,
    /// Audio kept before speech onset so the first syllable is not clipped.
    pub pre_roll_samples: usize,
    /// Trailing silence that ends a turn.
    pub end_silence_samples: usize,
    /// Longest turn before it is committed even while speech continues.
    pub max_turn_samples: usize,
}

impl Default for TurnConfig {
    fn default() -> Self {
        let millis = |value: usize| value * ONLINE_SAMPLE_RATE as usize / 1_000;
        Self {
            // About -50 dBFS: system audio is often mastered quietly, and a
            // higher gate splits or drops soft speech.
            speech_rms: 0.003,
            pre_roll_samples: millis(300),
            end_silence_samples: millis(700),
            max_turn_samples: millis(15_000),
        }
    }
}

/// Energy-based turn segmentation for services that need client-side commits.
///
/// Nothing is sent while no turn is open, so silence is never streamed. All
/// buffers are bounded by one analysis frame plus the pre-roll.
pub struct TurnDetector {
    config: TurnConfig,
    pending: Vec<f32>,
    pre_roll: VecDeque<f32>,
    in_turn: bool,
    turn_samples: usize,
    silence_samples: usize,
}

impl TurnDetector {
    /// Creates a detector with no open turn.
    pub fn new(config: TurnConfig) -> Self {
        Self {
            config,
            pending: Vec::with_capacity(FRAME_SAMPLES),
            pre_roll: VecDeque::with_capacity(config.pre_roll_samples),
            in_turn: false,
            turn_samples: 0,
            silence_samples: 0,
        }
    }

    /// Consumes mono samples at [`ONLINE_SAMPLE_RATE`] and returns the actions
    /// they complete.
    pub fn push(&mut self, samples: &[f32]) -> Vec<TurnAction> {
        let mut actions = Vec::new();
        for &sample in samples {
            self.pending.push(sample);
            if self.pending.len() == FRAME_SAMPLES {
                let frame = std::mem::replace(&mut self.pending, Vec::with_capacity(FRAME_SAMPLES));
                self.process_frame(frame, &mut actions);
            }
        }
        coalesce_appends(actions)
    }

    /// Ends the stream, committing any open turn.
    pub fn finish(&mut self) -> Vec<TurnAction> {
        let mut actions = Vec::new();
        if self.in_turn {
            let tail = std::mem::take(&mut self.pending);
            if !tail.is_empty() {
                actions.push(TurnAction::Append(tail));
            }
            actions.push(self.close_turn());
        }
        self.pending.clear();
        self.pre_roll.clear();
        actions
    }

    fn process_frame(&mut self, frame: Vec<f32>, actions: &mut Vec<TurnAction>) {
        let mean_square =
            frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len() as f32;
        let speech = mean_square >= self.config.speech_rms * self.config.speech_rms;
        let frame_len = frame.len();

        if !self.in_turn {
            if !speech {
                self.remember_pre_roll(&frame);
                return;
            }
            self.in_turn = true;
            let mut opening: Vec<f32> = self.pre_roll.drain(..).collect();
            self.turn_samples = opening.len();
            opening.extend_from_slice(&frame);
            actions.push(TurnAction::Append(opening));
        } else {
            actions.push(TurnAction::Append(frame));
        }

        self.turn_samples += frame_len;
        if speech {
            self.silence_samples = 0;
        } else {
            self.silence_samples += frame_len;
        }

        if self.silence_samples >= self.config.end_silence_samples {
            actions.push(self.close_turn());
        } else if self.turn_samples >= self.config.max_turn_samples {
            // Long uninterrupted speech: commit so finals keep flowing, and
            // continue in a new turn without losing the speaking state.
            actions.push(TurnAction::Commit);
            self.turn_samples = 0;
        }
    }

    /// Every turn is committed. The service has already transcribed the
    /// buffered audio by now, so discarding a short turn would only lose its
    /// words; a commit it considers too short is rejected harmlessly.
    fn close_turn(&mut self) -> TurnAction {
        self.in_turn = false;
        self.turn_samples = 0;
        self.silence_samples = 0;
        TurnAction::Commit
    }

    fn remember_pre_roll(&mut self, frame: &[f32]) {
        self.pre_roll.extend(frame.iter().copied());
        let excess = self
            .pre_roll
            .len()
            .saturating_sub(self.config.pre_roll_samples);
        self.pre_roll.drain(..excess);
    }
}

fn coalesce_appends(actions: Vec<TurnAction>) -> Vec<TurnAction> {
    let mut coalesced: Vec<TurnAction> = Vec::with_capacity(actions.len());
    for action in actions {
        match (coalesced.last_mut(), action) {
            (Some(TurnAction::Append(previous)), TurnAction::Append(next)) => previous.extend(next),
            (_, action) => coalesced.push(action),
        }
    }
    coalesced
}

#[cfg(test)]
mod tests {
    use super::{FRAME_SAMPLES, TurnAction, TurnConfig, TurnDetector, encode_pcm16};

    fn millis(value: usize) -> usize {
        value * 24
    }

    fn appended(actions: &[TurnAction]) -> usize {
        actions
            .iter()
            .map(|action| match action {
                TurnAction::Append(samples) => samples.len(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn pcm16_encoding_clips_and_is_little_endian() {
        let mut bytes = Vec::new();
        encode_pcm16(&[0.0, 1.0, -1.0, 2.5, -7.0, f32::NAN, 0.5], &mut bytes);
        let values: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(values, [0, 32_767, -32_767, 32_767, -32_767, 0, 16_384]);
    }

    #[test]
    fn empty_input_produces_nothing() {
        let mut bytes = Vec::new();
        encode_pcm16(&[], &mut bytes);
        assert!(bytes.is_empty());
        assert!(
            TurnDetector::new(TurnConfig::default())
                .push(&[])
                .is_empty()
        );
    }

    #[test]
    fn silence_is_never_sent() {
        let mut detector = TurnDetector::new(TurnConfig::default());
        assert!(detector.push(&vec![0.0; millis(5_000)]).is_empty());
        assert!(detector.finish().is_empty());
    }

    #[test]
    fn speech_then_trailing_silence_commits_one_turn_with_pre_roll() {
        let mut detector = TurnDetector::new(TurnConfig::default());
        let mut actions = detector.push(&vec![0.0; millis(1_000)]);
        actions.extend(detector.push(&vec![0.2; millis(1_000)]));
        actions.extend(detector.push(&vec![0.0; millis(1_000)]));

        assert_eq!(
            actions.iter().filter(|a| **a == TurnAction::Commit).count(),
            1
        );
        // 300 ms pre-roll + 1 s speech + 700 ms closing silence.
        assert_eq!(appended(&actions), millis(2_000));
        assert_eq!(actions.last(), Some(&TurnAction::Commit));
    }

    #[test]
    fn a_short_word_is_committed_not_discarded() {
        let mut detector = TurnDetector::new(TurnConfig::default());
        let mut actions = detector.push(&vec![0.2; millis(100)]);
        actions.extend(detector.push(&vec![0.0; millis(1_000)]));
        assert_eq!(
            actions.iter().filter(|a| **a == TurnAction::Commit).count(),
            1
        );
    }

    #[test]
    fn quietly_mastered_speech_opens_a_turn() {
        // -44 dBFS speech, as in quiet recordings, is not silence.
        let mut detector = TurnDetector::new(TurnConfig::default());
        let mut actions = detector.push(&vec![0.006; millis(1_000)]);
        actions.extend(detector.push(&vec![0.0; millis(1_000)]));
        assert!(appended(&actions) >= millis(1_000));
        assert_eq!(actions.last(), Some(&TurnAction::Commit));
    }

    #[test]
    fn long_speech_commits_periodically_without_losing_audio() {
        let mut detector = TurnDetector::new(TurnConfig::default());
        let actions = detector.push(&vec![0.2; millis(31_000)]);
        assert_eq!(
            actions.iter().filter(|a| **a == TurnAction::Commit).count(),
            2
        );
        assert_eq!(
            appended(&actions),
            millis(31_000) / FRAME_SAMPLES * FRAME_SAMPLES
        );
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_result() {
        let signal: Vec<f32> = [
            vec![0.0; millis(500)],
            vec![0.2; millis(800)],
            vec![0.0; millis(900)],
        ]
        .concat();
        let mut whole = TurnDetector::new(TurnConfig::default());
        let expected = whole.push(&signal);

        let mut split = TurnDetector::new(TurnConfig::default());
        let mut actual = Vec::new();
        for piece in signal.chunks(137) {
            actual.extend(split.push(piece));
        }
        assert_eq!(
            actual.iter().filter(|a| **a == TurnAction::Commit).count(),
            expected
                .iter()
                .filter(|a| **a == TurnAction::Commit)
                .count()
        );
        assert_eq!(appended(&actual), appended(&expected));
    }

    #[test]
    fn finish_commits_an_open_turn_including_its_final_partial_frame() {
        let mut speaking = TurnDetector::new(TurnConfig::default());
        speaking.push(&vec![0.2; millis(600) + 7]);
        let actions = speaking.finish();
        assert_eq!(actions.last(), Some(&TurnAction::Commit));
        assert_eq!(appended(&actions), 7);
    }
}
