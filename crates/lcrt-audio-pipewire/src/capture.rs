use std::{
    borrow::Cow,
    cell::Cell,
    mem,
    rc::Rc,
    sync::mpsc::{
        Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use lcrt_core::{
    AudioCapture, AudioCaptureError, AudioChunk, AudioInputEvent, AudioSourceDescriptor,
    AudioSourceKind,
};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::{
    param::audio::{AudioFormat, AudioInfoRaw},
    param::format::{MediaSubtype, MediaType},
    param::format_utils,
    pod::Pod,
};
use tracing::{info, warn};

use crate::PipeWireError;

/// Bounds PipeWire capture startup and buffering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipeWireCaptureConfig {
    /// Maximum number of PCM chunks waiting for the application pipeline.
    pub queue_capacity: usize,
    /// Maximum time to wait for PipeWire to enter the streaming state.
    pub startup_timeout: Duration,
    /// Maximum time to wait for the worker to stop after capture is stopped.
    pub shutdown_timeout: Duration,
    /// Maximum number of interleaved samples accepted in one PipeWire chunk.
    pub max_chunk_samples: usize,
}

impl Default for PipeWireCaptureConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 8,
            startup_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
            max_chunk_samples: 48_000,
        }
    }
}

impl PipeWireCaptureConfig {
    fn validate(&self) -> Result<(), PipeWireError> {
        if self.queue_capacity == 0 {
            return Err(PipeWireError::InvalidConfiguration(
                "queue capacity must be greater than zero",
            ));
        }
        if self.startup_timeout.is_zero() {
            return Err(PipeWireError::InvalidConfiguration(
                "startup timeout must be greater than zero",
            ));
        }
        if self.shutdown_timeout.is_zero() {
            return Err(PipeWireError::InvalidConfiguration(
                "shutdown timeout must be greater than zero",
            ));
        }
        if self.max_chunk_samples == 0 {
            return Err(PipeWireError::InvalidConfiguration(
                "maximum chunk samples must be greater than zero",
            ));
        }
        if self
            .max_chunk_samples
            .checked_mul(mem::size_of::<f32>())
            .is_none()
        {
            return Err(PipeWireError::InvalidConfiguration(
                "maximum chunk samples is too large",
            ));
        }
        Ok(())
    }
}

enum AudioEvent {
    Audio(AudioChunk),
}

enum ControlEvent {
    Failure(String),
    Stopped,
}

enum ControlPoll {
    Event(ControlEvent),
    Empty,
    Disconnected,
}

/// A live PipeWire session implementing the portable [`AudioCapture`] port.
///
/// Shutdown waits are bounded. If a worker misses that bound, the capture keeps
/// its join handle so a later [`AudioCapture::stop`] call can finish cleanup.
/// Dropping an instance whose worker remains permanently unresponsive still
/// detaches that Rust thread because stable Rust cannot safely terminate it.
pub struct PipeWireCapture {
    source: AudioSourceDescriptor,
    audio_events: Receiver<AudioEvent>,
    control_events: Receiver<ControlEvent>,
    stop_sender: Option<pw::channel::Sender<()>>,
    worker_done: Receiver<()>,
    worker: Option<JoinHandle<Result<(), PipeWireError>>>,
    shutdown_timeout: Duration,
    stop_requested: bool,
    stopped: bool,
    terminal: bool,
}

