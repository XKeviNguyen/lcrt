//! Realtime transcription protocol (`gpt-live-transcribe`) as a pure state
//! machine: outbound messages from audio, caption updates from server events.

use base64::Engine as _;
use lcrt_core::{Language, TranscriptUpdate};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::{
    audio::{TurnAction, TurnConfig, TurnDetector, encode_pcm16},
    protocol::{EventOutcome, Protocol, ServiceError, bounded_tail},
};

/// Realtime transcription endpoint; the model is chosen in `session.update`.
pub const TRANSCRIPTION_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";
/// Recommended realtime transcription model.
pub const TRANSCRIPTION_MODEL: &str = "gpt-live-transcribe";
/// Latency setting documented for live captions.
const TRANSCRIPTION_DELAY: &str = "low";
/// Recent caption text kept for display; older turns are dropped.
const MAX_CAPTION_BYTES: usize = 480;
/// Turns kept for ordering; older completed turns are dropped.
const MAX_TURNS: usize = 16;
/// Audio sent per append message (100 ms at 24 kHz).
const APPEND_SAMPLES: usize = 2_400;

#[derive(Debug)]
struct Turn {
    item_id: String,
    text: String,
    complete: bool,
}

/// Client state for one transcription session, reset on reconnect.
pub struct TranscriptionProtocol {
    language: Option<Language>,
    detector: TurnDetector,
    outgoing: Vec<f32>,
    turns: Vec<Turn>,
    uncommitted_turns: usize,
}

impl TranscriptionProtocol {
    /// Creates the protocol with an optional spoken-language hint.
    pub fn new(language: Option<Language>) -> Self {
        Self {
            language,
            detector: TurnDetector::new(TurnConfig::default()),
            outgoing: Vec::with_capacity(APPEND_SAMPLES),
            turns: Vec::new(),
            uncommitted_turns: 0,
        }
    }

    fn append_messages(&mut self, messages: &mut Vec<String>, flush: bool) {
        while self.outgoing.len() >= APPEND_SAMPLES || (flush && !self.outgoing.is_empty()) {
            let take = self.outgoing.len().min(APPEND_SAMPLES);
            let mut pcm = Vec::with_capacity(take * 2);
            encode_pcm16(&self.outgoing[..take], &mut pcm);
            self.outgoing.drain(..take);
            let audio = base64::engine::general_purpose::STANDARD.encode(pcm);
            messages.push(json!({"type": "input_audio_buffer.append", "audio": audio}).to_string());
        }
    }

    fn turn_mut(&mut self, item_id: &str) -> &mut Turn {
        let index = match self.turns.iter().position(|turn| turn.item_id == item_id) {
            Some(index) => index,
            None => {
                self.turns.push(Turn {
                    item_id: item_id.to_owned(),
                    text: String::new(),
                    complete: false,
                });
                self.turns.len() - 1
            }
        };
        &mut self.turns[index]
    }

    fn insert_committed(&mut self, item_id: String, previous_item_id: Option<String>) {
        if self.turns.iter().any(|turn| turn.item_id == item_id) {
            return;
        }
        let turn = Turn {
            item_id,
            text: String::new(),
            complete: false,
        };
        let position = previous_item_id
            .and_then(|previous| self.turns.iter().position(|turn| turn.item_id == previous))
            .map_or(self.turns.len(), |index| index + 1);
        self.turns.insert(position, turn);
    }

    fn prune(&mut self) {
        while self.turns.len() > MAX_TURNS && self.turns.first().is_some_and(|turn| turn.complete) {
            self.turns.remove(0);
        }
    }

    fn caption(&self) -> Option<TranscriptUpdate> {
        let split = self
            .turns
            .iter()
            .position(|turn| !turn.complete)
            .unwrap_or(self.turns.len());
        let join = |turns: &[Turn]| {
            turns
                .iter()
                .map(|turn| turn.text.trim())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let stable = join(&self.turns[..split]);
        let partial = join(&self.turns[split..]);
        let full = match (stable.is_empty(), partial.is_empty()) {
            (_, true) => stable.clone(),
            (true, false) => partial.clone(),
            (false, false) => format!("{stable} {partial}"),
        };
        let shown = bounded_tail(&full, MAX_CAPTION_BYTES);
        if shown.trim().is_empty() {
            return None;
        }
        let stable_len = stable.len().saturating_sub(full.len() - shown.len());
        if partial.is_empty() {
            return TranscriptUpdate::finalized(shown).ok();
        }
        let (stable_part, partial_part) = shown.split_at(floor_char_boundary(shown, stable_len));
        TranscriptUpdate::incremental(stable_part, partial_part).ok()
    }
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    item_id: Option<String>,
    previous_item_id: Option<String>,
    delta: Option<String>,
    transcript: Option<String>,
    error: Option<ServiceError>,
}

impl Protocol for TranscriptionProtocol {
    fn url(&self) -> String {
        TRANSCRIPTION_URL.to_owned()
    }

