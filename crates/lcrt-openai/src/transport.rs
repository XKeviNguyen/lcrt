//! Bounded, blocking WebSocket transport over verified TLS.

use std::{
    error::Error,
    fmt, io,
    net::{TcpStream, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

use tungstenite::{
    Connector, Message, WebSocket,
    client::IntoClientRequest,
    http::{HeaderValue, StatusCode, header::AUTHORIZATION},
    protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};

use crate::credentials::ApiKey;

/// Largest inbound WebSocket message accepted from the service.
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a connection could not be used, in terms the user can act on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportError {
    /// The service rejected the API key (HTTP 401).
    Unauthorized,
    /// The key lacks access to this model or endpoint (HTTP 403 or 404).
    Forbidden,
    /// The service is rate limiting or out of quota (HTTP 429).
    RateLimited,
    /// DNS, TCP, TLS, or a server-side (5xx) failure.
    Unreachable(String),
    /// The connection closed or broke after it was established.
    Closed,
    /// The service sent something outside the bounded protocol.
    Protocol(String),
}

impl TransportError {
    /// Whether retrying the same request can reasonably succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Unreachable(_) | Self::Closed)
    }

    /// Whether the user must change the API key before retrying.
    pub fn is_credential_rejected(&self) -> bool {
        matches!(self, Self::Unauthorized | Self::Forbidden)
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => formatter.write_str("the API key was rejected"),
            Self::Forbidden => formatter.write_str("the API key has no access to this service"),
            Self::RateLimited => formatter.write_str("the service is rate limiting requests"),
            Self::Unreachable(detail) => write!(formatter, "the service is unreachable: {detail}"),
            Self::Closed => formatter.write_str("the connection was lost"),
            Self::Protocol(detail) => write!(formatter, "unexpected service message: {detail}"),
        }
    }
}

impl Error for TransportError {}

/// Maps an HTTP status from a failed upgrade or request to a transport error.
pub(crate) fn error_for_status(status: u16) -> TransportError {
    match status {
        401 => TransportError::Unauthorized,
        403 | 404 => TransportError::Forbidden,
        429 => TransportError::RateLimited,
        other => TransportError::Unreachable(format!("HTTP {other}")),
    }
}

/// A connected, message-oriented channel to the service.
pub trait Transport: Send {
    /// Sends one JSON text message.
    fn send_text(&mut self, text: &str) -> Result<(), TransportError>;
    /// Waits up to `timeout` for the next text message; `Ok(None)` on timeout.
    fn receive(&mut self, timeout: Duration) -> Result<Option<String>, TransportError>;
    /// Closes the connection, best effort and without blocking indefinitely.
    fn close(&mut self);
}

/// Opens transports; a fake implementation drives deterministic tests.
pub trait Connect: Send {
    /// Connects to `url`, authenticating with `key` in the `Authorization` header.
    fn connect(&self, url: &str, key: &ApiKey) -> Result<Box<dyn Transport>, TransportError>;
}

/// Production connector: `tungstenite` over rustls with Mozilla's roots.
#[derive(Clone)]
pub struct WebSocketConnector {
    tls: Arc<rustls::ClientConfig>,
}

impl Default for WebSocketConnector {
    fn default() -> Self {
        Self { tls: tls_config() }
    }
}

/// Certificate-verifying TLS configuration shared by every online request.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

