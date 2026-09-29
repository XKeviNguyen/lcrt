use std::{
    env,
    ffi::OsString,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use lcrt_audio_pipewire::{PipeWireCapture, PipeWireCaptureConfig, enumerate_audio_sources};
use lcrt_core::{
    AudioSourceDescriptor, CaptionPipeline, CaptionSinkError, RunSummary, RuntimeConfig,
};
use lcrt_stt_whisper::{WhisperConfig, WhisperTranscriber};
use lcrt_ui_gtk::{
    CaptionUiAction, CaptionUiMode, CaptionUiOptions, GtkCaptionSink, run_caption_ui,
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const SOURCE_ENUMERATION_TIMEOUT: Duration = Duration::from_secs(3);
const CONTROLLER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const UI_ACTION_CAPACITY: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppConfig {
    model_path: Option<PathBuf>,
    language: Option<String>,
    list_sources: bool,
    smoke: Option<SmokeConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SmokeConfig {
    source_id: String,
    duration: Duration,
}

enum ParsedCommand {
    Run(AppConfig),
    Help,
}

struct PipelineSession {
    startup: Arc<StartupGate>,
    result: Receiver<Result<RunSummary, String>>,
    worker: JoinHandle<()>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControllerOutcome {
    Completed,
    SmokeSucceeded,
    SmokeFailed,
}

fn main() -> ExitCode {
    configure_logging();
    let command = match parse_arguments(env::args_os().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("error: {message}\n\n{}", usage());
            return ExitCode::FAILURE;
        }
    };
    let ParsedCommand::Run(config) = command else {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    };

    let sources = match enumerate_audio_sources(SOURCE_ENUMERATION_TIMEOUT) {
        Ok(sources) => sources,
        Err(error) if config.list_sources => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            return run_ui_with_startup_error(
                config,
                format!("Audio source discovery failed: {error}"),
            );
        }
    };
    if config.list_sources {
        for source in sources {
            println!("{:?}\t{}\t{}", source.kind(), source.id(), source.name());
        }
        return ExitCode::SUCCESS;
    }
    run_application(config, sources, None)
}

fn configure_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "lcrt=info,lcrt_core=info,lcrt_audio_pipewire=info,lcrt_stt_whisper=info,whisper_rs=warn",
        )
    });
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn run_ui_with_startup_error(config: AppConfig, message: String) -> ExitCode {
    run_application(config, Vec::new(), Some(message))
}

fn run_application(
    config: AppConfig,
    sources: Vec<AudioSourceDescriptor>,
    startup_error: Option<String>,
) -> ExitCode {
    let (sink, events) = GtkCaptionSink::bridge();
    let mut initial_errors = Vec::new();
    if let Some(error) = startup_error {
        initial_errors.push(error);
    }
    if config.model_path.is_none() {
        initial_errors.push(
            "No local Whisper model is configured. Pass --model PATH or set LCRT_MODEL_PATH."
                .to_owned(),
        );
    }
    if !initial_errors.is_empty() {
        notify_ui(sink.show_error(initial_errors.join("\n")));
    }

    let (actions, action_receiver) = sync_channel(UI_ACTION_CAPACITY);
    let controller_sources = sources.clone();
    let controller_config = config.clone();
    let controller = thread::Builder::new()
        .name("lcrt-application-controller".to_owned())
        .spawn(move || {
            run_controller(controller_config, controller_sources, sink, action_receiver)
        });
    let controller = match controller {
        Ok(controller) => controller,
        Err(error) => {
            error!(%error, "could not start the caption controller");
            return ExitCode::FAILURE;
        }
    };

    if let Some(smoke) = config.smoke.clone() {
        spawn_smoke_actions(actions.clone(), smoke);
    }
    let options = CaptionUiOptions {
        mode: if config.smoke.is_some() {
            CaptionUiMode::Diagnostic
        } else {
            CaptionUiMode::Normal
        },
        sources,
        ..CaptionUiOptions::default()
    };
    let status = run_caption_ui(events, actions, options);
    let controller_outcome = controller.join().ok();
    application_exit_status(status == gtk::glib::ExitCode::SUCCESS, controller_outcome)
}

