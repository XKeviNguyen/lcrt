mod controller;
mod models;
mod offline_translation;
mod settings;

use std::{
    env,
    ffi::OsString,
    path::PathBuf,
    process::ExitCode,
    sync::mpsc::{SyncSender, sync_channel},
    thread,
    time::Duration,
};

use lcrt_audio_pipewire::enumerate_audio_sources;
use lcrt_core::{
    AudioSourceDescriptor, Language, LanguageSelection, ProcessingMode, SessionOptions,
    TranslationTargets,
};
use lcrt_openai::credentials::API_KEY_ENVIRONMENT_VARIABLE;
use lcrt_ui_gtk::{
    CaptionUiAction, CaptionUiMode, CaptionUiOptions, GtkCaptionSink, run_caption_ui,
};
use tracing::error;
use tracing_subscriber::EnvFilter;

use crate::{
    controller::{Controller, ControllerOutcome, RunOverrides, notify_ui},
    models::built_in_model,
    settings::SettingsStore,
};

const SOURCE_ENUMERATION_TIMEOUT: Duration = Duration::from_secs(3);
const UI_ACTION_CAPACITY: usize = 16;
const MAX_SMOKE_SECONDS: u64 = 3_600;

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppConfig {
    model_path: Option<PathBuf>,
    language: Option<Language>,
    list_sources: bool,
    smoke: Option<SmokeConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SmokeConfig {
    source_id: String,
    duration: Duration,
    mode: ProcessingMode,
    target: Language,
}

enum ParsedCommand {
    Run(AppConfig),
    Help,
    Version,
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
    let config = match command {
        ParsedCommand::Run(config) => config,
        ParsedCommand::Help => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        ParsedCommand::Version => {
            println!("lcrt {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
    };

    let (sources, startup_error) = match enumerate_audio_sources(SOURCE_ENUMERATION_TIMEOUT) {
        Ok(sources) => (sources, None),
        Err(error) if config.list_sources => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => (
            Vec::new(),
            Some(format!("Audio source discovery failed: {error}")),
        ),
    };
    if config.list_sources {
        for source in sources {
            println!("{:?}\t{}\t{}", source.kind(), source.id(), source.name());
        }
        return ExitCode::SUCCESS;
    }
    run_application(config, sources, startup_error)
}

fn configure_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "lcrt=info,lcrt_core=info,lcrt_audio_pipewire=info,lcrt_stt_whisper=info,\
             lcrt_openai=info,whisper_rs=warn",
        )
    });
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn run_application(
    config: AppConfig,
    sources: Vec<AudioSourceDescriptor>,
    startup_error: Option<String>,
) -> ExitCode {
    let (sink, events) = GtkCaptionSink::bridge();
    if let Some(message) = startup_error {
        notify_ui(sink.show_error(message));
    }
    let store = SettingsStore::from_environment();
    let preferences = store.as_ref().map(SettingsStore::load).unwrap_or_default();
    let overrides = RunOverrides {
        model_path: config.model_path.clone(),
        built_in_model: env::current_exe()
            .ok()
            .and_then(|executable| built_in_model(&executable)),
        smoke: config.smoke.is_some(),
    };
    let environment_key = env::var(API_KEY_ENVIRONMENT_VARIABLE).ok();
    let (actions, action_receiver) = sync_channel(UI_ACTION_CAPACITY);
    let controller = Controller::new(
        overrides,
        sources.clone(),
        sink,
        preferences.clone(),
        store,
        environment_key,
    );
    let controller = match thread::Builder::new()
        .name("lcrt-application-controller".to_owned())
        .spawn(move || controller.run(action_receiver))
    {
        Ok(controller) => controller,
        Err(error) => {
            error!(%error, "could not start the caption controller");
            return ExitCode::FAILURE;
        }
    };

    if let Some(smoke) = config.smoke.clone() {
        spawn_smoke_actions(actions.clone(), smoke, config.language);
    }
    let options = CaptionUiOptions {
        mode: if config.smoke.is_some() {
            CaptionUiMode::Diagnostic
        } else {
            CaptionUiMode::Normal
        },
        sources,
        preferences,
        model_overridden: config.model_path.is_some(),
        ..CaptionUiOptions::default()
    };
    let status = run_caption_ui(events, actions, options);
    let controller_outcome = controller.join().ok();
    application_exit_status(status == gtk::glib::ExitCode::SUCCESS, controller_outcome)
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

fn spawn_smoke_actions(
    actions: SyncSender<CaptionUiAction>,
    smoke: SmokeConfig,
    language: Option<Language>,
) {
    let spoken_language = language.map_or(LanguageSelection::Auto, LanguageSelection::Language);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        let options = SessionOptions {
            mode: smoke.mode,
            source_id: smoke.source_id,
            spoken_language,
            translation_targets: TranslationTargets::resolve(
                Some(smoke.target),
                None,
                spoken_language.language(),
            ),
        };
        if actions.send(CaptionUiAction::Start(options)).is_err() {
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
    let mut smoke_mode = ProcessingMode::OfflineCaptions;
    let mut smoke_target = Language::English;
    let mut smoke_option_set = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_owned())?;
        match argument.as_str() {
            "--help" | "-h" => return Ok(ParsedCommand::Help),
            "--version" => return Ok(ParsedCommand::Version),
            "--model" => {
                model_path = Some(PathBuf::from(next_value(&mut arguments, "--model")?));
            }
            "--language" => {
                // Only a diagnostic run uses it; the window has its own.
                smoke_option_set = true;
                // A mistyped code must not quietly become Auto: a diagnostic
                // would then pass for a language it never used.
                let code = os_to_string(next_value(&mut arguments, "--language")?)?;
                language = Some(
                    Language::from_code(&code)
                        .ok_or_else(|| format!("unsupported --language: {code}"))?,
                );
            }
            "--list-sources" => list_sources = true,
            "--smoke-source" => {
                smoke_source = Some(os_to_string(next_value(&mut arguments, "--smoke-source")?)?);
            }
            "--smoke-seconds" => {
                smoke_option_set = true;
                let value = os_to_string(next_value(&mut arguments, "--smoke-seconds")?)?;
                let seconds = value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| (1..=MAX_SMOKE_SECONDS).contains(seconds));
                let Some(seconds) = seconds else {
                    return Err(format!(
                        "--smoke-seconds must be an integer from 1 to {MAX_SMOKE_SECONDS}"
                    ));
                };
                smoke_duration = Duration::from_secs(seconds);
            }
            "--smoke-mode" => {
                smoke_option_set = true;
                smoke_mode = match os_to_string(next_value(&mut arguments, "--smoke-mode")?)?
                    .as_str()
                {
                    "offline" => ProcessingMode::OfflineCaptions,
                    "offline-translation" => ProcessingMode::OfflineTranslation,
                    "online" => ProcessingMode::OnlineCaptions,
                    "translation" => ProcessingMode::Translation,
                    _ => {
                        return Err(
                                "--smoke-mode must be offline, offline-translation, online, or translation".to_owned()
                            );
                    }
                };
            }
            "--smoke-target" => {
                smoke_option_set = true;
                let code = os_to_string(next_value(&mut arguments, "--smoke-target")?)?;
                smoke_target = Language::from_code(&code)
                    .ok_or_else(|| format!("unsupported --smoke-target language: {code}"))?;
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if smoke_option_set && smoke_source.is_none() {
        return Err("smoke options require --smoke-source".to_owned());
    }
    // The diagnostic shows the source lane, and a target can't repeat it.
    if smoke_mode.translates() && language == Some(smoke_target) {
        return Err("--smoke-target must differ from --language".to_owned());
    }
    Ok(ParsedCommand::Run(AppConfig {
        model_path,
        language,
        list_sources,
        smoke: smoke_source.map(|source_id| SmokeConfig {
            source_id,
            duration: smoke_duration,
            mode: smoke_mode,
            target: smoke_target,
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
        "LILOPOP live captions\n\n",
        "Usage:\n",
        "  lcrt\n",
        "  lcrt --list-sources\n",
        "  lcrt --version\n",
        "  lcrt --smoke-source ID [--smoke-seconds 1..3600]\n",
        "       [--smoke-mode offline|offline-translation|online|translation] [--smoke-target CODE]\n\n",
        "Everyday settings, including languages and the OpenAI API key, are in\n",
        "Settings inside the app. Offline Captions use the built-in model.\n\n",
        "Developer options:\n",
        "  --model PATH or LCRT_MODEL_PATH   use this Whisper model for this run\n",
        "  --language CODE                   spoken language of a diagnostic run\n",
        "  OPENAI_API_KEY                    fallback API key; never shown in the app\n",
        "  RUST_LOG                          structured diagnostic logging"
    )
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, time::Duration};

    use lcrt_core::{Language, ProcessingMode};

    use super::{
        AppConfig, ControllerOutcome, ParsedCommand, SmokeConfig, application_exit_status,
        parse_arguments,
    };

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_model_language_and_bounded_smoke_configuration() {
        let command = parse_arguments(arguments(&[
            "--model",
            "model.bin",
            "--language",
            "en",
            "--smoke-source",
            "source-id",
            "--smoke-seconds",
            "12",
            "--smoke-mode",
            "translation",
            "--smoke-target",
            "ja",
        ]))
        .unwrap();

        assert!(matches!(
            command,
            ParsedCommand::Run(AppConfig {
                model_path: Some(path),
                language: Some(language),
                smoke: Some(SmokeConfig { source_id, duration, mode, target }),
                list_sources: false,
            }) if path.as_os_str() == "model.bin"
                && language == Language::English
                && source_id == "source-id"
                && duration == Duration::from_secs(12)
                && mode == ProcessingMode::Translation
                && target == Language::Japanese
        ));
    }

    #[test]
    fn rejects_unbounded_unknown_or_orphaned_arguments() {
        assert!(parse_arguments(arguments(&["--smoke-seconds", "0"])).is_err());
        assert!(parse_arguments(arguments(&["--smoke-seconds", "5"])).is_err());
        assert!(parse_arguments(arguments(&["--smoke-mode", "online"])).is_err());
        assert!(parse_arguments(arguments(&["--smoke-source", "s", "--smoke-mode", "x"])).is_err());
        assert!(
            parse_arguments(arguments(&["--smoke-source", "s", "--smoke-target", "xx"])).is_err()
        );
        assert!(parse_arguments(arguments(&["--unknown"])).is_err());
        assert!(parse_arguments(arguments(&["--language", "jp"])).is_err());
        // Outside a diagnostic run it would be ignored, so it is refused.
        assert!(parse_arguments(arguments(&["--language", "ja"])).is_err());
        // A translation diagnostic can't translate a language into itself.
        let translation = |language: &str, target: &str| {
            parse_arguments(arguments(&[
                "--smoke-source",
                "s",
                "--smoke-mode",
                "translation",
                "--language",
                language,
                "--smoke-target",
                target,
            ]))
        };
        assert!(translation("ja", "ja").is_err());
        assert!(translation("ja", "en").is_ok());
    }

    #[test]
    fn smoke_duration_allows_a_bounded_integrated_soak() {
        let smoke = |seconds: &str| {
            parse_arguments(arguments(&[
                "--smoke-source",
                "id",
                "--smoke-seconds",
                seconds,
            ]))
        };
        assert!(smoke("3600").is_ok());
        assert!(smoke("3601").is_err());
    }

    #[test]
    fn version_and_help_are_recognized() {
        assert!(matches!(
            parse_arguments(arguments(&["--version"])),
            Ok(ParsedCommand::Version)
        ));
        assert!(matches!(
            parse_arguments(arguments(&["-h"])),
            Ok(ParsedCommand::Help)
        ));
    }

    #[test]
    fn exit_status_reflects_the_controller_outcome() {
        assert_eq!(
            application_exit_status(true, Some(ControllerOutcome::SmokeSucceeded)),
            std::process::ExitCode::SUCCESS
        );
        assert_eq!(
            application_exit_status(true, Some(ControllerOutcome::SmokeFailed)),
            std::process::ExitCode::FAILURE
        );
    }
}
