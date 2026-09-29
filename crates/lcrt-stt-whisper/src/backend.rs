use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{
            Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, channel, sync_channel,
        },
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use lcrt_core::{AudioChunk, Transcriber, TranscriptUpdate, TranscriptionError};
use tracing::{debug, info};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use crate::{
    WhisperBackendError, WhisperConfig,
    resample::AudioConverter,
    transcript::TranscriptAssembler,
    window::{InferenceKind, StreamingWindow},
};

enum WorkerCommand {
    Audio(AudioChunk),
    Finish,
}

enum WorkerEvent {
    Update(TranscriptUpdate),
    Failure(String),
    Done,
}

/// A non-blocking [`Transcriber`] adapter backed by a dedicated whisper.cpp worker.
pub struct WhisperTranscriber {
    commands: Option<Sender<WorkerCommand>>,
    events: Receiver<WorkerEvent>,
    worker: Option<JoinHandle<Result<(), WhisperBackendError>>>,
    cancel: Arc<AtomicBool>,
    backlog: Arc<InputBacklog>,
    finish_timeout: Duration,
    finished: bool,
    inference_count: Arc<AtomicU64>,
}

impl WhisperTranscriber {
    /// Loads a local model on a worker thread and waits for bounded readiness.
    pub fn new(config: WhisperConfig) -> Result<Self, WhisperBackendError> {
        config.validate()?;
        let finish_timeout = config.finish_timeout;
        let startup_timeout = config.startup_timeout;
        // Pending audio is bounded by `InputBacklog` duration, not by a chunk
        // count, so the channel itself allocates only for queued commands.
        let (commands, command_receiver) = channel();
        let (event_sender, events) = sync_channel(8);
        let (startup_sender, startup_receiver) = sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let inference_count = Arc::new(AtomicU64::new(0));
        let worker_inference_count = Arc::clone(&inference_count);
        let backlog = Arc::new(InputBacklog::new(config.max_input_backlog));
        let worker_backlog = Arc::clone(&backlog);
        let worker = thread::Builder::new()
            .name("lcrt-whisper-stt".to_owned())
            .spawn(move || {
                let result = run_worker(
                    config,
                    command_receiver,
                    &worker_backlog,
                    event_sender.clone(),
                    startup_sender.clone(),
                    worker_cancel,
                    worker_inference_count,
                );
                if let Err(error) = &result {
                    let message = error.to_string();
                    let _ = startup_sender.try_send(Err(message.clone()));
                    let _ = event_sender.send(WorkerEvent::Failure(message));
                }
                let _ = event_sender.send(WorkerEvent::Done);
                result
            })
            .map_err(|error| WhisperBackendError::Worker(error.to_string()))?;

        match startup_receiver.recv_timeout(startup_timeout) {
            Ok(Ok(())) => Ok(Self {
                commands: Some(commands),
                events,
                worker: Some(worker),
                cancel,
                backlog,
                finish_timeout,
                finished: false,
                inference_count,
            }),
            Ok(Err(message)) => {
                drop(commands);
                let _ = worker.join();
                Err(WhisperBackendError::Whisper(message))
            }
            Err(RecvTimeoutError::Timeout) => {
                cancel.store(true, Ordering::Release);
                drop(commands);
                drop(worker);
                Err(WhisperBackendError::StartupTimeout(startup_timeout))
            }
            Err(RecvTimeoutError::Disconnected) => {
                drop(commands);
                let _ = worker.join();
                Err(WhisperBackendError::Worker(
                    "worker stopped while loading the model".to_owned(),
                ))
            }
        }
    }

