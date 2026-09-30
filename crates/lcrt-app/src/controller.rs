//! The application controller: the single owner of caption sessions,
//! preferences, and the API key. It runs on its own thread so the GTK thread
//! never waits on audio, models, the keyring, files, or the network.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, TryRecvError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use lcrt_audio_pipewire::{PipeWireCapture, PipeWireCaptureConfig};
use lcrt_core::{
    AudioSourceDescriptor, CaptionPipeline, CaptionSinkError, Language, PipelineError, Preferences,
    ProcessingMode, RunSummary, RuntimeConfig, SessionGeneration, SessionOptions, Transcriber,
    TranscriptionError,
};
use lcrt_openai::{
    credentials::{ApiKey, CredentialStatus, Credentials, KeyringStore},
    session::{OnlineSession, OnlineStatus, SessionLimits, user_message},
    transcription::TranscriptionProtocol,
    translation::TranslationProtocol,
    transport::WebSocketConnector,
    vocabulary::{self, VocabularyCache, VocabularyError, VocabularyRequest},
};
use lcrt_stt_whisper::{WhisperConfig, WhisperTranscriber};
use lcrt_ui_gtk::{
    CaptionUiAction, CredentialTone, CredentialView, EnteredApiKey, GtkCaptionSink, VocabularyCard,
    VocabularyOutcome, VocabularyProblem,
};
use tracing::{error, info, warn};

use crate::settings::SettingsStore;

const CONTROLLER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_VOCABULARY_LOOKUPS: usize = 3;

/// User-facing text for settings-fixable problems.
pub(crate) const MISSING_MODEL: &str =
    "Choose a local Whisper model in Settings to use Offline Captions.";
pub(crate) const MISSING_KEY: &str = "Online mode needs an OpenAI API key.";
const MISSING_VOCABULARY_KEY: &str = "Vocabulary explanations need an OpenAI API key.";

/// Settings that come from the command line for this run only.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunOverrides {
    /// `--model` or `LCRT_MODEL_PATH`.
    pub(crate) model_path: Option<PathBuf>,
    /// `--language`, for offline Whisper.
    pub(crate) language: Option<String>,
    /// A bounded diagnostic run that quits after its session.
    pub(crate) smoke: bool,
}

/// The startup phase guarded by [`StartupGate`].
///
/// `Cancelled` is reachable only when cancellation wins before audio
/// acquisition commits. Cancellation after that commit is delivered through
/// the gate's atomic flag so the active pipeline can stop without blocking on
/// the startup mutex.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupPhase {
    SttReady,
    AudioAcquisition,
    Active,
    Cancelled,
}

/// Linearizes the race between stopping a session and starting audio capture.
///
/// The lock spans only the transition from `SttReady` to
/// `AudioAcquisition`; native PipeWire startup runs after that transition has
/// committed. Therefore, cancellation either wins while STT is ready and no
/// audio starter is called, or acquisition wins and a later cancellation stops
/// the resulting session once startup returns.
struct StartupGate {
    phase: Mutex<StartupPhase>,
    cancelled: AtomicBool,
}

impl StartupGate {
    fn new() -> Self {
        Self {
            phase: Mutex::new(StartupPhase::SttReady),
            cancelled: AtomicBool::new(false),
        }
    }

    fn cancel(&self) {
        let mut phase = self.phase.lock().expect("startup phase mutex poisoned");
        if matches!(*phase, StartupPhase::SttReady) {
            *phase = StartupPhase::Cancelled;
        }
        self.cancelled.store(true, Ordering::Release);
    }

    fn begin_audio_acquisition(&self) -> bool {
        let mut phase = self.phase.lock().expect("startup phase mutex poisoned");
        if !matches!(*phase, StartupPhase::SttReady) {
            return false;
        }
        *phase = StartupPhase::AudioAcquisition;
        true
    }

