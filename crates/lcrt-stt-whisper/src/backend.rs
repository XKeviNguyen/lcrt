use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel},
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
    commands: Option<SyncSender<WorkerCommand>>,
    events: Receiver<WorkerEvent>,
    worker: Option<JoinHandle<Result<(), WhisperBackendError>>>,
    cancel: Arc<AtomicBool>,
    input_queue_capacity: usize,
    backlog: Arc<InputBacklog>,
    finish_timeout: Duration,
    finished: bool,
    inference_count: Arc<AtomicU64>,
}

impl WhisperTranscriber {
    /// Loads a local model on a worker thread and waits for bounded readiness.
    pub fn new(config: WhisperConfig) -> Result<Self, WhisperBackendError> {
        config.validate()?;
        let input_queue_capacity = config.input_queue_capacity;
        let finish_timeout = config.finish_timeout;
        let startup_timeout = config.startup_timeout;
        let (commands, command_receiver) = sync_channel(input_queue_capacity);
        let (event_sender, events) = sync_channel(8);
        let (startup_sender, startup_receiver) = sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let inference_count = Arc::new(AtomicU64::new(0));
        let worker_inference_count = Arc::clone(&inference_count);
        let backlog = Arc::new(InputBacklog::new(config.window_duration));
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
                input_queue_capacity,
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
        let Some(sender) = self.commands.clone() else {
            return Err(TranscriptionError::new(
                "Whisper worker command channel is unavailable",
            ));
        };
        let reservation = self.reserve_backlog(&chunk)?;
        let mut updates = Vec::new();
        let result = send_with_backpressure(
            &sender,
            WorkerCommand::Audio(chunk),
            timeout,
            || -> Result<(), WhisperBackendError> {
                updates.extend(self.collect_available()?);
                Ok(())
            },
        );
        if result.is_err() {
            self.backlog.release(reservation);
        }
        match result {
            Ok(()) => {
                updates.extend(
                    self.collect_available()
                        .map_err(|error| TranscriptionError::new(error.to_string()))?,
                );
                Ok(updates)
            }
            Err(BoundedSendError::Timeout(_)) => Err(TranscriptionError::new(
                WhisperBackendError::InputQueueTimeout {
                    capacity: self.input_queue_capacity,
                    timeout,
                }
                .to_string(),
            )),
            Err(BoundedSendError::Disconnected(_)) => Err(TranscriptionError::new(
                "Whisper worker command channel disconnected unexpectedly",
            )),
            Err(BoundedSendError::Wait(error)) => Err(TranscriptionError::new(error.to_string())),
        }
    }

    /// Returns the number of successful local Whisper inference passes.
    pub fn inference_count(&self) -> u64 {
        self.inference_count.load(Ordering::Relaxed)
    }

