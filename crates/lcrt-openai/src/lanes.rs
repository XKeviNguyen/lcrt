//! Translation into one or two target languages at once.
//!
//! The translation service takes one output language per session, so each
//! target lane is its own [`OnlineSession`]. There are never more sessions
//! than targets, and never more than [`lcrt_core::MAX_TRANSLATION_TARGETS`].
//! Every lane receives the same captured audio, and each keeps its own
//! bounded queue and bounded reconnects.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use lcrt_core::{
    AudioChunk, CaptionStatus, Language, Transcriber, TranscriptUpdate, TranscriptionError,
    TranslationLanes, TranslationTargets,
};
use tracing::warn;

use crate::{
    credentials::ApiKey,
    session::{OnlineSession, OnlineStatus, SessionLimits, StatusCallback},
    translation::TranslationProtocol,
    transport::Connect,
};

/// What a translation session translates into and how it ends.
pub struct TranslationOptions {
    /// One session is started per target.
    pub targets: TranslationTargets,
    /// Whether the source lane is shown, and therefore transcribed.
    pub show_original: bool,
    /// Timing bounds of each session.
    pub limits: SessionLimits,
    /// Set when the session is being replaced. Its last words would be
    /// discarded anyway, so Stop then closes at once instead of waiting for
    /// the service to deliver them.
    pub abandoned: Arc<AtomicBool>,
}

/// Told when one target's session fails while another keeps running.
pub type LaneFailureCallback = Arc<dyn Fn(Language, &TranscriptionError) + Send + Sync>;

/// One target language and the session that translates into it.
struct Lane {
    target: Language,
    /// `None` once the session has failed or finished.
    session: Option<OnlineSession>,
    translation: String,
    original: String,
}

impl Lane {
    /// Keeps the newest texts. Updates are cumulative, so the last one wins.
    fn apply(&mut self, updates: &[TranscriptUpdate]) {
        if let Some(update) = updates.last() {
            self.translation = update.text().to_owned();
            if let Some(original) = update.original() {
                self.original = original.to_owned();
            }
        }
    }
}

/// A translation session with one lane per target language.
pub struct MultiTargetTranslation {
    /// In target order; the order never changes.
    lanes: Vec<Lane>,
    show_original: bool,
    on_lane_failure: LaneFailureCallback,
    abandoned: Arc<AtomicBool>,
    progress: Arc<CombinedStatus>,
    last_sent: Option<TranslationLanes>,
    finished: bool,
}

impl MultiTargetTranslation {
    /// Starts one session per target. `connect` supplies each target's
    /// connector; `status` receives the combined connection progress.
    pub fn start(
        options: TranslationOptions,
        key: &ApiKey,
        mut connect: impl FnMut(Language) -> Box<dyn Connect>,
        status: StatusCallback,
        on_lane_failure: LaneFailureCallback,
    ) -> Result<Self, TranscriptionError> {
        let TranslationOptions {
            targets,
            show_original,
            limits,
            abandoned,
        } = options;
        let progress = Arc::new(CombinedStatus {
            lanes: Mutex::new(vec![Some(OnlineStatus::Connecting); targets.iter().count()]),
            report: status,
        });
        let mut lanes = Vec::new();
        for (index, target) in targets.iter().enumerate() {
            let lane_progress = Arc::clone(&progress);
            let session = OnlineSession::start(
                // Every lane transcribes the source, so the source lane
                // survives the loss of any one session.
                TranslationProtocol::new(target, show_original),
                key.clone(),
                connect(target),
                Arc::new(move |status| lane_progress.set(index, Some(status))),
                limits,
            )?;
            lanes.push(Lane {
                target,
                session: Some(session),
                translation: String::new(),
                original: String::new(),
            });
        }
        Ok(Self {
            lanes,
            show_original,
            on_lane_failure,
            abandoned,
            progress,
            last_sent: None,
            finished: false,
        })
    }