    fn complete_audio_acquisition(&self) {
        let mut phase = self.phase.lock().expect("startup phase mutex poisoned");
        if matches!(*phase, StartupPhase::AudioAcquisition) {
            *phase = StartupPhase::Active;
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancellation_flag(&self) -> &AtomicBool {
        &self.cancelled
    }

    #[cfg(test)]
    fn phase(&self) -> StartupPhase {
        *self.phase.lock().expect("startup phase mutex poisoned")
    }
}

/// Orders credential actions so a slow connection test can't overwrite the
/// status of a newer test, save or clear.
#[derive(Clone, Default)]
struct CredentialActions(Arc<AtomicU64>);

impl CredentialActions {
    fn begin(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn is_latest(&self, action: u64) -> bool {
        self.0.load(Ordering::Acquire) == action
    }
}

/// Why a caption session ended early, in words for the user.
#[derive(Debug)]
struct SessionFailure {
    message: String,
    /// The user must fix a setting (such as the API key) before retrying.
    needs_settings: bool,
}

impl SessionFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            needs_settings: false,
        }
    }

    fn from_pipeline(error: &(dyn std::error::Error + 'static)) -> Self {
        // Backend errors already carry user-facing text.
        let transcription = error.downcast_ref::<TranscriptionError>().or_else(|| {
            match error.downcast_ref::<PipelineError>() {
                Some(PipelineError::Transcription(error)) => Some(error),
                _ => None,
            }
        });
        match transcription {
            Some(transcription) => Self {
                message: transcription.to_string(),
                needs_settings: transcription.is_credential_rejected(),
            },
            None => Self::new(error.to_string()),
        }
    }
}

struct PipelineSession {
    startup: Arc<StartupGate>,
    result: Receiver<Result<RunSummary, SessionFailure>>,
    worker: JoinHandle<()>,
    /// Set once the backend is working: the model loaded, or the online
    /// connection became active. A diagnostic run passes only if it was.
    backend_ready: Arc<AtomicBool>,
}

/// The controller is the authoritative owner of application termination.
///
/// `Active` owns the only live pipeline session. A shutdown transitions through
/// `ShutdownRequested`, cancels that session if present, and then detaches it
/// before reaching `Terminated`; the GTK thread never waits for worker joins.
enum ControllerState {
    Idle,
    Active(PipelineSession),
    ShutdownRequested,
    Terminated,
}

/// How the controller ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControllerOutcome {
    Completed,
    SmokeSucceeded,
    SmokeFailed,
}

/// The processing backend a session will use, resolved before it starts.
enum Backend {
    Offline {
        model_path: PathBuf,
        language: Option<String>,
    },
    Online {
        key: ApiKey,
        options: SessionOptions,
    },
}

pub(crate) struct Controller {
    overrides: RunOverrides,
    sources: Vec<AudioSourceDescriptor>,
    sink: GtkCaptionSink,
    preferences: Preferences,
    store: Option<SettingsStore>,
    credentials: Credentials<KeyringStore>,
    vocabulary_cache: Arc<Mutex<VocabularyCache>>,
    vocabulary_lookups: Arc<AtomicUsize>,
    credential_actions: CredentialActions,
    /// Why the last write of `preferences` failed. Retried on the next change
    /// and at shutdown, and kept on screen until a write succeeds.
    preferences_save_error: Option<String>,
    /// Whether the window's error banner currently shows that save warning,
    /// so a later successful save clears only the warning.
    save_warning_shown: bool,
    state: ControllerState,
    generation: SessionGeneration,
    pending_start: Option<SessionOptions>,
}

impl Controller {
    pub(crate) fn new(
        overrides: RunOverrides,
        sources: Vec<AudioSourceDescriptor>,
        sink: GtkCaptionSink,
        preferences: Preferences,
        store: Option<SettingsStore>,
        environment_key: Option<String>,
    ) -> Self {
        Self {
            overrides,
            sources,
            sink,
            preferences,
            store,
            credentials: Credentials::new(KeyringStore, environment_key.as_deref()),
            vocabulary_cache: Arc::new(Mutex::new(VocabularyCache::default())),
            vocabulary_lookups: Arc::new(AtomicUsize::new(0)),
            credential_actions: CredentialActions::default(),
            preferences_save_error: None,
            save_warning_shown: false,
            state: ControllerState::Idle,
            generation: SessionGeneration::default(),
            pending_start: None,
        }
    }