impl PipeWireCapture {
    /// Starts capture from an enumerated microphone or system-output source.
    pub fn start(
        source: AudioSourceDescriptor,
        config: PipeWireCaptureConfig,
    ) -> Result<Self, PipeWireError> {
        config.validate()?;
        let startup_started = Instant::now();
        let (audio_sender, audio_events) = sync_channel(config.queue_capacity);
        // Control events have their own one-item bounded channel. Audio may be
        // dropped under backpressure, but a terminal failure can never be
        // displaced by queued PCM.
        let (control_sender, control_events) = sync_channel(1);
        let (startup_sender, startup_receiver) = sync_channel(1);
        let (worker_done_sender, worker_done) = sync_channel(1);
        let (stop_sender, stop_receiver) = pw::channel::channel();
        let worker_source = source.clone();
        let worker = thread::Builder::new()
            .name("lcrt-pipewire-capture".to_owned())
            .spawn(move || {
                let result = run_worker(
                    worker_source,
                    audio_sender,
                    control_sender.clone(),
                    startup_sender.clone(),
                    stop_receiver,
                    config.max_chunk_samples,
                );
                if let Err(error) = &result {
                    let message = error.to_string();
                    let _ = startup_sender.try_send(Err(error.clone()));
                    let _ = control_sender.try_send(ControlEvent::Failure(message));
                }
                let _ = worker_done_sender.try_send(());
                result
            })
            .map_err(|error| PipeWireError::Worker(error.to_string()))?;

        match startup_receiver.recv_timeout(remaining_budget(
            config.startup_timeout,
            startup_started.elapsed(),
        )) {
            Ok(Ok(())) => {
                info!(source_id = source.id(), "PipeWire capture streaming");
                Ok(Self {
                    source,
                    audio_events,
                    control_events,
                    stop_sender: Some(stop_sender),
                    worker_done,
                    worker: Some(worker),
                    shutdown_timeout: config.shutdown_timeout,
                    stop_requested: false,
                    stopped: false,
                    terminal: false,
                })
            }
            Ok(Err(error)) => {
                let _ = stop_sender.send(());
                wait_for_worker(
                    &worker_done,
                    remaining_budget(config.startup_timeout, startup_started.elapsed()),
                );
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = stop_sender.send(());
                // Returning remains bounded by startup_timeout. If PipeWire is
                // too unresponsive to process the stop request within the
                // remaining budget, Rust provides no safe way to terminate the
                // worker; dropping this handle detaches that worker.
                wait_for_worker(
                    &worker_done,
                    remaining_budget(config.startup_timeout, startup_started.elapsed()),
                );
                Err(PipeWireError::StartupTimeout(config.startup_timeout))
            }
            Err(RecvTimeoutError::Disconnected) => {
                wait_for_worker(
                    &worker_done,
                    remaining_budget(config.startup_timeout, startup_started.elapsed()),
                );
                Err(PipeWireError::WorkerStopped)
            }
        }
    }

    fn shutdown(&mut self) -> Result<(), AudioCaptureError> {
        self.shutdown_with_timeout(self.shutdown_timeout)
    }

    fn shutdown_with_timeout(&mut self, wait_timeout: Duration) -> Result<(), AudioCaptureError> {
        if self.stopped {
            return Ok(());
        }
        if !self.stop_requested {
            self.stop_requested = true;
            if let Some(sender) = self.stop_sender.take() {
                let _ = sender.send(());
            }
        }
        if self.worker.is_none() {
            self.stopped = true;
            return Ok(());
        }
        match self.worker_done.recv_timeout(wait_timeout) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {
                return Err(AudioCaptureError::new(format!(
                    "PipeWire capture worker did not stop within {:?}; shutdown remains pending and may be retried",
                    wait_timeout
                )));
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The worker may have panicked before it could acknowledge
                // completion; joining now is non-blocking because its sender
                // has been dropped.
            }
        }
        let worker = self
            .worker
            .take()
            .expect("worker was present while awaiting completion");
        self.stopped = true;
        match worker.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(AudioCaptureError::new(error.to_string())),
            Err(_) => Err(AudioCaptureError::new(
                "PipeWire capture worker panicked during shutdown",
            )),
        }
    }
}

impl AudioCapture for PipeWireCapture {
    fn source(&self) -> &AudioSourceDescriptor {
        &self.source
    }

    fn next_event(&mut self, timeout: Duration) -> Result<AudioInputEvent, AudioCaptureError> {
        if self.stop_requested || self.stopped || self.terminal {
            return Ok(AudioInputEvent::EndOfStream);
        }
        match poll_control(&self.control_events) {
            ControlPoll::Event(event) => return consume_control(event, &mut self.terminal),
            ControlPoll::Disconnected => {
                return Err(AudioCaptureError::new(
                    "PipeWire capture control channel disconnected unexpectedly",
                ));
            }
            ControlPoll::Empty => {}
        }
        match self.audio_events.recv_timeout(timeout) {
            Ok(AudioEvent::Audio(chunk)) => Ok(AudioInputEvent::Chunk(chunk)),
            Err(RecvTimeoutError::Timeout) => Ok(AudioInputEvent::Idle),
            Err(RecvTimeoutError::Disconnected) => {
                result_after_audio_disconnect(&self.control_events, &mut self.terminal)
            }
        }
    }

    fn stop(&mut self) -> Result<(), AudioCaptureError> {
        self.shutdown()
    }
}

fn poll_control(control_events: &Receiver<ControlEvent>) -> ControlPoll {
    match control_events.try_recv() {
        Ok(event) => ControlPoll::Event(event),
        Err(TryRecvError::Empty) => ControlPoll::Empty,
        Err(TryRecvError::Disconnected) => ControlPoll::Disconnected,
    }
}

fn consume_control(
    event: ControlEvent,
    terminal: &mut bool,
) -> Result<AudioInputEvent, AudioCaptureError> {
    *terminal = true;
    match event {
        ControlEvent::Failure(message) => Err(AudioCaptureError::new(message)),
        ControlEvent::Stopped => Err(AudioCaptureError::new(
            "PipeWire capture stopped before the application requested shutdown",
        )),
    }
}

