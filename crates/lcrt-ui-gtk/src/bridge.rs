use std::{
    fmt,
    sync::{Arc, Mutex, Weak},
};

use lcrt_core::{
    CaptionSink, CaptionSinkError, CaptionSnapshot, CaptionStatus, Language, Preferences,
    SessionGeneration, SessionOptions,
};

/// An API key typed into Preferences. `Debug` never reveals it.
#[derive(Clone, PartialEq)]
pub struct EnteredApiKey(String);

impl EnteredApiKey {
    /// Wraps text entered by the user.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The entered text, for validation and secure storage only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EnteredApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EnteredApiKey(<redacted>)")
    }
}

/// User intent emitted by the native window.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptionUiAction {
    /// Start a caption session. While one is active, this replaces it once
    /// the active session has fully stopped.
    Start(SessionOptions),
    /// Stop caption processing and release capture resources.
    Stop,
    /// Terminate the controller after cancelling any active caption session.
    Shutdown,
    /// Persist non-secret preferences.
    SavePreferences(Box<Preferences>),
    /// Store an API key in the system keyring.
    SaveApiKey(EnteredApiKey),
    /// Remove the stored API key.
    ClearApiKey,
    /// Verify an API key with the service: the entered one when present
    /// (without storing it), otherwise the key currently in use.
    TestConnection(Option<EnteredApiKey>),
    /// Explain the selected caption text (`start..end` in characters).
    ExplainSelection {
        /// Identifies the request; results for older ids are ignored.
        request_id: u64,
        /// Caption text containing the selection.
        caption: String,
        /// First selected character.
        start: usize,
        /// End of the selection, exclusive.
        end: usize,
        /// Language to write the explanation in.
        language: Language,
    },
}

/// How a credential status should be presented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialTone {
    /// Informational.
    Neutral,
    /// Verified or securely saved.
    Good,
    /// Needs the user's attention.
    Problem,
}

/// Credential state shown in Preferences; never contains the key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialView {
    /// Short status, such as "Saved securely".
    pub status: String,
    /// Tone for iconography; text always carries the meaning too.
    pub tone: CredentialTone,
}

/// A vocabulary explanation ready for the popover.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VocabularyCard {
    /// The explained term.
    pub term: String,
    /// Pronunciation aid, when useful.
    pub reading: Option<String>,
    /// Grammatical role, when meaningful.
    pub part_of_speech: Option<String>,
    /// Short meaning.
    pub meaning: String,
    /// Meaning in this caption.
    pub context_explanation: String,
}

/// Why no explanation can be shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VocabularyProblem {
    /// Actionable message.
    pub message: String,
    /// Whether an "Open Settings" action would fix it.
    pub needs_settings: bool,
}

/// The answer to one [`CaptionUiAction::ExplainSelection`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VocabularyOutcome {
    /// The request this answers.
    pub request_id: u64,
    /// The explanation or the reason there is none.
    pub result: Result<VocabularyCard, VocabularyProblem>,
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub(crate) struct UiPresentation {
    pub(crate) caption: Option<CaptionSnapshot>,
    pub(crate) running: Option<bool>,
    pub(crate) status: Option<String>,
}

impl UiPresentation {
    fn merge(&mut self, newer: Self) {
        if newer.caption.is_some() {
            self.caption = newer.caption;
        }
        if newer.running.is_some() {
            self.running = newer.running;
        }
        if newer.status.is_some() {
            self.status = newer.status;
        }
    }

    fn is_empty(&self) -> bool {
        self.caption.is_none() && self.running.is_none() && self.status.is_none()
    }
}

/// The changes the GTK thread should apply during one bounded poll.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UiUpdate {
    pub(crate) presentation: Option<UiPresentation>,
    pub(crate) error: Option<Option<String>>,
    pub(crate) error_needs_settings: bool,
    pub(crate) credential: Option<CredentialView>,
    pub(crate) vocabulary: Option<VocabularyOutcome>,
    pub(crate) quit: bool,
}

