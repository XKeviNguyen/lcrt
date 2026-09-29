//! What the realtime session worker needs from a service protocol.

use lcrt_core::TranscriptUpdate;
use serde::Deserialize;

/// An `error` object sent by the service.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct ServiceError {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

/// How a service error affects the session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceErrorImpact {
    /// The session cannot continue until the user fixes the API key.
    Unauthorized,
    /// Quota, billing, or rate limits stop the session.
    RateLimited,
    /// The service reports most errors as recoverable; keep going.
    Recoverable,
}

impl ServiceError {
    /// A loggable category built from the type and code only; service error
    /// messages can echo request content, so they are never logged.
    pub fn category(&self) -> String {
        format!(
            "{}/{}",
            self.kind.as_deref().unwrap_or("unknown"),
            self.code.as_deref().unwrap_or("none")
        )
    }

    /// Classifies the error by its documented type and code.
    pub fn impact(&self) -> ServiceErrorImpact {
        let kind = self.kind.as_deref().unwrap_or_default();
        let code = self.code.as_deref().unwrap_or_default();
        if kind == "authentication_error" || code == "invalid_api_key" {
            ServiceErrorImpact::Unauthorized
        } else if kind == "rate_limit_error"
            || matches!(
                code,
                "rate_limit_exceeded"
                    | "insufficient_quota"
                    | "credit_balance_exhausted"
                    | "organization_spend_limit_exceeded"
                    | "project_spend_limit_exceeded"
                    | "organization_usage_limit_exceeded"
            )
        {
            ServiceErrorImpact::RateLimited
        } else {
            ServiceErrorImpact::Recoverable
        }
    }
}

/// The effect of one server event.
#[derive(Debug)]
pub enum EventOutcome {
    /// The caption changed.
    Update(TranscriptUpdate),
    /// The service reported an error.
    ServiceError(ServiceError),
    /// Nothing visible changed.
    Ignored,
}

/// One realtime service protocol, free of I/O so it can be tested directly.
pub trait Protocol: Send {
    /// WebSocket URL, without credentials.
    fn url(&self) -> String;
    /// Messages sent after `session.created` to configure the session.
    fn configure(&self) -> Vec<String>;
    /// Messages for new mono audio at the online sample rate.
    fn on_audio(&mut self, samples: &[f32]) -> Vec<String>;
    /// Handles one server event.
    fn on_event(&mut self, text: &str) -> EventOutcome;
    /// Messages that end the stream when capture stops.
    fn finish(&mut self) -> Vec<String>;
    /// Whether everything requested by `finish` has arrived.
    fn is_drained(&self) -> bool;
    /// Forgets per-connection state before reconnecting.
    fn reset_connection(&mut self);
}

/// Returns at most `max_bytes` from the end of `text`, starting at a word
/// boundary when one is near and never inside a character.
pub fn bounded_tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    // Prefer to start after a space within the first quarter of the tail;
    // scripts without spaces (such as Japanese) keep the character boundary.
    let search_limit = tail.len() / 4;
    match tail
        .char_indices()
        .take_while(|(index, _)| *index < search_limit)
        .find(|(_, character)| *character == ' ')
    {
        Some((space, _)) => &tail[space + 1..],
        None => tail,
    }
}

#[cfg(test)]
mod tests {
    use super::{ServiceError, ServiceErrorImpact, bounded_tail};

    fn error(kind: &str, code: &str) -> ServiceError {
        ServiceError {
            kind: Some(kind.to_owned()),
            code: Some(code.to_owned()),
        }
    }

    #[test]
    fn service_errors_are_classified_by_type_and_code() {
        assert_eq!(
            error("authentication_error", "x").impact(),
            ServiceErrorImpact::Unauthorized
        );
        assert_eq!(
            error("invalid_request_error", "insufficient_quota").impact(),
            ServiceErrorImpact::RateLimited
        );
        assert_eq!(
            error("invalid_request_error", "invalid_event").impact(),
            ServiceErrorImpact::Recoverable
        );
        assert_eq!(error("a", "b").category(), "a/b");
    }

    #[test]
    fn tails_are_bounded_and_respect_characters_and_words() {
        assert_eq!(bounded_tail("short", 10), "short");
        assert_eq!(bounded_tail("one two three four", 12), "three four");
        let japanese = "今日は新しいプロジェクトについて話します";
        let tail = bounded_tail(japanese, 10);
        assert!(tail.len() <= 10);
        assert!(japanese.ends_with(tail));
    }
}
