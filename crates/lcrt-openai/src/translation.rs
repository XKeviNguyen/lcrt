//! Realtime speech translation protocol (`gpt-realtime-translate`) as a pure
//! state machine. One session yields both the source and translated text.

use base64::Engine as _;
use lcrt_core::{CaptionStatus, Language, TranscriptUpdate};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::{
    audio::encode_pcm16,
    protocol::{EventOutcome, Protocol, ServiceError, bounded_tail},
};

/// Dedicated realtime translation endpoint, including its model.
pub const TRANSLATION_URL: &str =
    "wss://api.openai.com/v1/realtime/translations?model=gpt-realtime-translate";
/// Input transcription model that produces the source-language transcript.
const SOURCE_TRANSCRIPTION_MODEL: &str = "gpt-realtime-whisper";
/// The service consumes 200 ms frames (4,800 samples at 24 kHz).
const APPEND_SAMPLES: usize = 4_800;
/// Recent text kept per lane; the stream has no turn boundaries.
const MAX_LANE_BYTES: usize = 360;

/// Client state for one translation session.
pub struct TranslationProtocol {
    target: Language,
    show_original: bool,
    outgoing: Vec<f32>,
    source: String,
    translation: String,
    closed: bool,
}

impl TranslationProtocol {
    /// Creates the protocol for one output language. The source language is
    /// detected by the service and cannot be specified.
    pub fn new(target: Language, show_original: bool) -> Self {
        Self {
            target,
            show_original,
            outgoing: Vec::with_capacity(APPEND_SAMPLES),
            source: String::new(),
            translation: String::new(),
            closed: false,
        }
    }

    fn append_lane(lane: &mut String, delta: &str) {
        lane.push_str(delta);
        if lane.len() > MAX_LANE_BYTES {
            *lane = bounded_tail(lane, MAX_LANE_BYTES).to_owned();
        }
    }

    fn caption(&self, status: CaptionStatus) -> EventOutcome {
        TranscriptUpdate::translated(self.translation.trim(), self.source.trim(), status)
            .map_or(EventOutcome::Ignored, EventOutcome::Update)
    }

    fn append_messages(&mut self, messages: &mut Vec<String>, flush: bool) {
        while self.outgoing.len() >= APPEND_SAMPLES || (flush && !self.outgoing.is_empty()) {
            let take = self.outgoing.len().min(APPEND_SAMPLES);
            let mut pcm = Vec::with_capacity(take * 2);
            encode_pcm16(&self.outgoing[..take], &mut pcm);
            self.outgoing.drain(..take);
            let audio = base64::engine::general_purpose::STANDARD.encode(pcm);
            messages.push(
                json!({"type": "session.input_audio_buffer.append", "audio": audio}).to_string(),
            );
        }
    }
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<String>,
    error: Option<ServiceError>,
}

impl Protocol for TranslationProtocol {
    fn url(&self) -> String {
        TRANSLATION_URL.to_owned()
    }

    fn configure(&self) -> Vec<String> {
        let mut audio = json!({"output": {"language": self.target.code()}});
        if self.show_original {
            audio["input"] = json!({"transcription": {"model": SOURCE_TRANSCRIPTION_MODEL}});
        }
        vec![json!({"type": "session.update", "session": {"audio": audio}}).to_string()]
    }

    fn on_audio(&mut self, samples: &[f32]) -> Vec<String> {
        // Translation keeps the stream continuous, silence included.
        self.outgoing.extend_from_slice(samples);
        let mut messages = Vec::new();
        self.append_messages(&mut messages, false);
        messages
    }

    fn on_event(&mut self, text: &str) -> EventOutcome {
        // Translated audio is not presented; skip it without a full parse.
        if text.contains("\"session.output_audio.delta\"") {
            return EventOutcome::Ignored;
        }
        let event: Event = match serde_json::from_str(text) {
            Ok(event) => event,
            Err(error) => {
                warn!(%error, "ignoring malformed translation event");
                return EventOutcome::Ignored;
            }
        };
        match event.kind.as_str() {
            "session.input_transcript.delta" => {
                if !self.show_original {
                    return EventOutcome::Ignored;
                }
                Self::append_lane(&mut self.source, event.delta.as_deref().unwrap_or_default());
                self.caption(CaptionStatus::Partial)
            }
            "session.output_transcript.delta" => {
                Self::append_lane(
                    &mut self.translation,
                    event.delta.as_deref().unwrap_or_default(),
                );
                self.caption(CaptionStatus::Partial)
            }
            "session.closed" => {
                self.closed = true;
                self.caption(CaptionStatus::Final)
            }
            "error" => event
                .error
                .map_or(EventOutcome::Ignored, EventOutcome::ServiceError),
            other => {
                debug!(event = other, "ignoring translation event");
                EventOutcome::Ignored
            }
        }
    }

    fn finish(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        self.append_messages(&mut messages, true);
        messages.push(json!({"type": "session.close"}).to_string());
        messages
    }

    fn is_drained(&self) -> bool {
        self.closed
    }