    fn collect_available(&mut self) -> Result<Vec<TranscriptUpdate>, WhisperBackendError> {
        let mut updates = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(WorkerEvent::Update(update)) => updates.push(update),
                Ok(WorkerEvent::Failure(message)) => {
                    self.finished = true;
                    self.join_worker()?;
                    return Err(WhisperBackendError::Whisper(message));
                }
                Ok(WorkerEvent::Done) => {
                    self.finished = true;
                    self.join_worker()?;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) if self.finished => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(WhisperBackendError::Worker(
                        "result channel disconnected unexpectedly".to_owned(),
                    ));
                }
            }
        }
        Ok(updates)
    }

    /// Enqueues audio with bounded producer backpressure while draining ready
    /// transcript events so the worker cannot deadlock on its output queue.
    ///
    /// Live capture should continue using [`Transcriber::push_audio`] so it
    /// remains non-blocking. This method is intended for finite offline input.
    pub fn push_audio_with_timeout(
        &mut self,
        chunk: AudioChunk,
        timeout: Duration,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        if self.finished {
            return Err(TranscriptionError::new(
                "Whisper transcriber received audio after it was finished",
            ));
        }
        let duration_us = audio_micros(&chunk);
        let deadline = Instant::now().checked_add(timeout);
        let mut updates = Vec::new();
        while !self.backlog.try_reserve(duration_us) {
            updates.extend(
                self.collect_available()
                    .map_err(|error| TranscriptionError::new(error.to_string()))?,
            );
            if self.finished {
                return Err(TranscriptionError::new(
                    "Whisper worker stopped while audio was waiting for backlog space",
                ));
            }
            if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                return Err(TranscriptionError::new(
                    WhisperBackendError::InputBacklogTimeout {
                        limit: self.backlog.limit(),
                        timeout,
                    }
                    .to_string(),
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.send_reserved(chunk, duration_us)?;
        updates.extend(
            self.collect_available()
                .map_err(|error| TranscriptionError::new(error.to_string()))?,
        );
        Ok(updates)
    }

    /// Returns the number of successful local Whisper inference passes.
    pub fn inference_count(&self) -> u64 {
        self.inference_count.load(Ordering::Relaxed)
    }

    /// Sends a chunk whose `duration_us` is already reserved in the backlog.
    fn send_reserved(&self, chunk: AudioChunk, duration_us: u64) -> Result<(), TranscriptionError> {
        let failure = match self.commands.as_ref() {
            Some(commands) => match commands.send(WorkerCommand::Audio(chunk)) {
                Ok(()) => return Ok(()),
                Err(_) => "Whisper worker command channel disconnected unexpectedly",
            },
            None => "Whisper worker command channel is unavailable",
        };
        self.backlog.release(duration_us);
        Err(TranscriptionError::new(failure))
    }

    fn join_worker(&mut self) -> Result<(), WhisperBackendError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        match worker.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(WhisperBackendError::Worker(
                "worker panicked during shutdown".to_owned(),
            )),
        }
    }

    fn finish_worker(&mut self) -> Result<Vec<TranscriptUpdate>, WhisperBackendError> {
        if self.finished {
            let updates = self.collect_available()?;
            self.join_worker()?;
            return Ok(updates);
        }
        let deadline = Instant::now() + self.finish_timeout;
        if let Some(commands) = self.commands.take() {
            commands.send(WorkerCommand::Finish).map_err(|_| {
                WhisperBackendError::Worker("command channel disconnected before flush".to_owned())
            })?;
        }
        let mut updates = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(WhisperBackendError::FinishTimeout(self.finish_timeout));
            }
            match self.events.recv_timeout(remaining) {
                Ok(WorkerEvent::Update(update)) => updates.push(update),
                Ok(WorkerEvent::Failure(message)) => {
                    self.finished = true;
                    self.join_worker()?;
                    return Err(WhisperBackendError::Whisper(message));
                }
                Ok(WorkerEvent::Done) => {
                    self.finished = true;
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(WhisperBackendError::FinishTimeout(self.finish_timeout));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(WhisperBackendError::Worker(
                        "result channel disconnected before flush completed".to_owned(),
                    ));
                }
            }
        }
        self.join_worker()?;
        Ok(updates)
    }
}

