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
        let worker = thread::Builder::new()
            .name("lcrt-whisper-stt".to_owned())
            .spawn(move || {
                let result = run_worker(
                    config,
                    command_receiver,
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
        match commands.try_send(WorkerCommand::Audio(chunk)) {
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
                let mut pending_kind = append_chunk(chunk, &mut converter, &mut window)?;
                let mut finish_requested = false;

                // Inference can be slower than one partial interval. Drain audio
                // captured during the previous pass and infer once over the
                // newest rolling window instead of repeatedly transcribing stale
                // intermediate windows while the bounded queue grows.
                // Preserve backlog coalescing, but never drain so far that the
                // first pending inference loses audio beyond the retained
                // window. Once full, inference must complete before more queued
                // input advances the rolling context.
                while !inference_due_before_drain(pending_kind, &window) {
                    match commands.try_recv() {
                        Ok(WorkerCommand::Audio(chunk)) => {
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
                        &config,
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
                        &config,
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
                    &config,
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
    config: &WhisperConfig,
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
            config,
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
    config: &WhisperConfig,
    events: &SyncSender<WorkerEvent>,
    transcript: &mut TranscriptAssembler,
    inference_count: &AtomicU64,
) -> Result<(), WhisperBackendError> {
    let started = Instant::now();
    let audio_duration_ms = window.samples().len() * 1_000 / 16_000;
    let text = transcribe_window(state, window.samples(), config, kind)?;
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
        || (pending_kind == Some(InferenceKind::Partial) && window.is_at_capacity())
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

fn transcribe_window(
    state: &mut WhisperState,
    samples: &[f32],
    config: &WhisperConfig,
    kind: InferenceKind,
) -> Result<String, WhisperBackendError> {
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
    // whisper.cpp otherwise re-decodes a rejected (typically repetitive)
    // result at up to five higher temperatures. On noisy live input that
    // multiplies one partial pass several-fold and overflows the bounded input
    // queue, while the next rolling-window pass re-decodes the same audio
    // anyway. A final pass has no later re-decode, so it keeps the fallback.
    if kind == InferenceKind::Partial {
        parameters.set_temperature_inc(0.0);
    }
    state
        .full(parameters, samples)
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

    use super::{
        BoundedSendError, WhisperTranscriber, inference_due_before_drain, send_with_backpressure,
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
        assert!(window.is_at_capacity());
        assert!(!window.rolled_since_inference());
    }
}