    /// Reserves backlog room for `chunk`, returning the reserved microseconds.
    fn reserve_backlog(&self, chunk: &AudioChunk) -> Result<u64, TranscriptionError> {
        let duration_us = audio_micros(chunk);
        if self.backlog.try_reserve(duration_us) {
            Ok(duration_us)
        } else {
            Err(TranscriptionError::new(
                WhisperBackendError::InputBacklogFull(self.backlog.limit()).to_string(),
            ))
        }
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

    fn send_finish_until(&mut self, deadline: Instant) -> Result<(), WhisperBackendError> {
        let Some(sender) = self.commands.as_ref() else {
            return Ok(());
        };
        let mut command = WorkerCommand::Finish;
        loop {
            match sender.try_send(command) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                    command = returned;
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Full(_)) => {
                    return Err(WhisperBackendError::FinishTimeout(self.finish_timeout));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(WhisperBackendError::Worker(
                        "command channel disconnected before flush".to_owned(),
                    ));
                }
            }
        }
    }

    fn finish_worker(&mut self) -> Result<Vec<TranscriptUpdate>, WhisperBackendError> {
        if self.finished {
            let updates = self.collect_available()?;
            self.join_worker()?;
            return Ok(updates);
        }
        let deadline = Instant::now() + self.finish_timeout;
        self.send_finish_until(deadline)?;
        self.commands.take();
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
        let Some(commands) = self.commands.as_ref() else {
            return Err(TranscriptionError::new(
                "Whisper worker command channel is unavailable",
            ));
        };
        let reservation = self.reserve_backlog(&chunk)?;
        let sent = commands.try_send(WorkerCommand::Audio(chunk));
        if sent.is_err() {
            self.backlog.release(reservation);
        }
        match sent {
            Ok(()) => self
                .collect_available()
                .map_err(|error| TranscriptionError::new(error.to_string())),
            Err(TrySendError::Full(_)) => Err(TranscriptionError::new(
                WhisperBackendError::InputQueueFull(self.input_queue_capacity).to_string(),
            )),
            Err(TrySendError::Disconnected(_)) => Err(TranscriptionError::new(
                "Whisper worker command channel disconnected unexpectedly",
            )),
        }
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
                backlog.release(audio_micros(&chunk));
                let mut pending_kind = append_chunk(chunk, &mut converter, &mut window)?;
                let mut finish_requested = false;

                // Inference can be slower than one partial interval. Drain all
                // audio captured during the previous pass and infer once over
                // the newest rolling window, so backlog never carries over from
                // one pass to the next. Stop only before the window would evict
                // audio that no pass has inferred yet.
                while !inference_due_before_drain(pending_kind, &window) {
                    match commands.try_recv() {
                        Ok(WorkerCommand::Audio(chunk)) => {
                            backlog.release(audio_micros(&chunk));
                            if let Some(kind) = append_chunk(chunk, &mut converter, &mut window)? {
                                pending_kind = Some(kind);
                            }
                        }
                        Ok(WorkerCommand::Finish) => {
                            finish_requested = true;
                            break;
                        }
                        Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                    }
                }

                if finish_requested {
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
                if let Some(kind) = pending_kind {
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

fn append_chunk(
    chunk: AudioChunk,
    converter: &mut Option<AudioConverter>,
    window: &mut StreamingWindow,
) -> Result<Option<InferenceKind>, WhisperBackendError> {
    let converter = match converter.as_mut() {
        Some(converter) => converter,
        None => converter.insert(AudioConverter::new(&chunk)?),
    };
    let mono = converter.push(&chunk)?;
    Ok(window.push(&mono))
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

fn inference_due_before_drain(
    pending_kind: Option<InferenceKind>,
    window: &StreamingWindow,
) -> bool {
    pending_kind == Some(InferenceKind::Final)
        || (pending_kind == Some(InferenceKind::Partial) && window.uninferred_audio_fills_window())
}

enum BoundedSendError<T, E> {
    Timeout(T),
    Disconnected(T),
    Wait(E),
}

fn send_with_backpressure<T, E>(
    sender: &SyncSender<T>,
    mut value: T,
    timeout: Duration,
    mut wait: impl FnMut() -> Result<(), E>,
) -> Result<(), BoundedSendError<T, E>> {
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return Err(BoundedSendError::Timeout(value));
    };
    loop {
        match sender.try_send(value) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                value = returned;
                wait().map_err(BoundedSendError::Wait)?;
                if Instant::now() >= deadline {
                    return Err(BoundedSendError::Timeout(value));
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(returned)) => {
                return Err(BoundedSendError::Disconnected(returned));
            }
        }
    }
}

/// Captured audio accepted for transcription but not yet taken by the worker.
///
/// The limit is one rolling window of audio: the worker coalesces all pending
/// audio into the window before the next pass, and a larger backlog could not
/// be inferred without evicting audio no pass has seen. Bounding by duration
/// rather than chunk count keeps the limit independent of the audio quantum.
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
    use std::{path::PathBuf, sync::mpsc::sync_channel, thread, time::Duration};

    use lcrt_core::AudioChunk;

    use super::{
        BoundedSendError, InputBacklog, WhisperTranscriber, audio_micros,
        inference_due_before_drain, send_with_backpressure, window_token_limit,
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

    #[test]
    fn bounded_send_waits_for_queue_capacity() {
        let (sender, receiver) = sync_channel(1);
        sender.send(1_u8).unwrap();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            assert_eq!(receiver.recv().unwrap(), 1);
            assert_eq!(receiver.recv().unwrap(), 2);
        });
        let mut waits = 0;

        let result = send_with_backpressure(
            &sender,
            2,
            Duration::from_millis(100),
            || -> Result<(), ()> {
                waits += 1;
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert!(waits > 0);
        worker.join().unwrap();
    }

    #[test]
    fn bounded_send_times_out_when_queue_stays_full() {
        let (sender, _receiver) = sync_channel(1);
        sender.send(1_u8).unwrap();

        let result = send_with_backpressure(
            &sender,
            2,
            Duration::from_millis(5),
            || -> Result<(), ()> { Ok(()) },
        );

        assert!(matches!(result, Err(BoundedSendError::Timeout(2))));
    }

    #[test]
    fn backlog_drain_stops_before_a_pending_partial_loses_audio() {
        let mut config = WhisperConfig::new(std::env::temp_dir().join("unused-test-model.bin"));
        config.window_duration = Duration::from_secs(2);
        config.partial_step = Duration::from_millis(500);
        config.minimum_speech = Duration::from_millis(250);
        let mut window = StreamingWindow::new(&config).unwrap();
        let mut pending_kind = None;
        let mut drained_chunks = 0;

        while !inference_due_before_drain(pending_kind, &window) {
            if let Some(kind) = window.push(&vec![0.1; 4_000]) {
                pending_kind = Some(kind);
            }
            drained_chunks += 1;
        }

        assert_eq!(pending_kind, Some(InferenceKind::Partial));
        assert_eq!(drained_chunks, 8);
        assert_eq!(window.samples().len(), 32_000);
        assert!(!window.rolled_since_inference());
    }

    #[test]
    fn backlog_after_an_inferred_window_drains_until_unseen_audio_would_be_evicted() {
        let mut config = WhisperConfig::new(std::env::temp_dir().join("unused-test-model.bin"));
        config.window_duration = Duration::from_secs(2);
        config.partial_step = Duration::from_millis(500);
        config.minimum_speech = Duration::from_millis(250);
        let mut window = StreamingWindow::new(&config).unwrap();
        for _ in 0..8 {
            window.push(&vec![0.1; 4_000]);
        }
        window.mark_inferred(InferenceKind::Partial);
        let mut pending_kind = None;
        let mut drained_chunks = 0;

        while !inference_due_before_drain(pending_kind, &window) {
            if let Some(kind) = window.push(&vec![0.1; 4_000]) {
                pending_kind = Some(kind);
            }
            drained_chunks += 1;
        }

        // A due partial after only 0.5 s does not stop the drain; the pass
        // runs once the whole 2 s window holds audio no pass has seen.
        assert_eq!(pending_kind, Some(InferenceKind::Partial));
        assert_eq!(drained_chunks, 8);
        assert!(window.rolled_since_inference());
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