fn result_after_audio_disconnect(
    control_events: &Receiver<ControlEvent>,
    terminal: &mut bool,
) -> Result<AudioInputEvent, AudioCaptureError> {
    match poll_control(control_events) {
        ControlPoll::Event(event) => consume_control(event, terminal),
        ControlPoll::Empty | ControlPoll::Disconnected => Err(AudioCaptureError::new(
            "PipeWire capture audio channel disconnected unexpectedly",
        )),
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        let wait_timeout = drop_shutdown_timeout(self.stop_requested, self.shutdown_timeout);
        if let Err(error) = self.shutdown_with_timeout(wait_timeout) {
            warn!(%error, "failed to stop PipeWire capture while dropping adapter");
        }
    }
}

fn drop_shutdown_timeout(stop_requested: bool, configured_timeout: Duration) -> Duration {
    if stop_requested {
        // An explicit stop already consumed its bounded wait. Poll completion
        // once so Drop can join a finished worker, but never repeat the wait.
        Duration::ZERO
    } else {
        configured_timeout
    }
}

struct WorkerData {
    format: Option<NegotiatedFormat>,
    audio_events: SyncSender<AudioEvent>,
    startup: Option<SyncSender<Result<(), PipeWireError>>>,
    mainloop: pw::main_loop::MainLoopWeak,
    dropped_chunks: Rc<Cell<u64>>,
    corrupted_chunks: Rc<Cell<u64>>,
    terminal_sent: Rc<Cell<bool>>,
    control_events: SyncSender<ControlEvent>,
    stop_requested: Rc<Cell<bool>>,
}

#[derive(Clone, Copy)]
struct NegotiatedFormat {
    sample_rate_hz: u32,
    channels: u16,
}

