//! Translation into one or two target languages, changeable while it runs.
//!
//! The translation service takes one output language per session, so each
//! target lane is its own [`OnlineSession`]. There are never more sessions
//! than running targets, and never more than
//! [`lcrt_core::MAX_TRANSLATION_TARGETS`]. Every running lane receives the
//! captured audio from the moment its session opens, and each keeps its own
//! bounded queue and bounded reconnects.
//!
//! The controller changes the targets through a [`TargetControl`]. The
//! session reconciles its lanes with it before the next audio chunk, so a
//! change opens or closes only the session of the target it names.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use lcrt_core::{
    AudioChunk, CaptionStatus, Language, TargetChange, TargetStatus, TargetText, Transcriber,
    TranscriptUpdate, TranscriptionError, TranslationLanes, TranslationTargets,
};
use tracing::{info, warn};

use crate::{
    credentials::ApiKey,
    session::{OnlineSession, OnlineStatus, SessionLimits},
    translation::TranslationProtocol,
    transport::Connect,
};

/// How a translation session is bounded and ends.
pub struct TranslationOptions {
    /// The targets, shared with the controller for live changes.
    pub control: TargetControl,
    /// Timing bounds of each session.
    pub limits: SessionLimits,
    /// Set when the session is being replaced. Its last words would be
    /// discarded anyway, so Stop then closes at once instead of waiting for
    /// the service to deliver them.
    pub abandoned: Arc<AtomicBool>,
}

/// Told what each target's session is doing.
pub type TargetStatusCallback = Arc<dyn Fn(Language, TargetStatus) + Send + Sync>;
/// Told when one target's session fails.
pub type LaneFailureCallback = Arc<dyn Fn(Language, &TranscriptionError) + Send + Sync>;
/// Supplies the connector for a new session into a target language.
pub type ConnectorFactory = Box<dyn FnMut(Language) -> Box<dyn Connect> + Send>;

/// The targets the user wants and which of them are paused, shared by the
/// controller, which changes them, and the running session, which follows.
#[derive(Clone)]
pub struct TargetControl(Arc<Mutex<Desired>>);

#[derive(Clone, Debug)]
struct Desired {
    /// Advances on every accepted change.
    revision: u64,
    targets: TranslationTargets,
    /// Targets without a session: paused by the user, or failed.
    stopped: Vec<Language>,
}

impl TargetControl {
    /// Starts with every target of `targets` running.
    pub fn new(targets: TranslationTargets) -> Self {
        Self(Arc::new(Mutex::new(Desired {
            revision: 0,
            targets,
            stopped: Vec::new(),
        })))
    }

    /// Applies `change` and returns whether it was accepted. A third
    /// target, a repeated one, removing the only target, and pausing or
    /// resuming a language that is not a target are refused. Which target
    /// may repeat the spoken language is decided before a change gets here.
    pub fn apply(&self, change: TargetChange) -> bool {
        let Ok(mut desired) = self.0.lock() else {
            return false;
        };
        let accepted = match change {
            TargetChange::Add(language) => {
                desired.targets.with_added(language, None).map(|targets| {
                    desired.targets = targets;
                    desired.stopped.retain(|stopped| *stopped != language);
                })
            }
            TargetChange::Remove(language) => desired.targets.without(language).map(|targets| {
                desired.targets = targets;
                desired.stopped.retain(|stopped| *stopped != language);
            }),
            TargetChange::Pause(language) => (desired.targets.contains(language)
                && !desired.stopped.contains(&language))
            .then(|| desired.stopped.push(language)),
            TargetChange::Resume(language) => desired
                .stopped
                .contains(&language)
                .then(|| desired.stopped.retain(|stopped| *stopped != language)),
        }
        .is_some();
        if accepted {
            desired.revision += 1;
        }
        accepted
    }

    /// The targets the session follows.
    pub fn targets(&self) -> Option<TranslationTargets> {
        self.0.lock().ok().map(|desired| desired.targets)
    }

    /// Records that `language`'s session ended by itself, so the desired
    /// state says what is true and Resume can open it again.
    fn mark_stopped(&self, language: Language) {
        if let Ok(mut desired) = self.0.lock()
            && !desired.stopped.contains(&language)
        {
            desired.stopped.push(language);
            desired.revision += 1;
        }
    }

    /// The desired state if it changed since `revision`.
    fn changed_since(&self, revision: Option<u64>) -> Option<Desired> {
        let desired = self.0.lock().ok()?;
        (Some(desired.revision) != revision).then(|| desired.clone())
    }
}

/// One target language, its session while it runs, and its latest text.
struct Lane {
    /// Identifies this lane's current session. It changes whenever a
    /// session opens, so reports from a closed session are recognized.
    id: u64,
    target: Language,
    /// `None` while paused, or after the session failed or finished.
    session: Option<OnlineSession>,
    translation: String,
    original: String,
}

