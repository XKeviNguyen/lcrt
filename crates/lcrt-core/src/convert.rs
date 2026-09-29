//! Channel mixdown and sample-rate conversion to a speech engine's input rate.

use std::{error::Error, fmt};

use rubato::{Fft, FixedSync, Indexing, Resampler, audioadapter_buffers::direct::InterleavedSlice};

use crate::AudioChunk;

const RESAMPLER_CHUNK_FRAMES: usize = 1_024;

/// Failure while converting captured audio for a speech engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioConversionError {
    /// The capture sample rate or channel count changed mid-session.
    FormatChanged,
    /// Resampler construction or processing failed.
    Failed(String),
}

impl fmt::Display for AudioConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FormatChanged => formatter.write_str("audio format changed during the session"),
            Self::Failed(message) => write!(formatter, "audio conversion failed: {message}"),
        }
    }
}

impl Error for AudioConversionError {}

/// Streams interleaved capture chunks into mono `f32` at a fixed output rate.
///
/// The converter keeps only one resampler block of pending input, so memory
/// stays bounded regardless of session length.
pub struct AudioConverter {
    output_rate: u32,
    input_rate: u32,
    input_channels: u16,
    resampler: Option<Fft<f32>>,
    pending_mono: Vec<f32>,
    delay_remaining: usize,
    total_input_frames: usize,
    total_output_frames: usize,
}

impl AudioConverter {
    /// Creates a converter for the format of `chunk`, producing mono audio at
    /// `output_rate` Hz.
    pub fn new(chunk: &AudioChunk, output_rate: u32) -> Result<Self, AudioConversionError> {
        let resampler = if chunk.sample_rate_hz() == output_rate {
            None
        } else {
            Some(
                Fft::new(
                    chunk.sample_rate_hz() as usize,
                    output_rate as usize,
                    RESAMPLER_CHUNK_FRAMES,
                    1,
                    FixedSync::Input,
                )
                .map_err(|error| AudioConversionError::Failed(error.to_string()))?,
            )
        };
        let delay_remaining = resampler
            .as_ref()
            .map_or(0, rubato::Resampler::output_delay);
        Ok(Self {
            output_rate,
            input_rate: chunk.sample_rate_hz(),
            input_channels: chunk.channels(),
            resampler,
            pending_mono: Vec::new(),
            delay_remaining,
            total_input_frames: 0,
            total_output_frames: 0,
        })
    }

    /// Converts one chunk, returning the output samples that are ready.
    pub fn push(&mut self, chunk: &AudioChunk) -> Result<Vec<f32>, AudioConversionError> {
        if chunk.sample_rate_hz() != self.input_rate || chunk.channels() != self.input_channels {
            return Err(AudioConversionError::FormatChanged);
        }
        let channels = usize::from(self.input_channels);
        let mono = chunk
            .samples()
            .chunks_exact(channels)
            .map(|frame| frame.iter().copied().sum::<f32>() / channels as f32)
            .collect::<Vec<_>>();
        self.total_input_frames = self.total_input_frames.saturating_add(mono.len());

        if self.resampler.is_none() {
            self.total_output_frames = self.total_output_frames.saturating_add(mono.len());
            return Ok(mono);
        }

        self.pending_mono.extend(mono);
        let mut output = Vec::new();
        loop {
            let Some(resampler) = self.resampler.as_ref() else {
                return Err(AudioConversionError::Failed(
                    "resampler disappeared during conversion".to_owned(),
                ));
            };
            let needed = resampler.input_frames_next();
            if self.pending_mono.len() < needed {
                break;
            }
            let input = self.pending_mono.drain(..needed).collect::<Vec<_>>();
            let block = self.process_block(&input, None)?;
            self.append_without_delay(block, &mut output);
        }
        Ok(output)
    }

