//! Bounded HTTPS requests: connection test and vocabulary explanations.

use std::{sync::OnceLock, time::Duration};

use serde::Serialize;
use tracing::info;

use crate::{
    credentials::ApiKey,
    transport::{TransportError, error_for_status},
};

const API_BASE: &str = "https://api.openai.com/v1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
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

/// Verifies that the service accepts `key`, without billable audio work.
pub fn test_connection(key: &ApiKey) -> Result<(), TransportError> {
    let response = agent()
        .get(format!("{API_BASE}/models"))
        .header("Authorization", format!("Bearer {}", key.expose()))
        .call()
        .map_err(map_request_error)?;
    let status = response.status().as_u16();
    info!(status, "connection test completed");
    if response.status().is_success() {
        Ok(())
    } else {
        Err(error_for_status(status))
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
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(error_for_status(status));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(map_request_error)
}