impl Connect for WebSocketConnector {
    fn connect(&self, url: &str, key: &ApiKey) -> Result<Box<dyn Transport>, TransportError> {
        let mut request = url
            .into_client_request()
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
        let authorization = HeaderValue::from_str(&format!("Bearer {}", key.expose()))
            .map_err(|_| TransportError::Unauthorized)?;
        request.headers_mut().insert(AUTHORIZATION, authorization);

        let host = request
            .uri()
            .host()
            .ok_or_else(|| TransportError::Protocol("URL has no host".to_owned()))?
            .to_owned();
        let port = request.uri().port_u16().unwrap_or(443);
        let address = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|error| TransportError::Unreachable(error.kind().to_string()))?
            .next()
            .ok_or_else(|| TransportError::Unreachable("no address for host".to_owned()))?;
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(|error| TransportError::Unreachable(error.kind().to_string()))?;
        stream
            .set_read_timeout(Some(CONNECT_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(WRITE_TIMEOUT)))
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|error| TransportError::Unreachable(error.kind().to_string()))?;

        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let (socket, _response) = tungstenite::client_tls_with_config(
            request,
            stream,
            Some(config),
            Some(Connector::Rustls(Arc::clone(&self.tls))),
        )
        .map_err(|error| match error {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response)) => {
                error_for_status(response.status().as_u16())
            }
            tungstenite::HandshakeError::Failure(other) => map_socket_error(other),
            tungstenite::HandshakeError::Interrupted(_) => {
                TransportError::Unreachable("handshake timed out".to_owned())
            }
        })?;
        Ok(Box::new(WebSocketTransport { socket }))
    }
}

struct WebSocketTransport {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl WebSocketTransport {
    fn tcp(&self) -> &TcpStream {
        match self.socket.get_ref() {
            MaybeTlsStream::Plain(stream) => stream,
            MaybeTlsStream::Rustls(stream) => stream.get_ref(),
            _ => unreachable!("only plain and rustls streams are constructed"),
        }
    }
}

impl Transport for WebSocketTransport {
    fn send_text(&mut self, text: &str) -> Result<(), TransportError> {
        self.socket
            .send(Message::text(text))
            .map_err(map_socket_error)
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<String>, TransportError> {
        self.tcp()
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
            .map_err(|error| TransportError::Unreachable(error.kind().to_string()))?;
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => return Ok(Some(text.as_str().to_owned())),
                Ok(Message::Close(_)) => return Err(TransportError::Closed),
                // Pings are answered by tungstenite; binary frames are unused.
                Ok(
                    Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_),
                ) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(map_socket_error(error)),
            }
        }
    }

    fn close(&mut self) {
        let _ = self
            .tcp()
            .set_read_timeout(Some(Duration::from_millis(500)));
        let _ = self.socket.close(None);
        // Drain until the peer acknowledges or the short timeout elapses.
        for _ in 0..8 {
            if self.socket.read().is_err() {
                break;
            }
        }
    }
}

fn map_socket_error(error: tungstenite::Error) -> TransportError {
    match error {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            TransportError::Closed
        }
        tungstenite::Error::Io(error) => match error.kind() {
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof => TransportError::Closed,
            kind => TransportError::Unreachable(kind.to_string()),
        },
        tungstenite::Error::Tls(error) => TransportError::Unreachable(format!("TLS: {error}")),
        tungstenite::Error::Capacity(error) => TransportError::Protocol(error.to_string()),
        tungstenite::Error::Http(response) => error_for_status(response.status().as_u16()),
        other => TransportError::Protocol(other.to_string()),
    }
}

// Referenced so the status mapping stays in sync with the `http` crate's names.
const _: () = {
    let _ = StatusCode::UNAUTHORIZED;
};

#[cfg(test)]
mod tests {
    use super::{TransportError, error_for_status};

    #[test]
    fn http_statuses_map_to_actionable_categories() {
        assert_eq!(error_for_status(401), TransportError::Unauthorized);
        assert_eq!(error_for_status(403), TransportError::Forbidden);
        assert_eq!(error_for_status(429), TransportError::RateLimited);
        assert!(matches!(
            error_for_status(503),
            TransportError::Unreachable(_)
        ));
    }

    #[test]
    fn only_connectivity_failures_are_retried() {
        assert!(TransportError::Closed.is_transient());
        assert!(TransportError::Unreachable("dns".to_owned()).is_transient());
        assert!(!TransportError::Unauthorized.is_transient());
        assert!(!TransportError::RateLimited.is_transient());
        assert!(!TransportError::Protocol("x".to_owned()).is_transient());
    }
}