impl Transcriber for WhisperTranscriber {
    fn push_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        if self.finished {
            return Err(TranscriptionError::new(
                "Whisper transcriber received audio after it was finished",
            ));
        }
        let duration_us = audio_micros(&chunk);
        if !self.backlog.try_reserve(duration_us) {
            return Err(TranscriptionError::new(
                WhisperBackendError::InputBacklogFull(self.backlog.limit()).to_string(),
            ));
        }
        self.send_reserved(chunk, duration_us)?;
        self.collect_available()
            .map_err(|error| TranscriptionError::new(error.to_string()))
    }

    fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        self.finish_worker()
            .map_err(|error| TranscriptionError::new(error.to_string()))
    }
}

impl Drop for WhisperTranscriber {
    fn drop(&mut self) {
        if self.finished {
            let _ = self.join_worker();
            return;
        }
        self.cancel.store(true, Ordering::Release);
        self.commands.take();
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            let Some(worker) = self.worker.take() else {
                return;
            };
            let _ = worker.join();
        }
    }
}

fn run_worker(
    config: WhisperConfig,
    commands: Receiver<WorkerCommand>,
    backlog: &InputBacklog,
    events: SyncSender<WorkerEvent>,
    startup: SyncSender<Result<(), String>>,
    cancel: Arc<AtomicBool>,
    inference_count: Arc<AtomicU64>,
) -> Result<(), WhisperBackendError> {
    whisper_rs::install_logging_hooks();
    let model_path = config.model_path.to_str().ok_or_else(|| {
        WhisperBackendError::InvalidConfiguration(
            "whisper-rs 0.15 requires a UTF-8 model path".to_owned(),
        )
    })?;
    let context = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
        .map_err(|error| WhisperBackendError::Whisper(error.to_string()))?;
    let mut state = context
        .create_state()
        .map_err(|error| WhisperBackendError::Whisper(error.to_string()))?;
    let parameters = decoding_parameters(&config);
    let mut window = StreamingWindow::new(&config)?;
    let mut converter = None;
    let mut transcript = TranscriptAssembler::new(config.max_transcript_bytes);
    startup
        .send(Ok(()))
        .map_err(|_| WhisperBackendError::Worker("startup receiver disconnected".to_owned()))?;
    info!(model_path = %config.model_path.display(), "local Whisper model loaded");

    while let Ok(command) = commands.recv() {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        match command {
            WorkerCommand::Audio(chunk) => {
                let drained =
                    drain_backlog(chunk, &commands, backlog, &mut converter, &mut window)?;
                if drained.finish_requested {
                    finish_stream(
                        &mut converter,
                        &mut window,
                        &mut state,
                        &parameters,
                        &events,
                        &mut transcript,
                        &inference_count,
                    )?;
                    return Ok(());
                }
                if let Some(kind) = drained.pending_kind {
                    infer_and_publish(
                        kind,
                        &mut window,
                        &mut state,
                        &parameters,
                        &events,
                        &mut transcript,
                        &inference_count,
                    )?;
                }
                // A held-back chunk can itself complete an utterance. Run that
                // pass now: resumed speech in the next chunk would clear the
                // silence that made it due.
                if let Some(samples) = drained.deferred
                    && let Some(kind) = window.push(&samples)
                {
                    infer_and_publish(
                        kind,
                        &mut window,
                        &mut state,
                        &parameters,
                        &events,
                        &mut transcript,
                        &inference_count,
                    )?;
                }
            }
            WorkerCommand::Finish => {
                finish_stream(
                    &mut converter,
                    &mut window,
                    &mut state,
                    &parameters,
                    &events,
                    &mut transcript,
                    &inference_count,
                )?;
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Audio taken from the command queue for the next inference pass.
struct DrainedAudio {
    pending_kind: Option<InferenceKind>,
    /// Converted audio held back because appending it before the pass would
    /// evict audio that no pass has inferred.
    deferred: Option<Vec<f32>>,
    finish_requested: bool,
}

/// Appends `first` and all audio captured during the previous pass, so
/// backlog never carries over from one pass to the next.
///
/// Draining stops at a due final, at a finish request, or before a chunk
/// would evict un-inferred audio of an utterance that has passed the
/// minimum-speech gate. That chunk is deferred until after a pass, which is
/// forced if none is due. Below the gate the window rolls as before, because
/// that audio is not eligible for inference.
fn drain_backlog(
    first: AudioChunk,
    commands: &Receiver<WorkerCommand>,
    backlog: &InputBacklog,
    converter: &mut Option<AudioConverter>,
    window: &mut StreamingWindow,
) -> Result<DrainedAudio, WhisperBackendError> {
    let mut drained = DrainedAudio {
        pending_kind: None,
        deferred: None,
        finish_requested: false,
    };
    let mut next = Some(first);
    while let Some(chunk) = next.take() {
        backlog.release(audio_micros(&chunk));
        let converter = match converter.as_mut() {
            Some(converter) => converter,
            None => converter.insert(AudioConverter::new(&chunk)?),
        };
        let samples = converter.push(&chunk)?;
        if window.would_evict_uninferred(samples.len()) && window.meets_minimum_speech() {
            drained.pending_kind.get_or_insert(InferenceKind::Partial);
            drained.deferred = Some(samples);
            break;
        }
        if let Some(kind) = window.push(&samples) {
            drained.pending_kind = Some(kind);
        }
        if drained.pending_kind == Some(InferenceKind::Final) {
            break;
        }
        match commands.try_recv() {
            Ok(WorkerCommand::Audio(chunk)) => next = Some(chunk),
            Ok(WorkerCommand::Finish) => drained.finish_requested = true,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
    }
    Ok(drained)
}

fn finish_stream(
    converter: &mut Option<AudioConverter>,
    window: &mut StreamingWindow,
    state: &mut WhisperState,
    parameters: &FullParams<'_, '_>,
    events: &SyncSender<WorkerEvent>,
    transcript: &mut TranscriptAssembler,
    inference_count: &AtomicU64,
) -> Result<(), WhisperBackendError> {
    if let Some(converter) = converter.as_mut() {
        let tail = converter.finish()?;
        window.push(&tail);
    }
    if let Some(kind) = window.finish_kind() {
        infer_and_publish(
            kind,
            window,
            state,
            parameters,
            events,
            transcript,
            inference_count,
        )?;
    }
    Ok(())
}

fn infer_and_publish(
    kind: InferenceKind,
    window: &mut StreamingWindow,
    state: &mut WhisperState,
    parameters: &FullParams<'_, '_>,
    events: &SyncSender<WorkerEvent>,
    transcript: &mut TranscriptAssembler,
    inference_count: &AtomicU64,
) -> Result<(), WhisperBackendError> {
    let started = Instant::now();
    let audio_duration_ms = window.samples().len() * 1_000 / 16_000;
    let text = transcribe_window(state, window.samples(), parameters)?;
    inference_count.fetch_add(1, Ordering::Relaxed);
    let window_rolled = window.rolled_since_inference();
    debug!(
        ?kind,
        audio_samples = window.samples().len(),
        audio_duration_ms,
        inference_us = started.elapsed().as_micros(),
        "Whisper inference completed"
    );
    window.mark_inferred(kind);
    let Some(update) = transcript.apply(kind, text, window_rolled)? else {
        return Ok(());
    };
    events
        .send(WorkerEvent::Update(update))
        .map_err(|_| WhisperBackendError::Worker("result receiver disconnected".to_owned()))
}

/// Captured audio accepted for transcription but not yet taken by the worker.
///
/// The limit is at most one rolling window of audio: the worker coalesces all
/// pending audio into the window before the next pass, and a larger backlog
/// could not be inferred without evicting audio that no pass has seen.
/// Bounding by duration rather than chunk count keeps the limit independent
/// of the audio quantum.
struct InputBacklog {
    queued_us: AtomicU64,
    limit_us: u64,
}

impl InputBacklog {
    fn new(limit: Duration) -> Self {
        Self {
            queued_us: AtomicU64::new(0),
            limit_us: u64::try_from(limit.as_micros()).unwrap_or(u64::MAX),
        }
    }

    fn limit(&self) -> Duration {
        Duration::from_micros(self.limit_us)
    }

    /// Reserves `duration_us`, or returns `false` when it would exceed the limit.
    fn try_reserve(&self, duration_us: u64) -> bool {
        let previous = self.queued_us.fetch_add(duration_us, Ordering::AcqRel);
        if previous.saturating_add(duration_us) > self.limit_us {
            self.queued_us.fetch_sub(duration_us, Ordering::AcqRel);
            return false;
        }
        true
    }

    fn release(&self, duration_us: u64) {
        self.queued_us.fetch_sub(duration_us, Ordering::AcqRel);
    }
}

fn audio_micros(chunk: &AudioChunk) -> u64 {
    u64::try_from(chunk.frame_count())
        .unwrap_or(u64::MAX)
        .saturating_mul(1_000_000)
        / u64::from(chunk.sample_rate_hz())
}

/// Builds decoding parameters once per worker. whisper-rs 0.15 never frees
/// the language string it allocates, so each pass clones these parameters
/// instead of building new ones.
fn decoding_parameters(config: &WhisperConfig) -> FullParams<'_, '_> {
    let mut parameters = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    parameters.set_n_threads(i32::from(config.inference_threads));
    parameters.set_language(config.language.as_deref());
    parameters.set_translate(false);
    parameters.set_no_context(true);
    parameters.set_no_timestamps(true);
    parameters.set_print_special(false);
    parameters.set_print_progress(false);
    parameters.set_print_realtime(false);
    parameters.set_print_timestamps(false);
    parameters.set_suppress_blank(true);
    parameters.set_suppress_nst(true);
    parameters.set_max_tokens(window_token_limit(config.window_duration));
    parameters
}

/// Scales whisper.cpp's own decode limit, `n_text_ctx / 2 - 4 = 220` tokens
/// for one 30 s segment, to the rolling window. Without it, a repetitive
/// decode emits up to 220 tokens for 8 s of audio on each of whisper.cpp's
/// fallback attempts, and one such pass was measured to outlast the window
/// itself, which no backlog bound can absorb.
///
/// `max_tokens` is per segment. Because passes decode without timestamps,
/// whisper.cpp ends each segment by advancing 30 s, so a window of at most
/// 30 s is one segment per fallback attempt and this bounds the whole pass.
fn window_token_limit(window: Duration) -> i32 {
    const SEGMENT_TOKEN_LIMIT: f64 = 220.0;
    const SEGMENT_SECONDS: f64 = 30.0;
    let limit = (SEGMENT_TOKEN_LIMIT * window.as_secs_f64() / SEGMENT_SECONDS).ceil();
    limit.clamp(1.0, SEGMENT_TOKEN_LIMIT) as i32
}

fn transcribe_window(
    state: &mut WhisperState,
    samples: &[f32],
    parameters: &FullParams<'_, '_>,
) -> Result<String, WhisperBackendError> {
    state
        .full(parameters.clone(), samples)
        .map_err(|error| WhisperBackendError::Whisper(error.to_string()))?;

    let mut text = String::new();
    for segment in state.as_iter() {
        let segment = segment
            .to_str_lossy()
            .map_err(|error| WhisperBackendError::Whisper(error.to_string()))?;
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(segment);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::mpsc::{Receiver, channel},
        time::Duration,
    };

    use lcrt_core::AudioChunk;

    use super::{
        InputBacklog, WhisperTranscriber, WorkerCommand, audio_micros, drain_backlog,
        window_token_limit,
    };
    use crate::window::{InferenceKind, StreamingWindow};
    use crate::{WhisperBackendError, WhisperConfig};

    #[test]
    fn missing_model_fails_before_worker_start() {
        let config = WhisperConfig::new(PathBuf::from("/definitely/missing/lcrt-model.bin"));

        let error = match WhisperTranscriber::new(config) {
            Ok(_) => panic!("missing model unexpectedly loaded"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            WhisperBackendError::ModelUnavailable(PathBuf::from(
                "/definitely/missing/lcrt-model.bin"
            ))
        );
    }

    fn two_second_window() -> WhisperConfig {
        let mut config = WhisperConfig::new(std::env::temp_dir().join("unused-test-model.bin"));
        config.window_duration = Duration::from_secs(2);
        config.partial_step = Duration::from_millis(500);
        config.minimum_speech = Duration::from_millis(250);
        config.final_silence = Duration::from_millis(300);
        config.speech_rms_threshold = 0.01;
        config
    }

    fn audio(samples: usize, level: f32) -> AudioChunk {
        AudioChunk::new(vec![level; samples], 16_000, 1).unwrap()
    }

    fn queue(chunks: impl IntoIterator<Item = WorkerCommand>) -> Receiver<WorkerCommand> {
        let (sender, receiver) = channel();
        for chunk in chunks {
            sender.send(chunk).unwrap();
        }
        receiver
    }

    fn quarter_seconds_of_speech(count: usize) -> Vec<WorkerCommand> {
        (0..count)
            .map(|_| WorkerCommand::Audio(audio(4_000, 0.1)))
            .collect()
    }

    #[test]
    fn first_utterance_drain_stops_before_unseen_audio_is_evicted() {
        let mut window = StreamingWindow::new(&two_second_window()).unwrap();
        let commands = queue(quarter_seconds_of_speech(9));
        let backlog = InputBacklog::new(Duration::from_secs(8));

        let drained = drain_backlog(
            audio(4_000, 0.1),
            &commands,
            &backlog,
            &mut None,
            &mut window,
        )
        .unwrap();

        assert_eq!(drained.pending_kind, Some(InferenceKind::Partial));
        assert_eq!(window.samples().len(), 32_000);
        assert!(!window.rolled_since_inference());
        assert_eq!(drained.deferred.map(|samples| samples.len()), Some(4_000));
        assert_eq!(commands.try_iter().count(), 1);
    }

    #[test]
    fn a_chunk_that_would_evict_unseen_audio_is_deferred_before_appending() {
        let mut window = StreamingWindow::new(&two_second_window()).unwrap();
        window.push(&vec![0.1; 30_000]);
        let commands = queue([]);

        let drained = drain_backlog(
            audio(4_000, 0.1),
            &commands,
            &InputBacklog::new(Duration::from_secs(8)),
            &mut None,
            &mut window,
        )
        .unwrap();

        // 1.875 s unseen plus a 0.25 s chunk exceeds the 2 s window, so the
        // chunk waits for a pass instead of evicting the oldest unseen audio.
        assert!(drained.deferred.is_some());
        assert!(drained.pending_kind.is_some());
        assert!(!window.rolled_since_inference());
        assert_eq!(window.samples().len(), 30_000);
    }

    #[test]
    fn backlog_after_an_inferred_window_drains_past_a_due_partial() {
        let mut window = StreamingWindow::new(&two_second_window()).unwrap();
        window.push(&vec![0.1; 32_000]);
        window.mark_inferred(InferenceKind::Partial);
        let commands = queue(quarter_seconds_of_speech(9));
        let chunk_us = audio_micros(&audio(4_000, 0.1));
        let backlog = InputBacklog::new(Duration::from_millis(2_500));
        for _ in 0..10 {
            assert!(backlog.try_reserve(chunk_us));
        }

        let drained = drain_backlog(
            audio(4_000, 0.1),
            &commands,
            &backlog,
            &mut None,
            &mut window,
        )
        .unwrap();

        // A due partial after 0.5 s does not stop the drain; the whole 2 s
        // backlog is appended before one pass, and taken chunks are released.
        assert_eq!(drained.pending_kind, Some(InferenceKind::Partial));
        assert!(window.rolled_since_inference());
        assert!(drained.deferred.is_some());
        assert_eq!(commands.try_iter().count(), 1);
        assert!(backlog.try_reserve(9 * chunk_us));
        assert!(!backlog.try_reserve(1));
    }

    #[test]
    fn sparse_speech_forces_a_pass_before_unseen_audio_is_evicted() {
        let mut config = two_second_window();
        config.partial_step = Duration::from_secs(2);
        let mut window = StreamingWindow::new(&config).unwrap();
        let mut sparse = Vec::new();
        for _ in 0..7 {
            sparse.push(WorkerCommand::Audio(audio(800, 0.1)));
            sparse.push(WorkerCommand::Audio(audio(4_000, 0.0)));
        }
        let commands = queue(sparse);

        let drained = drain_backlog(
            audio(800, 0.1),
            &commands,
            &InputBacklog::new(Duration::from_secs(8)),
            &mut None,
            &mut window,
        )
        .unwrap();

        // Too little speech for the step gate, yet the window is full of
        // unseen audio, so a pass is forced rather than rolling it away.
        assert_eq!(drained.pending_kind, Some(InferenceKind::Partial));
        assert!(drained.deferred.is_some());
        assert!(!window.rolled_since_inference());
    }

    #[test]
    fn below_minimum_speech_is_never_forced_into_a_pass() {
        let mut window = StreamingWindow::new(&two_second_window()).unwrap();
        let mut spikes = Vec::new();
        for _ in 0..8 {
            spikes.push(WorkerCommand::Audio(audio(4_000, 0.0)));
            spikes.push(WorkerCommand::Audio(audio(400, 0.1)));
        }
        let commands = queue(spikes);

        let drained = drain_backlog(
            audio(400, 0.1),
            &commands,
            &InputBacklog::new(Duration::from_secs(8)),
            &mut None,
            &mut window,
        )
        .unwrap();

        // Short spikes separated by less than final silence keep the window
        // open, but their 0.225 s of speech never meets the 0.25 s gate, so
        // no pass is manufactured; the window rolls as before.
        assert_eq!(drained.pending_kind, None);
        assert!(drained.deferred.is_none());
        assert!(window.rolled_since_inference());
    }

    #[test]
    fn finish_request_during_drain_is_reported() {
        let mut window = StreamingWindow::new(&two_second_window()).unwrap();
        let commands = queue([
            WorkerCommand::Audio(audio(4_000, 0.1)),
            WorkerCommand::Finish,
        ]);

        let drained = drain_backlog(
            audio(4_000, 0.1),
            &commands,
            &InputBacklog::new(Duration::from_secs(8)),
            &mut None,
            &mut window,
        )
        .unwrap();

        assert!(drained.finish_requested);
        assert!(drained.deferred.is_none());
        assert_eq!(window.samples().len(), 8_000);
    }

    #[test]
    fn input_backlog_is_bounded_by_audio_duration_not_chunk_count() {
        let backlog = InputBacklog::new(Duration::from_secs(8));
        let quantum_1024 = audio_micros(&AudioChunk::new(vec![0.0; 1_024 * 2], 48_000, 2).unwrap());
        let quantum_2048 = audio_micros(&AudioChunk::new(vec![0.0; 2_048 * 2], 48_000, 2).unwrap());

        let accepted_small = (0..)
            .take_while(|_| backlog.try_reserve(quantum_1024))
            .count();
        for _ in 0..accepted_small {
            backlog.release(quantum_1024);
        }
        let accepted_large = (0..)
            .take_while(|_| backlog.try_reserve(quantum_2048))
            .count();

        // Both quanta admit the same 8 s of audio: 375 x 21.3 ms, 187 x 42.7 ms.
        assert_eq!(accepted_small, 375);
        assert_eq!(accepted_large, 187);
        backlog.release(quantum_2048);
        assert!(backlog.try_reserve(quantum_2048));
        assert!(!backlog.try_reserve(quantum_2048));
    }

    #[test]
    fn token_limit_matches_whisper_segment_density_for_the_window() {
        assert_eq!(window_token_limit(Duration::from_secs(30)), 220);
        assert_eq!(window_token_limit(Duration::from_secs(8)), 59);
        assert_eq!(window_token_limit(Duration::from_millis(1)), 1);
        assert_eq!(window_token_limit(Duration::from_secs(120)), 220);
    }
}