    /// The source text comes from the first lane that is still running, or
    /// else from the first lane that has any.
    fn original(&self) -> Option<String> {
        if !self.show_original {
            return None;
        }
        let running = self.lanes.iter().find(|lane| lane.session.is_some());
        let lane = running
            .filter(|lane| !lane.original.is_empty())
            .or_else(|| self.lanes.iter().find(|lane| !lane.original.is_empty()))?;
        Some(lane.original.clone())
    }

    /// The combined update, if any lane changed since the last one.
    fn combined(&mut self, status: CaptionStatus) -> Vec<TranscriptUpdate> {
        let lanes = TranslationLanes {
            original: self.original(),
            first: self.lanes[0].translation.clone(),
            second: self.lanes.get(1).map(|lane| lane.translation.clone()),
        };
        if status == CaptionStatus::Partial && self.last_sent.as_ref() == Some(&lanes) {
            return Vec::new();
        }
        self.last_sent = Some(lanes.clone());
        TranscriptUpdate::lanes(lanes, status).into_iter().collect()
    }

    /// Drops lane `index` after `error`. While another lane is running the
    /// failure is only reported; the last lane's failure ends the session.
    fn lane_failed(
        &mut self,
        index: usize,
        error: TranscriptionError,
    ) -> Result<(), TranscriptionError> {
        self.lanes[index].session = None;
        if self.lanes.iter().all(|lane| lane.session.is_none()) {
            return Err(error);
        }
        self.progress.set(index, None);
        warn!(
            target_language = self.lanes[index].target.code(),
            "a translation lane stopped"
        );
        (self.on_lane_failure)(self.lanes[index].target, &error);
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
        for lane in &mut self.lanes {
            lane.session = None;
        }
        Ok(self.combined(CaptionStatus::Final))
    }
}

/// Folds each lane's connection progress into the one status shown.
struct CombinedStatus {
    /// `None` for a lane whose session ended.
    lanes: Mutex<Vec<Option<OnlineStatus>>>,
    report: StatusCallback,
}

impl CombinedStatus {
    fn set(&self, index: usize, status: Option<OnlineStatus>) {
        let Ok(mut lanes) = self.lanes.lock() else {
            return;
        };
        let before = combine(&lanes);
        lanes[index] = status;
        let after = combine(&lanes);
        drop(lanes);
        if after != before
            && let Some(status) = after
        {
            (self.report)(status);
        }
    }
}