    fn configure(&self) -> Vec<String> {
        let mut transcription = json!({
            "model": TRANSCRIPTION_MODEL,
            "delay": TRANSCRIPTION_DELAY,
        });
        if let Some(language) = self.language {
            transcription["languages"] = json!([language.code()]);
        }
        vec![
            json!({
                "type": "session.update",
                "session": {
                    "type": "transcription",
                    "audio": {"input": {
                        "format": {"type": "audio/pcm", "rate": 24_000},
                        "transcription": transcription,
                        "turn_detection": null,
                    }},
                },
            })
            .to_string(),
        ]
    }

    fn on_audio(&mut self, samples: &[f32]) -> Vec<String> {
        let mut messages = Vec::new();
        for action in self.detector.push(samples) {
            self.apply_turn_action(action, &mut messages);
        }
        messages
    }

    fn finish(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        for action in self.detector.finish() {
            self.apply_turn_action(action, &mut messages);
        }
        messages
    }

    fn is_drained(&self) -> bool {
        self.uncommitted_turns == 0 && self.turns.iter().all(|turn| turn.complete)
    }

    fn on_event(&mut self, text: &str) -> EventOutcome {
        let event: Event = match serde_json::from_str(text) {
            Ok(event) => event,
            Err(error) => {
                warn!(%error, "ignoring malformed transcription event");
                return EventOutcome::Ignored;
            }
        };
        match event.kind.as_str() {
            "input_audio_buffer.committed" => {
                let Some(item_id) = event.item_id else {
                    return EventOutcome::Ignored;
                };
                self.uncommitted_turns = self.uncommitted_turns.saturating_sub(1);
                self.insert_committed(item_id, event.previous_item_id);
                EventOutcome::Ignored
            }
            "conversation.item.input_audio_transcription.delta" => {
                let (Some(item_id), Some(delta)) = (event.item_id, event.delta) else {
                    return EventOutcome::Ignored;
                };
                let turn = self.turn_mut(&item_id);
                if !turn.complete {
                    turn.text.push_str(&delta);
                }
                self.caption()
                    .map_or(EventOutcome::Ignored, EventOutcome::Update)
            }
            "conversation.item.input_audio_transcription.completed" => {
                let Some(item_id) = event.item_id else {
                    return EventOutcome::Ignored;
                };
                let turn = self.turn_mut(&item_id);
                // The completed transcript is authoritative over accumulated deltas.
                turn.text = event.transcript.unwrap_or_default();
                turn.complete = true;
                self.prune();
                self.caption()
                    .map_or(EventOutcome::Ignored, EventOutcome::Update)
            }
            "conversation.item.input_audio_transcription.failed" => {
                if let Some(item_id) = event.item_id {
                    let turn = self.turn_mut(&item_id);
                    turn.complete = true;
                }
                if let Some(error) = event.error {
                    warn!(category = %error.category(), "a transcription turn failed");
                }
                self.prune();
                self.caption()
                    .map_or(EventOutcome::Ignored, EventOutcome::Update)
            }
            "error" => event
                .error
                .map_or(EventOutcome::Ignored, EventOutcome::ServiceError),
            other => {
                debug!(event = other, "ignoring transcription event");
                EventOutcome::Ignored
            }
        }
    }