    fn close_when_quiet(&mut self) -> EventOutcome {
        // After `session.close` the service takes seconds to send
        // `session.closed` (it is finishing speech audio LCRT ignores).
        // The transcripts are complete once they stop arriving.
        self.closed = true;
        self.caption(CaptionStatus::Final)
    }

    fn reset_connection(&mut self) {
        self.outgoing.clear();
        self.closed = false;
    }
}

#[cfg(test)]
mod tests {
    use lcrt_core::{CaptionStatus, Language};
    use serde_json::{Value, json};

    use super::TranslationProtocol;
    use crate::protocol::{EventOutcome, Protocol};

    fn event(kind: &str, delta: &str) -> String {
        json!({"type": kind, "delta": delta}).to_string()
    }

    fn lanes(outcome: EventOutcome) -> (String, Option<String>, CaptionStatus) {
        match outcome {
            EventOutcome::Update(update) => (
                update.text().to_owned(),
                update.original().map(str::to_owned),
                update.status(),
            ),
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn session_update_sets_the_target_and_requests_the_source_transcript() {
        let config: Value = serde_json::from_str(
            &TranslationProtocol::new(Language::Vietnamese, true).configure()[0],
        )
        .unwrap();
        assert_eq!(config["type"], "session.update");
        assert_eq!(config["session"]["audio"]["output"]["language"], "vi");
        assert_eq!(
            config["session"]["audio"]["input"]["transcription"]["model"],
            "gpt-realtime-whisper"
        );
    }

    #[test]
    fn original_off_requests_no_source_transcript() {
        let config: Value = serde_json::from_str(
            &TranslationProtocol::new(Language::English, false).configure()[0],
        )
        .unwrap();
        assert!(config["session"]["audio"].get("input").is_none());
    }

    #[test]
    fn audio_streams_continuously_in_200_ms_appends_including_silence() {
        let mut protocol = TranslationProtocol::new(Language::English, true);
        let messages = protocol.on_audio(&vec![0.0; 24_000]);
        assert_eq!(messages.len(), 5);
        let first: Value = serde_json::from_str(&messages[0]).unwrap();
        assert_eq!(first["type"], "session.input_audio_buffer.append");
        assert_eq!(first["audio"].as_str().unwrap().len(), 12_800); // base64 of 9,600 bytes
    }

    #[test]
    fn source_and_translated_deltas_fill_separate_lanes() {
        let mut protocol = TranslationProtocol::new(Language::Vietnamese, true);
        let (text, original, _) =
            lanes(protocol.on_event(&event("session.input_transcript.delta", "今日は")));
        assert_eq!((text.as_str(), original.as_deref()), ("", Some("今日は")));
        protocol.on_event(&event("session.input_transcript.delta", "新しい"));
        let (text, original, status) =
            lanes(protocol.on_event(&event("session.output_transcript.delta", "Hôm nay")));
        assert_eq!(text, "Hôm nay");
        assert_eq!(original.as_deref(), Some("今日は新しい"));
        assert_eq!(status, CaptionStatus::Partial);
    }

    #[test]
    fn output_audio_and_unknown_events_are_ignored_and_errors_surface() {
        let mut protocol = TranslationProtocol::new(Language::English, true);
        let audio = json!({"type": "session.output_audio.delta", "delta": "AAAA"}).to_string();
        assert!(matches!(protocol.on_event(&audio), EventOutcome::Ignored));
        assert!(matches!(
            protocol.on_event("][garbage"),
            EventOutcome::Ignored
        ));
        assert!(matches!(
            protocol.on_event(r#"{"type":"error","error":{"type":"server_error","code":"x"}}"#),
            EventOutcome::ServiceError(_)
        ));
    }

    #[test]
    fn close_flushes_audio_then_waits_for_session_closed() {
        let mut protocol = TranslationProtocol::new(Language::English, false);
        protocol.on_audio(&vec![0.1; 1_000]);
        let messages = protocol.finish();
        let kinds: Vec<String> = messages
            .iter()
            .map(|m| {
                serde_json::from_str::<Value>(m).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            kinds,
            ["session.input_audio_buffer.append", "session.close"]
        );
        assert!(!protocol.is_drained());
        protocol.on_event(&event("session.output_transcript.delta", "Hello"));
        let (_, _, status) = lanes(protocol.on_event(r#"{"type":"session.closed"}"#));
        assert_eq!(status, CaptionStatus::Final);
        assert!(protocol.is_drained());
    }

    #[test]
    fn lanes_stay_bounded_on_long_sessions() {
        let mut protocol = TranslationProtocol::new(Language::Japanese, true);
        for _ in 0..500 {
            protocol.on_event(&event(
                "session.output_transcript.delta",
                "字幕のテキスト。",
            ));
            protocol.on_event(&event("session.input_transcript.delta", "caption text. "));
        }
        assert!(protocol.translation.len() <= super::MAX_LANE_BYTES);
        assert!(protocol.source.len() <= super::MAX_LANE_BYTES);
    }
}