    pub(crate) fn run(mut self, actions: Receiver<CaptionUiAction>) -> ControllerOutcome {
        let status = self.credentials.status();
        notify_ui(self.sink.set_credential(credential_view(status)));
        loop {
            if let Some((completed, backend_ready)) = take_completed_session(&mut self.state) {
                // A diagnostic passes only if it exercised the whole path:
                // a working backend and captured audio.
                let captured_audio = completed
                    .as_ref()
                    .is_ok_and(|summary| summary.audio_chunks > 0);
                let succeeded = captured_audio && backend_ready;
                if self.overrides.smoke && completed.is_ok() && !succeeded {
                    error!(
                        backend_ready,
                        captured_audio, "diagnostic failed: the capture path did not run"
                    );
                }
                self.publish_completion(completed);
                if self.overrides.smoke {
                    notify_ui(self.sink.quit());
                    return if succeeded {
                        ControllerOutcome::SmokeSucceeded
                    } else {
                        ControllerOutcome::SmokeFailed
                    };
                }
                if let Some(options) = self.pending_start.take() {
                    self.start(options);
                }
            }

            match actions.recv_timeout(CONTROLLER_POLL_INTERVAL) {
                Ok(CaptionUiAction::Start(options)) => {
                    if matches!(self.state, ControllerState::Active(_)) {
                        // Replace the running session once it has fully stopped,
                        // so two sessions never overlap.
                        self.pending_start = Some(options);
                        self.cancel_active();
                    } else if !self.start(options) && self.overrides.smoke {
                        notify_ui(self.sink.quit());
                        return ControllerOutcome::SmokeFailed;
                    }
                }
                Ok(CaptionUiAction::Stop) => {
                    self.pending_start = None;
                    if self.cancel_active() {
                        notify_ui(self.sink.set_status("Stopping…"));
                    }
                }
                Ok(CaptionUiAction::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    if self.preferences_save_error.is_some() {
                        self.persist_preferences();
                    }
                    request_controller_shutdown(&mut self.state);
                    return if self.overrides.smoke {
                        ControllerOutcome::SmokeFailed
                    } else {
                        ControllerOutcome::Completed
                    };
                }
                Ok(CaptionUiAction::SavePreferences(preferences)) => {
                    self.save_preferences(*preferences);
                }
                Ok(CaptionUiAction::SaveApiKey(entered)) => {
                    self.credential_actions.begin();
                    self.save_api_key(entered.expose());
                }
                Ok(CaptionUiAction::ClearApiKey) => {
                    self.credential_actions.begin();
                    let view = match self.credentials.clear() {
                        Ok(status) => credential_view(status),
                        Err(error) => problem_view(&format!(
                            "Couldn't clear the key: {error}. Try again once the keyring is available."
                        )),
                    };
                    notify_ui(self.sink.set_credential(view));
                }
                Ok(CaptionUiAction::TestConnection(entered)) => {
                    self.test_connection(entered.as_ref());
                }
                Ok(CaptionUiAction::ExplainSelection {
                    request_id,
                    caption,
                    start,
                    end,
                    language,
                }) => self.explain(request_id, &caption, start, end, language),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Shows an error in the window's banner, replacing whatever it showed.
    fn show_error(&mut self, message: impl Into<String>, needs_settings: bool) {
        self.save_warning_shown = false;
        let message = message.into();
        notify_ui(if needs_settings {
            self.sink.show_settings_error(message)
        } else {
            self.sink.show_error(message)
        });
    }

    fn cancel_active(&self) -> bool {
        if let ControllerState::Active(session) = &self.state {
            session.startup.cancel();
            true
        } else {
            false
        }
    }

    /// Starts a session; returns false when it could not start.
    fn start(&mut self, options: SessionOptions) -> bool {
        let Some(source) = self
            .sources
            .iter()
            .find(|source| source.id() == options.source_id)
            .cloned()
        else {
            self.show_error("The selected audio source is no longer available.", false);
            return false;
        };
        let backend = match self.resolve_backend(&options) {
            Ok(backend) => backend,
            Err(message) => {
                self.show_error(message, true);
                notify_ui(self.sink.set_running(false));
                notify_ui(self.sink.set_status("Ready"));
                return false;
            }
        };
        self.generation = self.generation.next();
        // A new session retires the previous session's error, but an unsaved
        // settings warning stays until a write succeeds.
        match &self.preferences_save_error {
            Some(message) => {
                notify_ui(self.sink.show_error(message.clone()));
                self.save_warning_shown = true;
            }
            None => {
                notify_ui(self.sink.clear_error());
                self.save_warning_shown = false;
            }
        }
        let session_sink = match self.sink.start_session(self.generation) {
            Ok(sink) => sink,
            Err(error) => {
                warn!(%error, "caption window is gone; not starting");
                return false;
            }
        };
        let starting = match options.mode {
            ProcessingMode::OfflineCaptions => "Loading model…",
            ProcessingMode::OnlineCaptions | ProcessingMode::Translation => "Connecting…",
        };
        notify_ui(session_sink.set_status(starting));
        info!(
            generation = %self.generation,
            mode = options.mode.label(),
            "starting caption session"
        );
        match start_pipeline(source, backend, session_sink) {
            Ok(session) => {
                self.state = ControllerState::Active(session);
                true
            }
            Err(message) => {
                notify_ui(self.sink.set_running(false));
                notify_ui(self.sink.set_status("Error"));
                self.show_error(message, false);
                false
            }
        }
    }

    fn resolve_backend(&self, options: &SessionOptions) -> Result<Backend, &'static str> {
        match options.mode {
            ProcessingMode::OfflineCaptions => {
                let model_path = self
                    .overrides
                    .model_path
                    .clone()
                    .or_else(|| self.preferences.general.model_path.clone())
                    .ok_or(MISSING_MODEL)?;
                Ok(Backend::Offline {
                    model_path,
                    language: self.overrides.language.clone(),
                })
            }
            ProcessingMode::OnlineCaptions | ProcessingMode::Translation => {
                let (key, _) = self.credentials.resolve().ok_or(MISSING_KEY)?;
                Ok(Backend::Online {
                    key,
                    options: options.clone(),
                })
            }
        }
    }

    fn publish_completion(&mut self, result: Result<RunSummary, SessionFailure>) {
        let replacing = self.pending_start.is_some();
        match result {
            Ok(summary) => {
                info!(
                    audio_chunks = summary.audio_chunks,
                    caption_updates = summary.caption_updates,
                    "caption session completed"
                );
                if !replacing {
                    notify_ui(self.sink.set_running(false));
                    notify_ui(self.sink.set_status("Stopped"));
                }
            }
            Err(failure) => {
                error!(message = %failure.message, "caption session failed");
                notify_ui(self.sink.set_running(false));
                notify_ui(self.sink.set_status("Error"));
                self.show_error(failure.message, failure.needs_settings);
            }
        }
    }

    fn save_preferences(&mut self, preferences: Preferences) {
        self.preferences = preferences.normalized();
        self.persist_preferences();
    }

    /// Writes the authoritative preferences. On failure they still apply
    /// until LCRT quits, and the user is told they were not saved.
    fn persist_preferences(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        match store.save(&self.preferences) {
            Ok(()) => {
                // Clear the banner only if it still shows the save warning,
                // not an error that replaced it since.
                if self.preferences_save_error.take().is_some() && self.save_warning_shown {
                    self.save_warning_shown = false;
                    notify_ui(self.sink.clear_error());
                }
            }
            Err(error) => {
                warn!(%error, "could not save preferences");
                let message =
                    format!("Couldn't save settings ({error}). Changes apply until LCRT quits.");
                self.show_error(message.clone(), false);
                self.save_warning_shown = true;
                self.preferences_save_error = Some(message);
            }
        }
    }

    fn save_api_key(&mut self, entered: &str) {
        let view = match ApiKey::parse(entered) {
            Err(invalid) => problem_view(&invalid.to_string()),
            Ok(key) => match self.credentials.save(key) {
                Ok(status) => credential_view(status),
                Err(error) => problem_view(&format!("Couldn't save the key: {error}")),
            },
        };
        notify_ui(self.sink.set_credential(view));
    }

    fn test_connection(&self, entered: Option<&EnteredApiKey>) {
        let action = self.credential_actions.begin();
        let key = match key_to_test(entered, || self.credentials.resolve().map(|(key, _)| key)) {
            Ok(key) => key,
            Err(message) => {
                notify_ui(self.sink.set_credential(problem_view(&message)));
                return;
            }
        };
        let sink = self.sink.clone();
        let credential_actions = self.credential_actions.clone();
        let spawned = thread::Builder::new()
            .name("lcrt-connection-test".to_owned())
            .spawn(move || {
                let view = match lcrt_openai::http::test_connection(&key) {
                    Ok(()) => CredentialView {
                        status: "Connection verified".to_owned(),
                        tone: CredentialTone::Good,
                    },
                    Err(error) => problem_view(user_message(&error)),
                };
                // A newer test, save or clear owns the status now.
                if credential_actions.is_latest(action) {
                    notify_ui(sink.set_credential(view));
                }
            });
        if spawned.is_err() {
            notify_ui(
                self.sink
                    .set_credential(problem_view("Couldn't start the connection test.")),
            );
        }
    }

    fn explain(
        &self,
        request_id: u64,
        caption: &str,
        start: usize,
        end: usize,
        language: Language,
    ) {
        if !self.preferences.vocabulary.enabled {
            return;
        }
        let reply = move |sink: &GtkCaptionSink, result| {
            notify_ui(sink.set_vocabulary(VocabularyOutcome { request_id, result }));
        };
        let request = match VocabularyRequest::from_caption(caption, start, end, language) {
            Ok(request) => request,
            Err(error) => {
                reply(&self.sink, Err(problem(&error.to_string(), false)));
                return;
            }
        };
        if let Some(cached) = self
            .vocabulary_cache
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(&request))
        {
            reply(&self.sink, Ok(card(cached)));
            return;
        }
        let Some((key, _)) = self.credentials.resolve() else {
            reply(&self.sink, Err(problem(MISSING_VOCABULARY_KEY, true)));
            return;
        };
        if self.vocabulary_lookups.load(Ordering::Acquire) >= MAX_VOCABULARY_LOOKUPS {
            reply(
                &self.sink,
                Err(problem(
                    "Still looking up earlier selections. Try again shortly.",
                    false,
                )),
            );
            return;
        }
        self.vocabulary_lookups.fetch_add(1, Ordering::AcqRel);
        let lookups = Arc::clone(&self.vocabulary_lookups);
        let cache = Arc::clone(&self.vocabulary_cache);
        let sink = self.sink.clone();
        let spawned = thread::Builder::new()
            .name("lcrt-vocabulary".to_owned())
            .spawn(move || {
                let result = vocabulary::explain(&key, &request);
                lookups.fetch_sub(1, Ordering::AcqRel);
                let result = match result {
                    Ok(explanation) => {
                        if let Ok(mut cache) = cache.lock() {
                            cache.insert(request, explanation.clone());
                        }
                        Ok(card(explanation))
                    }
                    Err(error) => {
                        let needs_settings = matches!(&error, VocabularyError::Service(service)
                            if service.is_credential_rejected());
                        Err(problem(&error.to_string(), needs_settings))
                    }
                };
                reply(&sink, result);
            });
        if spawned.is_err() {
            self.vocabulary_lookups.fetch_sub(1, Ordering::AcqRel);
            reply(
                &self.sink,
                Err(problem("Couldn't start the lookup.", false)),
            );
        }
    }
}

fn card(explanation: vocabulary::Explanation) -> VocabularyCard {
    VocabularyCard {
        term: explanation.term,
        reading: explanation.reading,
        part_of_speech: explanation.part_of_speech,
        meaning: explanation.meaning,
        context_explanation: explanation.context_explanation,
    }
}

fn problem(message: &str, needs_settings: bool) -> VocabularyProblem {
    VocabularyProblem {
        message: message.to_owned(),
        needs_settings,
    }
}

/// The key a connection test checks: the one just entered, without storing
/// it, otherwise the key currently in use.
fn key_to_test(
    entered: Option<&EnteredApiKey>,
    in_use: impl FnOnce() -> Option<ApiKey>,
) -> Result<ApiKey, String> {
    match entered {
        Some(entered) => ApiKey::parse(entered.expose()).map_err(|invalid| invalid.to_string()),
        None => in_use().ok_or_else(|| "Enter an API key to test.".to_owned()),
    }
}

fn credential_view(status: CredentialStatus) -> CredentialView {
    CredentialView {
        status: status.label().to_owned(),
        tone: match status {
            CredentialStatus::NotConfigured => CredentialTone::Neutral,
            CredentialStatus::SavedSecurely | CredentialStatus::UsingEnvironment => {
                CredentialTone::Good
            }
            CredentialStatus::SessionOnly => CredentialTone::Problem,
        },
    }
}

fn problem_view(message: &str) -> CredentialView {
    CredentialView {
        status: message.to_owned(),
        tone: CredentialTone::Problem,
    }
}

fn request_controller_shutdown(state: &mut ControllerState) {
    let previous = std::mem::replace(state, ControllerState::ShutdownRequested);
    if let ControllerState::Active(session) = previous {
        session.startup.cancel();
    }
    *state = ControllerState::Terminated;
}

fn start_pipeline(
    source: AudioSourceDescriptor,
    backend: Backend,
    sink: GtkCaptionSink,
) -> Result<PipelineSession, String> {
    let startup = Arc::new(StartupGate::new());
    let worker_startup = Arc::clone(&startup);
    let backend_ready = Arc::new(AtomicBool::new(false));
    let worker_ready = Arc::clone(&backend_ready);
    let (result_sender, result) = sync_channel(1);
    let worker = thread::Builder::new()
        .name("lcrt-caption-pipeline".to_owned())
        .spawn(move || {
            let result = run_pipeline(source, backend, sink, &worker_startup, worker_ready)
                .map_err(|error| SessionFailure::from_pipeline(error.as_ref()));
            let _ = result_sender.send(result);
        })
        .map_err(|error| format!("could not start the caption pipeline worker: {error}"))?;
    Ok(PipelineSession {
        startup,
        result,
        worker,
        backend_ready,
    })
}

fn open_backend(
    backend: Backend,
    sink: &GtkCaptionSink,
    ready: Arc<AtomicBool>,
) -> Result<Box<dyn Transcriber>, Box<dyn std::error::Error + Send + Sync>> {
    match backend {
        Backend::Offline {
            model_path,
            language,
        } => {
            let mut config = WhisperConfig::new(model_path);
            config.language = language;
            let transcriber = WhisperTranscriber::new(config)?;
            ready.store(true, Ordering::Release);
            Ok(Box::new(transcriber))
        }
        Backend::Online { key, options } => {
            let status_sink = sink.clone();
            let active = match options.mode {
                ProcessingMode::Translation => "Translating…",
                _ => "Listening…",
            };
            let status = Arc::new(move |status: OnlineStatus| {
                let text = match status {
                    OnlineStatus::Connecting => "Connecting…",
                    OnlineStatus::Active => {
                        ready.store(true, Ordering::Release);
                        active
                    }
                    OnlineStatus::Reconnecting => "Reconnecting…",
                };
                notify_ui(status_sink.set_status(text));
            });
            let connector = Box::new(WebSocketConnector::default());
            let limits = SessionLimits::default();
            let session = match options.mode {
                ProcessingMode::Translation => OnlineSession::start(
                    TranslationProtocol::new(options.translation_target, options.show_original),
                    key,
                    connector,
                    status,
                    limits,
                )?,
                _ => OnlineSession::start(
                    TranscriptionProtocol::new(options.spoken_language.language()),
                    key,
                    connector,
                    status,
                    limits,
                )?,
            };
            Ok(Box::new(session))
        }
    }
}

fn run_pipeline(
    source: AudioSourceDescriptor,
    backend: Backend,
    sink: GtkCaptionSink,
    startup: &StartupGate,
    backend_ready: Arc<AtomicBool>,
) -> Result<RunSummary, Box<dyn std::error::Error + Send + Sync>> {
    let offline = matches!(backend, Backend::Offline { .. });
    let transcriber = open_backend(backend, &sink, backend_ready)?;
    let Some(audio) = start_audio_after_stt(startup, || {
        PipeWireCapture::start(source, PipeWireCaptureConfig::default())
    })?
    else {
        return Ok(RunSummary::default());
    };
    // Online sessions report their own progress as the connection opens.
    if offline && !startup.is_cancelled() {
        sink.set_status("Listening…")?;
    }
    let pipeline = CaptionPipeline::new(audio, transcriber, sink, RuntimeConfig::default())?;
    Ok(pipeline.run(startup.cancellation_flag())?)
}

fn start_audio_after_stt<A, E>(
    startup: &StartupGate,
    start_audio: impl FnOnce() -> Result<A, E>,
) -> Result<Option<A>, E> {
    if !startup.begin_audio_acquisition() {
        return Ok(None);
    }
    let audio = start_audio()?;
    startup.complete_audio_acquisition();
    Ok(Some(audio))
}

/// The finished session's result, and whether its backend ever became ready.
fn take_completed_session(
    state: &mut ControllerState,
) -> Option<(Result<RunSummary, SessionFailure>, bool)> {
    let result = match state {
        ControllerState::Active(session) => match session.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(SessionFailure::new(
                "caption pipeline result channel disconnected",
            )),
        },
        ControllerState::Idle
        | ControllerState::ShutdownRequested
        | ControllerState::Terminated => return None,
    };
    let previous = std::mem::replace(state, ControllerState::Idle);
    let ControllerState::Active(completed) = previous else {
        unreachable!("only an active session can produce a completion");
    };
    let backend_ready = completed.backend_ready.load(Ordering::Acquire);
    if completed.worker.join().is_err() {
        return Some((
            Err(SessionFailure::new("caption pipeline worker panicked")),
            backend_ready,
        ));
    }
    Some((result, backend_ready))
}

