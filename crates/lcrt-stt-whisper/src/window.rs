use std::time::Duration;

use crate::{WhisperBackendError, WhisperConfig};

pub(crate) const WHISPER_SAMPLE_RATE: usize = 16_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InferenceKind {
    Partial,
    Final,
}

pub(crate) struct StreamingWindow {
    samples: Vec<f32>,
    max_samples: usize,
    step_samples: usize,
    minimum_samples: usize,
    final_silence_samples: usize,
    speech_samples: usize,
    speech_samples_since_inference: usize,
    samples_since_inference: usize,
    silence_samples: usize,
    heard_speech: bool,
    rolled_since_inference: bool,
    rms_threshold_squared: f32,
}

impl StreamingWindow {
    pub(crate) fn new(config: &WhisperConfig) -> Result<Self, WhisperBackendError> {
        Ok(Self {
            samples: Vec::with_capacity(duration_samples(config.window_duration)?),
            max_samples: duration_samples(config.window_duration)?,
            step_samples: duration_samples(config.partial_step)?,
            minimum_samples: duration_samples(config.minimum_speech)?,
            final_silence_samples: duration_samples(config.final_silence)?,
            speech_samples: 0,
            speech_samples_since_inference: 0,
            samples_since_inference: 0,
            silence_samples: 0,
            heard_speech: false,
            rolled_since_inference: false,
            rms_threshold_squared: config.speech_rms_threshold.powi(2),
        })
    }

    pub(crate) fn push(&mut self, samples: &[f32]) -> Option<InferenceKind> {
        if samples.is_empty() {
            return None;
        }

        let mean_square =
            samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
        let contains_speech = mean_square >= self.rms_threshold_squared;

        // Idle audio is irrelevant to both Whisper context and speech-relative
        // inference gates. Keeping it out also prevents old silence from
        // consuming the rolling-window budget when a new utterance starts.
        if !self.heard_speech && !contains_speech {
            return None;
        }

        if contains_speech && !self.heard_speech {
            self.heard_speech = true;
            self.speech_samples = 0;
            self.speech_samples_since_inference = 0;
            self.silence_samples = 0;
            self.rolled_since_inference = false;
        }

        if samples.len() >= self.max_samples {
            self.samples.clear();
            self.samples
                .extend_from_slice(&samples[samples.len() - self.max_samples..]);
            self.rolled_since_inference = true;
        } else {
            let overflow = self
                .samples
                .len()
                .saturating_add(samples.len())
                .saturating_sub(self.max_samples);
            if overflow > 0 {
                self.samples.drain(..overflow);
                self.rolled_since_inference = true;
            }
            self.samples.extend_from_slice(samples);
        }
        self.samples_since_inference = self.samples_since_inference.saturating_add(samples.len());

        if contains_speech {
            self.speech_samples = self.speech_samples.saturating_add(samples.len());
            self.speech_samples_since_inference = self
                .speech_samples_since_inference
                .saturating_add(samples.len());
            self.silence_samples = 0;
        } else {
            self.silence_samples = self.silence_samples.saturating_add(samples.len());
        }

        if self.silence_samples >= self.final_silence_samples {
            if self.speech_samples >= self.minimum_samples {
                return Some(InferenceKind::Final);
            }
            self.reset_utterance();
            return None;
        }

        if self.speech_samples >= self.minimum_samples
            && self.speech_samples_since_inference >= self.step_samples
        {
            Some(InferenceKind::Partial)
        } else {
            None
        }
    }

    pub(crate) fn finish_kind(&self) -> Option<InferenceKind> {
        (self.heard_speech && self.speech_samples >= self.minimum_samples)
            .then_some(InferenceKind::Final)
    }

    pub(crate) fn samples(&self) -> &[f32] {
        &self.samples
    }

    pub(crate) fn rolled_since_inference(&self) -> bool {
        self.rolled_since_inference
    }

    /// Whether appending `incoming` samples would evict audio that no pass
    /// has inferred yet.
    pub(crate) fn would_evict_uninferred(&self, incoming: usize) -> bool {
        self.samples_since_inference.saturating_add(incoming) > self.max_samples
    }

    pub(crate) fn mark_inferred(&mut self, kind: InferenceKind) {
        self.speech_samples_since_inference = 0;
        self.samples_since_inference = 0;
        self.rolled_since_inference = false;
        if kind == InferenceKind::Final {
            self.reset_utterance();
        }
    }

    fn reset_utterance(&mut self) {
        self.samples.clear();
        self.speech_samples = 0;
        self.speech_samples_since_inference = 0;
        self.samples_since_inference = 0;
        self.silence_samples = 0;
        self.heard_speech = false;
        self.rolled_since_inference = false;
    }
}

fn duration_samples(duration: Duration) -> Result<usize, WhisperBackendError> {
    let samples = duration.as_secs_f64() * WHISPER_SAMPLE_RATE as f64;
    if !samples.is_finite() || samples > usize::MAX as f64 {
        return Err(WhisperBackendError::InvalidConfiguration(
            "configured duration is too large".to_owned(),
        ));
    }
    Ok(samples.round() as usize)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{InferenceKind, StreamingWindow};
    use crate::WhisperConfig;

    fn test_config() -> WhisperConfig {
        let path = std::env::temp_dir().join("lcrt-window-test-model.bin");
        let mut config = WhisperConfig::new(path);
        config.window_duration = Duration::from_secs(2);
        config.partial_step = Duration::from_millis(500);
        config.minimum_speech = Duration::from_millis(250);
        config.final_silence = Duration::from_millis(300);
        config.speech_rms_threshold = 0.01;
        config
    }

    #[test]
    fn emits_partial_then_final_after_bounded_silence() {
        let mut window = StreamingWindow::new(&test_config()).unwrap();

        assert_eq!(window.push(&vec![0.1; 8_000]), Some(InferenceKind::Partial));
        window.mark_inferred(InferenceKind::Partial);
        assert_eq!(window.push(&vec![0.0; 4_800]), Some(InferenceKind::Final));
        window.mark_inferred(InferenceKind::Final);
        assert!(window.samples().is_empty());
    }

    #[test]
    fn rolling_window_never_exceeds_configured_bound() {
        let mut window = StreamingWindow::new(&test_config()).unwrap();
        window.push(&vec![0.1; 48_000]);

        assert_eq!(window.samples().len(), 32_000);
    }

    #[test]
    fn leading_silence_does_not_satisfy_speech_or_step_gates() {
        let mut window = StreamingWindow::new(&test_config()).unwrap();

        assert_eq!(window.push(&vec![0.0; 32_000]), None);
        assert!(window.samples().is_empty());
        assert_eq!(window.push(&vec![0.1; 4_000]), None);
        assert_eq!(window.push(&vec![0.0; 1_600]), None);
        assert_eq!(window.push(&vec![0.1; 4_000]), Some(InferenceKind::Partial));
    }

    #[test]
    fn short_speech_is_discarded_after_final_silence() {
        let mut window = StreamingWindow::new(&test_config()).unwrap();

        assert_eq!(window.push(&vec![0.1; 1_600]), None);
        assert_eq!(window.push(&vec![0.0; 4_800]), None);
        assert!(window.samples().is_empty());
        assert_eq!(window.finish_kind(), None);
    }
}