    /// Flushes the resampler's bounded delay at the end of a stream.
    pub fn finish(&mut self) -> Result<Vec<f32>, AudioConversionError> {
        if self.resampler.is_none() {
            return Ok(Vec::new());
        }
        let desired_total = ((self.total_input_frames as u128 * self.output_rate as u128)
            .div_ceil(self.input_rate as u128)) as usize;
        let mut output = Vec::new();

        if !self.pending_mono.is_empty() {
            let valid = self.pending_mono.len();
            let Some(resampler) = self.resampler.as_ref() else {
                return Err(AudioConversionError::Failed(
                    "resampler disappeared during flush".to_owned(),
                ));
            };
            let needed = resampler.input_frames_next();
            let mut input = std::mem::take(&mut self.pending_mono);
            input.resize(needed, 0.0);
            let block = self.process_block(&input, Some(valid))?;
            self.append_without_delay(block, &mut output);
        }

        for _ in 0..8 {
            if self.total_output_frames >= desired_total {
                break;
            }
            let Some(resampler) = self.resampler.as_ref() else {
                return Err(AudioConversionError::Failed(
                    "resampler disappeared during delay flush".to_owned(),
                ));
            };
            let needed = resampler.input_frames_next();
            let block = self.process_block(&vec![0.0; needed], Some(0))?;
            self.append_without_delay(block, &mut output);
        }
        if self.total_output_frames < desired_total {
            return Err(AudioConversionError::Failed(
                "resampler did not flush its bounded delay".to_owned(),
            ));
        }
        let excess = self.total_output_frames.saturating_sub(desired_total);
        if excess > 0 {
            output.truncate(output.len().saturating_sub(excess));
            self.total_output_frames = desired_total;
        }
        Ok(output)
    }

    fn process_block(
        &mut self,
        input: &[f32],
        partial_len: Option<usize>,
    ) -> Result<Vec<f32>, AudioConversionError> {
        let Some(resampler) = self.resampler.as_mut() else {
            return Err(AudioConversionError::Failed(
                "resampling was requested without a configured resampler".to_owned(),
            ));
        };
        let output_capacity = resampler.output_frames_next();
        let input_adapter = InterleavedSlice::new(input, 1, input.len())
            .map_err(|error| AudioConversionError::Failed(error.to_string()))?;
        let mut output = vec![0.0; output_capacity];
        let mut output_adapter = InterleavedSlice::new_mut(&mut output, 1, output_capacity)
            .map_err(|error| AudioConversionError::Failed(error.to_string()))?;
        let indexing = partial_len.map(|length| Indexing::new().partial_len(length));
        let (_, written) = resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, indexing.as_ref())
            .map_err(|error| AudioConversionError::Failed(error.to_string()))?;
        output.truncate(written);
        Ok(output)
    }

    fn append_without_delay(&mut self, mut block: Vec<f32>, output: &mut Vec<f32>) {
        let discard = self.delay_remaining.min(block.len());
        if discard > 0 {
            block.drain(..discard);
            self.delay_remaining -= discard;
        }
        self.total_output_frames = self.total_output_frames.saturating_add(block.len());
        output.extend(block);
    }
}

#[cfg(test)]
mod tests {
    use super::AudioConverter;
    use crate::AudioChunk;

    #[test]
    fn downmixes_stereo_without_resampling() {
        let chunk = AudioChunk::new(vec![0.5, -0.5, 0.25, 0.75], 16_000, 2).unwrap();
        let mut converter = AudioConverter::new(&chunk, 16_000).unwrap();

        assert_eq!(converter.push(&chunk).unwrap(), vec![0.0, 0.5]);
        assert!(converter.finish().unwrap().is_empty());
    }

    #[test]
    fn resamples_48khz_stereo_to_16khz_mono_with_bounded_tail_flush() {
        let samples = (0..48_000)
            .flat_map(|index| {
                let value = (index as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
                [value, value]
            })
            .collect();
        let chunk = AudioChunk::new(samples, 48_000, 2).unwrap();
        let mut converter = AudioConverter::new(&chunk, 16_000).unwrap();

        let mut output = converter.push(&chunk).unwrap();
        output.extend(converter.finish().unwrap());

        assert_eq!(output.len(), 16_000);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().any(|sample| sample.abs() > 0.5));
    }
}