pub(crate) fn notify_ui(result: Result<(), CaptionSinkError>) {
    if let Err(error) = result {
        // While the GTK receiver is live, the state bridge cannot reject an
        // update for ordinary UI lag. A failure therefore means it has ended.
        warn!(%error, "GTK caption UI is no longer available");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
            mpsc::sync_channel,
        },
        thread,
    };

    use super::{
        ControllerState, CredentialActions, PipelineSession, SessionFailure, StartupGate,
        StartupPhase, credential_view, key_to_test, request_controller_shutdown,
        start_audio_after_stt, take_completed_session,
    };
    use lcrt_core::{PipelineError, RunSummary, TranscriptionError};
    use lcrt_openai::credentials::{ApiKey, CredentialStatus};
    use lcrt_ui_gtk::{CredentialTone, EnteredApiKey};

    #[test]
    fn only_a_rejected_credential_sends_the_user_to_settings() {
        let rejected = TranscriptionError::credential_rejected("Your OpenAI API key was rejected.");
        let direct = SessionFailure::from_pipeline(&rejected);
        assert!(direct.needs_settings);
        assert_eq!(direct.message, "Your OpenAI API key was rejected.");
        let wrapped = SessionFailure::from_pipeline(&PipelineError::Transcription(rejected));
        assert!(wrapped.needs_settings);
        assert_eq!(wrapped.message, "Your OpenAI API key was rejected.");
        // Mentioning the key is not the same as the key being rejected.
        let other = TranscriptionError::new("API key accepted but the service is down");
        assert!(!SessionFailure::from_pipeline(&other).needs_settings);
    }

    #[test]
    fn only_the_latest_credential_action_reports_its_result() {
        let actions = CredentialActions::default();
        let slow_test = actions.begin();
        let newer_test = actions.begin();
        assert!(!actions.is_latest(slow_test));
        assert!(actions.is_latest(newer_test));
        actions.begin(); // Save or Clear
        assert!(!actions.is_latest(newer_test));
    }

    #[test]
    fn connection_test_prefers_the_entered_key_without_consulting_storage() {
        let entered = EnteredApiKey::new(" sk-entered ".to_owned());
        let key = key_to_test(Some(&entered), || panic!("storage must not be read"));
        assert_eq!(key, Ok(ApiKey::parse("sk-entered").unwrap()));
    }

    #[test]
    fn connection_test_falls_back_to_the_key_in_use_and_explains_problems() {
        let saved = ApiKey::parse("sk-saved").unwrap();
        assert_eq!(key_to_test(None, || Some(saved.clone())), Ok(saved));
        assert_eq!(
            key_to_test(None, || None),
            Err("Enter an API key to test.".to_owned())
        );
        let malformed = EnteredApiKey::new("sk bad".to_owned());
        assert!(key_to_test(Some(&malformed), || None).is_err());
    }

    #[test]
    fn shutdown_without_a_session_terminates_the_controller() {
        let mut state = ControllerState::Idle;
        request_controller_shutdown(&mut state);
        assert!(matches!(state, ControllerState::Terminated));
    }

    #[test]
    fn shutdown_cancels_an_active_session_without_waiting_for_its_worker() {
        let startup = Arc::new(StartupGate::new());
        let worker_startup = Arc::clone(&startup);
        let (_result_sender, result) = sync_channel(1);
        let worker = thread::spawn(move || {
            while !worker_startup.is_cancelled() {
                thread::yield_now();
            }
        });
        let mut state = ControllerState::Active(PipelineSession {
            startup: Arc::clone(&startup),
            result,
            worker,
            backend_ready: Arc::new(AtomicBool::new(false)),
        });

        request_controller_shutdown(&mut state);

        assert!(startup.is_cancelled());
        assert!(matches!(state, ControllerState::Terminated));
    }

    #[test]
    fn a_session_that_never_connected_is_reported_as_not_ready() {
        // An online session stopped before connecting completes normally, but
        // a diagnostic run must not count it as having exercised the service.
        for ready in [false, true] {
            let (result_sender, result) = sync_channel(1);
            result_sender.send(Ok(RunSummary::default())).unwrap();
            let mut state = ControllerState::Active(PipelineSession {
                startup: Arc::new(StartupGate::new()),
                result,
                worker: thread::spawn(|| {}),
                backend_ready: Arc::new(AtomicBool::new(ready)),
            });
            let (completed, backend_ready) = take_completed_session(&mut state).unwrap();
            assert!(completed.is_ok());
            assert_eq!(backend_ready, ready);
        }
    }

    #[test]
    fn repeated_shutdown_is_idempotent() {
        let mut state = ControllerState::Idle;
        request_controller_shutdown(&mut state);
        request_controller_shutdown(&mut state);
        assert!(matches!(state, ControllerState::Terminated));
    }

    #[test]
    fn cancellation_winning_at_the_audio_boundary_does_not_start_audio() {
        let startup = Arc::new(StartupGate::new());
        let at_boundary = Arc::new(Barrier::new(2));
        let release_worker = Arc::new(Barrier::new(2));
        let started = Arc::new(AtomicBool::new(false));
        let worker_startup = Arc::clone(&startup);
        let worker_at_boundary = Arc::clone(&at_boundary);
        let worker_release = Arc::clone(&release_worker);
        let worker_started = Arc::clone(&started);
        let worker = thread::spawn(move || {
            worker_at_boundary.wait();
            worker_release.wait();
            start_audio_after_stt(&worker_startup, || {
                worker_started.store(true, Ordering::Release);
                Ok::<(), ()>(())
            })
            .unwrap()
        });

        at_boundary.wait();
        startup.cancel();
        release_worker.wait();

        assert!(worker.join().unwrap().is_none());
        assert!(!started.load(Ordering::Acquire));
        assert_eq!(startup.phase(), StartupPhase::Cancelled);
    }

    #[test]
    fn audio_acquisition_winning_before_stop_remains_a_valid_cancelled_session() {
        let startup = Arc::new(StartupGate::new());
        let starter_entered = Arc::new(Barrier::new(2));
        let release_starter = Arc::new(Barrier::new(2));
        let started = Arc::new(AtomicBool::new(false));
        let worker_startup = Arc::clone(&startup);
        let worker_starter_entered = Arc::clone(&starter_entered);
        let worker_release_starter = Arc::clone(&release_starter);
        let worker_started = Arc::clone(&started);
        let worker = thread::spawn(move || {
            start_audio_after_stt(&worker_startup, || {
                worker_starter_entered.wait();
                worker_release_starter.wait();
                worker_started.store(true, Ordering::Release);
                Ok::<(), ()>(())
            })
            .unwrap()
        });

        starter_entered.wait();
        startup.cancel();
        release_starter.wait();

        assert_eq!(worker.join().unwrap(), Some(()));
        assert!(started.load(Ordering::Acquire));
        assert!(startup.is_cancelled());
        assert_eq!(startup.phase(), StartupPhase::Active);
    }

    #[test]
    fn credential_statuses_map_to_tones_without_revealing_keys() {
        assert_eq!(
            credential_view(CredentialStatus::SavedSecurely).tone,
            CredentialTone::Good
        );
        assert_eq!(
            credential_view(CredentialStatus::SessionOnly).tone,
            CredentialTone::Problem
        );
        assert_eq!(
            credential_view(CredentialStatus::UsingEnvironment).status,
            "Using environment credential"
        );
    }
}
