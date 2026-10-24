//! Saves caption history: a recorder on each session's pipeline thread, and
//! one writer thread that owns every history file.
//!
//! The recorder never blocks the capture path: it hands records to a bounded
//! queue, and if the writer falls behind the text it can't queue becomes a
//! visible gap. Storage problems stop saving, never captioning. Transcript
//! text is never logged.

use std::{
    env, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use lcrt_core::{
    AcceptedText, AudioSourceKind, CaptionLane, HistoryPreferences, SessionOptions,
    TranscriptRecorder,
    history::{
        CueClock, EndReason, EndRecord, FORMAT_VERSION, GapReason, GapRecord, HistoryFile,
        HistoryRecord, HistoryStore, SegmentRecord, SessionHeader, SessionId, TranslationRecord,
        bound_text,
    },
};
use lcrt_ui_gtk::{GtkCaptionSink, HistoryStatus};
use tracing::{info, warn};

use crate::controller::notify_ui;

/// Records waiting for the writer. A few minutes of speech at the worst.
const QUEUE_CAPACITY: usize = 256;
/// Sessions open at once: a stopping session can still be flushing while
/// the next one starts.
const MAX_OPEN_SESSIONS: usize = 4;
/// How long the end of a session may wait for room in the queue.
const END_WAIT: Duration = Duration::from_millis(200);
/// How long quitting waits for the writer to finish what is queued.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// `$XDG_DATA_HOME/lcrt/history`, or `~/.local/share/lcrt/history`.
pub(crate) fn history_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(base.join("lcrt").join("history"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

enum WriterEvent {
    Begin {
        generation: u64,
        header: SessionHeader,
    },
    Record {
        generation: u64,
        record: HistoryRecord,
    },
    End {
        generation: u64,
        reason: EndReason,
        audio_ms: u64,
    },
    /// Preferences changed, or a session that can't be saved started.
    Refresh { unsupported: bool },
}

/// Caption history for the application: settings shared with recorders,
/// and the writer thread.
pub(crate) struct History {
    enabled: Arc<AtomicBool>,
    limit_bytes: Arc<AtomicU64>,
    events: Option<SyncSender<WriterEvent>>,
    done: Receiver<()>,
    writer: Option<JoinHandle<()>>,
}

impl History {
    /// Starts the writer for `store`; it reports to `sink`.
    pub(crate) fn start(
        store: HistoryStore,
        preferences: &HistoryPreferences,
        sink: GtkCaptionSink,
    ) -> io::Result<Self> {
        let enabled = Arc::new(AtomicBool::new(preferences.enabled));
        let limit_bytes = Arc::new(AtomicU64::new(limit_bytes(preferences)));
        let (events, receiver) = sync_channel(QUEUE_CAPACITY);
        let (done_sender, done) = sync_channel(1);
        let writer = Writer {
            store,
            enabled: Arc::clone(&enabled),
            limit_bytes: Arc::clone(&limit_bytes),
            sink,
            open: Vec::new(),
            usage: 0,
            problem: None,
        };
        let writer = thread::Builder::new()
            .name("lcrt-history-writer".to_owned())
            .spawn(move || {
                writer.run(receiver);
                let _ = done_sender.send(());
            })?;
        let history = Self {
            enabled,
            limit_bytes,
            events: Some(events),
            done,
            writer: Some(writer),
        };
        history.refresh(false);
        Ok(history)
    }

    /// Applies changed history preferences. Turning history off or on takes
    /// effect at the running session's next step.
    pub(crate) fn configure(&self, preferences: &HistoryPreferences) {
        self.limit_bytes
            .store(limit_bytes(preferences), Ordering::Release);
        if self.enabled.swap(preferences.enabled, Ordering::AcqRel) != preferences.enabled {
            info!(
                enabled = preferences.enabled,
                "caption history setting changed"
            );
        }
        self.refresh(false);
    }

    /// A session started whose text can't be saved.
    pub(crate) fn unsupported(&self) {
        self.refresh(true);
    }

    fn refresh(&self, unsupported: bool) {
        if let Some(events) = &self.events {
            let _ = events.try_send(WriterEvent::Refresh { unsupported });
        }
    }

    /// A recorder for one session. It saves only while history is on.
    pub(crate) fn recorder(
        &self,
        generation: u64,
        options: &SessionOptions,
        source: AudioSourceKind,
    ) -> Option<Box<dyn TranscriptRecorder>> {
        let events = self.events.clone()?;
        Some(Box::new(Recorder {
            generation,
            template: SessionHeader {
                version: FORMAT_VERSION,
                started_at_ms: 0,
                mode: options.mode,
                spoken_language: options.spoken_language,
                targets: if options.mode.translates() {
                    options.translation_targets.iter().collect()
                } else {
                    Vec::new()
                },
                source: source.into(),
                started_late: false,
            },
            enabled: Arc::clone(&self.enabled),
            events,
            clock: CueClock::default(),
            state: RecorderState::Idle,
            heard_speech: false,
            lost: false,
            audio: Duration::ZERO,
        }))
    }
}

impl Drop for History {
    fn drop(&mut self) {
        // The writer stops once every recorder is gone too; give sessions
        // that are still stopping a moment to write their end.
        self.events.take();
        if self.done.recv_timeout(SHUTDOWN_WAIT) == Err(RecvTimeoutError::Timeout) {
            warn!("caption history writer did not finish in time");
            return;
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn limit_bytes(preferences: &HistoryPreferences) -> u64 {
    u64::from(preferences.storage_limit_mb) * 1_000_000
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecorderState {
    /// Nothing saved yet.
    Idle,
    Saving,
    /// History was turned off after saving began.
    Paused,
}

/// Records one session's accepted text. Runs on the pipeline thread.
struct Recorder {
    generation: u64,
    template: SessionHeader,
    enabled: Arc<AtomicBool>,
    events: SyncSender<WriterEvent>,
    clock: CueClock,
    state: RecorderState,
    /// Captions changed before saving began, so the start is missing.
    heard_speech: bool,
    /// A record was dropped; the next segment follows a gap.
    lost: bool,
    audio: Duration,
}

impl Recorder {
    fn send(&mut self, event: WriterEvent) -> bool {
        match self.events.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.lost = true;
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    fn follow_setting(&mut self) {
        let enabled = self.enabled.load(Ordering::Acquire);
        match (self.state, enabled) {
            (RecorderState::Idle, true) => {
                let header = SessionHeader {
                    started_at_ms: now_ms(),
                    started_late: self.heard_speech,
                    ..self.template.clone()
                };
                let generation = self.generation;
                if self.send(WriterEvent::Begin { generation, header }) {
                    self.state = RecorderState::Saving;
                    self.lost = false;
                }
            }
            (RecorderState::Saving, false) => {
                let generation = self.generation;
                self.send(WriterEvent::Record {
                    generation,
                    record: HistoryRecord::Gap(GapRecord {
                        reason: GapReason::Paused,
                    }),
                });
                self.state = RecorderState::Paused;
            }
            (RecorderState::Paused, true) => self.state = RecorderState::Saving,
            _ => {}
        }
    }
}

impl TranscriptRecorder for Recorder {
    fn record(&mut self, audio: Duration, presented: bool, accepted: Vec<AcceptedText>) {
        self.audio = audio;
        if presented {
            self.clock.presented(audio);
        }
        self.follow_setting();
        if self.state != RecorderState::Saving {
            self.heard_speech |= presented || !accepted.is_empty();
            if self.state == RecorderState::Idle {
                // Timing restarts with what is saved.
                self.clock = CueClock::default();
            }
            return;
        }
        let weights: Vec<usize> = accepted
            .iter()
            .filter(|text| text.lane == CaptionLane::Source)
            .map(|text| text.text.chars().count())
            .collect();
        let mut cues = self.clock.cues(audio, &weights).into_iter();
        for text in accepted {
            let AcceptedText {
                segment,
                lane,
                mut text,
                after_gap,
            } = text;
            let cut = bound_text(&mut text);
            let record = match lane {
                CaptionLane::Source => {
                    let (start, end) = cues.next().unwrap_or((audio, audio));
                    HistoryRecord::Segment(SegmentRecord {
                        id: segment,
                        start_ms: start.as_millis() as u64,
                        end_ms: end.as_millis() as u64,
                        text,
                        after_gap: after_gap || cut || std::mem::take(&mut self.lost),
                    })
                }
                CaptionLane::Target(language) => HistoryRecord::Translation(TranslationRecord {
                    segment,
                    language,
                    text,
                }),
            };
            let generation = self.generation;
            self.send(WriterEvent::Record { generation, record });
        }
    }

    fn finish(&mut self, completed: bool) {
        if self.state == RecorderState::Idle {
            return;
        }
        let generation = self.generation;
        let mut end = WriterEvent::End {
            generation,
            reason: if completed {
                EndReason::Completed
            } else {
                EndReason::Failed
            },
            audio_ms: self.audio.as_millis() as u64,
        };
        // Capture has stopped, so a short bounded wait for room is fine; a
        // session whose end can't be written reads as interrupted.
        let deadline = Instant::now() + END_WAIT;
        loop {
            match self.events.try_send(end) {
                Err(TrySendError::Full(event)) if Instant::now() < deadline => {
                    end = event;
                    thread::yield_now();
                }
                _ => break,
            }
        }
        self.state = RecorderState::Idle;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // A pipeline that panicked never finished its recorder.
        self.finish(false);
    }
}

struct OpenSession {
    generation: u64,
    file: HistoryFile,
    segments: u64,
    gaps: u64,
    /// The end of the newest segment, for a session the writer closes.
    audio_ms: u64,
}

/// Owns every history file. Runs on its own thread.
struct Writer {
    store: HistoryStore,
    enabled: Arc<AtomicBool>,
    limit_bytes: Arc<AtomicU64>,
    sink: GtkCaptionSink,
    open: Vec<OpenSession>,
    /// Bytes used by history, measured when a session begins.
    usage: u64,
    /// Why the latest session isn't being saved: storage full, a storage
    /// error, or a mode that can't be saved. It stays on screen until a
    /// session begins saving, whatever else changes.
    problem: Option<HistoryStatus>,
}

impl Writer {
    fn run(mut self, events: Receiver<WriterEvent>) {
        // Ends once every sender is gone. A session still open then lost
        // its recorder without an end, and reads as interrupted.
        for event in events {
            match event {
                WriterEvent::Begin { generation, header } => self.begin(generation, &header),
                WriterEvent::Record { generation, record } => self.write(generation, &record),
                WriterEvent::End {
                    generation,
                    reason,
                    audio_ms,
                } => self.end(generation, reason, audio_ms),
                WriterEvent::Refresh { unsupported } => {
                    if unsupported {
                        self.problem = Some(HistoryStatus::Unsupported);
                    }
                    self.report();
                }
            }
        }
    }

    fn status(&self, status: HistoryStatus) {
        notify_ui(self.sink.set_history(status));
    }

    fn report(&self) {
        // Off wins at once: the session stops saving at its next step.
        let status = if !self.enabled.load(Ordering::Acquire) {
            HistoryStatus::Off
        } else if let Some(session) = self.open.last() {
            HistoryStatus::Recording(session.file.id().clone())
        } else {
            self.problem.clone().unwrap_or(HistoryStatus::Ready)
        };
        self.status(status);
    }

    fn stop_saving(&mut self, problem: HistoryStatus) {
        self.problem = Some(problem);
        self.report();
    }

    fn fail(&mut self, generation: u64, error: &io::Error) {
        warn!(kind = %error.kind(), "caption history stopped saving after a storage error");
        self.open.retain(|session| session.generation != generation);
        self.stop_saving(HistoryStatus::Failed(describe(error)));
    }

    fn begin(&mut self, generation: u64, header: &SessionHeader) {
        self.usage = match self.store.usage() {
            Ok(usage) => usage,
            Err(error) => return self.fail(generation, &error),
        };
        if self.usage >= self.limit_bytes.load(Ordering::Acquire) {
            info!("caption history is at its storage limit; not saving this session");
            return self.stop_saving(HistoryStatus::StorageFull);
        }
        let id = SessionId::new(header.started_at_ms, generation);
        match self.store.create(id, header) {
            Ok(file) => {
                info!(session = file.id().as_str(), "saving caption history");
                if self.open.len() == MAX_OPEN_SESSIONS {
                    // Its end never came; it reads as interrupted.
                    self.open.remove(0);
                }
                self.problem = None;
                self.open.push(OpenSession {
                    generation,
                    file,
                    segments: 0,
                    gaps: 0,
                    audio_ms: 0,
                });
                self.report();
            }
            Err(error) => self.fail(generation, &error),
        }
    }

    fn write(&mut self, generation: u64, record: &HistoryRecord) {
        let Some(session) = self
            .open
            .iter_mut()
            .find(|session| session.generation == generation)
        else {
            // Not saving this session: never begun, failed, or full.
            return;
        };
        match record {
            HistoryRecord::Segment(segment) => {
                session.segments += 1;
                session.gaps += u64::from(segment.after_gap);
                session.audio_ms = session.audio_ms.max(segment.end_ms);
            }
            HistoryRecord::Gap(_) => session.gaps += 1,
            _ => {}
        }
        match session.file.append(record) {
            Ok(bytes) => self.usage += bytes,
            Err(error) => return self.fail(generation, &error),
        }
        if self.usage > self.limit_bytes.load(Ordering::Acquire) {
            info!("caption history reached its storage limit; stopped saving");
            self.close(generation, EndReason::StorageFull, None);
            self.stop_saving(HistoryStatus::StorageFull);
        }
    }

    fn end(&mut self, generation: u64, reason: EndReason, audio_ms: u64) {
        if self.close(generation, reason, Some(audio_ms)) {
            self.report();
        }
    }

    /// Writes the end of a session and closes it. Returns whether it was
    /// open.
    fn close(&mut self, generation: u64, reason: EndReason, audio_ms: Option<u64>) -> bool {
        let Some(index) = self
            .open
            .iter()
            .position(|session| session.generation == generation)
        else {
            return false;
        };
        let mut session = self.open.remove(index);
        let end = EndRecord {
            ended_at_ms: now_ms(),
            reason,
            audio_ms: audio_ms.unwrap_or(session.audio_ms),
            segments: session.segments,
            gaps: session.gaps,
        };
        let result = session
            .file
            .append(&HistoryRecord::End(end))
            .and_then(|_| session.file.sync());
        if let Err(error) = result {
            self.fail(generation, &error);
        }
        true
    }
}

/// A storage error in words, without paths or text.
fn describe(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::StorageFull => "the disk is full".to_owned(),
        io::ErrorKind::PermissionDenied => "no permission to write the history folder".to_owned(),
        io::ErrorKind::ReadOnlyFilesystem => "the disk is read-only".to_owned(),
        io::ErrorKind::QuotaExceeded => "the disk quota is exceeded".to_owned(),
        kind => format!("storage error ({kind})"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use lcrt_core::{
        Language, LanguageSelection, ProcessingMode, TranslationTargets,
        history::{SessionStatus, find_matches},
    };

    use super::*;

    fn options(mode: ProcessingMode) -> SessionOptions {
        SessionOptions {
            mode,
            source_id: "monitor".to_owned(),
            spoken_language: LanguageSelection::Language(Language::Japanese),
            translation_targets: TranslationTargets::resolve(
                Some(Language::English),
                None,
                Some(Language::Japanese),
            ),
        }
    }

    fn source(segment: u64, text: &str) -> AcceptedText {
        AcceptedText {
            segment,
            lane: CaptionLane::Source,
            text: text.to_owned(),
            after_gap: false,
        }
    }

    fn english(segment: u64, text: &str) -> AcceptedText {
        AcceptedText {
            segment,
            lane: CaptionLane::Target(Language::English),
            text: text.to_owned(),
            after_gap: false,
        }
    }

    fn temp_store(name: &str) -> HistoryStore {
        let dir =
            std::env::temp_dir().join(format!("lcrt-app-history-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        HistoryStore::new(dir)
    }

    fn history(store: &HistoryStore, enabled: bool) -> History {
        let (sink, receiver) = GtkCaptionSink::bridge();
        // Statuses go nowhere; the bridge just has to stay open.
        std::mem::forget(receiver);
        History::start(
            store.clone(),
            &HistoryPreferences {
                enabled,
                ..HistoryPreferences::default()
            },
            sink,
        )
        .unwrap()
    }

    #[test]
    fn nothing_is_saved_until_history_is_turned_on() {
        let store = temp_store("off");
        let history = history(&store, false);
        let mut recorder = history
            .recorder(
                1,
                &options(ProcessingMode::OfflineCaptions),
                AudioSourceKind::SystemOutput,
            )
            .unwrap();
        recorder.record(
            Duration::from_secs(1),
            true,
            vec![source(1, "private words")],
        );
        recorder.finish(true);
        drop(recorder);
        drop(history);
        assert!(!store.dir().exists());
    }

    #[test]
    fn a_session_is_saved_in_order_with_translations_and_survives_reopening() {
        let store = temp_store("saved");
        let history = history(&store, true);
        let mut recorder = history
            .recorder(
                3,
                &options(ProcessingMode::OfflineTranslation),
                AudioSourceKind::SystemOutput,
            )
            .unwrap();
        recorder.record(Duration::from_millis(500), true, Vec::new());
        recorder.record(
            Duration::from_secs(2),
            true,
            vec![source(1, "こんにちは。")],
        );
        recorder.record(
            Duration::from_secs(4),
            true,
            vec![english(1, "Hello."), source(2, "Xin chào thế giới")],
        );
        recorder.finish(true);
        drop(recorder);
        drop(history);

        let library = HistoryStore::new(store.dir()).list().unwrap();
        assert_eq!(library.sessions.len(), 1);
        let summary = &library.sessions[0];
        assert_eq!(summary.segments, 2);
        assert_eq!(
            SessionStatus::of(&summary.header, summary.end.as_ref(), summary.gaps, false),
            SessionStatus::Completed
        );
        let session = store.load(&summary.id).unwrap();
        let texts: Vec<_> = session.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["こんにちは。", "Xin chào thế giới"]);
        assert_eq!(
            session.segments[0].translation(Language::English),
            Some("Hello.")
        );
        assert!(session.segments[0].start < session.segments[0].end);
        assert!(session.segments[0].end <= session.segments[1].start);
        assert_eq!(find_matches(&session.segments[1].text, "THẾ GIỚI").len(), 1);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn turning_history_on_and_off_mid_session_is_visible_in_the_saved_text() {
        let store = temp_store("toggle");
        let history = history(&store, false);
        let mut recorder = history
            .recorder(
                4,
                &options(ProcessingMode::OfflineCaptions),
                AudioSourceKind::Microphone,
            )
            .unwrap();
        recorder.record(Duration::from_secs(1), true, vec![source(1, "before")]);
        history.configure(&HistoryPreferences {
            enabled: true,
            ..HistoryPreferences::default()
        });
        recorder.record(Duration::from_secs(2), true, vec![source(2, "during")]);
        history.configure(&HistoryPreferences::default());
        recorder.record(Duration::from_secs(3), true, vec![source(3, "paused")]);
        history.configure(&HistoryPreferences {
            enabled: true,
            ..HistoryPreferences::default()
        });
        recorder.record(Duration::from_secs(4), true, vec![source(4, "again")]);
        recorder.finish(true);
        drop(recorder);
        drop(history);

        let summary = store.list().unwrap().sessions.remove(0);
        let session = store.load(&summary.id).unwrap();
        assert!(session.header.started_late);
        let texts: Vec<_> = session
            .segments
            .iter()
            .map(|s| (s.text.as_str(), s.after_gap))
            .collect();
        assert_eq!(texts, [("during", false), ("again", true)]);
        assert_eq!(
            SessionStatus::of(&session.header, session.end.as_ref(), session.gaps, false),
            SessionStatus::Incomplete
        );
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn at_the_storage_limit_nothing_more_is_saved_and_nothing_is_deleted() {
        let store = temp_store("full");
        store.ensure_dir().unwrap();
        // An earlier session already fills the smallest budget.
        fs::write(store.dir().join("1-1.jsonl"), vec![b' '; 26_000_000]).unwrap();
        let history = history(&store, true);
        history.configure(&HistoryPreferences {
            enabled: true,
            storage_limit_mb: 25,
        });
        let mut recorder = history
            .recorder(
                5,
                &options(ProcessingMode::OfflineCaptions),
                AudioSourceKind::SystemOutput,
            )
            .unwrap();
        recorder.record(Duration::from_secs(1), true, vec![source(1, "not saved")]);
        recorder.finish(true);
        drop(recorder);
        drop(history);
        let names: Vec<_> = fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["1-1.jsonl"]);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn an_unwritable_folder_stops_saving_without_failing_the_session() {
        // The folder can't be created: its parent is a file.
        let blocker = temp_store("blocked");
        fs::write(blocker.dir(), b"not a folder").unwrap();
        let store = HistoryStore::new(blocker.dir().join("history"));
        let history = history(&store, true);
        let mut recorder = history
            .recorder(
                6,
                &options(ProcessingMode::OfflineCaptions),
                AudioSourceKind::SystemOutput,
            )
            .unwrap();
        // Recording keeps accepting text; nothing panics or blocks.
        for second in 1..50 {
            recorder.record(
                Duration::from_secs(second),
                true,
                vec![source(second, "text")],
            );
        }
        recorder.finish(true);
        drop(recorder);
        drop(history);
        assert!(!store.dir().exists());
        fs::remove_file(blocker.dir()).unwrap();
    }
}