fn run_worker(
    source: AudioSourceDescriptor,
    audio_events: SyncSender<AudioEvent>,
    control_events: SyncSender<ControlEvent>,
    startup: SyncSender<Result<(), PipeWireError>>,
    stop_receiver: pw::channel::Receiver<()>,
    max_chunk_samples: usize,
) -> Result<(), PipeWireError> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pipewire_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pipewire_error)?;
    let core = context.connect_rc(None).map_err(pipewire_error)?;
    let stop_requested = Rc::new(Cell::new(false));
    let _stop_listener = stop_receiver.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let stop_requested = Rc::clone(&stop_requested);
        move |_| {
            stop_requested.set(true);
            mainloop.quit();
        }
    });

    let stream_properties = stream_properties(&source);
    let stream = pw::stream::StreamBox::new(&core, "lcrt-audio-capture", stream_properties)
        .map_err(pipewire_error)?;
    let dropped_chunks = Rc::new(Cell::new(0));
    let corrupted_chunks = Rc::new(Cell::new(0));
    let terminal_sent = Rc::new(Cell::new(false));
    let max_chunk_bytes = max_chunk_samples.checked_mul(mem::size_of::<f32>()).ok_or(
        PipeWireError::InvalidConfiguration("maximum chunk samples is too large"),
    )?;
    let worker_data = WorkerData {
        format: None,
        audio_events,
        startup: Some(startup),
        mainloop: mainloop.downgrade(),
        dropped_chunks: Rc::clone(&dropped_chunks),
        corrupted_chunks: Rc::clone(&corrupted_chunks),
        terminal_sent: Rc::clone(&terminal_sent),
        control_events: control_events.clone(),
        stop_requested: Rc::clone(&stop_requested),
    };

    let _listener = stream
        .add_local_listener_with_user_data(worker_data)
        .state_changed(|_, data, old_state, new_state| match new_state {
            pw::stream::StreamState::Streaming => {
                if let Some(startup) = data.startup.take() {
                    let _ = startup.try_send(Ok(()));
                }
            }
            pw::stream::StreamState::Error(message) => {
                report_fatal(data, PipeWireError::StreamFailure(message));
            }
            pw::stream::StreamState::Unconnected
                if !data.stop_requested.get()
                    && !matches!(old_state, pw::stream::StreamState::Unconnected) =>
            {
                report_fatal(data, PipeWireError::SelectedSourceUnavailable);
            }
            _ => {}
        })
        .param_changed(|_, data, id, parameter| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(parameter) = parameter else {
                data.format = None;
                return;
            };
            let (media_type, subtype) = match format_utils::parse_format(parameter) {
                Ok(format) => format,
                Err(error) => {
                    report_fatal(
                        data,
                        PipeWireError::InvalidNegotiatedFormat(format!(
                            "failed to identify the format: {error}"
                        )),
                    );
                    return;
                }
            };
            if media_type != MediaType::Audio || subtype != MediaSubtype::Raw {
                report_fatal(
                    data,
                    PipeWireError::InvalidNegotiatedFormat("expected raw audio".to_owned()),
                );
                return;
            }
            let mut info = AudioInfoRaw::new();
            if let Err(error) = info.parse(parameter) {
                report_fatal(
                    data,
                    PipeWireError::InvalidNegotiatedFormat(format!(
                        "failed to parse the format: {error}"
                    )),
                );
                return;
            }
            match info.format() {
                AudioFormat::F32LE => match u16::try_from(info.channels()) {
                    Ok(channels) if channels > 0 && info.rate() > 0 => {
                        data.format = Some(NegotiatedFormat {
                            sample_rate_hz: info.rate(),
                            channels,
                        });
                    }
                    _ => report_fatal(
                        data,
                        PipeWireError::InvalidNegotiatedFormat(
                            "channel count or sample rate was zero or too large".to_owned(),
                        ),
                    ),
                },
                _ => report_fatal(
                    data,
                    PipeWireError::InvalidNegotiatedFormat(
                        "only interleaved F32LE PCM is supported".to_owned(),
                    ),
                ),
            }
        })
        .process(move |stream, data| {
            let Some(format) = data.format else {
                return;
            };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(plane) = buffer.datas_mut().first_mut() else {
                return;
            };
            let offset = plane.chunk().offset();
            let size = plane.chunk().size();
            let flags = plane.chunk().flags().bits();
            let max_size = match usize::try_from(plane.as_raw().maxsize) {
                Ok(max_size) => max_size,
                Err(error) => {
                    report_fatal(
                        data,
                        PipeWireError::MalformedBuffer(format!(
                            "mapped backing size cannot be represented: {error}"
                        )),
                    );
                    return;
                }
            };
            let layout = match chunk_layout(max_size, offset, size, flags, max_chunk_bytes) {
                Ok(layout) => layout,
                Err(error) => {
                    report_fatal(data, PipeWireError::MalformedBuffer(error));
                    return;
                }
            };
            let chunk = match layout {
                ChunkLayout::Corrupted => {
                    data.corrupted_chunks
                        .set(data.corrupted_chunks.get().saturating_add(1));
                    return;
                }
                ChunkLayout::NoMedia => return,
                ChunkLayout::Silence { byte_len } => match decode_silence(byte_len, format) {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        report_fatal(data, PipeWireError::MalformedBuffer(error));
                        return;
                    }
                },
                ChunkLayout::Payload { start, byte_len } => {
                    let Some(backing) = plane.data() else {
                        report_fatal(
                            data,
                            PipeWireError::MalformedBuffer(
                                "mapped PCM data was unavailable".to_owned(),
                            ),
                        );
                        return;
                    };
                    let bytes = match normalized_chunk_bytes(backing, max_size, start, byte_len) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            report_fatal(data, PipeWireError::MalformedBuffer(error));
                            return;
                        }
                    };
                    match decode_f32le(&bytes, format) {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            report_fatal(data, PipeWireError::MalformedBuffer(error));
                            return;
                        }
                    }
                }
            };
            match data.audio_events.try_send(AudioEvent::Audio(chunk)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => data
                    .dropped_chunks
                    .set(data.dropped_chunks.get().saturating_add(1)),
                Err(TrySendError::Disconnected(_)) => {
                    if let Some(mainloop) = data.mainloop.upgrade() {
                        mainloop.quit();
                    }
                }
            }
        })
        .register()
        .map_err(pipewire_error)?;

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|error| PipeWireError::PipeWire(format!("failed to serialize audio format: {error}")))?
    .0
    .into_inner();
    let mut parameters = [Pod::from_bytes(&values).ok_or_else(|| {
        PipeWireError::PipeWire("serialized audio format was not a valid SPA pod".to_owned())
    })?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            capture_stream_flags(),
            &mut parameters,
        )
        .map_err(pipewire_error)?;

    mainloop.run();
    let dropped_chunks = dropped_chunks.get();
    if dropped_chunks > 0 {
        warn!(
            dropped_chunks,
            "dropped PipeWire audio chunks because the bounded queue was full"
        );
    }
    let corrupted_chunks = corrupted_chunks.get();
    if corrupted_chunks > 0 {
        warn!(
            corrupted_chunks,
            "dropped PipeWire audio chunks marked as corrupted"
        );
    }
    if !terminal_sent.get() && !stop_requested.get() {
        let _ = control_events.try_send(ControlEvent::Stopped);
    }
    Ok(())
}

fn report_fatal(data: &mut WorkerData, error: PipeWireError) {
    if let Some(startup) = data.startup.take() {
        let _ = startup.try_send(Err(error.clone()));
    }
    try_send_terminal_failure(&data.control_events, &data.terminal_sent, error.to_string());
    if let Some(mainloop) = data.mainloop.upgrade() {
        mainloop.quit();
    }
}

