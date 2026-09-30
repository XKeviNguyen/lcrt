//! Platform-independent audio input types and adapter boundary.

use std::{
    error::Error,
    fmt,
    time::{Duration, Instant},
};

/// The user-facing type of an audio source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioSourceKind {
    /// A microphone or other physical capture device.
    Microphone,
    /// A monitor that captures system/output audio.
    SystemOutput,
}

/// Stable information about an audio source exposed by a platform adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioSourceDescriptor {
    id: String,
    name: String,
    kind: AudioSourceKind,
}

impl AudioSourceDescriptor {
    /// Creates a source descriptor.
    pub fn new(id: impl Into<String>, name: impl Into<String>, kind: AudioSourceKind) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind,
        }
    }

    /// Returns the platform adapter's stable source identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns whether this source captures a microphone or system output.
    pub fn kind(&self) -> AudioSourceKind {
        self.kind
    }
}

/// A bounded chunk of interleaved normalized PCM samples.
#[derive(Clone, Debug)]
pub struct AudioChunk {
    samples: Vec<f32>,
    sample_rate_hz: u32,
    channels: u16,
    captured_at: Instant,
}

impl PartialEq for AudioChunk {
    fn eq(&self, other: &Self) -> bool {
        self.samples == other.samples
            && self.sample_rate_hz == other.sample_rate_hz
            && self.channels == other.channels
    }
}

impl AudioChunk {
    /// Creates a validated audio chunk.
    pub fn new(
        samples: Vec<f32>,
        sample_rate_hz: u32,
        channels: u16,
    ) -> Result<Self, AudioChunkError> {
        if samples.is_empty() {
            return Err(AudioChunkError::Empty);
        }
        if sample_rate_hz == 0 {
            return Err(AudioChunkError::ZeroSampleRate);
        }
        if channels == 0 {
            return Err(AudioChunkError::ZeroChannels);
        }
        if !samples.len().is_multiple_of(usize::from(channels)) {
            return Err(AudioChunkError::IncompleteFrame {
                sample_count: samples.len(),
                channels,
            });
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(AudioChunkError::NonFiniteSample);
        }
        Ok(Self {
            samples,
            sample_rate_hz,
            channels,
            captured_at: Instant::now(),
        })
    }

    /// Returns interleaved normalized PCM samples.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Returns the sample rate reported by the capture adapter.
    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Returns the interleaved channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Returns the number of sample frames in this chunk.
    pub fn frame_count(&self) -> usize {
        self.samples.len() / usize::from(self.channels)
    }

    /// Returns elapsed time since the adapter constructed this chunk.
    pub fn capture_age(&self) -> Duration {
        self.captured_at.elapsed()
    }
}

/// Validation failure while creating an [`AudioChunk`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioChunkError {
    /// The chunk had no samples.
    Empty,
    /// The capture adapter reported a zero sample rate.
    ZeroSampleRate,
    /// The capture adapter reported zero channels.
    ZeroChannels,
    /// Interleaved samples did not contain complete channel frames.
    IncompleteFrame { sample_count: usize, channels: u16 },
    /// At least one PCM sample was NaN or infinite.
    NonFiniteSample,
}

impl fmt::Display for AudioChunkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("audio chunk contains no samples"),
            Self::ZeroSampleRate => formatter.write_str("audio sample rate must be non-zero"),
            Self::ZeroChannels => formatter.write_str("audio channel count must be non-zero"),
            Self::IncompleteFrame {
                sample_count,
                channels,
            } => write!(
                formatter,
                "{sample_count} interleaved samples do not form complete {channels}-channel frames"
            ),
            Self::NonFiniteSample => {
                formatter.write_str("audio chunk contains a non-finite sample")
            }
        }
    }
}

impl Error for AudioChunkError {}

/// The result of one bounded wait for captured audio.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioInputEvent {
    /// Captured PCM is ready for transcription.
    Chunk(AudioChunk),
    /// No audio became available before the requested timeout.
    Idle,
    /// The source ended cleanly and will produce no further samples.
    EndOfStream,
}

/// Actionable error reported by an audio adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioCaptureError {
    message: String,
}

impl AudioCaptureError {
    /// Creates an adapter error with user-actionable context.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AudioCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AudioCaptureError {}

/// A running platform audio capture session.
///
/// Implementations must bound internal buffering. `next_event` must return no
/// later than the supplied timeout so application shutdown stays responsive.
pub trait AudioCapture: Send {
    /// Returns the selected source.
    fn source(&self) -> &AudioSourceDescriptor;
    /// Waits for the next audio event for at most `timeout`.
    fn next_event(&mut self, timeout: Duration) -> Result<AudioInputEvent, AudioCaptureError>;
    /// Stops capture and releases platform resources.
    fn stop(&mut self) -> Result<(), AudioCaptureError>;
}

#[cfg(test)]
mod tests {
    use super::{AudioChunk, AudioChunkError};

    #[test]
    fn audio_chunk_reports_complete_interleaved_frames() {
        let chunk = AudioChunk::new(vec![0.1, -0.1, 0.2, -0.2], 48_000, 2).unwrap();
        assert_eq!(chunk.frame_count(), 2);
        assert_eq!(chunk.channels(), 2);
        assert_eq!(chunk.sample_rate_hz(), 48_000);
    }

    #[test]
    fn audio_chunk_rejects_incomplete_frames() {
        let error = AudioChunk::new(vec![0.1, -0.1, 0.2], 48_000, 2).unwrap_err();
        assert_eq!(
            error,
            AudioChunkError::IncompleteFrame {
                sample_count: 3,
                channels: 2
            }
        );
    }
}
