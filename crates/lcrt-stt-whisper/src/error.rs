use std::{error::Error, fmt, path::PathBuf, time::Duration};

/// Failure while configuring or running the local whisper.cpp backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WhisperBackendError {
    /// A runtime limit or language value was invalid.
    InvalidConfiguration(String),
    /// The configured model path did not point to a readable model file.
    ModelUnavailable(PathBuf),
    /// Model loading or speech inference failed.
    Whisper(String),
    /// Input audio changed format during a live session.
    AudioFormatChanged,
    /// Audio conversion or resampling failed.
    AudioConversion(String),
    /// Pending captured audio reached its bound; inference fell behind.
    InputBacklogFull(Duration),
    /// A bounded producer wait expired while pending audio remained at its bound.
    InputBacklogTimeout { limit: Duration, timeout: Duration },
    /// The worker did not start within the configured limit.
    StartupTimeout(Duration),
    /// The worker did not flush within the configured limit.
    FinishTimeout(Duration),
    /// The worker channel disconnected or the thread panicked.
    Worker(String),
}

impl fmt::Display for WhisperBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(formatter, "invalid Whisper configuration: {message}")
            }
            Self::ModelUnavailable(path) => write!(
                formatter,
                "Whisper model is unavailable at {}; download a compatible ggml model and configure its path",
                path.display()
            ),
            Self::Whisper(message) => write!(formatter, "whisper.cpp failed: {message}"),
            Self::AudioFormatChanged => formatter.write_str(
                "audio sample rate or channel count changed during transcription; restart captioning",
            ),
            Self::AudioConversion(message) => {
                write!(formatter, "audio conversion for Whisper failed: {message}")
            }
            Self::InputBacklogFull(limit) => write!(
                formatter,
                "Whisper input backlog reached {limit:?} of audio; transcription cannot keep up with capture"
            ),
            Self::InputBacklogTimeout { limit, timeout } => write!(
                formatter,
                "Whisper input backlog remained at {limit:?} of audio for {timeout:?}; transcription cannot keep up with the producer"
            ),
            Self::StartupTimeout(timeout) => {
                write!(formatter, "Whisper model did not load within {timeout:?}")
            }
            Self::FinishTimeout(timeout) => {
                write!(formatter, "Whisper worker did not flush within {timeout:?}")
            }
            Self::Worker(message) => write!(formatter, "Whisper worker failed: {message}"),
        }
    }
}

impl Error for WhisperBackendError {}

impl From<lcrt_core::AudioConversionError> for WhisperBackendError {
    fn from(error: lcrt_core::AudioConversionError) -> Self {
        match error {
            lcrt_core::AudioConversionError::FormatChanged => Self::AudioFormatChanged,
            lcrt_core::AudioConversionError::Failed(message) => Self::AudioConversion(message),
        }
    }
}