fn try_send_terminal_failure(
    control_events: &SyncSender<ControlEvent>,
    terminal_sent: &Cell<bool>,
    message: String,
) {
    if !terminal_sent.replace(true) {
        // This control channel is distinct from the lossy bounded audio queue.
        // A single worker emits at most one terminal event, so its capacity is
        // sufficient without blocking a PipeWire callback.
        let _ = control_events.try_send(ControlEvent::Failure(message));
    }
}

fn stream_properties(source: &AudioSourceDescriptor) -> pw::properties::PropertiesBox {
    let mut properties = properties! {
        *pw::keys::APP_NAME => "LCRT",
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::TARGET_OBJECT => source.id(),
        // PipeWire documents this as destroying the node when its target is
        // removed instead of reconnecting it to a default compatible source.
        *pw::keys::NODE_DONT_RECONNECT => "true",
    };
    if source.kind() == AudioSourceKind::SystemOutput {
        properties.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    properties
}

fn capture_stream_flags() -> pw::stream::StreamFlags {
    pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::DONT_RECONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
}

fn wait_for_worker(worker_done: &Receiver<()>, timeout: Duration) {
    // A failed startup must not turn an externally bounded start call into an
    // unbounded join. Once the completion signal arrives, dropping the handle
    // is safe because the worker has already exited.
    let _ = worker_done.recv_timeout(timeout);
}

const SPA_CHUNK_FLAG_EMPTY: i32 = 1 << 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChunkLayout {
    Corrupted,
    NoMedia,
    Silence { byte_len: usize },
    Payload { start: usize, byte_len: usize },
}

fn chunk_layout(
    max_size: usize,
    offset: u32,
    size: u32,
    flags: i32,
    max_chunk_bytes: usize,
) -> Result<ChunkLayout, String> {
    if flags & spa::buffer::ChunkFlags::CORRUPTED.bits() != 0 {
        return Ok(ChunkLayout::Corrupted);
    }
    if size == 0 {
        return Ok(ChunkLayout::NoMedia);
    }
    if max_size == 0 {
        return Err("chunk contains media but its maximum backing size is zero".to_owned());
    }

    let size = usize::try_from(size)
        .map_err(|_| "chunk size cannot be represented on this platform".to_owned())?
        .min(max_size);
    if size > max_chunk_bytes {
        return Err(format!(
            "clamped chunk size {size} exceeds configured maximum {max_chunk_bytes} bytes"
        ));
    }
    // libspa 0.10 exposes raw chunk flags but does not name EMPTY. The
    // supported SPA headers define bit 1 as SPA_CHUNK_FLAG_EMPTY. For F32LE
    // audio its clamped size represents neutral PCM duration, not stale bytes.
    if flags & SPA_CHUNK_FLAG_EMPTY != 0 {
        return Ok(ChunkLayout::Silence { byte_len: size });
    }

    let offset = usize::try_from(offset)
        .map_err(|_| "chunk offset cannot be represented on this platform".to_owned())?;
    Ok(ChunkLayout::Payload {
        start: offset % max_size,
        byte_len: size,
    })
}

fn normalized_chunk_bytes(
    backing: &[u8],
    max_size: usize,
    start: usize,
    byte_len: usize,
) -> Result<Cow<'_, [u8]>, String> {
    if backing.len() != max_size {
        return Err(format!(
            "mapped backing length {} does not match advertised maximum size {max_size}",
            backing.len()
        ));
    }
    if max_size == 0 || start >= max_size || byte_len > max_size {
        return Err("normalized chunk range is inconsistent with its backing buffer".to_owned());
    }

    let end = start
        .checked_add(byte_len)
        .ok_or_else(|| "normalized chunk range overflowed".to_owned())?;
    if end <= max_size {
        return Ok(Cow::Borrowed(&backing[start..end]));
    }

    let wrapped_end = end - max_size;
    let mut normalized = Vec::with_capacity(byte_len);
    normalized.extend_from_slice(&backing[start..]);
    normalized.extend_from_slice(&backing[..wrapped_end]);
    Ok(Cow::Owned(normalized))
}

fn decode_silence(byte_len: usize, format: NegotiatedFormat) -> Result<AudioChunk, String> {
    let sample_size = mem::size_of::<f32>();
    if !byte_len.is_multiple_of(sample_size) {
        return Err("PipeWire returned an EMPTY chunk with a partial F32 sample".to_owned());
    }
    let sample_count = byte_len / sample_size;
    let channels = usize::from(format.channels);
    if !sample_count.is_multiple_of(channels) {
        return Err(format!(
            "PipeWire returned an EMPTY chunk whose {sample_count} samples do not form complete {}-channel frames",
            format.channels
        ));
    }
    AudioChunk::new(
        vec![0.0; sample_count],
        format.sample_rate_hz,
        format.channels,
    )
    .map_err(|error| format!("PipeWire produced invalid silent PCM: {error}"))
}