#[derive(Debug)]
struct BridgeState {
    receiver_open: bool,
    pending_final: Option<UiPresentation>,
    current: UiPresentation,
    latest_caption: Option<(u64, CaptionStatus)>,
    error: Option<Option<String>>,
    error_needs_settings: bool,
    credential: Option<CredentialView>,
    vocabulary: Option<VocabularyOutcome>,
    current_generation: SessionGeneration,
    quit: bool,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            receiver_open: true,
            pending_final: None,
            current: UiPresentation::default(),
            latest_caption: None,
            error: None,
            error_needs_settings: false,
            credential: None,
            vocabulary: None,
            current_generation: SessionGeneration::default(),
            quit: false,
        }
    }
}

/// Cloneable producer endpoint for the GTK-local caption state bridge.
///
/// The bridge retains at most one pending final presentation and one current,
/// replaceable presentation. Lifecycle state does not compete with captions
/// for queue capacity, and ordinary GTK lag cannot reject a publication.
#[derive(Clone, Debug)]
pub struct GtkCaptionSink {
    state: Arc<Mutex<BridgeState>>,
    /// `Some` for a handle owned by one caption session; its updates are
    /// dropped once a newer session has started.
    generation: Option<SessionGeneration>,
    _producers: Arc<()>,
}

/// GTK-thread endpoint for the caption state bridge.
///
/// A weak lifetime token lets GTK detect when the last real producer disappears
/// while retaining pending state long enough to drain it first.
#[derive(Debug)]
pub struct GtkCaptionReceiver {
    state: Arc<Mutex<BridgeState>>,
    producers: Weak<()>,
}

impl GtkCaptionSink {
    /// Creates a state bridge whose receiver must be owned by the GTK thread.
    pub fn bridge() -> (Self, GtkCaptionReceiver) {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let producers = Arc::new(());
        (
            Self {
                state: Arc::clone(&state),
                generation: None,
                _producers: Arc::clone(&producers),
            },
            GtkCaptionReceiver {
                state,
                producers: Arc::downgrade(&producers),
            },
        )
    }

    /// Makes `generation` the current session and returns a handle whose
    /// updates are ignored once a later session starts.
    pub fn start_session(
        &self,
        generation: SessionGeneration,
    ) -> Result<GtkCaptionSink, CaptionSinkError> {
        self.update(|state| state.current_generation = generation)?;
        let mut handle = self.clone();
        handle.generation = Some(generation);
        handle.set_running(true)?;
        Ok(handle)
    }

    /// Shows credential status in Preferences.
    pub fn set_credential(&self, view: CredentialView) -> Result<(), CaptionSinkError> {
        self.update(move |state| state.credential = Some(view))
    }

    /// Delivers a vocabulary explanation or problem.
    pub fn set_vocabulary(&self, outcome: VocabularyOutcome) -> Result<(), CaptionSinkError> {
        self.update(move |state| state.vocabulary = Some(outcome))
    }

    /// Updates whether captioning is running.
    pub fn set_running(&self, running: bool) -> Result<(), CaptionSinkError> {
        self.update(|state| {
            if running {
                // A new pipeline starts a new caption revision sequence. Keep
                // an unobserved prior final, but begin new live state after it.
                state.current = UiPresentation::default();
                state.latest_caption = None;
                state.current.running = Some(true);
                state.current.status = Some("Listening…".to_owned());
            } else {
                let presentation = state.control_presentation();
                presentation.running = Some(false);
                presentation.status = Some("Stopped".to_owned());
            }
        })
    }

    /// Shows a short pipeline lifecycle status.
    pub fn set_status(&self, status: impl Into<String>) -> Result<(), CaptionSinkError> {
        let status = status.into();
        if status.trim().is_empty() {
            return Err(CaptionSinkError::new(
                "GTK caption status must not be empty",
            ));
        }
        self.update(move |state| state.control_presentation().status = Some(status))
    }

    /// Shows an actionable error in the caption window.
    pub fn show_error(&self, message: impl Into<String>) -> Result<(), CaptionSinkError> {
        let message = message.into();
        if message.trim().is_empty() {
            return Err(CaptionSinkError::new(
                "GTK caption error message must not be empty",
            ));
        }
        self.update(move |state| {
            state.error = Some(Some(message));
            state.error_needs_settings = false;
        })
    }

    /// Shows an error that the user fixes in Preferences, with a way there.
    pub fn show_settings_error(&self, message: impl Into<String>) -> Result<(), CaptionSinkError> {
        let message = message.into();
        if message.trim().is_empty() {
            return Err(CaptionSinkError::new(
                "GTK caption error message must not be empty",
            ));
        }
        self.update(move |state| {
            state.error = Some(Some(message));
            state.error_needs_settings = true;
        })
    }