impl Lane {
    /// Keeps the newest texts. Updates are cumulative, so the last one wins;
    /// an empty text is a session that has said nothing yet, such as one
    /// just resumed, and keeps the text the lane already shows.
    fn apply(&mut self, updates: &[TranscriptUpdate]) {
        let Some(lanes) = updates.last().and_then(TranscriptUpdate::translation_lanes) else {
            return;
        };
        if let Some(translation) = lanes.target(self.target).filter(|text| !text.is_empty()) {
            translation.clone_into(&mut self.translation);
        }
        if !lanes.original.is_empty() {
            lanes.original.clone_into(&mut self.original);
        }
    }
}

/// The sessions whose reports still count, and where reports go. A report
/// from a session that is no longer current is dropped, so a closed session
/// can never change the status of a lane that replaced it.
struct Reports {
    live: Mutex<Vec<u64>>,
    on_status: TargetStatusCallback,
}

impl Reports {
    fn open(&self, id: u64, target: Language) {
        if let Ok(mut live) = self.live.lock() {
            live.push(id);
            (self.on_status)(target, TargetStatus::Connecting);
        }
    }

    /// A report from session `id`'s own worker.
    fn session(&self, id: u64, target: Language, status: OnlineStatus) {
        if let Ok(live) = self.live.lock()
            && live.contains(&id)
        {
            (self.on_status)(
                target,
                match status {
                    OnlineStatus::Connecting => TargetStatus::Connecting,
                    OnlineStatus::Active => TargetStatus::Active,
                    OnlineStatus::Reconnecting => TargetStatus::Reconnecting,
                },
            );
        }
    }

    /// Session `id` closed; `status` says why, or `None` if its lane is gone.
    fn close(&self, id: u64, target: Language, status: Option<TargetStatus>) {
        if let Ok(mut live) = self.live.lock() {
            live.retain(|current| *current != id);
            if let Some(status) = status {
                (self.on_status)(target, status);
            }
        }
    }
}

/// A translation session with one lane per target language.
pub struct MultiTargetTranslation {
    /// In target order.
    lanes: Vec<Lane>,
    control: TargetControl,
    /// The revision of `control` the lanes follow.
    revision: Option<u64>,
    key: ApiKey,
    connect: ConnectorFactory,
    limits: SessionLimits,
    next_id: u64,
    /// The lane whose transcript of the source is shown. Two sessions hear
    /// the same audio but transcribe it at their own pace, so the first one
    /// to say anything keeps the source lane until its session ends.
    source_lane: Option<u64>,
    /// The source text last shown, kept when its lane is paused or removed
    /// until another running lane has a transcript of its own.
    source_text: String,
    reports: Arc<Reports>,
    on_lane_failure: LaneFailureCallback,
    abandoned: Arc<AtomicBool>,
    last_sent: Option<TranslationLanes>,
    finished: bool,
}

impl MultiTargetTranslation {
    /// Opens a session for every target in `options.control`. `connect`
    /// supplies each new session's connector.
    pub fn start(
        options: TranslationOptions,
        key: &ApiKey,
        connect: ConnectorFactory,
        on_status: TargetStatusCallback,
        on_lane_failure: LaneFailureCallback,
    ) -> Result<Self, TranscriptionError> {
        let TranslationOptions {
            control,
            limits,
            abandoned,
        } = options;
        let mut translation = Self {
            lanes: Vec::new(),
            control,
            revision: None,
            key: key.clone(),
            connect,
            limits,
            next_id: 0,
            source_lane: None,
            source_text: String::new(),
            reports: Arc::new(Reports {
                live: Mutex::new(Vec::new()),
                on_status,
            }),
            on_lane_failure,
            abandoned,
            last_sent: None,
            finished: false,
        };
        translation.reconcile()?;
        Ok(translation)
    }

    /// Opens a session for `lane`.
    fn open(&mut self, lane: &mut Lane) -> Result<(), TranscriptionError> {
        self.next_id += 1;
        lane.id = self.next_id;
        let (id, target) = (lane.id, lane.target);
        self.reports.open(id, target);
        let reports = Arc::clone(&self.reports);
        let session = OnlineSession::start(
            // Every lane transcribes the source, so the source lane
            // survives the loss, pause or removal of any one session.
            TranslationProtocol::new(target),
            self.key.clone(),
            (self.connect)(target),
            Arc::new(move |status| reports.session(id, target, status)),
            self.limits,
        );
        match session {
            Ok(session) => {
                lane.session = Some(session);
                Ok(())
            }
            Err(error) => {
                self.reports.close(id, target, Some(TargetStatus::Failed));
                self.control.mark_stopped(target);
                Err(error)
            }
        }
    }