fn remaining_budget(limit: Duration, elapsed: Duration) -> Duration {
    limit.saturating_sub(elapsed)
}

fn decode_f32le(bytes: &[u8], format: NegotiatedFormat) -> Result<AudioChunk, String> {
    if !bytes.len().is_multiple_of(mem::size_of::<f32>()) {
        return Err("PipeWire returned a partial F32 sample".to_owned());
    }
    let samples = bytes
        .as_chunks::<{ mem::size_of::<f32>() }>()
        .0
        .iter()
        .map(|sample| f32::from_le_bytes(*sample))
        .collect();
    AudioChunk::new(samples, format.sample_rate_hz, format.channels)
        .map_err(|error| format!("PipeWire produced invalid PCM: {error}"))
}

fn pipewire_error(error: pw::Error) -> PipeWireError {
    PipeWireError::PipeWire(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        sync::mpsc::{RecvTimeoutError, sync_channel},
        thread,
        time::Duration,
    };

    use super::{
        AudioEvent, ChunkLayout, ControlEvent, NegotiatedFormat, PipeWireCapture,
        PipeWireCaptureConfig, SPA_CHUNK_FLAG_EMPTY, capture_stream_flags, chunk_layout,
        decode_f32le, decode_silence, drop_shutdown_timeout, normalized_chunk_bytes,
        remaining_budget, result_after_audio_disconnect, stream_properties,
        try_send_terminal_failure,
    };
    use crate::PipeWireError;
    use lcrt_core::{AudioChunk, AudioSourceDescriptor, AudioSourceKind};

    #[test]
    fn config_rejects_an_unbounded_zero_capacity_queue() {
        let config = PipeWireCaptureConfig {
            queue_capacity: 0,
            ..PipeWireCaptureConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(PipeWireError::InvalidConfiguration(
                "queue capacity must be greater than zero"
            ))
        );
    }

    #[test]
    fn config_rejects_zero_startup_timeout() {
        let config = PipeWireCaptureConfig {
            startup_timeout: Duration::ZERO,
            ..PipeWireCaptureConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(PipeWireError::InvalidConfiguration(
                "startup timeout must be greater than zero"
            ))
        );
    }

    #[test]
    fn config_rejects_zero_shutdown_timeout() {
        let config = PipeWireCaptureConfig {
            shutdown_timeout: Duration::ZERO,
            ..PipeWireCaptureConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(PipeWireError::InvalidConfiguration(
                "shutdown timeout must be greater than zero"
            ))
        );
    }

    #[test]
    fn config_rejects_an_unbounded_zero_chunk_limit() {
        let config = PipeWireCaptureConfig {
            max_chunk_samples: 0,
            ..PipeWireCaptureConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(PipeWireError::InvalidConfiguration(
                "maximum chunk samples must be greater than zero"
            ))
        );
    }

    #[test]
    fn selected_source_is_pinned_and_never_reconnected() {
        let source = AudioSourceDescriptor::new(
            "alsa_input.test",
            "Test microphone",
            AudioSourceKind::Microphone,
        );
        let properties = stream_properties(&source);

        assert_eq!(
            properties.get(*pipewire::keys::TARGET_OBJECT),
            Some("alsa_input.test")
        );
        assert_eq!(
            properties.get(*pipewire::keys::NODE_DONT_RECONNECT),
            Some("true")
        );
        assert!(capture_stream_flags().contains(pipewire::stream::StreamFlags::DONT_RECONNECT));
        assert!(
            PipeWireError::SelectedSourceUnavailable
                .to_string()
                .contains("select another source")
        );
    }

    #[test]
    fn normalizes_a_contiguous_chunk_range() {
        let layout = chunk_layout(4, 1, 2, 0, usize::MAX).unwrap();
        assert_eq!(
            layout,
            ChunkLayout::Payload {
                start: 1,
                byte_len: 2
            }
        );
        let bytes = normalized_chunk_bytes(&[0, 1, 2, 3], 4, 1, 2).unwrap();
        assert_eq!(bytes.as_ref(), &[1, 2]);
    }

    #[test]
    fn normalizes_large_offset_and_reconstructs_wrapped_chunk_range() {
        assert_eq!(
            chunk_layout(4, u32::MAX, 3, 0, usize::MAX).unwrap(),
            ChunkLayout::Payload {
                start: 3,
                byte_len: 3
            }
        );
        let bytes = normalized_chunk_bytes(&[0, 1, 2, 3], 4, 3, 3).unwrap();
        assert_eq!(bytes.as_ref(), &[3, 0, 1]);
    }

    #[test]
    fn clamps_oversized_contiguous_and_wrapped_chunk_ranges() {
        assert_eq!(
            chunk_layout(4, 0, 5, 0, usize::MAX).unwrap(),
            ChunkLayout::Payload {
                start: 0,
                byte_len: 4
            }
        );
        assert_eq!(
            normalized_chunk_bytes(&[0, 1, 2, 3], 4, 0, 4)
                .unwrap()
                .as_ref(),
            &[0, 1, 2, 3]
        );
        assert_eq!(
            chunk_layout(4, 3, u32::MAX, 0, usize::MAX).unwrap(),
            ChunkLayout::Payload {
                start: 3,
                byte_len: 4
            }
        );
        assert_eq!(
            normalized_chunk_bytes(&[0, 1, 2, 3], 4, 3, 4)
                .unwrap()
                .as_ref(),
            &[3, 0, 1, 2]
        );
    }

    #[test]
    fn handles_zero_size_and_rejects_empty_backing_with_media() {
        assert_eq!(
            chunk_layout(0, u32::MAX, 0, 0, usize::MAX).unwrap(),
            ChunkLayout::NoMedia
        );
        assert_eq!(
            chunk_layout(0, 0, 1, 0, usize::MAX),
            Err("chunk contains media but its maximum backing size is zero".to_owned())
        );
    }

    #[test]
    fn empty_chunk_becomes_bounded_neutral_pcm_without_reading_backing_bytes() {
        let layout = chunk_layout(16, u32::MAX, 8, SPA_CHUNK_FLAG_EMPTY, usize::MAX).unwrap();
        assert_eq!(layout, ChunkLayout::Silence { byte_len: 8 });
        let chunk = decode_silence(
            8,
            NegotiatedFormat {
                sample_rate_hz: 48_000,
                channels: 2,
            },
        )
        .unwrap();

        assert_eq!(chunk.samples(), &[0.0, 0.0]);
        assert_eq!(chunk.frame_count(), 1);
    }

    #[test]
    fn oversized_empty_chunk_is_rejected_before_silence_allocation() {
        assert_eq!(
            chunk_layout(
                u32::MAX as usize,
                0,
                u32::MAX,
                SPA_CHUNK_FLAG_EMPTY,
                48_000 * std::mem::size_of::<f32>(),
            ),
            Err("clamped chunk size 4294967295 exceeds configured maximum 192000 bytes".to_owned())
        );
    }

    #[test]
    fn empty_chunk_requires_complete_samples_and_frames() {
        assert_eq!(
            decode_silence(
                3,
                NegotiatedFormat {
                    sample_rate_hz: 48_000,
                    channels: 1,
                }
            ),
            Err("PipeWire returned an EMPTY chunk with a partial F32 sample".to_owned())
        );
        assert_eq!(
            decode_silence(
                4,
                NegotiatedFormat {
                    sample_rate_hz: 48_000,
                    channels: 2,
                }
            ),
            Err(
                "PipeWire returned an EMPTY chunk whose 1 samples do not form complete 2-channel frames"
                    .to_owned()
            )
        );
    }

    #[test]
    fn corrupted_chunk_is_dropped_without_becoming_a_terminal_error() {
        assert_eq!(
            chunk_layout(
                4,
                0,
                4,
                pipewire::spa::buffer::ChunkFlags::CORRUPTED.bits(),
                usize::MAX,
            )
            .unwrap(),
            ChunkLayout::Corrupted
        );
    }

    #[test]
    fn malformed_backing_relationship_is_rejected() {
        assert_eq!(
            normalized_chunk_bytes(&[0, 1, 2], 4, 0, 2),
            Err("mapped backing length 3 does not match advertised maximum size 4".to_owned())
        );
    }

    #[test]
    fn fatal_control_event_survives_a_full_audio_queue() {
        let (audio_sender, _audio_receiver) = sync_channel(1);
        let (control_sender, control_receiver) = sync_channel(1);
        audio_sender
            .try_send(AudioEvent::Audio(
                AudioChunk::new(vec![0.0], 48_000, 1).unwrap(),
            ))
            .unwrap();
        control_sender
            .try_send(ControlEvent::Failure(
                "selected source disconnected".to_owned(),
            ))
            .unwrap();

        match control_receiver.try_recv().unwrap() {
            ControlEvent::Failure(message) => assert_eq!(message, "selected source disconnected"),
            ControlEvent::Stopped => panic!("expected fatal failure"),
        }
    }

    #[test]
    fn queued_terminal_failure_wins_after_blocked_audio_receive_disconnects() {
        let (audio_sender, audio_receiver) = sync_channel::<AudioEvent>(1);
        let (control_sender, control_receiver) = sync_channel(1);
        control_sender
            .try_send(ControlEvent::Failure(
                "selected source disconnected".to_owned(),
            ))
            .unwrap();
        drop(audio_sender);

        assert!(matches!(
            audio_receiver.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        ));
        let mut terminal = false;
        let error = result_after_audio_disconnect(&control_receiver, &mut terminal).unwrap_err();

        assert_eq!(error.to_string(), "selected source disconnected");
        assert!(terminal);
    }

    #[test]
    fn terminal_failure_is_emitted_exactly_once() {
        let (control_sender, control_receiver) = sync_channel(1);
        let terminal_sent = Cell::new(false);

        try_send_terminal_failure(&control_sender, &terminal_sent, "first failure".to_owned());
        try_send_terminal_failure(&control_sender, &terminal_sent, "second failure".to_owned());

        match control_receiver.try_recv().unwrap() {
            ControlEvent::Failure(message) => assert_eq!(message, "first failure"),
            ControlEvent::Stopped => panic!("expected fatal failure"),
        }
        assert!(control_receiver.try_recv().is_err());
    }

    #[test]
    fn startup_cleanup_uses_only_the_remaining_startup_budget() {
        assert_eq!(
            remaining_budget(Duration::from_secs(5), Duration::from_secs(3)),
            Duration::from_secs(2)
        );
        assert_eq!(
            remaining_budget(Duration::from_secs(5), Duration::from_secs(5)),
            Duration::ZERO
        );
        assert_eq!(
            remaining_budget(Duration::from_secs(5), Duration::from_secs(9)),
            Duration::ZERO
        );
    }

    #[test]
    fn startup_channel_preserves_typed_failure() {
        let (startup_sender, startup_receiver) = sync_channel::<Result<(), PipeWireError>>(1);
        startup_sender
            .try_send(Err(PipeWireError::SelectedSourceUnavailable))
            .unwrap();

        assert_eq!(
            startup_receiver.try_recv().unwrap(),
            Err(PipeWireError::SelectedSourceUnavailable)
        );
    }

    #[test]
    fn drop_does_not_repeat_an_exhausted_shutdown_wait() {
        assert_eq!(
            drop_shutdown_timeout(true, Duration::from_secs(5)),
            Duration::ZERO
        );
        assert_eq!(
            drop_shutdown_timeout(false, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn timed_out_shutdown_remains_retryable_until_worker_is_joined() {
        let (_audio_sender, audio_events) = sync_channel(1);
        let (_control_sender, control_events) = sync_channel(1);
        let (worker_done_sender, worker_done) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);
        let (completion_sender, completion_receiver) = sync_channel(1);
        let worker = thread::spawn(move || {
            release_receiver.recv().unwrap();
            worker_done_sender.try_send(()).unwrap();
            completion_sender.try_send(()).unwrap();
            Ok(())
        });
        let mut capture = PipeWireCapture {
            source: AudioSourceDescriptor::new(
                "test-source",
                "Test source",
                AudioSourceKind::Microphone,
            ),
            audio_events,
            control_events,
            stop_sender: None,
            worker_done,
            worker: Some(worker),
            shutdown_timeout: Duration::ZERO,
            stop_requested: false,
            stopped: false,
            terminal: false,
        };

        let first_error = capture.shutdown().unwrap_err();
        assert!(first_error.to_string().contains("shutdown remains pending"));
        assert!(capture.stop_requested);
        assert!(!capture.stopped);
        assert!(capture.worker.is_some());

        release_sender.try_send(()).unwrap();
        completion_receiver.recv().unwrap();
        capture.shutdown().unwrap();
        capture.shutdown().unwrap();
        assert!(capture.stopped);
        assert!(capture.worker.is_none());
    }

    #[test]
    fn decoder_preserves_f32le_samples_and_format() {
        let bytes = [0.25_f32.to_le_bytes(), (-0.5_f32).to_le_bytes()].concat();
        let chunk = decode_f32le(
            &bytes,
            NegotiatedFormat {
                sample_rate_hz: 48_000,
                channels: 1,
            },
        )
        .unwrap();

        assert_eq!(chunk.samples(), &[0.25, -0.5]);
        assert_eq!(chunk.sample_rate_hz(), 48_000);
        assert_eq!(chunk.channels(), 1);
    }

    #[test]
    fn decoder_rejects_partial_f32_sample() {
        let error = decode_f32le(
            &[0, 1, 2],
            NegotiatedFormat {
                sample_rate_hz: 48_000,
                channels: 1,
            },
        )
        .unwrap_err();

        assert_eq!(error, "PipeWire returned a partial F32 sample");
    }
}
