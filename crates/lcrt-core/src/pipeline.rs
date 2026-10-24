//! Portable audio-to-caption orchestration.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use tracing::{debug, info, warn};

use crate::{
    AudioCapture, AudioCaptureError, AudioInputEvent, CaptionSink, CaptionSinkError, CaptionState,
    CaptionStateError, RuntimeConfig, RuntimeConfigError, Transcriber, TranscriptUpdate,
    TranscriptionError, transcription::AcceptedText,
};

/// Receives the accepted transcript as transcription produces it, before the
/// UI coalesces anything. It must never block: it runs on the capture path.
pub trait TranscriptRecorder: Send {
    /// Called after each transcription step. `audio` is how much audio had
    /// been passed to transcription; `presented` is whether the step changed
    /// the captions.
    fn record(&mut self, audio: Duration, presented: bool, accepted: Vec<AcceptedText>);
    /// The session ended, normally when `completed`, or with an error.
    fn finish(&mut self, completed: bool);
}

/// Counts useful work completed by one bounded pipeline run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunSummary {
    /// Number of audio chunks passed to transcription.
    pub audio_chunks: u64,
    /// Number of caption snapshots published to the UI.
    pub caption_updates: u64,
}

/// Portable caption application pipeline.
pub struct CaptionPipeline<A, T, U> {
    audio: A,
    transcriber: T,
    sink: U,
    captions: CaptionState,
    config: RuntimeConfig,
    recorder: Option<Box<dyn TranscriptRecorder>>,
    /// Audio passed to transcription so far, by its sample count.
    audio_time: Duration,
}

