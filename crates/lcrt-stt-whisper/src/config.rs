use std::{path::PathBuf, time::Duration};

use crate::WhisperBackendError;

/// Explicit limits and inference behavior for local streaming STT.
#[derive(Clone, Debug, PartialEq)]
pub struct WhisperConfig {
    /// Path to a whisper.cpp-compatible ggml model; model data is never bundled.
    pub model_path: PathBuf,
    /// ISO language code, `auto`, or `None` for automatic detection.
    pub language: Option<String>,
    /// CPU threads used by whisper.cpp inference.
    pub inference_threads: u8,
    /// Maximum captured audio waiting behind inference, independent of chunk
    /// size. It may not exceed `window_duration`: a larger backlog could not
    /// be inferred without evicting audio that no pass has seen.
    pub max_input_backlog: Duration,
    /// Rolling audio context retained for each inference pass.
    pub window_duration: Duration,
    /// New audio required between partial inference passes.
    pub partial_step: Duration,
    /// Minimum buffered speech before the first inference pass.
    pub minimum_speech: Duration,
    /// Consecutive low-energy audio that finalizes the current utterance.
    pub final_silence: Duration,
    /// RMS threshold in normalized PCM units used by the lightweight energy gate.
    pub speech_rms_threshold: f32,
    /// Maximum UTF-8 bytes retained across committed and partial transcript text.
    pub max_transcript_bytes: usize,
    /// Maximum time allowed for model loading.
    pub startup_timeout: Duration,
    /// Maximum time allowed to flush queued audio on stop.
    pub finish_timeout: Duration,
}

impl WhisperConfig {
    /// Creates practical CPU defaults for a configured model path.
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            language: None,
            inference_threads: 4,
            max_input_backlog: Duration::from_secs(8),
            window_duration: Duration::from_secs(8),
            partial_step: Duration::from_millis(1_500),
            minimum_speech: Duration::from_millis(750),
            final_silence: Duration::from_millis(900),
            speech_rms_threshold: 0.008,
            max_transcript_bytes: 16 * 1_024,
            startup_timeout: Duration::from_secs(30),
            finish_timeout: Duration::from_secs(30),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), WhisperBackendError> {
        if !self.model_path.is_file() {
            return Err(WhisperBackendError::ModelUnavailable(
                self.model_path.clone(),
            ));
        }
        if self
            .language
            .as_deref()
            .is_some_and(|language| language.is_empty() || language.contains('\0'))
        {
            return Err(WhisperBackendError::InvalidConfiguration(
                "language must be non-empty and contain no NUL byte".to_owned(),
            ));
        }
        if self.inference_threads == 0 {
            return Err(WhisperBackendError::InvalidConfiguration(
                "inference thread count must be greater than zero".to_owned(),
            ));
        }
        if self.max_input_backlog.is_zero() || self.max_input_backlog > self.window_duration {
            return Err(WhisperBackendError::InvalidConfiguration(
                "maximum input backlog must be greater than zero and not exceed the rolling window"
                    .to_owned(),
            ));
        }
        if self.partial_step.is_zero()
            || self.minimum_speech.is_zero()
            || self.window_duration.is_zero()
            || self.final_silence.is_zero()
        {
            return Err(WhisperBackendError::InvalidConfiguration(
                "window, step, minimum speech, and final silence must be greater than zero"
                    .to_owned(),
            ));
        }
        if self.partial_step > self.window_duration
            || self.minimum_speech > self.window_duration
            || self.final_silence > self.window_duration
        {
            return Err(WhisperBackendError::InvalidConfiguration(
                "step, minimum speech, and final silence must not exceed the rolling window"
                    .to_owned(),
            ));
        }
        if !self.speech_rms_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.speech_rms_threshold)
        {
            return Err(WhisperBackendError::InvalidConfiguration(
                "speech RMS threshold must be finite and between zero and one".to_owned(),
            ));
        }
        if self.max_transcript_bytes < 16 {
            return Err(WhisperBackendError::InvalidConfiguration(
                "maximum transcript bytes must be at least 16".to_owned(),
            ));
        }
        if self.startup_timeout.is_zero() || self.finish_timeout.is_zero() {
            return Err(WhisperBackendError::InvalidConfiguration(
                "startup and finish timeouts must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::WhisperConfig;

    #[test]
    fn input_backlog_may_not_exceed_the_rolling_window() {
        // Validation only requires an existing file; the test binary is one,
        // so no file needs to be written.
        let mut config = WhisperConfig::new(std::env::current_exe().unwrap());

        assert!(config.validate().is_ok());
        config.max_input_backlog = config.window_duration + Duration::from_millis(1);
        assert!(config.validate().is_err());
        config.max_input_backlog = Duration::ZERO;
        assert!(config.validate().is_err());
    }
}