    fn reset_connection(&mut self) {
        self.detector = TurnDetector::new(TurnConfig::default());
        self.outgoing.clear();
        self.uncommitted_turns = 0;
        // Turns cut off by the lost connection keep their partial text.
        for turn in &mut self.turns {
            turn.complete = true;
        }
    }
}

impl TranscriptionProtocol {
    fn apply_turn_action(&mut self, action: TurnAction, messages: &mut Vec<String>) {
        match action {
            TurnAction::Append(samples) => {
                self.outgoing.extend(samples);
                self.append_messages(messages, false);
            }
            TurnAction::Commit => {
                self.append_messages(messages, true);
                self.uncommitted_turns += 1;
                messages.push(json!({"type": "input_audio_buffer.commit"}).to_string());
            }
            TurnAction::Clear => {
                self.outgoing.clear();
                messages.push(json!({"type": "input_audio_buffer.clear"}).to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use lcrt_core::{CaptionStatus, Language};
    use serde_json::{Value, json};

    use super::TranscriptionProtocol;
    use crate::protocol::{EventOutcome, Protocol};

    fn committed(item: &str, previous: Option<&str>) -> String {
        json!({"type": "input_audio_buffer.committed", "item_id": item, "previous_item_id": previous})
            .to_string()
    }

    fn delta(item: &str, text: &str) -> String {
        json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": item, "delta": text})
            .to_string()
    }

    fn completed(item: &str, text: &str) -> String {
        json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": item, "transcript": text})
            .to_string()
    }

    fn text_of(outcome: EventOutcome) -> (String, CaptionStatus) {
        match outcome {
            EventOutcome::Update(update) => (update.text().to_owned(), update.status()),
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn session_update_uses_documented_fields_and_plural_language_hint() {
        let config: Value = serde_json::from_str(
            &TranscriptionProtocol::new(Some(Language::Japanese)).configure()[0],
        )
        .unwrap();
        let input = &config["session"]["audio"]["input"];
        assert_eq!(config["type"], "session.update");
        assert_eq!(config["session"]["type"], "transcription");
        assert_eq!(
            input["format"],
            json!({"type": "audio/pcm", "rate": 24_000})
        );
        assert_eq!(input["transcription"]["model"], "gpt-live-transcribe");
        assert_eq!(input["transcription"]["languages"], json!(["ja"]));
        assert_eq!(input["transcription"]["delay"], "low");
        assert!(input["transcription"].get("language").is_none());
        assert!(input["turn_detection"].is_null());
    }

    #[test]
    fn auto_language_sends_no_language_hint() {
        let config: Value =
            serde_json::from_str(&TranscriptionProtocol::new(None).configure()[0]).unwrap();
        assert!(
            config["session"]["audio"]["input"]["transcription"]
                .get("languages")
                .is_none()
        );
    }

    #[test]
    fn speech_is_appended_in_bounded_messages_and_committed_after_silence() {
        let mut protocol = TranscriptionProtocol::new(None);
        let mut messages = protocol.on_audio(&vec![0.2; 24_000]);
        messages.extend(protocol.on_audio(&vec![0.0; 24_000]));
        let kinds: Vec<Value> = messages
            .iter()
            .map(|message| serde_json::from_str::<Value>(message).unwrap()["type"].clone())
            .collect();
        assert_eq!(kinds.last().unwrap(), "input_audio_buffer.commit");
        assert!(
            kinds
                .iter()
                .filter(|kind| *kind == "input_audio_buffer.append")
                .count()
                >= 10
        );
        assert!(messages.iter().all(|message| message.len() < 16 * 1024));
        assert!(!protocol.is_drained());
    }

    #[test]
    fn deltas_accumulate_and_the_completed_transcript_replaces_them() {
        let mut protocol = TranscriptionProtocol::new(None);
        protocol.on_event(&committed("a", None));
        assert_eq!(text_of(protocol.on_event(&delta("a", "Hel"))).0, "Hel");
        assert_eq!(
            text_of(protocol.on_event(&delta("a", "lo wrld"))),
            ("Hello wrld".to_owned(), CaptionStatus::Partial)
        );
        assert_eq!(
            text_of(protocol.on_event(&completed("a", "Hello world."))),
            ("Hello world.".to_owned(), CaptionStatus::Final)
        );
    }

    #[test]
    fn out_of_order_completions_keep_speech_order() {
        let mut protocol = TranscriptionProtocol::new(None);
        protocol.on_event(&committed("a", None));
        protocol.on_event(&committed("b", Some("a")));
        let (text, status) = text_of(protocol.on_event(&completed("b", "second.")));
        assert_eq!(text, "second.");
        assert_eq!(status, CaptionStatus::Partial);
        let (text, status) = text_of(protocol.on_event(&completed("a", "First.")));
        assert_eq!(text, "First. second.");
        assert_eq!(status, CaptionStatus::Final);
    }

    #[test]
    fn late_deltas_after_completion_do_not_corrupt_the_final_text() {
        let mut protocol = TranscriptionProtocol::new(None);
        protocol.on_event(&committed("a", None));
        protocol.on_event(&completed("a", "Done."));
        assert_eq!(text_of(protocol.on_event(&delta("a", " stray"))).0, "Done.");
    }

    #[test]
    fn malformed_unknown_and_error_events_are_contained() {
        let mut protocol = TranscriptionProtocol::new(None);
        assert!(matches!(
            protocol.on_event("{not json"),
            EventOutcome::Ignored
        ));
        assert!(matches!(
            protocol.on_event(r#"{"type":"future.event","x":1}"#),
            EventOutcome::Ignored
        ));
        let error = protocol.on_event(
            r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_event","message":"m"}}"#,
        );
        assert!(matches!(error, EventOutcome::ServiceError(_)));
    }

    #[test]
    fn caption_history_stays_bounded() {
        let mut protocol = TranscriptionProtocol::new(None);
        let mut previous: Option<String> = None;
        for index in 0..200 {
            let id = format!("item{index}");
            protocol.on_event(&committed(&id, previous.as_deref()));
            protocol.on_event(&completed(
                &id,
                "a reasonably long sentence of caption text.",
            ));
            previous = Some(id);
        }
        let (text, _) = text_of(protocol.on_event(&completed("item199", "last.")));
        assert!(text.len() <= super::MAX_CAPTION_BYTES);
        assert!(text.ends_with("last."));
        assert!(protocol.turns.len() <= super::MAX_TURNS);
    }

    #[test]
    fn reconnect_finalizes_interrupted_turns_and_resets_audio_state() {
        let mut protocol = TranscriptionProtocol::new(None);
        protocol.on_audio(&vec![0.2; 12_000]);
        protocol.on_event(&committed("a", None));
        protocol.on_event(&delta("a", "partial"));
        protocol.reset_connection();
        assert!(protocol.is_drained());
        assert_eq!(protocol.caption().unwrap().status(), CaptionStatus::Final);
    }
}
