//! A realtime service session behind the portable [`Transcriber`] port.
//!
//! One worker thread owns at most one connection at a time. Audio reaches it
//! through a bounded queue and caption updates return through another, so a
//! slow network can neither block capture nor grow memory.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use lcrt_core::{AudioChunk, AudioConverter, Transcriber, TranscriptUpdate, TranscriptionError};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::{
    audio::ONLINE_SAMPLE_RATE,
    credentials::ApiKey,
    protocol::{EventOutcome, Protocol, ServiceErrorImpact},
    transport::{Connect, Transport, TransportError},
};

/// Audio blocks waiting for the network (about 2 s at typical quanta).
const AUDIO_QUEUE_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 64;

/// Connection progress shown to the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnlineStatus {
    /// Opening the first connection.
    Connecting,
    /// Connected and processing audio.
    Active,
    /// The connection dropped; trying again.
    Reconnecting,
}

/// Receives connection progress from the worker thread.
pub type StatusCallback = Arc<dyn Fn(OnlineStatus) + Send + Sync>;

/// User-facing text for a failed online session.
pub fn user_message(error: &TransportError) -> &'static str {
    match error {
        TransportError::Unauthorized => "Your OpenAI API key was rejected.",
        TransportError::Forbidden => {
            "Your OpenAI API key doesn't have access to this online service."
        }
        TransportError::RateLimited => {
            "The online service is temporarily rate limited. Try again shortly."
        }
        TransportError::Unreachable(_) => "Can't reach the online service. Check your connection.",
        TransportError::Closed => "Connection was lost.",
        TransportError::Protocol(_) => "The online service sent an unexpected response.",
    }
}

/// Timing bounds for one session; tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct SessionLimits {
    /// Longest wait for the service to acknowledge a new connection.
    pub handshake: Duration,
    /// Longest wait for final results after Stop.
    pub finish: Duration,
    /// Receive poll interval between outbound audio batches.
    pub poll: Duration,
    /// Backoff before each consecutive reconnect attempt.
    pub reconnect_delays: [Duration; 3],
    /// Reconnects allowed in one session.
    pub max_reconnects: u32,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(10),
            finish: Duration::from_secs(8),
            poll: Duration::from_millis(20),
            reconnect_delays: [
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
            ],
            max_reconnects: 10,
        }
    }
}

enum Command {
    Audio(Vec<f32>),
    Finish,
}

enum WorkerEvent {
    Update(TranscriptUpdate),
    Failed(TransportError),
    Done,
}

/// A running online session. Dropping it cancels the worker.
pub struct OnlineSession {
    commands: Option<SyncSender<Command>>,
    events: Receiver<WorkerEvent>,
    worker: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    converter: Option<AudioConverter>,
    finish_timeout: Duration,
    finished: bool,
    dropped_blocks: u64,
}

impl OnlineSession {
    /// Starts the worker; it connects in the background and reports progress
    /// through `status`.
    pub fn start<P: Protocol + 'static>(
        protocol: P,
        key: ApiKey,
        connector: Box<dyn Connect>,
        status: StatusCallback,
        limits: SessionLimits,
    ) -> Result<Self, TranscriptionError> {
        let (commands, command_receiver) = sync_channel(AUDIO_QUEUE_CAPACITY);
        let (event_sender, events) = sync_channel(EVENT_QUEUE_CAPACITY);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker = thread::Builder::new()
            .name("lcrt-online-session".to_owned())
            .spawn(move || {
                let mut worker = Worker {
                    protocol,
                    key,
                    connector,
                    commands: command_receiver,
                    events: event_sender,
                    cancel: worker_cancel,
                    status,
                    limits,
                    finishing: false,
                };
                worker.run();
            })
            .map_err(|error| {
                TranscriptionError::new(format!("could not start the online session: {error}"))
            })?;
        Ok(Self {
            commands: Some(commands),
            events,
            worker: Some(worker),
            cancel,
            converter: None,
            finish_timeout: limits.finish + limits.handshake,
            finished: false,
            dropped_blocks: 0,
        })
    }

    fn collect(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        let mut updates = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(WorkerEvent::Update(update)) => updates.push(update),
                Ok(WorkerEvent::Failed(error)) => {
                    self.finished = true;
                    return Err(TranscriptionError::new(user_message(&error)));
                }
                Ok(WorkerEvent::Done) => {
                    self.finished = true;
                    break;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        Ok(updates)
    }

    fn convert(&mut self, chunk: &AudioChunk) -> Result<Vec<f32>, TranscriptionError> {
        let converter = match self.converter.as_mut() {
            Some(converter) => converter,
            None => self.converter.insert(
                AudioConverter::new(chunk, ONLINE_SAMPLE_RATE)
                    .map_err(|error| TranscriptionError::new(error.to_string()))?,
            ),
        };
        converter
            .push(chunk)
            .map_err(|error| TranscriptionError::new(error.to_string()))
    }
}