impl<A, T, U> CaptionPipeline<A, T, U>
where
    A: AudioCapture,
    T: Transcriber,
    U: CaptionSink,
{
    /// Creates a validated pipeline from concrete platform adapters.
    pub fn new(
        audio: A,
        transcriber: T,
        sink: U,
        config: RuntimeConfig,
    ) -> Result<Self, PipelineError> {
        config.validate().map_err(PipelineError::Configuration)?;
        Ok(Self {
            audio,
            transcriber,
            sink,
            captions: CaptionState::new(),
            config,
            recorder: None,
            audio_time: Duration::ZERO,
        })
    }

    /// Hands the accepted transcript to `recorder` as it is produced.
    pub fn with_recorder(mut self, recorder: Box<dyn TranscriptRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Runs until cancellation, clean end-of-stream, or an adapter error.
    ///
    /// The audio adapter is stopped before transcription is flushed, and is
    /// still stopped when capture, transcription, or UI publication fails. If
    /// shutdown also fails, the original processing error is kept.
    pub fn run(mut self, cancelled: &AtomicBool) -> Result<RunSummary, PipelineError> {
        let source_id = self.audio.source().id().to_owned();
        info!(source_id, "caption pipeline started");
        let capture_result = self.capture_until_stopped(cancelled);
        let stop_result = self.audio.stop().map_err(PipelineError::Audio);
        match capture_result {
            Ok(mut summary) => {
                stop_result?;
                let final_updates = self
                    .transcriber
                    .finish()
                    .map_err(PipelineError::Transcription)?;
                self.publish_updates(final_updates, &mut summary)?;
                if let Some(recorder) = &mut self.recorder {
                    recorder.finish(true);
                }
                info!(
                    source_id,
                    audio_chunks = summary.audio_chunks,
                    caption_updates = summary.caption_updates,
                    "caption pipeline stopped"
                );
                Ok(summary)
            }
            Err(error) => {
                let audio_stopped = match stop_result {
                    Ok(()) => true,
                    Err(stop_error) => {
                        warn!(%stop_error, "audio shutdown also failed after pipeline error");
                        false
                    }
                };
                if audio_stopped && matches!(&error, PipelineError::Audio(_)) {
                    self.finish_after_capture_failure();
                }
                if let Some(recorder) = &mut self.recorder {
                    recorder.finish(false);
                }
                Err(error)
            }
        }
    }

    fn capture_until_stopped(
        &mut self,
        cancelled: &AtomicBool,
    ) -> Result<RunSummary, PipelineError> {
        let mut summary = RunSummary::default();
        while !cancelled.load(Ordering::Acquire) {
            let event = self
                .audio
                .next_event(self.config.audio_poll_timeout)
                .map_err(PipelineError::Audio)?;
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            match event {
                AudioInputEvent::Chunk(chunk) => {
                    if chunk.samples().len() > self.config.max_audio_chunk_samples {
                        return Err(PipelineError::AudioChunkTooLarge {
                            actual_samples: chunk.samples().len(),
                            maximum_samples: self.config.max_audio_chunk_samples,
                        });
                    }
                    debug!(
                        capture_to_pipeline_us = chunk.capture_age().as_micros(),
                        frames = chunk.frame_count(),
                        "audio chunk reached pipeline"
                    );
                    summary.audio_chunks += 1;
                    let rate = u64::from(chunk.sample_rate_hz().max(1));
                    self.audio_time += Duration::from_nanos(
                        (chunk.frame_count() as u64).saturating_mul(1_000_000_000) / rate,
                    );
                    let updates = self
                        .transcriber
                        .push_audio(chunk)
                        .map_err(PipelineError::Transcription)?;
                    self.publish_updates(updates, &mut summary)?;
                }
                AudioInputEvent::Idle => debug!("audio poll completed without samples"),
                AudioInputEvent::EndOfStream => break,
            }
        }

        Ok(summary)
    }

    fn finish_after_capture_failure(&mut self) {
        let mut ignored_summary = RunSummary::default();
        match self.transcriber.finish() {
            Ok(updates) => {
                if let Err(error) = self.publish_updates(updates, &mut ignored_summary) {
                    warn!(%error, "could not publish final captions after capture failure");
                }
            }
            Err(error) => warn!(%error, "could not finalize transcription after capture failure"),
        }
    }

    fn publish_updates(
        &mut self,
        updates: Vec<TranscriptUpdate>,
        summary: &mut RunSummary,
    ) -> Result<(), PipelineError> {
        // Accepted text is taken every step, recorded or not, so a backend
        // never holds more than one step's worth of it.
        let accepted = self.transcriber.take_accepted();
        if let Some(recorder) = &mut self.recorder {
            recorder.record(self.audio_time, !updates.is_empty(), accepted);
        }
        for update in updates {
            let state_started = std::time::Instant::now();
            let snapshot = self
                .captions
                .apply(update)
                .map_err(PipelineError::CaptionState)?;
            let revision = snapshot.revision();
            let caption_state_us = state_started.elapsed().as_micros();
            let publish_started = std::time::Instant::now();
            self.sink
                .publish(snapshot)
                .map_err(PipelineError::CaptionSink)?;
            debug!(
                revision,
                caption_state_us,
                ui_enqueue_us = publish_started.elapsed().as_micros(),
                "caption update published"
            );
            summary.caption_updates += 1;
        }
        Ok(())
    }
}

/// Typed failure from a pipeline boundary.
#[derive(Debug)]
pub enum PipelineError {
    /// Runtime limits were invalid.
    Configuration(RuntimeConfigError),
    /// The audio adapter failed.
    Audio(AudioCaptureError),
    /// An adapter violated the configured per-chunk bound.
    AudioChunkTooLarge {
        actual_samples: usize,
        maximum_samples: usize,
    },
    /// The speech-to-text backend failed.
    Transcription(TranscriptionError),
    /// Caption state could not be advanced.
    CaptionState(CaptionStateError),
    /// The native UI sink failed.
    CaptionSink(CaptionSinkError),
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(error) => {
                write!(formatter, "invalid runtime configuration: {error}")
            }
            Self::Audio(error) => write!(formatter, "audio capture failed: {error}"),
            Self::AudioChunkTooLarge {
                actual_samples,
                maximum_samples,
            } => write!(
                formatter,
                "audio adapter produced {actual_samples} samples; configured maximum is {maximum_samples}"
            ),
            Self::Transcription(error) => write!(formatter, "transcription failed: {error}"),
            Self::CaptionState(error) => write!(formatter, "caption state failed: {error}"),
            Self::CaptionSink(error) => write!(formatter, "caption UI failed: {error}"),
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Configuration(error) => Some(error),
            Self::Audio(error) => Some(error),
            Self::AudioChunkTooLarge { .. } => None,
            Self::Transcription(error) => Some(error),
            Self::CaptionState(error) => Some(error),
            Self::CaptionSink(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use crate::{
        AudioCapture, AudioCaptureError, AudioChunk, AudioInputEvent, AudioSourceDescriptor,
        AudioSourceKind, CaptionSink, CaptionSinkError, CaptionSnapshot, RuntimeConfig,
        Transcriber, TranscriptUpdate, TranscriptionError,
    };

    use super::{CaptionPipeline, PipelineError, RunSummary};

    struct FakeAudio {
        source: AudioSourceDescriptor,
        events: VecDeque<Result<AudioInputEvent, AudioCaptureError>>,
        stopped: Arc<AtomicBool>,
        cancel_after_poll: Option<Arc<AtomicBool>>,
        stop_error: Option<AudioCaptureError>,
    }

    impl AudioCapture for FakeAudio {
        fn source(&self) -> &AudioSourceDescriptor {
            &self.source
        }

        fn next_event(&mut self, _timeout: Duration) -> Result<AudioInputEvent, AudioCaptureError> {
            let event = self
                .events
                .pop_front()
                .unwrap_or(Ok(AudioInputEvent::EndOfStream));
            if let Some(cancelled) = self.cancel_after_poll.take() {
                cancelled.store(true, Ordering::Release);
            }
            event
        }

        fn stop(&mut self) -> Result<(), AudioCaptureError> {
            if let Some(error) = self.stop_error.take() {
                return Err(error);
            }
            self.stopped.store(true, Ordering::Release);
            Ok(())
        }
    }

    struct FakeTranscriber {
        audio_stopped: Arc<AtomicBool>,
        pushed_chunks: Arc<AtomicUsize>,
        finish_calls: Arc<AtomicUsize>,
        finish_result: Result<Vec<TranscriptUpdate>, TranscriptionError>,
    }

    impl Transcriber for FakeTranscriber {
        fn push_audio(
            &mut self,
            _chunk: AudioChunk,
        ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
            self.pushed_chunks.fetch_add(1, Ordering::Relaxed);
            Ok(vec![TranscriptUpdate::partial("hello").unwrap()])
        }

        fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
            self.finish_calls.fetch_add(1, Ordering::Relaxed);
            if !self.audio_stopped.load(Ordering::Acquire) {
                return Err(TranscriptionError::new(
                    "audio was not stopped before transcription flush",
                ));
            }
            self.finish_result.clone()
        }
    }

    #[derive(Default)]
    struct CollectingSink {
        snapshots: Vec<CaptionSnapshot>,
    }

    impl CaptionSink for CollectingSink {
        fn publish(&mut self, snapshot: CaptionSnapshot) -> Result<(), CaptionSinkError> {
            self.snapshots.push(snapshot);
            Ok(())
        }
    }

    struct SharedSink {
        snapshots: Arc<Mutex<Vec<CaptionSnapshot>>>,
    }

    impl CaptionSink for SharedSink {
        fn publish(&mut self, snapshot: CaptionSnapshot) -> Result<(), CaptionSinkError> {
            self.snapshots.lock().unwrap().push(snapshot);
            Ok(())
        }
    }

    struct FailingSink;

    impl CaptionSink for FailingSink {
        fn publish(&mut self, _snapshot: CaptionSnapshot) -> Result<(), CaptionSinkError> {
            Err(CaptionSinkError::new("caption receiver disconnected"))
        }
    }

    fn audio_with(events: Vec<AudioInputEvent>, stopped: Arc<AtomicBool>) -> FakeAudio {
        FakeAudio {
            source: AudioSourceDescriptor::new(
                "test-mic",
                "Test microphone",
                AudioSourceKind::Microphone,
            ),
            events: events.into_iter().map(Ok).collect(),
            stopped,
            cancel_after_poll: None,
            stop_error: None,
        }
    }

    fn audio_error(error: AudioCaptureError, stopped: Arc<AtomicBool>) -> FakeAudio {
        FakeAudio {
            source: AudioSourceDescriptor::new(
                "test-mic",
                "Test microphone",
                AudioSourceKind::Microphone,
            ),
            events: VecDeque::from([Err(error)]),
            stopped,
            cancel_after_poll: None,
            stop_error: None,
        }
    }

    fn transcriber(
        stopped: Arc<AtomicBool>,
        pushed_chunks: Arc<AtomicUsize>,
        finish_calls: Arc<AtomicUsize>,
        finish_result: Result<Vec<TranscriptUpdate>, TranscriptionError>,
    ) -> FakeTranscriber {
        FakeTranscriber {
            audio_stopped: stopped,
            pushed_chunks,
            finish_calls,
            finish_result,
        }
    }

    #[test]
    fn pipeline_publishes_incremental_and_final_updates_then_stops_audio() {
        let stopped = Arc::new(AtomicBool::new(false));
        let audio = audio_with(
            vec![
                AudioInputEvent::Chunk(AudioChunk::new(vec![0.0; 160], 16_000, 1).unwrap()),
                AudioInputEvent::EndOfStream,
            ],
            Arc::clone(&stopped),
        );
        let pipeline = CaptionPipeline::new(
            audio,
            transcriber(
                Arc::clone(&stopped),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Ok(vec![TranscriptUpdate::finalized("hello world").unwrap()]),
            ),
            CollectingSink::default(),
            RuntimeConfig::default(),
        )
        .unwrap();
        let summary = pipeline.run(&AtomicBool::new(false)).unwrap();
        assert_eq!(
            summary,
            RunSummary {
                audio_chunks: 1,
                caption_updates: 2
            }
        );
        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn pipeline_enforces_chunk_bound_and_still_stops_audio() {
        let stopped = Arc::new(AtomicBool::new(false));
        let audio = audio_with(
            vec![AudioInputEvent::Chunk(
                AudioChunk::new(vec![0.0; 5], 16_000, 1).unwrap(),
            )],
            Arc::clone(&stopped),
        );
        let config = RuntimeConfig {
            max_audio_chunk_samples: 4,
            ..RuntimeConfig::default()
        };
        let pipeline = CaptionPipeline::new(
            audio,
            transcriber(
                Arc::clone(&stopped),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Ok(vec![TranscriptUpdate::finalized("hello world").unwrap()]),
            ),
            CollectingSink::default(),
            config,
        )
        .unwrap();
        let error = pipeline.run(&AtomicBool::new(false)).unwrap_err();
        assert!(matches!(
            error,
            PipelineError::AudioChunkTooLarge {
                actual_samples: 5,
                maximum_samples: 4
            }
        ));
        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_after_audio_poll_does_not_transcribe_the_returned_chunk() {
        let stopped = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let pushed_chunks = Arc::new(AtomicUsize::new(0));
        let mut audio = audio_with(
            vec![AudioInputEvent::Chunk(
                AudioChunk::new(vec![0.0; 160], 16_000, 1).unwrap(),
            )],
            Arc::clone(&stopped),
        );
        audio.cancel_after_poll = Some(Arc::clone(&cancelled));
        let pipeline = CaptionPipeline::new(
            audio,
            transcriber(
                Arc::clone(&stopped),
                Arc::clone(&pushed_chunks),
                Arc::new(AtomicUsize::new(0)),
                Ok(vec![TranscriptUpdate::finalized("hello world").unwrap()]),
            ),
            CollectingSink::default(),
            RuntimeConfig::default(),
        )
        .unwrap();

        let summary = pipeline.run(&cancelled).unwrap();

        assert_eq!(summary.audio_chunks, 0);
        assert_eq!(pushed_chunks.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn capture_error_flushes_final_transcript_without_hiding_the_capture_error() {
        let stopped = Arc::new(AtomicBool::new(false));
        let finish_calls = Arc::new(AtomicUsize::new(0));
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let pipeline = CaptionPipeline::new(
            audio_error(
                AudioCaptureError::new("selected source disconnected"),
                Arc::clone(&stopped),
            ),
            transcriber(
                Arc::clone(&stopped),
                Arc::new(AtomicUsize::new(0)),
                Arc::clone(&finish_calls),
                Ok(vec![
                    TranscriptUpdate::finalized("last buffered words").unwrap(),
                ]),
            ),
            SharedSink {
                snapshots: Arc::clone(&snapshots),
            },
            RuntimeConfig::default(),
        )
        .unwrap();

        let error = pipeline.run(&AtomicBool::new(false)).unwrap_err();

        assert!(matches!(error, PipelineError::Audio(_)));
        assert!(error.to_string().contains("selected source disconnected"));
        assert_eq!(finish_calls.load(Ordering::Relaxed), 1);
        assert_eq!(snapshots.lock().unwrap().len(), 1);
        assert_eq!(
            snapshots.lock().unwrap()[0].caption().text(),
            "last buffered words"
        );
    }

    #[test]
    fn capture_error_remains_primary_when_finalization_fails() {
        let stopped = Arc::new(AtomicBool::new(false));
        let finish_calls = Arc::new(AtomicUsize::new(0));
        let pipeline = CaptionPipeline::new(
            audio_error(
                AudioCaptureError::new("selected source disconnected"),
                Arc::clone(&stopped),
            ),
            transcriber(
                Arc::clone(&stopped),
                Arc::new(AtomicUsize::new(0)),
                Arc::clone(&finish_calls),
                Err(TranscriptionError::new("could not flush model")),
            ),
            CollectingSink::default(),
            RuntimeConfig::default(),
        )
        .unwrap();

        let error = pipeline.run(&AtomicBool::new(false)).unwrap_err();

        assert!(matches!(error, PipelineError::Audio(_)));
        assert!(error.to_string().contains("selected source disconnected"));
        assert_eq!(finish_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn capture_error_does_not_flush_while_audio_shutdown_is_incomplete() {
        let stopped = Arc::new(AtomicBool::new(false));
        let finish_calls = Arc::new(AtomicUsize::new(0));
        let mut audio = audio_error(
            AudioCaptureError::new("selected source disconnected"),
            Arc::clone(&stopped),
        );
        audio.stop_error = Some(AudioCaptureError::new("capture shutdown timed out"));
        let pipeline = CaptionPipeline::new(
            audio,
            transcriber(
                Arc::clone(&stopped),
                Arc::new(AtomicUsize::new(0)),
                Arc::clone(&finish_calls),
                Ok(vec![
                    TranscriptUpdate::finalized("last buffered words").unwrap(),
                ]),
            ),
            CollectingSink::default(),
            RuntimeConfig::default(),
        )
        .unwrap();

        let error = pipeline.run(&AtomicBool::new(false)).unwrap_err();

        assert!(matches!(error, PipelineError::Audio(_)));
        assert!(error.to_string().contains("selected source disconnected"));
        assert!(!stopped.load(Ordering::Acquire));
        assert_eq!(finish_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn capture_error_remains_primary_when_final_caption_publish_fails() {
        let stopped = Arc::new(AtomicBool::new(false));
        let pipeline = CaptionPipeline::new(
            audio_error(
                AudioCaptureError::new("selected source disconnected"),
                Arc::clone(&stopped),
            ),
            transcriber(
                Arc::clone(&stopped),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Ok(vec![
                    TranscriptUpdate::finalized("last buffered words").unwrap(),
                ]),
            ),
            FailingSink,
            RuntimeConfig::default(),
        )
        .unwrap();

        let error = pipeline.run(&AtomicBool::new(false)).unwrap_err();

        assert!(matches!(error, PipelineError::Audio(_)));
        assert!(error.to_string().contains("selected source disconnected"));
    }

    /// Accepts one text per chunk, and one more when finished.
    struct AcceptingTranscriber {
        accepted: Vec<crate::AcceptedText>,
        next: u64,
    }

    impl AcceptingTranscriber {
        fn accept(&mut self) {
            self.next += 1;
            self.accepted.push(crate::AcceptedText {
                segment: self.next,
                lane: crate::CaptionLane::Source,
                text: format!("segment {}", self.next),
                after_gap: false,
            });
        }
    }

    impl Transcriber for AcceptingTranscriber {
        fn push_audio(
            &mut self,
            _chunk: AudioChunk,
        ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
            self.accept();
            Ok(vec![TranscriptUpdate::partial("hello").unwrap()])
        }

        fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
            self.accept();
            Ok(Vec::new())
        }

        fn take_accepted(&mut self) -> Vec<crate::AcceptedText> {
            std::mem::take(&mut self.accepted)
        }
    }

    type Recorded = Arc<Mutex<(Vec<(Duration, bool, Vec<u64>)>, Option<bool>)>>;

    struct Recording(Recorded);

    impl super::TranscriptRecorder for Recording {
        fn record(&mut self, audio: Duration, presented: bool, accepted: Vec<crate::AcceptedText>) {
            let segments = accepted.iter().map(|text| text.segment).collect();
            self.0.lock().unwrap().0.push((audio, presented, segments));
        }

        fn finish(&mut self, completed: bool) {
            self.0.lock().unwrap().1 = Some(completed);
        }
    }

    fn accepting_run(sink: impl CaptionSink) -> (Result<RunSummary, PipelineError>, Recorded) {
        let stopped = Arc::new(AtomicBool::new(false));
        let chunk =
            || AudioInputEvent::Chunk(AudioChunk::new(vec![0.0; 1_600], 16_000, 1).unwrap());
        let audio = audio_with(
            vec![chunk(), chunk(), AudioInputEvent::EndOfStream],
            stopped,
        );
        let recorded = Recorded::default();
        let pipeline = CaptionPipeline::new(
            audio,
            AcceptingTranscriber {
                accepted: Vec::new(),
                next: 0,
            },
            sink,
            RuntimeConfig::default(),
        )
        .unwrap()
        .with_recorder(Box::new(Recording(Arc::clone(&recorded))));
        (pipeline.run(&AtomicBool::new(false)), recorded)
    }

    #[test]
    fn every_accepted_text_reaches_the_recorder_once_with_the_audio_clock() {
        let (result, recorded) = accepting_run(CollectingSink::default());
        result.unwrap();
        let (steps, finished) = recorded.lock().unwrap().clone();
        let ms = Duration::from_millis;
        assert_eq!(
            steps,
            [
                (ms(100), true, vec![1]),
                (ms(200), true, vec![2]),
                (ms(200), false, vec![3]),
            ]
        );
        assert_eq!(finished, Some(true));
    }

    #[test]
    fn accepted_text_is_recorded_even_when_the_window_is_gone() {
        let (result, recorded) = accepting_run(FailingSink);
        assert!(matches!(result, Err(PipelineError::CaptionSink(_))));
        let (steps, finished) = recorded.lock().unwrap().clone();
        assert_eq!(steps[0].2, [1]);
        assert_eq!(finished, Some(false));
    }
}
