use std::{env, process::ExitCode, sync::mpsc::sync_channel, thread, time::Duration};

use lcrt_core::{
    AudioSourceDescriptor, AudioSourceKind, CaptionSink, CaptionState, TranscriptUpdate,
};
use lcrt_ui_gtk::{
    CaptionUiAction, CaptionUiMode, CaptionUiOptions, GtkCaptionSink, run_caption_ui,
};

fn main() -> ExitCode {
    let smoke_test = env::args()
        .skip(1)
        .any(|argument| argument == "--smoke-test");
    let (sink, events) = GtkCaptionSink::bridge();
    let (actions, action_receiver) = sync_channel(4);
    let controller = if smoke_test {
        spawn_smoke_controller(sink)
    } else {
        spawn_demo_controller(sink, action_receiver)
    };

    let options = CaptionUiOptions {
        mode: if smoke_test {
            CaptionUiMode::Diagnostic
        } else {
            CaptionUiMode::Normal
        },
        sources: vec![AudioSourceDescriptor::new(
            "demo",
            "Deterministic demo",
            AudioSourceKind::Microphone,
        )],
        ..CaptionUiOptions::default()
    };
    let status = run_caption_ui(events, actions, options);
    let controller_outcome = controller.join().ok();
    if application_succeeded(status == gtk::glib::ExitCode::SUCCESS, controller_outcome) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn spawn_smoke_controller(sink: GtkCaptionSink) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        publish_demo(&sink, Duration::from_millis(250), None)?;
        thread::sleep(Duration::from_millis(400));
        sink.quit().map_err(|error| error.to_string())
    })
}

fn spawn_demo_controller(
    sink: GtkCaptionSink,
    actions: std::sync::mpsc::Receiver<CaptionUiAction>,
) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || {
        while let Ok(action) = actions.recv() {
            match action {
                CaptionUiAction::Start(_) => {
                    publish_demo(&sink, Duration::from_millis(450), Some(&actions))?;
                }
                CaptionUiAction::Stop => {
                    sink.set_running(false).map_err(|error| error.to_string())?;
                }
                CaptionUiAction::Shutdown => break,
                // The demo has no preferences, credentials, or vocabulary backend.
                _ => {}
            }
        }
        Ok(())
    })
}

fn publish_demo(
    sink: &GtkCaptionSink,
    interval: Duration,
    actions: Option<&std::sync::mpsc::Receiver<CaptionUiAction>>,
) -> Result<(), String> {
    sink.clear_error().map_err(|error| error.to_string())?;
    sink.set_running(true).map_err(|error| error.to_string())?;
    let mut state = CaptionState::new();
    let updates = [
        ("Native captions", false),
        ("Native captions update incrementally", false),
        ("Native captions update incrementally on Ubuntu.", true),
    ];
    for (text, is_final) in updates {
        let update = if is_final {
            TranscriptUpdate::finalized(text)
        } else {
            TranscriptUpdate::partial(text)
        };
        let update = update.map_err(|error| error.to_string())?;
        let snapshot = state.apply(update).map_err(|error| error.to_string())?;
        let mut caption_sink = sink.clone();
        caption_sink
            .publish(snapshot)
            .map_err(|error| error.to_string())?;
        let stopped = actions.is_some_and(|actions| match actions.recv_timeout(interval) {
            Ok(CaptionUiAction::Stop)
            | Ok(CaptionUiAction::Shutdown)
            | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
        });
        if stopped {
            break;
        }
        if actions.is_none() {
            thread::sleep(interval);
        }
    }
    sink.set_running(false).map_err(|error| error.to_string())
}

fn application_succeeded(
    gtk_succeeded: bool,
    controller_outcome: Option<Result<(), String>>,
) -> bool {
    gtk_succeeded && matches!(controller_outcome, Some(Ok(())))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lcrt_ui_gtk::GtkCaptionSink;

    use super::{application_succeeded, publish_demo};

    #[test]
    fn diagnostic_failure_produces_a_failed_application_outcome() {
        assert!(!application_succeeded(
            true,
            Some(Err("caption publication failed".to_owned()))
        ));
        assert!(!application_succeeded(false, Some(Ok(()))));
        assert!(!application_succeeded(true, None));
    }

    #[test]
    fn diagnostic_success_requires_gtk_and_controller_success() {
        assert!(application_succeeded(true, Some(Ok(()))));
    }

    #[test]
    fn demo_reports_bridge_teardown_as_failure() {
        let (sink, receiver) = GtkCaptionSink::bridge();
        drop(receiver);

        assert!(publish_demo(&sink, Duration::ZERO, None).is_err());
    }
}
