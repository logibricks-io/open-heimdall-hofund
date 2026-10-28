//! Typed TCP client for the hofund control protocol.

use std::fmt;
use std::net::SocketAddr;

use hofund_control_proto::{
    ControlError, ErrorEnvelope, HealthResponse, MAX_RESPONSE_BODY_BYTES, StatusResponse,
    decode_error_envelope, decode_response,
};
use http::header;
use http_body_util::{BodyExt, Empty, Limited};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

pub use hofund_control_proto;

/// A control client connected by explicit TCP addresses.
#[derive(Clone, Debug)]
pub struct Client {
    addr: SocketAddr,
    token: Token,
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

/// A transport, protocol-decoding, or control-server failure.
#[derive(Debug)]
pub enum ClientError {
    /// TCP or HTTP transport failed without exposing credential material.
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

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => formatter.write_str(message),
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
        Self {
            addr,
            token: Token(token.into()),
        }
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
        let stream = TcpStream::connect(self.addr)
            .await
            .map_err(|error| transport("control TCP connection failed", error))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| transport("control HTTP handshake failed", error))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri(path)
            .header(header::HOST, self.addr.to_string())
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

fn transport(context: &str, error: impl fmt::Display) -> ClientError {
    ClientError::Transport(format!("{context}: {error}"))
}
