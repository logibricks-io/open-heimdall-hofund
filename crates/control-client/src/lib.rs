//! Typed TCP and local IPC client for the hofund control protocol.

use std::fmt;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::PathBuf;

use hofund_control_proto::{
    ControlError, ErrorEnvelope, HealthResponse, MAX_RESPONSE_BODY_BYTES, StatusResponse,
    decode_error_envelope, decode_response,
};
use http::header;
use http_body_util::{BodyExt, Empty, Limited};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

mod discovery;

pub use discovery::{DiscoveryOptions, DiscoveryRuntime};
pub use hofund_control_proto;

/// The only Host value used over local IPC.
pub const LOCAL_IPC_HOST: &str = "hofund-control";

/// A control client connected by an explicit local endpoint.
#[derive(Clone, Debug)]
pub struct Client {
    endpoint: Endpoint,
    token: Token,
}

#[derive(Clone, Debug)]
pub(crate) enum Endpoint {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

#[derive(Clone)]
struct Token(String);

impl Token {
    fn header(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// A discovery, transport, protocol-decoding, or control-server failure.
#[derive(Debug)]
pub enum ClientError {
    /// Discovery metadata or an explicit endpoint is invalid.
    Discovery(String),
    /// The endpoint refused a connection or does not exist.
    ConnectionUnavailable(String),
    /// TCP, local IPC, or HTTP transport failed without exposing credentials.
    Transport(String),
    /// A success or error body failed protocol decoding.
    Decode(ControlError),
    /// The server returned a decoded control error envelope.
    Server {
        /// HTTP status returned by the server.
        status: http::StatusCode,
        /// Decoded control failure.
        error: ErrorEnvelope,
    },
}

impl ClientError {
    pub(crate) fn is_unavailable(&self) -> bool {
        matches!(self, Self::ConnectionUnavailable(_))
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovery(message) => formatter.write_str(message),
            Self::ConnectionUnavailable(message) | Self::Transport(message) => {
                formatter.write_str(message)
            }
            Self::Decode(error) => write!(
                formatter,
                "control response decode failed: {}",
                error.message
            ),
            Self::Server { status, error } => {
                write!(
                    formatter,
                    "control server returned {status}: {}",
                    error.error.message
                )
            }
        }
    }
}

impl std::error::Error for ClientError {}

impl Client {
    /// Creates a TCP client for `addr` using `token` for Bearer authentication.
    #[must_use]
    pub fn tcp(addr: SocketAddr, token: impl Into<String>) -> Self {
        Self::from_endpoint(Endpoint::Tcp(addr), token)
    }

    /// Creates a Unix-socket client using `token` for Bearer authentication.
    #[cfg(unix)]
    #[must_use]
    pub fn unix(path: impl Into<PathBuf>, token: impl Into<String>) -> Self {
        Self::from_endpoint(Endpoint::Unix(path.into()), token)
    }

    pub(crate) fn from_endpoint(endpoint: Endpoint, token: impl Into<String>) -> Self {
        Self {
            endpoint,
            token: Token(token.into()),
        }
    }

    /// Discovers a running config daemon and verifies its control health.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid discovery metadata, an unreachable
    /// daemon, or a health response that does not match this protocol.
    pub async fn discover(
        config_path: &std::path::Path,
        token: impl Into<String>,
        options: DiscoveryOptions,
    ) -> Result<Self, ClientError> {
        discovery::discover(config_path, token.into(), options).await
    }

    /// Gets control health.
    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        self.get("/v1/hofund/health").await
    }

    /// Gets daemon status.
    pub async fn status(&self) -> Result<StatusResponse, ClientError> {
        self.get("/v1/hofund/status").await
    }

    async fn get<T: hofund_control_proto::Response>(&self, path: &str) -> Result<T, ClientError> {
        match &self.endpoint {
            Endpoint::Tcp(addr) => {
                let stream = TcpStream::connect(addr)
                    .await
                    .map_err(|error| connect_error("control TCP connection failed", error))?;
                self.get_over(stream, path, addr.to_string()).await
            }
            #[cfg(unix)]
            Endpoint::Unix(socket) => {
                verify_unix_socket(socket)?;
                let stream = tokio::net::UnixStream::connect(socket)
                    .await
                    .map_err(|error| connect_error("control Unix connection failed", error))?;
                self.get_over(stream, path, LOCAL_IPC_HOST.to_owned()).await
            }
        }
    }

    async fn get_over<T, S>(&self, stream: S, path: &str, host: String) -> Result<T, ClientError>
    where
        T: hofund_control_proto::Response,
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| transport("control HTTP handshake failed", error))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri(path)
            .header(header::HOST, host)
            .header(header::AUTHORIZATION, self.token.header())
            .body(Empty::<&[u8]>::new())
            .map_err(|_| ClientError::Transport("cannot construct control request".to_owned()))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| transport("control HTTP request failed", error))?;
        let status = response.status();
        let body = Limited::new(response.into_body(), MAX_RESPONSE_BODY_BYTES)
            .collect()
            .await
            .map_err(|error| transport("control response body failed", error))?
            .to_bytes();
        if !status.is_success() {
            let error = decode_error_envelope(hofund_control_proto::API_VERSION, &body)
                .map_err(ClientError::Decode)?;
            return Err(ClientError::Server { status, error });
        }
        decode_response(hofund_control_proto::API_VERSION, &body).map_err(ClientError::Decode)
    }
}

#[cfg(unix)]
fn verify_unix_socket(path: &std::path::Path) -> Result<(), ClientError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            connect_error("control Unix connection failed", error)
        } else {
            ClientError::Discovery(format!(
                "cannot inspect control socket {}: {error}",
                path.display()
            ))
        }
    })?;
    let uid = discovery::current_uid(&std::env::temp_dir())?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != uid
    {
        return Err(ClientError::Discovery(format!(
            "control socket must be a current-user-owned socket, not a symlink: {}",
            path.display()
        )));
    }
    Ok(())
}

fn connect_error(context: &str, error: std::io::Error) -> ClientError {
    let message = format!("{context}: {error}");
    if matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    ) {
        ClientError::ConnectionUnavailable(message)
    } else {
        ClientError::Transport(message)
    }
}

fn transport(context: &str, error: impl fmt::Display) -> ClientError {
    ClientError::Transport(format!("{context}: {error}"))
}