impl Transcriber for OnlineSession {
    fn push_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        if self.finished {
            return self.collect().and_then(|_| {
                Err(TranscriptionError::new(
                    "The online session has already ended.",
                ))
            });
        }
        let samples = self.convert(&chunk)?;
        if !samples.is_empty()
            && let Some(commands) = &self.commands
        {
            match commands.try_send(Command::Audio(samples)) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(_)) => {
                    // The network is behind; live captions drop stale audio
                    // instead of buffering it.
                    self.dropped_blocks += 1;
                    if self.dropped_blocks.is_power_of_two() {
                        warn!(
                            dropped_blocks = self.dropped_blocks,
                            "online audio queue full"
                        );
                    }
                }
            }
        }
        self.collect()
    }

    fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        let mut updates = self.collect()?;
        if self.finished {
            return Ok(updates);
        }
        let tail = match self.converter.as_mut() {
            Some(converter) => converter
                .finish()
                .map_err(|error| TranscriptionError::new(error.to_string()))?,
            None => Vec::new(),
        };
        let deadline = Instant::now() + self.finish_timeout;
        if let Some(commands) = self.commands.take() {
            for command in [Command::Audio(tail), Command::Finish] {
                let mut command = command;
                loop {
                    match commands.try_send(command) {
                        Ok(()) | Err(TrySendError::Disconnected(_)) => break,
                        Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                            command = returned;
                            updates.extend(self.collect()?);
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(TrySendError::Full(_)) => break,
                    }
                }
            }
        }
        while !self.finished {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!("online session did not finish in time; closing it locally");
                self.cancel.store(true, Ordering::Release);
                break;
            }
            match self.events.recv_timeout(remaining) {
                Ok(WorkerEvent::Update(update)) => updates.push(update),
                Ok(WorkerEvent::Failed(error)) => {
                    self.finished = true;
                    return Err(TranscriptionError::new(user_message(&error)));
                }
                Ok(WorkerEvent::Done) | Err(RecvTimeoutError::Disconnected) => {
                    self.finished = true;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if let Some(worker) = self.worker.take()
            && worker.is_finished()
        {
            let _ = worker.join();
        }
        Ok(updates)
    }
}

impl Drop for OnlineSession {
    fn drop(&mut self) {
        // Cancel promptly; the worker checks the flag at every poll and
        // closes its connection itself, so no join is needed here.
        self.cancel.store(true, Ordering::Release);
        self.commands.take();
    }
}

#[derive(Deserialize)]
struct EventType {
    #[serde(rename = "type")]
    kind: String,
}

struct Worker<P> {
    protocol: P,
    key: ApiKey,
    connector: Box<dyn Connect>,
    commands: Receiver<Command>,
    events: SyncSender<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    status: StatusCallback,
    limits: SessionLimits,
    finishing: bool,
}