/// Reconnecting if any lane is; active once every running lane is.
fn combine(lanes: &[Option<OnlineStatus>]) -> Option<OnlineStatus> {
    let mut running = lanes.iter().flatten().peekable();
    running.peek()?;
    let mut combined = OnlineStatus::Active;
    for status in running {
        match status {
            OnlineStatus::Reconnecting => return Some(OnlineStatus::Reconnecting),
            OnlineStatus::Connecting => combined = OnlineStatus::Connecting,
            OnlineStatus::Active => {}
        }
    }
    Some(combined)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use lcrt_core::{CaptionStatus, Language, Transcriber, TranscriptUpdate, TranslationTargets};
    use serde_json::json;

    use super::{LaneFailureCallback, MultiTargetTranslation, TranslationOptions, combine};
    use crate::{
        session::{
            OnlineStatus,
            tests::{
                FakeConnector, Record, Script, created, fast_limits, key, silence, statuses,
                translated_delta,
            },
        },
        transport::TransportError,
    };

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

    struct Started {
        session: MultiTargetTranslation,
        records: Vec<Record>,
        failures: Arc<Mutex<Vec<(Language, String)>>>,
        seen: Arc<Mutex<Vec<OnlineStatus>>>,
        abandoned: Arc<AtomicBool>,
    }

    fn start(show_original: bool, lanes: Vec<(Language, Vec<Script>)>) -> Started {
        let targets =
            TranslationTargets::resolve(Some(lanes[0].0), lanes.get(1).map(|lane| lane.0), None);
        let mut records = Vec::new();
        let mut connectors: Vec<_> = lanes
            .into_iter()
            .map(|(target, scripts)| {
                let (connector, record) = FakeConnector::new(scripts);
                records.push(record);
                (target, Some(connector))
            })
            .collect();
        let failures = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&failures);
        let on_failure: LaneFailureCallback = Arc::new(move |target, error| {
            sink.lock().unwrap().push((target, error.to_string()));
        });
        let (status, seen) = statuses();
        let abandoned = Arc::new(AtomicBool::new(false));
        let session = MultiTargetTranslation::start(
            TranslationOptions {
                targets,
                show_original,
                limits: fast_limits(),
                abandoned: Arc::clone(&abandoned),
            },
            &key(),
            |target| {
                let lane = connectors
                    .iter_mut()
                    .find(|(language, _)| *language == target)
                    .unwrap();
                lane.1.take().unwrap()
            },
            status,
            on_failure,
        )
        .unwrap();
        Started {
            session,
            records,
            failures,
            seen,
            abandoned,
        }
    }

    /// Feeds silence until `done` accepts the newest update.
    fn pump(
        session: &mut MultiTargetTranslation,
        done: impl Fn(&TranscriptUpdate) -> bool,
    ) -> TranscriptUpdate {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut latest = None;
        loop {
            if let Some(update) = session.push_audio(silence(0.1)).unwrap().pop() {
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

    #[test]
    fn two_targets_fill_their_own_lanes_in_target_order() {
        let mut started = start(
            true,
            vec![
                (Language::English, vec![serving("こんにちは", "Hello")]),
                (
                    Language::Vietnamese,
                    vec![serving("こんにちは", "Xin chào")],
                ),
            ],
        );
        let update = pump(&mut started.session, |update| {
            !update.text().is_empty()
                && update
                    .second_translation()
                    .is_some_and(|text| !text.is_empty())
        });
        assert_eq!(update.original(), Some("こんにちは"));
        assert_eq!(update.text(), "Hello");
        assert_eq!(update.second_translation(), Some("Xin chào"));
        // One connection per target, and no more.
        for record in &started.records {
            assert_eq!(record.connects.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn one_target_with_a_hidden_source_has_a_single_lane() {
        let mut started = start(
            false,
            vec![(Language::Japanese, vec![serving("Hello", "こんにちは")])],
        );
        let update = pump(&mut started.session, |update| !update.text().is_empty());
        assert_eq!(update.text(), "こんにちは");
        assert_eq!(update.original(), None);
        assert_eq!(update.second_translation(), None);
        assert_eq!(started.records.len(), 1);
    }

    #[test]
    fn every_lane_receives_the_same_audio() {
        let mut started = start(
            false,
            vec![
                (Language::English, vec![serving("a", "one")]),
                (Language::Vietnamese, vec![serving("a", "một")]),
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
        assert!(appended(&started.records[0]) > 0);
        assert_eq!(appended(&started.records[0]), appended(&started.records[1]));
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
            true,
            vec![
                (Language::English, vec![serving("こんにちは", "Hello")]),
                (Language::Vietnamese, vec![failing]),
            ],
        );
        let update = pump(&mut started.session, |update| !update.text().is_empty());
        assert_eq!(update.text(), "Hello");
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
        assert_eq!(failures[0].0, Language::Vietnamese);
        assert!(failures[0].1.contains("rejected the request"));
        // The working lane still finishes normally with its text.
        let last = started.session.finish().unwrap().pop().unwrap();
        assert_eq!(last.text(), "Hello");
        assert_eq!(last.status(), CaptionStatus::Final);
    }

    #[test]
    fn the_session_fails_only_when_its_last_lane_does() {
        let refuse = || vec![Script::Refuse(TransportError::Unauthorized)];
        let mut started = start(
            false,
            vec![
                (Language::English, refuse()),
                (Language::Japanese, refuse()),
            ],
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
            false,
            vec![
                (Language::English, vec![serving("a", "steady")]),
                (
                    Language::Vietnamese,
                    vec![dropping, serving("a", "trở lại")],
                ),
            ],
        );
        let update = pump(&mut started.session, |update| {
            update.second_translation() == Some("trở lại")
        });
        assert_eq!(update.text(), "steady");
        assert_eq!(started.records[0].connects.load(Ordering::SeqCst), 1);
        assert_eq!(started.records[1].connects.load(Ordering::SeqCst), 2);
        assert!(
            started
                .seen
                .lock()
                .unwrap()
                .contains(&OnlineStatus::Reconnecting)
        );
        assert!(started.failures.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_closes_every_session_and_is_idempotent() {
        let mut started = start(
            true,
            vec![
                (Language::English, vec![serving("こんにちは", "Hello")]),
                (
                    Language::Vietnamese,
                    vec![serving("こんにちは", "Xin chào")],
                ),
            ],
        );
        pump(&mut started.session, |update| {
            update
                .second_translation()
                .is_some_and(|text| !text.is_empty())
        });
        let began = Instant::now();
        let last = started.session.finish().unwrap().pop().unwrap();
        assert_eq!(last.status(), CaptionStatus::Final);
        assert_eq!(last.second_translation(), Some("Xin chào"));
        // Both sessions were asked to close, and both connections are gone.
        for record in &started.records {
            let sent = record.sent.lock().unwrap();
            assert!(sent.iter().any(|message| message.contains("session.close")));
            drop(sent);
            let deadline = Instant::now() + Duration::from_secs(2);
            while record.open.load(Ordering::SeqCst) != 0 {
                assert!(Instant::now() < deadline, "a connection stayed open");
                thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(began.elapsed() < Duration::from_secs(2));
        // A second Stop does nothing, and audio after Stop is refused.
        assert!(started.session.finish().unwrap().is_empty());
        assert!(started.session.push_audio(silence(0.1)).is_err());
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
            false,
            vec![
                (Language::English, vec![silent()]),
                (Language::Vietnamese, vec![silent()]),
            ],
        );
        pump(&mut started.session, |update| !update.text().is_empty());
        started.abandoned.store(true, Ordering::Release);
        let began = Instant::now();
        assert!(started.session.finish().unwrap().is_empty());
        // Far less than the finish wait of an unanswered close.
        assert!(
            began.elapsed() < Duration::from_millis(200),
            "{:?}",
            began.elapsed()
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while started
            .records
            .iter()
            .any(|record| record.open.load(Ordering::SeqCst) != 0)
        {
            assert!(
                Instant::now() < deadline,
                "a connection outlived the session"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn dropping_the_session_closes_every_connection() {
        let started = start(
            false,
            vec![
                (Language::English, vec![serving("a", "one")]),
                (Language::Vietnamese, vec![serving("a", "một")]),
            ],
        );
        thread::sleep(Duration::from_millis(50));
        let records = started.records.clone();
        drop(started);
        let deadline = Instant::now() + Duration::from_secs(2);
        while records
            .iter()
            .any(|record| record.open.load(Ordering::SeqCst) != 0)
        {
            assert!(
                Instant::now() < deadline,
                "a connection outlived the session"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn combined_status_follows_the_least_ready_lane() {
        use OnlineStatus::{Active, Connecting, Reconnecting};
        assert_eq!(combine(&[Some(Active), Some(Active)]), Some(Active));
        assert_eq!(combine(&[Some(Active), Some(Connecting)]), Some(Connecting));
        assert_eq!(
            combine(&[Some(Reconnecting), Some(Active)]),
            Some(Reconnecting)
        );
        // A lane that ended no longer holds the status back.
        assert_eq!(combine(&[Some(Active), None]), Some(Active));
        assert_eq!(combine(&[None, None]), None);
    }
}