    /// Clears a previously displayed error.
    pub fn clear_error(&self) -> Result<(), CaptionSinkError> {
        self.update(|state| state.error = Some(None))
    }

    /// Requests an orderly application exit.
    pub fn quit(&self) -> Result<(), CaptionSinkError> {
        self.update(|state| state.quit = true)
    }

    fn update(&self, update: impl FnOnce(&mut BridgeState)) -> Result<(), CaptionSinkError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CaptionSinkError::new("GTK caption bridge state is unavailable"))?;
        if !state.receiver_open {
            return Err(CaptionSinkError::new(
                "GTK caption window is no longer available",
            ));
        }
        if self
            .generation
            .is_some_and(|generation| generation != state.current_generation)
        {
            // A late event from a session that has already been replaced.
            return Ok(());
        }
        update(&mut state);
        Ok(())
    }
}

impl BridgeState {
    fn control_presentation(&mut self) -> &mut UiPresentation {
        if self.current.is_empty()
            && let Some(pending) = self.pending_final.as_mut()
        {
            return pending;
        }
        &mut self.current
    }
}

impl CaptionSink for GtkCaptionSink {
    /// Coalesces obsolete partials while retaining an unobserved final boundary.
    fn publish(&mut self, snapshot: CaptionSnapshot) -> Result<(), CaptionSinkError> {
        self.update(|state| {
            let status = snapshot.caption().status();
            let identity = (snapshot.revision(), status);
            let replace = match state.latest_caption {
                None => true,
                Some((revision, _)) if snapshot.revision() > revision => true,
                Some((revision, _)) if snapshot.revision() < revision => false,
                Some((_, CaptionStatus::Partial)) => status == CaptionStatus::Final,
                Some((_, CaptionStatus::Final)) => false,
            };
            if !replace {
                return;
            }
            state.latest_caption = Some(identity);
            state.current.caption = Some(snapshot);
            state.current.status = Some(
                match status {
                    CaptionStatus::Partial => "Listening…",
                    CaptionStatus::Final => "Final",
                }
                .to_owned(),
            );
            if status == CaptionStatus::Final {
                let finalized = std::mem::take(&mut state.current);
                if let Some(pending) = state.pending_final.as_mut() {
                    pending.merge(finalized);
                } else {
                    state.pending_final = Some(finalized);
                }
            }
        })
    }
}

impl GtkCaptionReceiver {
    /// Takes at most one bounded presentation plus current control state.
    pub(crate) fn take_update(&self) -> Result<UiUpdate, CaptionSinkError> {
        let producers_alive = self.producers.upgrade().is_some();
        let mut state = self
            .state
            .lock()
            .map_err(|_| CaptionSinkError::new("GTK caption bridge state is unavailable"))?;
        let presentation = state
            .pending_final
            .take()
            .or_else(|| (!state.current.is_empty()).then(|| std::mem::take(&mut state.current)));
        let update = UiUpdate {
            presentation,
            error: state.error.take(),
            error_needs_settings: state.error_needs_settings,
            credential: state.credential.take(),
            vocabulary: state.vocabulary.take(),
            quit: state.quit,
        };
        if !producers_alive
            && update.presentation.is_none()
            && update.error.is_none()
            && update.credential.is_none()
            && update.vocabulary.is_none()
            && !update.quit
        {
            return Err(CaptionSinkError::new(
                "GTK caption controller is no longer available",
            ));
        }
        Ok(update)
    }
}