    /// Brings the lanes in line with the targets the controller wants:
    /// opens sessions for new and resumed targets, closes paused ones, and
    /// drops removed lanes. Lanes that did not change are left alone.
    fn reconcile(&mut self) -> Result<(), TranscriptionError> {
        let Some(desired) = self.control.changed_since(self.revision) else {
            return Ok(());
        };
        self.revision = Some(desired.revision);
        let mut previous = std::mem::take(&mut self.lanes);
        let mut lanes = Vec::new();
        let mut failures = Vec::new();
        for target in desired.targets.iter() {
            let mut lane = match previous.iter().position(|lane| lane.target == target) {
                Some(index) => previous.remove(index),
                None => Lane {
                    id: 0,
                    target,
                    session: None,
                    translation: String::new(),
                    original: String::new(),
                },
            };
            let running = !desired.stopped.contains(&target);
            if running && lane.session.is_none() {
                info!(
                    target_language = target.code(),
                    "opening a translation lane"
                );
                if let Err(error) = self.open(&mut lane) {
                    failures.push((target, error));
                }
            } else if !running && lane.session.take().is_some() {
                // Dropping a session cancels it and closes its connection.
                info!(
                    target_language = target.code(),
                    "pausing a translation lane"
                );
                self.reports
                    .close(lane.id, target, Some(TargetStatus::Paused));
            }
            lanes.push(lane);
        }
        for removed in previous {
            info!(
                target_language = removed.target.code(),
                "removing a translation lane"
            );
            self.reports.close(removed.id, removed.target, None);
        }
        self.lanes = lanes;
        if !failures.is_empty() && !self.lanes.iter().any(|lane| lane.session.is_some()) {
            return Err(failures.swap_remove(0).1);
        }
        // Another lane runs: report each failure, as a failure while running
        // is, and keep translating.
        for (target, error) in failures {
            warn!(%error, target_language = target.code(), "a translation lane could not open");
            (self.on_lane_failure)(target, &error);
        }
        Ok(())
    }

    /// The source text. A running lane keeps the source lane once it has it;
    /// when its session ends, the first running lane with a transcript
    /// takes over, and until one has, the last lane's text stays.
    fn original(&mut self) -> String {
        let running = |lane: &Lane| lane.session.is_some();
        let authority = self
            .source_lane
            .and_then(|id| self.lanes.iter().find(|lane| lane.id == id));
        if !authority.is_some_and(running)
            && let Some(lane) = self
                .lanes
                .iter()
                .find(|lane| running(lane) && !lane.original.is_empty())
        {
            self.source_lane = Some(lane.id);
        }
        if let Some(lane) = self
            .source_lane
            .and_then(|id| self.lanes.iter().find(|lane| lane.id == id))
            .filter(|lane| !lane.original.is_empty())
        {
            lane.original.clone_into(&mut self.source_text);
        }
        self.source_text.clone()
    }

    /// The combined update, if any lane changed since the last one.
    fn combined(&mut self, status: CaptionStatus) -> Vec<TranscriptUpdate> {
        let lanes = TranslationLanes {
            original: self.original(),
            targets: self
                .lanes
                .iter()
                .map(|lane| TargetText {
                    language: lane.target,
                    text: lane.translation.clone(),
                })
                .collect(),
        };
        if status == CaptionStatus::Partial && self.last_sent.as_ref() == Some(&lanes) {
            return Vec::new();
        }
        self.last_sent = Some(lanes.clone());
        TranscriptUpdate::lanes(lanes, status).into_iter().collect()
    }

    /// Closes lane `index` after `error`. While another lane is running the
    /// failure is only reported; the last running lane's failure ends the
    /// session.
    fn lane_failed(
        &mut self,
        index: usize,
        error: TranscriptionError,
    ) -> Result<(), TranscriptionError> {
        let lane = &mut self.lanes[index];
        lane.session = None;
        let (id, target) = (lane.id, lane.target);
        self.reports.close(id, target, Some(TargetStatus::Failed));
        self.control.mark_stopped(target);
        if self.lanes.iter().all(|lane| lane.session.is_none()) {
            return Err(error);
        }
        warn!(
            target_language = target.code(),
            "a translation lane stopped"
        );
        (self.on_lane_failure)(target, &error);
        Ok(())
    }
}

