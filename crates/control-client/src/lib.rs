//! Typed TCP and local IPC client for the hofund control protocol.

use std::fmt;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::PathBuf;

use hofund_control_proto::{
    AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest, AgentListResponse,
    AgentPlanRequest, AgentPlanResponse, AgentStatusResponse, ControlError, ErrorEnvelope,
    HealthResponse, MAX_RESPONSE_BODY_BYTES, ModelListResponse, PicksResponse, PicksSetRequest,
    ProviderListResponse, ProviderTestRequest, ProviderTestResponse, RouteModelListResponse,
    ShutdownRequest, ShutdownResponse, StatusResponse, TokenRotateRequest, TokenRotateResponse,
    UsagePeriod, UsageQuery, UsageResponse, decode_error_envelope, decode_response,
};
use http::header;
use http_body_util::{BodyExt, Full, Limited};
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

    /// Lists configured providers.
    pub async fn providers(&self) -> Result<ProviderListResponse, ClientError> {
        self.get("/v1/hofund/providers").await
    }

    /// Reads a provider's cached models.
    pub async fn provider_models(&self, id: &str) -> Result<ModelListResponse, ClientError> {
        self.get(&format!(
            "/v1/hofund/providers/{}/models",
            encode_segment(id)
        ))
        .await
    }

    /// Refreshes a provider's models.
    pub async fn refresh_models(&self, id: &str) -> Result<ModelListResponse, ClientError> {
        self.request(
            http::Method::POST,
            &format!("/v1/hofund/providers/{}/models/refresh", encode_segment(id)),
            Some(Vec::new()),
        )
        .await
    }

    /// Tests connectivity to a provider.
    pub async fn test_provider(
        &self,
        id: &str,
        request: ProviderTestRequest,
    ) -> Result<ProviderTestResponse, ClientError> {
        self.request(
            http::Method::POST,
            &format!("/v1/hofund/providers/{}/test", encode_segment(id)),
            Some(json_body(&request)?),
        )
        .await
    }

    /// Reads a provider's model picks.
    pub async fn picks(&self, id: &str) -> Result<PicksResponse, ClientError> {
        self.get(&format!(
            "/v1/hofund/providers/{}/picks",
            encode_segment(id)
        ))
        .await
    }

    /// Replaces or edits a provider's model picks.
    pub async fn set_picks(
        &self,
        id: &str,
        request: PicksSetRequest,
    ) -> Result<PicksResponse, ClientError> {
        self.request(
            http::Method::PUT,
            &format!("/v1/hofund/providers/{}/picks", encode_segment(id)),
            Some(json_body(&request)?),
        )
        .await
    }

    /// Lists configured client-facing route models.
    pub async fn route_models(&self) -> Result<RouteModelListResponse, ClientError> {
        self.get("/v1/hofund/models").await
    }

    /// Lists known agents.
    pub async fn agents(&self) -> Result<AgentListResponse, ClientError> {
        self.get("/v1/hofund/agents").await
    }

    /// Reports agent connection states.
    pub async fn agent_status(&self) -> Result<AgentStatusResponse, ClientError> {
        self.get("/v1/hofund/agents/status").await
    }

    /// Plans an agent lifecycle change.
    pub async fn plan_agent(
        &self,
        id: &str,
        request: AgentPlanRequest,
    ) -> Result<AgentPlanResponse, ClientError> {
        self.request(
            http::Method::POST,
            &format!("/v1/hofund/agents/{}/plan", encode_segment(id)),
            Some(json_body(&request)?),
        )
        .await
    }

    /// Connects an agent.
    pub async fn connect_agent(
        &self,
        id: &str,
        request: AgentConnectRequest,
    ) -> Result<AgentChangeResponse, ClientError> {
        self.request(
            http::Method::POST,
            &format!("/v1/hofund/agents/{}/connect", encode_segment(id)),
            Some(json_body(&request)?),
        )
        .await
    }

    /// Disconnects an agent.
    pub async fn disconnect_agent(
        &self,
        id: &str,
        request: AgentDisconnectRequest,
    ) -> Result<AgentChangeResponse, ClientError> {
        self.request(
            http::Method::POST,
            &format!("/v1/hofund/agents/{}/disconnect", encode_segment(id)),
            Some(json_body(&request)?),
        )
        .await
    }

    /// Reports usage for a period.
    pub async fn usage(&self, query: UsageQuery) -> Result<UsageResponse, ClientError> {
        let period = match query.period {
            UsagePeriod::Day => "day",
            UsagePeriod::Week => "week",
            UsagePeriod::Month => "month",
            UsagePeriod::All => "all",
            _ => "unknown",
        };
        self.get(&format!("/v1/hofund/usage?period={period}")).await
    }

    /// Rotates the control and model bearer token.
    pub async fn rotate_token(
        &self,
        request: TokenRotateRequest,
    ) -> Result<TokenRotateResponse, ClientError> {
        self.request(
            http::Method::POST,
            "/v1/hofund/token/rotate",
            Some(json_body(&request)?),
        )
        .await
    }

    /// Starts graceful daemon shutdown.
    pub async fn shutdown(
        &self,
        request: ShutdownRequest,
    ) -> Result<ShutdownResponse, ClientError> {
        self.request(
            http::Method::POST,
            "/v1/hofund/shutdown",
            Some(json_body(&request)?),
        )
        .await
    }

    async fn get<T: hofund_control_proto::Response>(&self, path: &str) -> Result<T, ClientError> {
        self.request(http::Method::GET, path, None).await
    }

    async fn request<T: hofund_control_proto::Response>(
        &self,
        method: http::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<T, ClientError> {
        match &self.endpoint {
            Endpoint::Tcp(addr) => {
                let stream = TcpStream::connect(addr)
                    .await
                    .map_err(|error| connect_error("control TCP connection failed", error))?;
                self.request_over(stream, method, path, body, addr.to_string())
                    .await
            }
            #[cfg(unix)]
            Endpoint::Unix(socket) => {
                verify_unix_socket(socket)?;
                let stream = tokio::net::UnixStream::connect(socket)
                    .await
                    .map_err(|error| connect_error("control Unix connection failed", error))?;
                self.request_over(stream, method, path, body, LOCAL_IPC_HOST.to_owned())
                    .await
            }
        }
    }

    async fn request_over<T, S>(
        &self,
        stream: S,
        method: http::Method,
        path: &str,
        body: Option<Vec<u8>>,
        host: String,
    ) -> Result<T, ClientError>
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
        let mut builder = http::Request::builder()
            .method(method.clone())
            .uri(path)
            .header(header::HOST, host)
            .header(header::AUTHORIZATION, self.token.header());
        if method != http::Method::GET {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let request = builder
            .body(Full::new(std::io::Cursor::new(body.unwrap_or_default())))
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

fn json_body<T: serde::Serialize>(request: &T) -> Result<Vec<u8>, ClientError> {
    serde_json::to_vec(request)
        .map_err(|_| ClientError::Transport("cannot encode control request".to_owned()))
}

fn encode_segment(id: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

#[cfg(unix)]
fn verify_unix_socket(path: &std::path::Path) -> Result<(), ClientError> {
    let uid = discovery::current_uid(&std::env::temp_dir())?;
    verify_unix_socket_for(path, uid)
}

#[cfg(unix)]
fn verify_unix_socket_for(path: &std::path::Path, expected_uid: u32) -> Result<(), ClientError> {
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
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != expected_uid
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

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = PathBuf::from("/tmp").join(format!(
                "hofund-client-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            let socket = path.join("control.sock");
            assert!(
                socket.as_os_str().as_encoded_bytes().len() < 100,
                "socket path must be shorter than 100 bytes: {}",
                socket.display()
            );
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn unix_socket_verifier_rejects_wrong_owner_uid() {
        use std::os::unix::fs::MetadataExt;
        let dir = Scratch::new();
        let socket = dir.0.join("control.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("Unix listener");
        let owner_uid = std::fs::metadata(&socket).expect("socket metadata").uid();
        super::verify_unix_socket_for(&socket, owner_uid).expect("current owner");
        let error = super::verify_unix_socket_for(&socket, owner_uid.wrapping_add(1))
            .expect_err("wrong owner uid");
        assert!(matches!(error, super::ClientError::Discovery(_)));
    }
}

#[cfg(test)]
mod route_tests {
    use std::convert::Infallible;

    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;

    #[tokio::test]
    async fn client_percent_encodes_provider_id_segments() {
        assert_eq!(super::encode_segment("AZaz09-._~"), "AZaz09-._~");
        let id = "a/b c%é";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("connection");
            let service = service_fn(
                move |request: http::Request<hyper::body::Incoming>| async move {
                    assert_eq!(request.method(), http::Method::GET);
                    assert_eq!(
                        request.uri().path(),
                        "/v1/hofund/providers/a%2Fb%20c%25%C3%A9/models"
                    );
                    let mut response = hofund_control_proto::ModelListResponse::default();
                    response.provider = id.to_owned();
                    let body = serde_json::to_vec(&response).expect("response JSON");
                    let response = http::Response::builder()
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(Full::new(std::io::Cursor::new(body)))
                        .expect("response");
                    Ok::<_, Infallible>(response)
                },
            );
            hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
                .expect("serve connection");
        });
        let client = super::Client::tcp(address, "test-token");
        let response = client.provider_models(id).await.expect("model response");
        assert_eq!(response.provider, id);
        server.await.expect("server");
    }
}