fn run_controller(
    config: AppConfig,
    sources: Vec<AudioSourceDescriptor>,
    sink: GtkCaptionSink,
    actions: Receiver<CaptionUiAction>,
) -> ControllerOutcome {
    let mut state = ControllerState::Idle;
    loop {
        if let Some(completed) = take_completed_session(&mut state) {
            let smoke_succeeded = completed.is_ok();
            publish_completion(&sink, completed);
            if config.smoke.is_some() {
                notify_ui(sink.quit());
                return if smoke_succeeded {
                    ControllerOutcome::SmokeSucceeded
                } else {
                    ControllerOutcome::SmokeFailed
                };
            }
        }

        match actions.recv_timeout(CONTROLLER_POLL_INTERVAL) {
            Ok(CaptionUiAction::Start { source_id }) => {
                if !matches!(state, ControllerState::Idle) {
                    notify_ui(sink.show_error("Captioning is already running."));
                    continue;
                }
                let Some(source) = sources
                    .iter()
                    .find(|source| source.id() == source_id)
                    .cloned()
                else {
                    notify_ui(
                        sink.show_error("The selected PipeWire source is no longer available."),
                    );
                    if config.smoke.is_some() {
                        notify_ui(sink.quit());
                        return ControllerOutcome::SmokeFailed;
                    }
                    continue;
                };
                let Some(model_path) = config.model_path.clone() else {
                    notify_ui(sink.show_error(
                        "No local Whisper model is configured. Pass --model PATH or set LCRT_MODEL_PATH.",
                    ));
                    if config.smoke.is_some() {
                        notify_ui(sink.quit());
                        return ControllerOutcome::SmokeFailed;
                    }
                    continue;
                };
                notify_ui(sink.clear_error());
                notify_ui(sink.set_running(true));
                notify_ui(sink.set_status("Loading model…"));
                match start_pipeline(source, model_path, config.language.clone(), sink.clone()) {
                    Ok(started) => state = ControllerState::Active(started),
                    Err(message) => {
                        notify_ui(sink.set_running(false));
                        notify_ui(sink.set_status("Error"));
                        notify_ui(sink.show_error(message));
                        if config.smoke.is_some() {
                            notify_ui(sink.quit());
                            return ControllerOutcome::SmokeFailed;
                        }
                    }
                }
            }
            Ok(CaptionUiAction::Stop) => {
                if let ControllerState::Active(session) = &state {
                    session.startup.cancel();
                    notify_ui(sink.set_status("Stopping…"));
                }
            }
            Ok(CaptionUiAction::Shutdown) => {
                request_controller_shutdown(&mut state);
                return if config.smoke.is_some() {
                    ControllerOutcome::SmokeFailed
                } else {
                    ControllerOutcome::Completed
                };
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                request_controller_shutdown(&mut state);
                return if config.smoke.is_some() {
                    ControllerOutcome::SmokeFailed
                } else {
                    ControllerOutcome::Completed
                };
            }
        }
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
    model_path: PathBuf,
    language: Option<String>,
    sink: GtkCaptionSink,
) -> Result<PipelineSession, String> {
    let startup = Arc::new(StartupGate::new());
    let worker_startup = Arc::clone(&startup);
    let (result_sender, result) = sync_channel(1);
    let worker = thread::Builder::new()
        .name("lcrt-caption-pipeline".to_owned())
        .spawn(move || {
            let result = run_pipeline(source, model_path, language, sink, &worker_startup)
                .map_err(|error| error.to_string());
            let _ = result_sender.send(result);
        })
        .map_err(|error| format!("could not start the caption pipeline worker: {error}"))?;
    Ok(PipelineSession {
        startup,
        result,
        worker,
    })
}

fn run_pipeline(
    source: AudioSourceDescriptor,
    model_path: PathBuf,
    language: Option<String>,
    sink: GtkCaptionSink,
    startup: &StartupGate,
) -> Result<RunSummary, Box<dyn std::error::Error + Send + Sync>> {
    let mut whisper_config = WhisperConfig::new(model_path);
    whisper_config.language = language;
    let transcriber = WhisperTranscriber::new(whisper_config)?;
    let Some(audio) = start_audio_after_stt(startup, || {
        PipeWireCapture::start(source, PipeWireCaptureConfig::default())
    })?
    else {
        return Ok(RunSummary::default());
    };
    if !startup.is_cancelled() {
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

fn take_completed_session(state: &mut ControllerState) -> Option<Result<RunSummary, String>> {
    let result = match state {
        ControllerState::Active(session) => match session.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                Err("caption pipeline result channel disconnected".to_owned())
            }
        },
        ControllerState::Idle
        | ControllerState::ShutdownRequested
        | ControllerState::Terminated => return None,
    };
    let previous = std::mem::replace(state, ControllerState::Idle);
    let ControllerState::Active(completed) = previous else {
        unreachable!("only an active session can produce a completion");
    };
    if completed.worker.join().is_err() {
        return Some(Err("caption pipeline worker panicked".to_owned()));
    }
    Some(result)
}

fn application_exit_status(
    gtk_succeeded: bool,
    controller_outcome: Option<ControllerOutcome>,
) -> ExitCode {
    if gtk_succeeded
        && matches!(
            controller_outcome,
            Some(ControllerOutcome::Completed | ControllerOutcome::SmokeSucceeded)
        )
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn publish_completion(sink: &GtkCaptionSink, result: Result<RunSummary, String>) {
    notify_ui(sink.set_running(false));
    match result {
        Ok(summary) => {
            info!(
                audio_chunks = summary.audio_chunks,
                caption_updates = summary.caption_updates,
                "caption session completed"
            );
            notify_ui(sink.set_status(format!(
                "Stopped · {} chunks · {} captions",
                summary.audio_chunks, summary.caption_updates
            )));
        }
        Err(message) => {
            error!(%message, "caption session failed");
            notify_ui(sink.set_status("Error"));
            notify_ui(sink.show_error(message));
        }
    }
}

fn notify_ui(result: Result<(), CaptionSinkError>) {
    if let Err(error) = result {
        // While the GTK receiver is live, the state bridge cannot reject an
        // update for ordinary UI lag. A failure therefore means it has ended.
        warn!(%error, "GTK caption UI is no longer available");
    }
}

fn spawn_smoke_actions(actions: SyncSender<CaptionUiAction>, smoke: SmokeConfig) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        if actions
            .send(CaptionUiAction::Start {
                source_id: smoke.source_id,
            })
            .is_err()
        {
            return;
        }
        thread::sleep(smoke.duration);
        let _ = actions.send(CaptionUiAction::Stop);
    });
}