impl Transcriber for MultiTargetTranslation {
    fn push_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        if self.finished {
            return Err(TranscriptionError::new(
                "The translation session has already ended.",
            ));
        }
        self.reconcile()?;
        for index in 0..self.lanes.len() {
            let Some(session) = self.lanes[index].session.as_mut() else {
                continue;
            };
            match session.push_audio(chunk.clone()) {
                Ok(updates) => self.lanes[index].apply(&updates),
                Err(error) => self.lane_failed(index, error)?,
            }
        }
        Ok(self.combined(CaptionStatus::Partial))
    }

    fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        if self.abandoned.load(Ordering::Acquire) {
            // Dropping a session cancels it and closes its connection.
            for lane in &mut self.lanes {
                lane.session = None;
            }
            return Ok(Vec::new());
        }
        // Ask every session to end before waiting for any of them, so the
        // waits overlap instead of adding up.
        for wait in [false, true] {
            for index in 0..self.lanes.len() {
                let Some(session) = self.lanes[index].session.as_mut() else {
                    continue;
                };
                let result = if wait {
                    session.wait_finished()
                } else {
                    session.begin_finish()
                };
                match result {
                    Ok(updates) => self.lanes[index].apply(&updates),
                    Err(error) => self.lane_failed(index, error)?,
                }
            }
        }
        // Combined while the sessions still count as running, so a source
        // lane that failed during Stop hands over to one that finished.
        let last = self.combined(CaptionStatus::Final);
        for lane in &mut self.lanes {
            lane.session = None;
        }
        Ok(last)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use lcrt_core::{
        CaptionStatus, Language, TargetChange, TargetStatus, Transcriber, TranscriptUpdate,
        TranslationTargets,
    };
    use serde_json::json;

    use super::{
        LaneFailureCallback, MultiTargetTranslation, TargetControl, TargetStatusCallback,
        TranslationOptions,
    };
    use crate::{
        credentials::ApiKey,
        session::tests::{
            FakeConnector, Record, Script, created, fast_limits, key, silence, translated_delta,
        },
        transport::{Connect, Transport, TransportError},
    };

    use Language::{English, German, Japanese, Vietnamese};

    fn source_delta(text: &str) -> String {
        json!({"type": "session.input_transcript.delta", "delta": text}).to_string()
    }

    fn closed() -> String {
        json!({"type": "session.closed"}).to_string()
    }

    /// A lane that says `source` and `translation`, then closes on request.
    fn serving(source: &str, translation: &str) -> Script {
        Script::Serve {
            on_open: vec![created()],
            replies: vec![
                (
                    "session.update",
                    vec![source_delta(source), translated_delta(translation)],
                ),
                ("session.close", vec![closed()]),
            ],
            break_after_messages: None,
        }
    }

    /// One scripted connector per language, shared by every session opened
    /// into it, so a lane that reopens uses the language's next script.
    struct Shared(Arc<FakeConnector>);

    impl Connect for Shared {
        fn connect(&self, url: &str, key: &ApiKey) -> Result<Box<dyn Transport>, TransportError> {
            self.0.connect(url, key)
        }
    }

    struct Started {
        session: MultiTargetTranslation,
        control: TargetControl,
        records: Vec<(Language, Record)>,
        statuses: Arc<Mutex<Vec<(Language, TargetStatus)>>>,
        failures: Arc<Mutex<Vec<(Language, String)>>>,
        abandoned: Arc<AtomicBool>,
    }

    impl Started {
        fn record(&self, language: Language) -> &Record {
            &self.records.iter().find(|(l, _)| *l == language).unwrap().1
        }

        fn last_status(&self, language: Language) -> Option<TargetStatus> {
            self.statuses
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(l, _)| *l == language)
                .map(|(_, status)| *status)
        }

        fn change(&self, change: TargetChange) {
            assert!(self.control.apply(change), "{change:?} was refused");
        }

        /// Feeds silence until `done` accepts the newest update.
        fn pump(&mut self, done: impl Fn(&TranscriptUpdate) -> bool) -> TranscriptUpdate {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut latest = None;
            loop {
                if let Some(update) = self.session.push_audio(silence(0.1)).unwrap().pop() {
                    latest = Some(update);
                }
                if let Some(update) = &latest
                    && done(update)
                {
                    return latest.unwrap();
                }
                assert!(Instant::now() < deadline, "lanes never filled: {latest:?}");
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn wait_closed(&self, language: Language) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.record(language).open.load(Ordering::SeqCst) != 0 {
                assert!(Instant::now() < deadline, "{language:?} stayed connected");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Starts translating into `targets`; `scripts` serve every language
    /// that may be opened, including ones added later.
    fn start(targets: &[Language], scripts: Vec<(Language, Vec<Script>)>) -> Started {
        let mut records = Vec::new();
        let connectors: Vec<(Language, Arc<FakeConnector>)> = scripts
            .into_iter()
            .map(|(language, scripts)| {
                let (connector, record) = FakeConnector::new(scripts);
                records.push((language, record));
                (language, Arc::from(connector))
            })
            .collect();
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let status_sink = Arc::clone(&statuses);
        let on_status: TargetStatusCallback = Arc::new(move |language, status| {
            status_sink.lock().unwrap().push((language, status));
        });
        let failures = Arc::new(Mutex::new(Vec::new()));
        let failure_sink = Arc::clone(&failures);
        let on_failure: LaneFailureCallback = Arc::new(move |target, error| {
            failure_sink
                .lock()
                .unwrap()
                .push((target, error.to_string()));
        });
        let control = TargetControl::new(TranslationTargets::resolve(
            Some(targets[0]),
            targets.get(1).copied(),
            None,
        ));
        let abandoned = Arc::new(AtomicBool::new(false));
        let session = MultiTargetTranslation::start(
            TranslationOptions {
                control: control.clone(),
                limits: fast_limits(),
                abandoned: Arc::clone(&abandoned),
            },
            &key(),
            Box::new(move |target| {
                let (_, connector) = connectors
                    .iter()
                    .find(|(language, _)| *language == target)
                    .unwrap();
                Box::new(Shared(Arc::clone(connector)))
            }),
            on_status,
            on_failure,
        )
        .unwrap();
        Started {
            session,
            control,
            records,
            statuses,
            failures,
            abandoned,
        }
    }

    fn text(update: &TranscriptUpdate, language: Language) -> Option<&str> {
        update.translation_lanes()?.target(language)
    }

    fn original(update: &TranscriptUpdate) -> &str {
        &update.translation_lanes().unwrap().original
    }

    fn targets(update: &TranscriptUpdate) -> Vec<Language> {
        update
            .translation_lanes()
            .unwrap()
            .targets
            .iter()
            .map(|target| target.language)
            .collect()
    }

    fn has(language: Language, expected: &str) -> impl Fn(&TranscriptUpdate) -> bool {
        let expected = expected.to_owned();
        move |update| text(update, language) == Some(expected.as_str())
    }

    #[test]
    fn two_targets_fill_their_own_lanes_in_target_order() {
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("こんにちは", "Hello")]),
                (Vietnamese, vec![serving("こんにちは", "Xin chào")]),
            ],
        );
        let update = started
            .pump(|update| has(English, "Hello")(update) && has(Vietnamese, "Xin chào")(update));
        assert_eq!(original(&update), "こんにちは");
        assert_eq!(targets(&update), [English, Vietnamese]);
        // One connection per target, and no more.
        for (_, record) in &started.records {
            assert_eq!(record.connects.load(Ordering::SeqCst), 1);
        }
        assert_eq!(started.last_status(English), Some(TargetStatus::Active));
    }

    #[test]
    fn every_lane_receives_the_same_audio() {
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("a", "one")]),
                (Vietnamese, vec![serving("a", "một")]),
            ],
        );
        for _ in 0..20 {
            started.session.push_audio(silence(0.1)).unwrap();
        }
        started.session.finish().unwrap();
        let appended = |record: &Record| {
            record
                .sent
                .lock()
                .unwrap()
                .iter()
                .filter(|message| message.contains("input_audio_buffer.append"))
                .map(String::len)
                .sum::<usize>()
        };
        assert!(appended(started.record(English)) > 0);
        assert_eq!(
            appended(started.record(English)),
            appended(started.record(Vietnamese))
        );
    }

    #[test]
    fn a_target_added_while_running_leaves_the_other_untouched() {
        let mut started = start(
            &[English],
            vec![
                (English, vec![serving("こんにちは", "Hello")]),
                (Vietnamese, vec![serving("こんにちは", "Xin chào")]),
            ],
        );
        started.pump(has(English, "Hello"));
        // A few seconds of audio go to English only.
        for _ in 0..20 {
            started.session.push_audio(silence(0.1)).unwrap();
        }
        started.change(TargetChange::Add(Vietnamese));
        let update = started.pump(has(Vietnamese, "Xin chào"));
        // English kept its session and its text.
        assert_eq!(text(&update, English), Some("Hello"));
        assert_eq!(started.record(English).connects.load(Ordering::SeqCst), 1);
        assert_eq!(targets(&update), [English, Vietnamese]);
        let reported = started.statuses.lock().unwrap().clone();
        assert!(reported.contains(&(Vietnamese, TargetStatus::Connecting)));
        // The new session starts from the live point: it received none of
        // the 2 s (ten 200 ms appends) captured before it was added. Workers
        // send asynchronously, so wait for English to send its backlog.
        let appends = |language| {
            started
                .record(language)
                .sent
                .lock()
                .unwrap()
                .iter()
                .filter(|message| message.contains("input_audio_buffer.append"))
                .count()
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while appends(English) < appends(Vietnamese) + 10 {
            assert!(
                Instant::now() < deadline,
                "Vietnamese received audio from before it was added"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_removed_target_closes_only_its_session_and_its_late_text_is_ignored() {
        // Vietnamese would keep talking; removing it must silence it.
        let chatty = Script::Serve {
            on_open: vec![created(), translated_delta("Xin chào")],
            replies: vec![(
                "session.input_audio_buffer.append",
                vec![translated_delta(" nữa")],
            )],
            break_after_messages: None,
        };
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("こんにちは", "Hello")]),
                (Vietnamese, vec![chatty]),
            ],
        );
        started.pump(|update| {
            has(English, "Hello")(update) && text(update, Vietnamese).is_some_and(|t| !t.is_empty())
        });
        started.change(TargetChange::Remove(Vietnamese));
        let update = started.pump(|update| targets(update) == [English]);
        let reports = |started: &Started| {
            started
                .statuses
                .lock()
                .unwrap()
                .iter()
                .filter(|(language, _)| *language == Vietnamese)
                .count()
        };
        let reported_before = reports(&started);
        assert_eq!(text(&update, English), Some("Hello"));
        started.wait_closed(Vietnamese);
        assert_eq!(started.record(English).open.load(Ordering::SeqCst), 1);
        for _ in 0..20 {
            for update in started.session.push_audio(silence(0.1)).unwrap() {
                assert_eq!(text(&update, Vietnamese), None, "a removed lane came back");
            }
        }
        // The removed lane's session is gone, so its reports are too.
        assert_eq!(reports(&started), reported_before);
    }

    #[test]
    fn a_paused_target_stops_receiving_audio_and_resumes_from_the_live_point() {
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("こんにちは", "Hello")]),
                (
                    Vietnamese,
                    vec![serving("こんにちは", "Xin chào"), serving("mới", "lại")],
                ),
            ],
        );
        started
            .pump(|update| has(English, "Hello")(update) && has(Vietnamese, "Xin chào")(update));
        started.change(TargetChange::Pause(Vietnamese));
        started.session.push_audio(silence(0.1)).unwrap();
        started.wait_closed(Vietnamese);
        assert_eq!(started.last_status(Vietnamese), Some(TargetStatus::Paused));
        let sent_while_paused = started.record(Vietnamese).sent.lock().unwrap().len();
        let english_before = started.record(English).sent.lock().unwrap().len();
        for _ in 0..10 {
            let updates = started.session.push_audio(silence(0.1)).unwrap();
            // The paused lane keeps its text.
            if let Some(update) = updates.last() {
                assert_eq!(text(update, Vietnamese), Some("Xin chào"));
            }
        }
        assert_eq!(
            started.record(Vietnamese).sent.lock().unwrap().len(),
            sent_while_paused
        );
        // English keeps receiving audio; its worker sends asynchronously.
        let deadline = Instant::now() + Duration::from_secs(2);
        while started.record(English).sent.lock().unwrap().len() <= english_before {
            assert!(Instant::now() < deadline, "English stopped receiving audio");
            thread::sleep(Duration::from_millis(5));
        }
        // Resuming opens only the paused target's session again.
        started.change(TargetChange::Resume(Vietnamese));
        started.pump(has(Vietnamese, "lại"));
        assert_eq!(
            started.record(Vietnamese).connects.load(Ordering::SeqCst),
            2
        );
        assert_eq!(started.record(English).connects.load(Ordering::SeqCst), 1);
        assert!(started.failures.lock().unwrap().is_empty());
    }

    #[test]
    fn a_resumed_lane_keeps_its_text_until_its_session_translates() {
        let mut lane = super::Lane {
            id: 1,
            target: Vietnamese,
            session: None,
            translation: "Xin chào".to_owned(),
            original: "こんにちは".to_owned(),
        };
        // A resumed session's first update often has the source but no
        // translation yet.
        let source_only =
            TranscriptUpdate::translated(Vietnamese, "", "mới", CaptionStatus::Partial).unwrap();
        lane.apply(&[source_only]);
        assert_eq!(lane.translation, "Xin chào");
        assert_eq!(lane.original, "mới");
        let translated =
            TranscriptUpdate::translated(Vietnamese, "lại", "mới", CaptionStatus::Partial).unwrap();
        lane.apply(&[translated]);
        assert_eq!(lane.translation, "lại");
    }

    #[test]
    fn rapid_changes_open_one_session_per_target_at_most() {
        let mut started = start(
            &[English],
            vec![
                (English, vec![serving("a", "one")]),
                (
                    Vietnamese,
                    (0..4).map(|n| serving("a", &format!("một {n}"))).collect(),
                ),
            ],
        );
        // Several changes before the session looks: only the last state
        // counts, and it opens one session.
        started.change(TargetChange::Add(Vietnamese));
        started.change(TargetChange::Remove(Vietnamese));
        started.change(TargetChange::Add(Vietnamese));
        started.pump(has(Vietnamese, "một 0"));
        assert_eq!(
            started.record(Vietnamese).connects.load(Ordering::SeqCst),
            1
        );
        // Changes between audio chunks each take effect, one at a time.
        for _ in 0..2 {
            started.change(TargetChange::Remove(Vietnamese));
            started.session.push_audio(silence(0.1)).unwrap();
            started.change(TargetChange::Add(Vietnamese));
            started.session.push_audio(silence(0.1)).unwrap();
        }
        // Once settled, exactly one Vietnamese session is connected. A
        // closed session that was still connecting may overlap it until its
        // next poll.
        let deadline = Instant::now() + Duration::from_secs(2);
        while started.record(Vietnamese).open.load(Ordering::SeqCst) != 1 {
            started.session.push_audio(silence(0.1)).unwrap();
            assert!(Instant::now() < deadline, "no single Vietnamese session");
            thread::sleep(Duration::from_millis(5));
        }
        // At most one session per Add; one closed before it connected never
        // connects at all.
        assert!(started.record(Vietnamese).connects.load(Ordering::SeqCst) <= 3);
        assert_eq!(started.record(English).connects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_changes_are_refused() {
        let started = start(
            &[English, Vietnamese],
            vec![(English, vec![]), (Vietnamese, vec![])],
        );
        let control = &started.control;
        assert!(!control.apply(TargetChange::Add(German)), "a third target");
        assert!(
            !control.apply(TargetChange::Add(English)),
            "a repeated target"
        );
        assert!(
            !control.apply(TargetChange::Pause(Japanese)),
            "not a target"
        );
        assert!(!control.apply(TargetChange::Resume(English)), "not paused");
        assert!(control.apply(TargetChange::Remove(English)));
        assert!(
            !control.apply(TargetChange::Remove(Vietnamese)),
            "the only target"
        );
    }

    #[test]
    fn a_closed_session_cannot_report_over_the_lane_that_replaced_it() {
        // Vietnamese's first session is still connecting when it is
        // removed; its late report must not reach the lane added after it.
        let (release, held) = mpsc::channel();
        let mut started = start(
            &[English],
            vec![
                (English, vec![serving("a", "one")]),
                (
                    Vietnamese,
                    vec![
                        Script::Hold(held, Box::new(serving("a", "cũ"))),
                        Script::Slow(Duration::from_millis(1), Box::new(serving("a", "mới"))),
                    ],
                ),
            ],
        );
        started.change(TargetChange::Add(Vietnamese));
        started.session.push_audio(silence(0.1)).unwrap();
        // The first session is inside its connect, held there.
        let deadline = Instant::now() + Duration::from_secs(2);
        while started.record(Vietnamese).connects.load(Ordering::SeqCst) != 1 {
            assert!(
                Instant::now() < deadline,
                "the first session never connected"
            );
            thread::sleep(Duration::from_millis(5));
        }
        started.change(TargetChange::Remove(Vietnamese));
        started.session.push_audio(silence(0.1)).unwrap();
        started.change(TargetChange::Add(Vietnamese));
        started.pump(has(Vietnamese, "mới"));
        let before = started.statuses.lock().unwrap().len();
        release.send(()).unwrap();
        thread::sleep(Duration::from_millis(100));
        started.session.push_audio(silence(0.1)).unwrap();
        // Nothing reported since: the old session's Active never counted.
        assert_eq!(started.statuses.lock().unwrap().len(), before);
    }

    #[test]
    fn a_failed_lane_is_reported_while_the_other_keeps_its_captions() {
        let rejection = json!({"type": "error", "error": {"type": "invalid_request_error"}});
        let failing = Script::Serve {
            on_open: vec![created()],
            replies: vec![("session.update", vec![rejection.to_string()])],
            break_after_messages: None,
        };
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("こんにちは", "Hello")]),
                (Vietnamese, vec![failing, serving("こんにちは", "Xin chào")]),
            ],
        );
        started.pump(has(English, "Hello"));
        // Let the failure surface; the session itself keeps running.
        let deadline = Instant::now() + Duration::from_secs(2);
        while started.failures.lock().unwrap().is_empty() {
            started.session.push_audio(silence(0.1)).unwrap();
            assert!(
                Instant::now() < deadline,
                "the lane failure was never reported"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let failures = started.failures.lock().unwrap().clone();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, Vietnamese);
        assert!(failures[0].1.contains("rejected the request"));
        assert_eq!(started.last_status(Vietnamese), Some(TargetStatus::Failed));
        // Resume tries the failed target again.
        started.change(TargetChange::Resume(Vietnamese));
        started.pump(has(Vietnamese, "Xin chào"));
        // The working lane still finishes normally with its text.
        let last = started.session.finish().unwrap().pop().unwrap();
        assert_eq!(text(&last, English), Some("Hello"));
        assert_eq!(last.status(), CaptionStatus::Final);
    }

    #[test]
    fn the_session_fails_only_when_its_last_running_lane_does() {
        let refuse = || vec![Script::Refuse(TransportError::Unauthorized)];
        let mut started = start(
            &[English, Japanese],
            vec![(English, refuse()), (Japanese, refuse())],
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let error = loop {
            match started.session.push_audio(silence(0.1)) {
                Ok(_) => assert!(Instant::now() < deadline, "the session never failed"),
                Err(error) => break error,
            }
            thread::sleep(Duration::from_millis(5));
        };
        // The credential problem is kept, so the user is sent to Settings.
        assert!(error.is_credential_rejected());
        assert_eq!(started.failures.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_lane_that_reconnects_keeps_the_other_lane_running() {
        let dropping = Script::Serve {
            on_open: vec![created()],
            replies: vec![],
            break_after_messages: Some(2),
        };
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("a", "steady")]),
                (Vietnamese, vec![dropping, serving("a", "trở lại")]),
            ],
        );
        let update = started.pump(has(Vietnamese, "trở lại"));
        assert_eq!(text(&update, English), Some("steady"));
        assert_eq!(started.record(English).connects.load(Ordering::SeqCst), 1);
        assert_eq!(
            started.record(Vietnamese).connects.load(Ordering::SeqCst),
            2
        );
        assert!(
            started
                .statuses
                .lock()
                .unwrap()
                .contains(&(Vietnamese, TargetStatus::Reconnecting))
        );
        assert!(started.failures.lock().unwrap().is_empty());
    }

    #[test]
    fn the_source_lane_stays_with_the_session_that_spoke_first() {
        let (release, held) = mpsc::channel();
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (
                    English,
                    vec![Script::Hold(held, Box::new(serving("one", "Hello")))],
                ),
                (Vietnamese, vec![serving("one two three", "Xin chào")]),
            ],
        );
        // The second target's session is ahead: it has the source lane.
        let update = started.pump(|update| !original(update).is_empty());
        assert_eq!(original(&update), "one two three");
        // The first target's session catches up with a shorter transcript,
        // which must not take back words that are already on screen.
        release.send(()).unwrap();
        let update = started.pump(has(English, "Hello"));
        assert_eq!(original(&update), "one two three");
        let last = started.session.finish().unwrap().pop().unwrap();
        assert_eq!(original(&last), "one two three");
    }

    #[test]
    fn the_source_lane_moves_to_a_running_session_when_its_own_is_paused() {
        let (release, held) = mpsc::channel();
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("one", "Hello")]),
                (
                    Vietnamese,
                    vec![Script::Hold(held, Box::new(serving("one two", "Xin chào")))],
                ),
            ],
        );
        // English has said everything before it is paused; a paused lane
        // keeps the text it had.
        let update = started.pump(has(English, "Hello"));
        assert_eq!(original(&update), "one");
        started.change(TargetChange::Pause(English));
        // The paused session's words stay until another session has some.
        let update = started.session.push_audio(silence(0.1)).unwrap();
        if let Some(update) = update.last() {
            assert_eq!(original(update), "one");
        }
        release.send(()).unwrap();
        let update = started.pump(|update| original(update) == "one two");
        assert_eq!(text(&update, English), Some("Hello"));
        assert_eq!(text(&update, Vietnamese), Some("Xin chào"));
    }

    #[test]
    fn removing_the_source_lane_keeps_its_text_until_another_lane_has_one() {
        let (release, held) = mpsc::channel();
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("one", "Hello")]),
                (
                    Vietnamese,
                    vec![Script::Hold(held, Box::new(serving("one two", "Xin chào")))],
                ),
            ],
        );
        let update = started.pump(has(English, "Hello"));
        assert_eq!(original(&update), "one");
        // English provided the source text; removing it must not blank it.
        started.change(TargetChange::Remove(English));
        let update = started.pump(|update| targets(update) == [Vietnamese]);
        assert_eq!(original(&update), "one");
        release.send(()).unwrap();
        let update = started.pump(|update| original(update) == "one two");
        assert_eq!(text(&update, Vietnamese), Some("Xin chào"));
    }

    #[test]
    fn stop_closes_every_session_and_is_idempotent() {
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("こんにちは", "Hello")]),
                (Vietnamese, vec![serving("こんにちは", "Xin chào")]),
            ],
        );
        started.pump(has(Vietnamese, "Xin chào"));
        let began = Instant::now();
        let last = started.session.finish().unwrap().pop().unwrap();
        assert_eq!(last.status(), CaptionStatus::Final);
        assert_eq!(text(&last, Vietnamese), Some("Xin chào"));
        // Both sessions were asked to close, and both connections are gone.
        for (language, record) in &started.records {
            let sent = record.sent.lock().unwrap();
            assert!(sent.iter().any(|message| message.contains("session.close")));
            drop(sent);
            started.wait_closed(*language);
        }
        assert!(began.elapsed() < Duration::from_secs(2));
        // A second Stop does nothing, and audio after Stop is refused.
        assert!(started.session.finish().unwrap().is_empty());
        assert!(started.session.push_audio(silence(0.1)).is_err());
    }

    #[test]
    fn stop_while_a_target_is_still_connecting_ends_in_bounded_time() {
        let (release, held) = mpsc::channel::<()>();
        let mut started = start(
            &[English],
            vec![
                (English, vec![serving("a", "one")]),
                (
                    Vietnamese,
                    vec![Script::Hold(held, Box::new(serving("a", "một")))],
                ),
            ],
        );
        started.pump(has(English, "one"));
        started.change(TargetChange::Add(Vietnamese));
        started.session.push_audio(silence(0.1)).unwrap();
        let began = Instant::now();
        started.session.finish().unwrap();
        // Bounded by the finish and handshake limits, not by the service.
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
        drop(release);
        started.wait_closed(English);
        started.wait_closed(Vietnamese);
    }

    #[test]
    fn a_replaced_session_closes_at_once_without_waiting_for_the_service() {
        // Neither lane ever answers `session.close`.
        let silent = || Script::Serve {
            on_open: vec![created()],
            replies: vec![("session.update", vec![translated_delta("text")])],
            break_after_messages: None,
        };
        let mut started = start(
            &[English, Vietnamese],
            vec![(English, vec![silent()]), (Vietnamese, vec![silent()])],
        );
        started.pump(has(English, "text"));
        started.abandoned.store(true, Ordering::Release);
        let began = Instant::now();
        assert!(started.session.finish().unwrap().is_empty());
        // Far less than the finish wait of an unanswered close.
        assert!(
            began.elapsed() < Duration::from_millis(200),
            "{:?}",
            began.elapsed()
        );
        started.wait_closed(English);
        started.wait_closed(Vietnamese);
    }

    #[test]
    fn dropping_the_session_while_a_lane_resumes_closes_every_connection() {
        let (release, held) = mpsc::channel::<()>();
        let mut started = start(
            &[English, Vietnamese],
            vec![
                (English, vec![serving("a", "one")]),
                (
                    Vietnamese,
                    vec![
                        serving("a", "một"),
                        Script::Hold(held, Box::new(serving("a", "hai"))),
                    ],
                ),
            ],
        );
        started.pump(has(Vietnamese, "một"));
        started.change(TargetChange::Pause(Vietnamese));
        started.session.push_audio(silence(0.1)).unwrap();
        started.change(TargetChange::Resume(Vietnamese));
        started.session.push_audio(silence(0.1)).unwrap();
        let records = started.records.clone();
        drop(started);
        drop(release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while records
            .iter()
            .any(|(_, record)| record.open.load(Ordering::SeqCst) != 0)
        {
            assert!(
                Instant::now() < deadline,
                "a connection outlived the session"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