impl<P: Protocol> Worker<P> {
    fn run(&mut self) {
        let mut consecutive_failures = 0_usize;
        let mut reconnects = 0_u32;
        loop {
            if self.cancelled() {
                return;
            }
            (self.status)(if reconnects == 0 && consecutive_failures == 0 {
                OnlineStatus::Connecting
            } else {
                OnlineStatus::Reconnecting
            });
            let result = self.run_connection(&mut consecutive_failures);
            match result {
                Ok(()) => break,
                Err(error)
                    if error.is_transient()
                        && !self.finishing
                        && consecutive_failures < self.limits.reconnect_delays.len()
                        && reconnects < self.limits.max_reconnects =>
                {
                    let delay = self.limits.reconnect_delays[consecutive_failures];
                    consecutive_failures += 1;
                    reconnects += 1;
                    info!(
                        attempt = consecutive_failures,
                        ?delay,
                        "online connection lost; reconnecting"
                    );
                    self.protocol.reset_connection();
                    if !self.wait_before_reconnect(delay) {
                        break;
                    }
                }
                Err(error) if self.finishing && error.is_transient() => {
                    warn!(%error, "online connection lost while stopping");
                    break;
                }
                Err(error) => {
                    warn!(%error, "online session failed");
                    let _ = self.events.send(WorkerEvent::Failed(error));
                    break;
                }
            }
        }
        let _ = self.events.send(WorkerEvent::Done);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Sleeps for `delay` unless Stop or cancellation arrives first. Audio
    /// captured during the outage is discarded. Returns whether to reconnect.
    fn wait_before_reconnect(&mut self, delay: Duration) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            if self.cancelled() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            match self
                .commands
                .recv_timeout(remaining.min(self.limits.poll * 5))
            {
                Ok(Command::Audio(_)) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Command::Finish) | Err(RecvTimeoutError::Disconnected) => {
                    self.finishing = true;
                    return false;
                }
            }
        }
    }

    fn run_connection(&mut self, consecutive_failures: &mut usize) -> Result<(), TransportError> {
        let started = Instant::now();
        let mut transport = self.connector.connect(&self.protocol.url(), &self.key)?;
        let result = self.drive(transport.as_mut(), started, consecutive_failures);
        transport.close();
        result
    }

    fn drive(
        &mut self,
        transport: &mut dyn Transport,
        started: Instant,
        consecutive_failures: &mut usize,
    ) -> Result<(), TransportError> {
        self.await_session_created(transport)?;
        for message in self.protocol.configure() {
            transport.send_text(&message)?;
        }
        info!(
            connect_ms = started.elapsed().as_millis(),
            "online session ready"
        );
        *consecutive_failures = 0;
        (self.status)(OnlineStatus::Active);

        let mut finish_deadline = None;
        loop {
            if self.cancelled() {
                return Ok(());
            }
            self.send_pending_audio(transport, &mut finish_deadline)?;
            if self.finishing && self.protocol.is_drained() {
                return Ok(());
            }
            if finish_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                warn!("online service did not confirm the end of the session in time");
                return Ok(());
            }
            if let Some(text) = transport.receive(self.limits.poll)? {
                self.handle_event(&text)?;
            }
        }
    }

    fn await_session_created(
        &mut self,
        transport: &mut dyn Transport,
    ) -> Result<(), TransportError> {
        let deadline = Instant::now() + self.limits.handshake;
        loop {
            if self.cancelled() {
                return Err(TransportError::Closed);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TransportError::Unreachable(
                    "no session acknowledgement".to_owned(),
                ));
            }
            let Some(text) = transport.receive(remaining.min(self.limits.poll * 5))? else {
                continue;
            };
            match serde_json::from_str::<EventType>(&text).map(|event| event.kind) {
                Ok(kind) if kind == "session.created" => return Ok(()),
                Ok(kind) if kind == "error" => self.handle_event(&text)?,
                Ok(kind) => debug!(event = kind, "event before session.created"),
                Err(_) => return Err(TransportError::Protocol("malformed first event".to_owned())),
            }
        }
    }

    fn send_pending_audio(
        &mut self,
        transport: &mut dyn Transport,
        finish_deadline: &mut Option<Instant>,
    ) -> Result<(), TransportError> {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Audio(samples)) => {
                    for message in self.protocol.on_audio(&samples) {
                        transport.send_text(&message)?;
                    }
                }
                Ok(Command::Finish) => self.begin_finish(transport, finish_deadline)?,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    if !self.finishing {
                        self.begin_finish(transport, finish_deadline)?;
                    }
                    return Ok(());
                }
            }
        }
    }

    fn begin_finish(
        &mut self,
        transport: &mut dyn Transport,
        finish_deadline: &mut Option<Instant>,
    ) -> Result<(), TransportError> {
        self.finishing = true;
        *finish_deadline = Some(Instant::now() + self.limits.finish);
        for message in self.protocol.finish() {
            transport.send_text(&message)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, text: &str) -> Result<(), TransportError> {
        match self.protocol.on_event(text) {
            EventOutcome::Update(update) => match self.events.try_send(WorkerEvent::Update(update))
            {
                // Updates are cumulative snapshots; a newer one supersedes a dropped one.
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => self.cancel.store(true, Ordering::Release),
            },
            EventOutcome::ServiceError(error) => match error.impact() {
                ServiceErrorImpact::Unauthorized => return Err(TransportError::Unauthorized),
                ServiceErrorImpact::RateLimited => return Err(TransportError::RateLimited),
                ServiceErrorImpact::Recoverable => {
                    warn!(category = %error.category(), "online service reported an error");
                }
            },
            EventOutcome::Ignored => {}
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use lcrt_core::{AudioChunk, CaptionStatus, Language, Transcriber};
    use serde_json::json;

    use super::{OnlineSession, OnlineStatus, SessionLimits};
    use crate::{
        credentials::ApiKey,
        transcription::TranscriptionProtocol,
        translation::TranslationProtocol,
        transport::{Connect, Transport, TransportError},
    };

    /// What one scripted connection does.
    pub(crate) enum Script {
        Refuse(TransportError),
        /// Replies to each client message whose type matches with the given
        /// server events; sends `on_open` first; then optionally breaks.
        Serve {
            on_open: Vec<String>,
            replies: Vec<(&'static str, Vec<String>)>,
            break_after_messages: Option<usize>,
        },
    }

    #[derive(Clone, Default)]
    pub(crate) struct Record {
        pub(crate) connects: Arc<AtomicUsize>,
        pub(crate) open: Arc<AtomicUsize>,
        pub(crate) max_open: Arc<AtomicUsize>,
        pub(crate) sent: Arc<Mutex<Vec<String>>>,
    }

    pub(crate) struct FakeConnector {
        pub(crate) scripts: Mutex<VecDeque<Script>>,
        pub(crate) record: Record,
    }

    impl FakeConnector {
        pub(crate) fn new(scripts: Vec<Script>) -> (Box<Self>, Record) {
            let record = Record::default();
            (
                Box::new(Self {
                    scripts: Mutex::new(scripts.into()),
                    record: record.clone(),
                }),
                record,
            )
        }
    }

    struct FakeTransport {
        inbox: VecDeque<String>,
        replies: Vec<(&'static str, Vec<String>)>,
        break_after: Option<usize>,
        messages: usize,
        record: Record,
        closed: bool,
    }

    impl Connect for FakeConnector {
        fn connect(&self, _url: &str, _key: &ApiKey) -> Result<Box<dyn Transport>, TransportError> {
            self.record.connects.fetch_add(1, Ordering::SeqCst);
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Script::Refuse(TransportError::Unreachable(
                    "exhausted".to_owned(),
                )));
            match script {
                Script::Refuse(error) => Err(error),
                Script::Serve {
                    on_open,
                    replies,
                    break_after_messages,
                } => {
                    let open = self.record.open.fetch_add(1, Ordering::SeqCst) + 1;
                    self.record.max_open.fetch_max(open, Ordering::SeqCst);
                    Ok(Box::new(FakeTransport {
                        inbox: on_open.into(),
                        replies,
                        break_after: break_after_messages,
                        messages: 0,
                        record: self.record.clone(),
                        closed: false,
                    }))
                }
            }
        }
    }

    impl Transport for FakeTransport {
        fn send_text(&mut self, text: &str) -> Result<(), TransportError> {
            self.messages += 1;
            if self.break_after.is_some_and(|limit| self.messages > limit) {
                return Err(TransportError::Closed);
            }
            self.record.sent.lock().unwrap().push(text.to_owned());
            let kind = serde_json::from_str::<serde_json::Value>(text).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned();
            // Each scripted reply fires once, on the first matching message.
            if let Some(index) = self
                .replies
                .iter()
                .position(|(trigger, _)| *trigger == kind)
            {
                let (_, events) = self.replies.remove(index);
                self.inbox.extend(events);
            }
            Ok(())
        }

        fn receive(&mut self, timeout: Duration) -> Result<Option<String>, TransportError> {
            match self.inbox.pop_front() {
                Some(event) => Ok(Some(event)),
                None => {
                    thread::sleep(timeout.min(Duration::from_millis(2)));
                    Ok(None)
                }
            }
        }

        fn close(&mut self) {
            if !self.closed {
                self.closed = true;
                self.record.open.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    impl Drop for FakeTransport {
        fn drop(&mut self) {
            self.close();
        }
    }

    pub(crate) fn fast_limits() -> SessionLimits {
        SessionLimits {
            handshake: Duration::from_millis(500),
            finish: Duration::from_millis(400),
            poll: Duration::from_millis(2),
            reconnect_delays: [Duration::from_millis(5); 3],
            max_reconnects: 10,
        }
    }

    fn created() -> String {
        json!({"type": "session.created", "session": {"id": "sess_1"}}).to_string()
    }

    fn statuses() -> (super::StatusCallback, Arc<Mutex<Vec<OnlineStatus>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (
            Arc::new(move |status| sink.lock().unwrap().push(status)),
            seen,
        )
    }

    fn key() -> ApiKey {
        ApiKey::parse("test-key").unwrap()
    }

    fn speech(seconds: f32) -> AudioChunk {
        AudioChunk::new(vec![0.2; (24_000.0 * seconds) as usize], 24_000, 1).unwrap()
    }

    fn silence(seconds: f32) -> AudioChunk {
        AudioChunk::new(vec![0.0; (24_000.0 * seconds) as usize], 24_000, 1).unwrap()
    }

    fn transcription_turn_replies() -> Vec<(&'static str, Vec<String>)> {
        vec![(
            "input_audio_buffer.commit",
            vec![
                json!({"type": "input_audio_buffer.committed", "item_id": "a", "previous_item_id": null}).to_string(),
                json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": "a", "delta": "Hello"}).to_string(),
                json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "a", "transcript": "Hello there."}).to_string(),
            ],
        )]
    }

    #[test]
    fn transcription_session_streams_commits_and_finishes_with_the_final_text() {
        let (connector, record) = FakeConnector::new(vec![Script::Serve {
            on_open: vec![created()],
            replies: transcription_turn_replies(),
            break_after_messages: None,
        }]);
        let (status, seen) = statuses();
        let mut session = OnlineSession::start(
            TranscriptionProtocol::new(Some(Language::English)),
            key(),
            connector,
            status,
            fast_limits(),
        )
        .unwrap();

        let mut updates = session.push_audio(speech(1.0)).unwrap();
        thread::sleep(Duration::from_millis(50));
        updates.extend(session.push_audio(silence(1.0)).unwrap());
        updates.extend(session.finish().unwrap());

        let last = updates.last().expect("caption updates");
        assert_eq!(last.text(), "Hello there.");
        assert_eq!(last.status(), CaptionStatus::Final);
        assert_eq!(record.connects.load(Ordering::SeqCst), 1);
        assert_eq!(record.open.load(Ordering::SeqCst), 0);
        assert_eq!(
            seen.lock().unwrap()[..2],
            [OnlineStatus::Connecting, OnlineStatus::Active]
        );
        let sent = record.sent.lock().unwrap();
        assert!(sent[0].contains("session.update"));
        assert!(sent.iter().any(|m| m.contains("input_audio_buffer.commit")));
    }

    #[test]
    fn lost_connection_reconnects_once_without_overlapping_connections() {
        let (connector, record) = FakeConnector::new(vec![
            Script::Serve {
                on_open: vec![created()],
                replies: vec![],
                break_after_messages: Some(2),
            },
            Script::Serve {
                on_open: vec![created()],
                replies: transcription_turn_replies(),
                break_after_messages: None,
            },
        ]);
        let (status, seen) = statuses();
        let mut session = OnlineSession::start(
            TranscriptionProtocol::new(None),
            key(),
            connector,
            status,
            fast_limits(),
        )
        .unwrap();
        for _ in 0..10 {
            session.push_audio(speech(0.2)).unwrap();
            thread::sleep(Duration::from_millis(10));
        }
        session.push_audio(silence(1.0)).unwrap();
        let updates = session.finish().unwrap();

        assert_eq!(record.connects.load(Ordering::SeqCst), 2);
        assert_eq!(record.max_open.load(Ordering::SeqCst), 1);
        assert!(seen.lock().unwrap().contains(&OnlineStatus::Reconnecting));
        assert!(updates.iter().any(|u| u.text().contains("Hello")));
    }

    #[test]
    fn rejected_key_fails_without_retrying() {
        let (connector, record) =
            FakeConnector::new(vec![Script::Refuse(TransportError::Unauthorized)]);
        let (status, _) = statuses();
        let mut session = OnlineSession::start(
            TranscriptionProtocol::new(None),
            key(),
            connector,
            status,
            fast_limits(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(50));
        let error = session.finish().unwrap_err();
        assert_eq!(error.to_string(), "Your OpenAI API key was rejected.");
        assert_eq!(record.connects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unreachable_service_retries_a_bounded_number_of_times_then_fails_visibly() {
        let (connector, record) = FakeConnector::new(vec![]);
        let (status, _) = statuses();
        let mut session = OnlineSession::start(
            TranscriptionProtocol::new(None),
            key(),
            connector,
            status,
            fast_limits(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(200));
        let error = session.push_audio(silence(0.1)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Can't reach the online service. Check your connection."
        );
        assert_eq!(record.connects.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn stop_cancels_reconnect_backoff_immediately() {
        let (connector, record) = FakeConnector::new(vec![]);
        let (status, _) = statuses();
        let mut limits = fast_limits();
        limits.reconnect_delays = [Duration::from_secs(30); 3];
        let mut session = OnlineSession::start(
            TranscriptionProtocol::new(None),
            key(),
            connector,
            status,
            limits,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        session.finish().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(record.connects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn translation_closes_gracefully_through_session_closed() {
        let (connector, record) = FakeConnector::new(vec![Script::Serve {
            on_open: vec![created()],
            replies: vec![
                (
                    "session.input_audio_buffer.append",
                    vec![
                        json!({"type": "session.input_transcript.delta", "delta": "今日は"})
                            .to_string(),
                        json!({"type": "session.output_transcript.delta", "delta": "Today"})
                            .to_string(),
                    ],
                ),
                (
                    "session.close",
                    vec![json!({"type": "session.closed"}).to_string()],
                ),
            ],
            break_after_messages: None,
        }]);
        let (status, _) = statuses();
        let mut session = OnlineSession::start(
            TranslationProtocol::new(Language::English, true),
            key(),
            connector,
            status,
            fast_limits(),
        )
        .unwrap();
        let mut updates = session.push_audio(silence(0.3)).unwrap();
        thread::sleep(Duration::from_millis(50));
        updates.extend(session.finish().unwrap());

        let last = updates.last().unwrap();
        assert_eq!(last.status(), CaptionStatus::Final);
        assert_eq!(last.original(), Some("今日は"));
        assert!(last.text().starts_with("Today"));
        assert_eq!(record.max_open.load(Ordering::SeqCst), 1);
        assert!(
            record
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.contains("\"session.close\""))
        );
    }

    #[test]
    fn missing_session_closed_forces_a_bounded_local_close() {
        let (connector, record) = FakeConnector::new(vec![Script::Serve {
            on_open: vec![created()],
            replies: vec![],
            break_after_messages: None,
        }]);
        let (status, _) = statuses();
        let mut session = OnlineSession::start(
            TranslationProtocol::new(Language::Japanese, false),
            key(),
            connector,
            status,
            fast_limits(),
        )
        .unwrap();
        session.push_audio(silence(0.3)).unwrap();
        thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        session.finish().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(30));
        assert_eq!(record.open.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_stalled_network_drops_audio_instead_of_buffering_it() {
        // The first connection never acknowledges, so no audio is consumed.
        let (connector, _) = FakeConnector::new(vec![Script::Serve {
            on_open: vec![],
            replies: vec![],
            break_after_messages: None,
        }]);
        let (status, _) = statuses();
        let mut limits = fast_limits();
        limits.handshake = Duration::from_secs(5);
        let mut session = OnlineSession::start(
            TranscriptionProtocol::new(None),
            key(),
            connector,
            status,
            limits,
        )
        .unwrap();
        for _ in 0..500 {
            session.push_audio(silence(0.02)).unwrap();
        }
        assert!(session.dropped_blocks >= 400);
    }

    #[test]
    fn dropping_a_session_closes_its_connection() {
        let (connector, record) = FakeConnector::new(vec![Script::Serve {
            on_open: vec![created()],
            replies: vec![],
            break_after_messages: None,
        }]);
        let (status, _) = statuses();
        let session = OnlineSession::start(
            TranscriptionProtocol::new(None),
            key(),
            connector,
            status,
            fast_limits(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(30));
        assert_eq!(record.open.load(Ordering::SeqCst), 1);
        drop(session);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(record.open.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn fatal_in_session_errors_end_the_session_with_actionable_text() {
        let (connector, _) = FakeConnector::new(vec![Script::Serve {
            on_open: vec![
                created(),
                json!({"type": "error", "error": {"type": "invalid_request_error", "code": "insufficient_quota"}}).to_string(),
            ],
            replies: vec![],
            break_after_messages: None,
        }]);
        let (status, _) = statuses();
        let mut session = OnlineSession::start(
            TranscriptionProtocol::new(None),
            key(),
            connector,
            status,
            fast_limits(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(50));
        let error = session.finish().unwrap_err();
        assert_eq!(
            error.to_string(),
            "The online service is temporarily rate limited. Try again shortly."
        );
    }
}