fn parse_arguments(arguments: impl IntoIterator<Item = OsString>) -> Result<ParsedCommand, String> {
    let mut model_path = env::var_os("LCRT_MODEL_PATH").map(PathBuf::from);
    let mut language = None;
    let mut list_sources = false;
    let mut smoke_source = None;
    let mut smoke_duration = Duration::from_secs(10);
    let mut smoke_duration_was_set = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_owned())?;
        match argument.as_str() {
            "--help" | "-h" => return Ok(ParsedCommand::Help),
            "--model" => {
                model_path = Some(PathBuf::from(next_value(&mut arguments, "--model")?));
            }
            "--language" => {
                language = Some(os_to_string(next_value(&mut arguments, "--language")?)?);
            }
            "--list-sources" => list_sources = true,
            "--smoke-source" => {
                smoke_source = Some(os_to_string(next_value(&mut arguments, "--smoke-source")?)?);
            }
            "--smoke-seconds" => {
                smoke_duration_was_set = true;
                let value = os_to_string(next_value(&mut arguments, "--smoke-seconds")?)?;
                let seconds = value
                    .parse::<u64>()
                    .map_err(|_| "--smoke-seconds must be an integer from 1 to 3600".to_owned())?;
                if !(1..=3_600).contains(&seconds) {
                    return Err("--smoke-seconds must be an integer from 1 to 3600".to_owned());
                }
                smoke_duration = Duration::from_secs(seconds);
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if smoke_duration_was_set && smoke_source.is_none() {
        return Err("--smoke-seconds requires --smoke-source".to_owned());
    }
    Ok(ParsedCommand::Run(AppConfig {
        model_path,
        language,
        list_sources,
        smoke: smoke_source.map(|source_id| SmokeConfig {
            source_id,
            duration: smoke_duration,
        }),
    }))
}

fn next_value(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<OsString, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn os_to_string(value: OsString) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| "arguments must be valid UTF-8".to_owned())
}

