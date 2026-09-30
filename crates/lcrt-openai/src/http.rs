//! Bounded HTTPS requests: connection test and vocabulary explanations.

use std::{sync::OnceLock, time::Duration};

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    credentials::ApiKey,
    protocol::{ServiceError, ServiceErrorImpact},
    transport::{TransportError, error_for_status},
};

const API_BASE: &str = "https://api.openai.com/v1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest error body read to classify a failure.
const MAX_ERROR_BODY_BYTES: u64 = 16 * 1024;
/// Largest response body read from the service.
pub(crate) const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into()
    })
}

fn map_request_error(error: ureq::Error) -> TransportError {
    match error {
        ureq::Error::StatusCode(status) => error_for_status(status),
        ureq::Error::Timeout(_) => TransportError::Unreachable("request timed out".to_owned()),
        ureq::Error::BodyExceedsLimit(_) => {
            TransportError::Protocol("response too large".to_owned())
        }
        other => TransportError::Unreachable(other.to_string()),
    }
}

/// Classifies a failed response. HTTP 429 means either temporary rate
/// limiting or exhausted quota; only the error body tells them apart.
fn error_for_response(status: u16, body: Option<&str>) -> TransportError {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: ServiceError,
    }
    let quota_exhausted = status == 429
        && body
            .and_then(|body| serde_json::from_str::<ErrorBody>(body).ok())
            .is_some_and(|parsed| parsed.error.impact() == ServiceErrorImpact::QuotaExhausted);
    if quota_exhausted {
        TransportError::QuotaExhausted
    } else {
        error_for_status(status)
    }
}

fn failure(mut response: ureq::http::Response<ureq::Body>) -> TransportError {
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_ERROR_BODY_BYTES)
        .read_to_string()
        .ok();
    error_for_response(status, body.as_deref())
}

/// Verifies that the service accepts `key`, without billable audio work.
pub fn test_connection(key: &ApiKey) -> Result<(), TransportError> {
    let response = agent()
        .get(format!("{API_BASE}/models"))
        .header("Authorization", format!("Bearer {}", key.expose()))
        .call()
        .map_err(map_request_error)?;
    info!(
        status = response.status().as_u16(),
        "connection test completed"
    );
    if response.status().is_success() {
        Ok(())
    } else {
        Err(failure(response))
    }
}

/// POSTs JSON and returns the bounded response body.
pub(crate) fn post_json(
    key: &ApiKey,
    path: &str,
    body: &impl Serialize,
) -> Result<String, TransportError> {
    let mut response = agent()
        .post(format!("{API_BASE}/{path}"))
        .header("Authorization", format!("Bearer {}", key.expose()))
        .send_json(body)
        .map_err(map_request_error)?;
    if !response.status().is_success() {
        return Err(failure(response));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(map_request_error)
}

#[cfg(test)]
mod tests {
    use super::error_for_response;
    use crate::transport::TransportError;

    #[test]
    fn a_429_is_exhausted_quota_only_when_the_body_says_so() {
        let quota = r#"{"error": {"type": "insufficient_quota", "code": "insufficient_quota"}}"#;
        assert_eq!(
            error_for_response(429, Some(quota)),
            TransportError::QuotaExhausted
        );
        let rate = r#"{"error": {"type": "rate_limit_error", "code": "rate_limit_exceeded"}}"#;
        assert_eq!(
            error_for_response(429, Some(rate)),
            TransportError::RateLimited
        );
        assert_eq!(error_for_response(429, None), TransportError::RateLimited);
        assert_eq!(
            error_for_response(429, Some("not json")),
            TransportError::RateLimited
        );
        assert_eq!(
            error_for_response(401, Some(quota)),
            TransportError::Unauthorized
        );
    }
}