impl Drop for GtkCaptionReceiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.receiver_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use lcrt_core::{
        CaptionSink, CaptionSnapshot, CaptionState, CaptionStatus, SessionGeneration,
        TranscriptUpdate,
    };

    use super::{
        CaptionUiAction, CredentialTone, CredentialView, EnteredApiKey, GtkCaptionSink,
        VocabularyOutcome, VocabularyProblem,
    };

    fn snapshot(
        state: &mut CaptionState,
        text: impl Into<String>,
        final_caption: bool,
    ) -> CaptionSnapshot {
        let update = if final_caption {
            TranscriptUpdate::finalized(text)
        } else {
            TranscriptUpdate::partial(text)
        };
        state.apply(update.unwrap()).unwrap()
    }

    fn presentation(receiver: &super::GtkCaptionReceiver) -> super::UiPresentation {
        receiver.take_update().unwrap().presentation.unwrap()
    }

    #[test]
    fn caption_flood_is_coalesced_to_the_newest_snapshot() {
        let (mut sink, receiver) = GtkCaptionSink::bridge();
        let mut captions = CaptionState::new();
        for revision in 1..=10_000 {
            sink.publish(snapshot(
                &mut captions,
                format!("caption {revision}"),
                false,
            ))
            .unwrap();
        }

        assert_eq!(presentation(&receiver).caption.unwrap().revision(), 10_000);
        assert!(receiver.take_update().unwrap().presentation.is_none());
    }

    #[test]
    fn stale_partial_cannot_replace_a_newer_final() {
        let (mut sink, receiver) = GtkCaptionSink::bridge();
        let mut captions = CaptionState::new();
        let partial = snapshot(&mut captions, "draft", false);
        let final_caption = snapshot(&mut captions, "final", true);

        sink.publish(final_caption).unwrap();
        sink.publish(partial).unwrap();

        let presentation = presentation(&receiver);
        assert_eq!(
            presentation.caption.unwrap().caption().status(),
            CaptionStatus::Final
        );
        assert_eq!(presentation.status.as_deref(), Some("Final"));
    }

    #[test]
    fn final_is_observed_before_a_newer_utterance_partial() {
        let (mut sink, receiver) = GtkCaptionSink::bridge();
        let mut captions = CaptionState::new();

        sink.publish(snapshot(&mut captions, "utterance A", true))
            .unwrap();
        sink.publish(snapshot(&mut captions, "utterance B", false))
            .unwrap();

        let final_presentation = presentation(&receiver);
        assert_eq!(
            final_presentation.caption.unwrap().caption().text(),
            "utterance A"
        );
        assert_eq!(final_presentation.status.as_deref(), Some("Final"));
        let partial_presentation = presentation(&receiver);
        assert_eq!(
            partial_presentation.caption.unwrap().caption().text(),
            "utterance B"
        );
        assert_eq!(partial_presentation.status.as_deref(), Some("Listening…"));
        assert!(receiver.take_update().unwrap().presentation.is_none());
    }

    #[test]
    fn repeated_rollover_retains_only_one_final_and_one_partial() {
        let (mut sink, receiver) = GtkCaptionSink::bridge();
        let mut captions = CaptionState::new();
        for utterance in 0..1_000 {
            sink.publish(snapshot(&mut captions, format!("final {utterance}"), true))
                .unwrap();
            sink.publish(snapshot(
                &mut captions,
                format!("partial {utterance}"),
                false,
            ))
            .unwrap();
        }

        assert_eq!(
            presentation(&receiver).caption.unwrap().caption().text(),
            "final 999"
        );
        assert_eq!(
            presentation(&receiver).caption.unwrap().caption().text(),
            "partial 999"
        );
        assert!(receiver.take_update().unwrap().presentation.is_none());
    }

    #[test]
    fn later_caption_status_wins_over_earlier_explicit_status() {
        let (mut sink, receiver) = GtkCaptionSink::bridge();
        let mut captions = CaptionState::new();
        sink.set_status("Listening…").unwrap();
        sink.publish(snapshot(&mut captions, "complete", true))
            .unwrap();
        assert_eq!(presentation(&receiver).status.as_deref(), Some("Final"));

        sink.set_status("Stopping…").unwrap();
        sink.publish(snapshot(&mut captions, "next words", false))
            .unwrap();
        assert_eq!(
            presentation(&receiver).status.as_deref(),
            Some("Listening…")
        );
    }

    #[test]
    fn later_explicit_status_wins_over_final_caption_status() {
        let (mut sink, receiver) = GtkCaptionSink::bridge();
        let mut captions = CaptionState::new();
        sink.publish(snapshot(&mut captions, "complete", true))
            .unwrap();
        sink.set_status("Error").unwrap();

        let presentation = presentation(&receiver);
        assert_eq!(
            presentation.caption.unwrap().caption().status(),
            CaptionStatus::Final
        );
        assert_eq!(presentation.status.as_deref(), Some("Error"));
    }

    #[test]
    fn running_and_status_follow_publication_order() {
        let (sink, receiver) = GtkCaptionSink::bridge();
        sink.set_running(false).unwrap();
        sink.set_status("Error").unwrap();
        let first_presentation = presentation(&receiver);
        assert_eq!(first_presentation.running, Some(false));
        assert_eq!(first_presentation.status.as_deref(), Some("Error"));

        sink.set_status("Stopping…").unwrap();
        sink.set_running(false).unwrap();
        assert_eq!(presentation(&receiver).status.as_deref(), Some("Stopped"));
    }

    #[test]
    fn control_state_remains_observable_during_caption_saturation() {
        let (mut sink, receiver) = GtkCaptionSink::bridge();
        let mut captions = CaptionState::new();
        for revision in 0..1_024 {
            sink.publish(snapshot(
                &mut captions,
                format!("caption {revision}"),
                false,
            ))
            .unwrap();
        }
        sink.show_error("capture failed").unwrap();
        sink.set_running(false).unwrap();
        sink.set_status("Error").unwrap();
        sink.quit().unwrap();

        let update = receiver.take_update().unwrap();
        assert_eq!(
            update.presentation.unwrap().caption.unwrap().revision(),
            1_024
        );
        assert_eq!(update.error, Some(Some("capture failed".to_owned())));
        assert!(update.quit);
    }

    #[test]
    fn last_producer_disappearance_is_terminal_for_receiver() {
        let (sink, receiver) = GtkCaptionSink::bridge();
        drop(sink);

        assert!(receiver.take_update().is_err());
    }

    #[test]
    fn last_producer_state_is_drained_before_disconnect() {
        let (sink, receiver) = GtkCaptionSink::bridge();
        sink.quit().unwrap();
        drop(sink);

        assert!(receiver.take_update().unwrap().quit);
    }

    #[test]
    fn receiver_disappearance_is_terminal_for_producer() {
        let (sink, receiver) = GtkCaptionSink::bridge();
        drop(receiver);

        assert!(sink.quit().is_err());
    }

    #[test]
    fn stale_session_events_never_reach_the_current_session() {
        let (controller, receiver) = GtkCaptionSink::bridge();
        let mut old = controller
            .start_session(SessionGeneration::default().next())
            .unwrap();
        receiver.take_update().unwrap();
        let mut current = controller
            .start_session(SessionGeneration::default().next().next())
            .unwrap();
        receiver.take_update().unwrap();

        let mut captions = CaptionState::new();
        old.publish(snapshot(
            &mut captions,
            "late words from the old session",
            true,
        ))
        .unwrap();
        old.set_status("Reconnecting…").unwrap();
        old.set_running(false).unwrap();
        old.show_error("Connection was lost.").unwrap();
        let update = receiver.take_update().unwrap();
        assert!(update.presentation.is_none());
        assert!(update.error.is_none());

        current
            .publish(snapshot(&mut captions, "current words", false))
            .unwrap();
        assert_eq!(
            presentation(&receiver).caption.unwrap().caption().text(),
            "current words"
        );
    }

    #[test]
    fn credential_and_vocabulary_results_are_delivered_latest_first() {
        let (sink, receiver) = GtkCaptionSink::bridge();
        sink.set_credential(CredentialView {
            status: "Not configured".to_owned(),
            tone: CredentialTone::Neutral,
        })
        .unwrap();
        sink.set_credential(CredentialView {
            status: "Saved securely".to_owned(),
            tone: CredentialTone::Good,
        })
        .unwrap();
        sink.set_vocabulary(VocabularyOutcome {
            request_id: 7,
            result: Err(VocabularyProblem {
                message: "Vocabulary explanations need an OpenAI API key.".to_owned(),
                needs_settings: true,
            }),
        })
        .unwrap();
        let update = receiver.take_update().unwrap();
        assert_eq!(update.credential.unwrap().status, "Saved securely");
        assert_eq!(update.vocabulary.unwrap().request_id, 7);
    }

    #[test]
    fn entered_api_keys_are_redacted_in_debug_output() {
        let action = CaptionUiAction::SaveApiKey(EnteredApiKey::new("sk-secret-value".to_owned()));
        assert!(!format!("{action:?}").contains("secret"));
    }

    #[test]
    fn bridge_rejects_empty_errors_and_statuses() {
        let (sink, _receiver) = GtkCaptionSink::bridge();
        assert!(sink.show_error("  ").is_err());
        assert!(sink.set_status("").is_err());
    }
}