fn usage() -> &'static str {
    concat!(
        "LCRT live captions\n\n",
        "Usage:\n",
        "  lcrt [--model PATH] [--language CODE]\n",
        "  lcrt --list-sources\n",
        "  lcrt --model PATH --smoke-source ID [--smoke-seconds 1..3600]\n\n",
        "Configuration:\n",
        "  LCRT_MODEL_PATH may be used instead of --model.\n",
        "  RUST_LOG controls structured diagnostic logging."
    )
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
            mpsc::sync_channel,
        },
        thread,
        time::Duration,
    };

    use super::{
        AppConfig, ControllerOutcome, ControllerState, ParsedCommand, PipelineSession, SmokeConfig,
        StartupGate, StartupPhase, application_exit_status, parse_arguments,
        request_controller_shutdown, start_audio_after_stt,
    };

    #[test]
    fn parses_model_language_and_bounded_smoke_configuration() {
        let command = parse_arguments([
            OsString::from("--model"),
            OsString::from("model.bin"),
            OsString::from("--language"),
            OsString::from("en"),
            OsString::from("--smoke-source"),
            OsString::from("source-id"),
            OsString::from("--smoke-seconds"),
            OsString::from("12"),
        ])
        .unwrap();

        assert!(matches!(
            command,
            ParsedCommand::Run(AppConfig {
                model_path: Some(path),
                language: Some(language),
                smoke: Some(SmokeConfig { source_id, duration }),
                list_sources: false,
            }) if path.as_os_str() == "model.bin"
                && language == "en"
                && source_id == "source-id"
                && duration == Duration::from_secs(12)
        ));
    }

    #[test]
    fn rejects_unbounded_or_unknown_arguments() {
        assert!(parse_arguments([OsString::from("--smoke-seconds"), OsString::from("0")]).is_err());
        assert!(parse_arguments([OsString::from("--smoke-seconds"), OsString::from("5")]).is_err());
        assert!(parse_arguments([OsString::from("--unknown")]).is_err());
    }

    #[test]
    fn smoke_duration_allows_a_bounded_integrated_soak() {
        let smoke_seconds = |seconds: &str| {
            parse_arguments([
                OsString::from("--smoke-source"),
                OsString::from("source-id"),
                OsString::from("--smoke-seconds"),
                OsString::from(seconds),
            ])
        };

        assert!(smoke_seconds("3600").is_ok());
        assert!(smoke_seconds("3601").is_err());
    }

    #[test]
    fn successful_smoke_outcome_returns_success() {
        assert_eq!(
            application_exit_status(true, Some(ControllerOutcome::SmokeSucceeded)),
            std::process::ExitCode::SUCCESS
        );
    }

    #[test]
    fn failed_smoke_outcome_returns_failure() {
        assert_eq!(
            application_exit_status(true, Some(ControllerOutcome::SmokeFailed)),
            std::process::ExitCode::FAILURE
        );
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
        });

        request_controller_shutdown(&mut state);

        assert!(startup.is_cancelled());
        assert!(matches!(state, ControllerState::Terminated));
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
}
