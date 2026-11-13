//! Loopback-only HTTP servers for subscription plugin tests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// One recorded HTTP/1.1 request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedHttp1Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RecordedHttp1Request {
    /// Request method.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Request target path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// All recorded headers, in wire order.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// One header value, with a case-insensitive name lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Complete request body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// A scripted HTTP/1.1 response.
#[derive(Clone, Debug)]
pub struct Http1Response {
    status: u16,
    headers: Vec<(String, String)>,
    chunks: Vec<Vec<u8>>,
    chunked: bool,
    chunk_delay: Duration,
    header_gate: Option<watch::Receiver<bool>>,
    hold_open: bool,
}

impl Http1Response {
    /// A response with an empty body.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            chunks: Vec::new(),
            chunked: false,
            chunk_delay: Duration::ZERO,
            header_gate: None,
            hold_open: false,
        }
    }

    /// A chunked response containing `chunks`.
    #[must_use]
    pub fn chunked(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks,
            chunked: true,
            ..Self::new(200)
        }
    }

    /// Replaces the response body with one chunk.
    #[must_use]
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.chunks = vec![body];
        self
    }

    /// Adds a response header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Waits before each body chunk.
    #[must_use]
    pub fn chunk_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = delay;
        self
    }

    /// Parks the status and header block until `gate` becomes `true`.
    #[must_use]
    pub fn park_headers(mut self, gate: watch::Receiver<bool>) -> Self {
        self.header_gate = Some(gate);
        self
    }

    /// Leaves the response body open until server shutdown.
    #[must_use]
    pub fn hold_open(mut self) -> Self {
        self.hold_open = true;
        self
    }
}

/// A queued-response HTTP/1.1 server bound to IPv4 loopback.
pub struct Http1Server {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedHttp1Request>>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Http1Server {
    /// Binds an ephemeral loopback port and serves `responses` in request order.
    ///
    /// # Panics
    ///
    /// Panics when the loopback listener cannot be bound.
    pub async fn start(responses: Vec<Http1Response>) -> Self {
        Self::start_on(0, responses).await
    }

    /// Binds `port` on IPv4 loopback. Port zero selects an ephemeral port.
    ///
    /// # Panics
    ///
    /// Panics when the loopback listener cannot be bound.
    pub async fn start_on(port: u16, responses: Vec<Http1Response>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind loopback HTTP/1.1 test server");
        let address = listener.local_addr().expect("loopback address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let scripts = Arc::new(Mutex::new(VecDeque::from(responses)));
        let (shutdown, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let seen = Arc::clone(&seen);
                        let scripts = Arc::clone(&scripts);
                        let stopped = stopped.clone();
                        tokio::spawn(async move {
                            serve_http1(stream, seen, scripts, stopped).await;
                        });
                    }
                }
            }
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            shutdown,
            task: Some(task),
        }
    }

    /// Base URL with no trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Requests recorded so far.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedHttp1Request> {
        self.requests.lock().expect("request records").clone()
    }

    /// Stops accepting connections and closes held responses.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Http1Server {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve_http1(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<RecordedHttp1Request>>>,
    scripts: Arc<Mutex<VecDeque<Http1Response>>>,
    mut stopped: watch::Receiver<bool>,
) {
    let Some(request) = read_http1_request(&mut stream).await else {
        return;
    };
    requests.lock().expect("request records").push(request);
    let response = scripts
        .lock()
        .expect("response scripts")
        .pop_front()
        .unwrap_or_else(|| Http1Response::new(200));
    if let Some(mut gate) = response.header_gate {
        loop {
            if *gate.borrow() {
                break;
            }
            tokio::select! {
                _ = stopped.changed() => return,
                changed = gate.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }
    let reason = http::StatusCode::from_u16(response.status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Test Response");
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if response.chunked {
        head.push_str("transfer-encoding: chunked\r\n");
    } else {
        head.push_str(&format!(
            "content-length: {}\r\n",
            response.chunks.iter().map(Vec::len).sum::<usize>()
        ));
    }
    head.push_str("connection: close\r\n\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for chunk in response.chunks {
        if !response.chunk_delay.is_zero() {
            tokio::time::sleep(response.chunk_delay).await;
        }
        if response.chunked {
            let frame = format!("{:x}\r\n", chunk.len());
            if stream.write_all(frame.as_bytes()).await.is_err()
                || stream.write_all(&chunk).await.is_err()
                || stream.write_all(b"\r\n").await.is_err()
            {
                return;
            }
        } else if stream.write_all(&chunk).await.is_err() {
            return;
        }
    }
    if response.chunked && !response.hold_open && stream.write_all(b"0\r\n\r\n").await.is_err() {
        return;
    }
    if response.hold_open {
        let _ = stopped.changed().await;
    }
    let _ = stream.shutdown().await;
}

async fn read_http1_request(stream: &mut TcpStream) -> Option<RecordedHttp1Request> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let head_end = loop {
        if let Some(index) = find_bytes(&bytes, b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 64 * 1024 {
            return None;
        }
        let count = stream.read(&mut buffer).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
    };
    let head = std::str::from_utf8(&bytes[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < head_end + content_length {
        let count = stream.read(&mut buffer).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Some(RecordedHttp1Request {
        method,
        path,
        headers,
        body: bytes[head_end..head_end + content_length].to_vec(),
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// One recorded HTTP/2 stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedHttp2Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    data: Vec<Vec<u8>>,
}

impl RecordedHttp2Request {
    /// Request method.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Request path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// All request headers.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// One header value, with a case-insensitive name lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Inbound DATA payloads in arrival order.
    #[must_use]
    pub fn data(&self) -> &[Vec<u8>] {
        &self.data
    }
}

/// Keeps parked response headers unreleased until dropped or [`HeaderPark::release`].
pub struct HeaderPark {
    tx: watch::Sender<bool>,
}

impl HeaderPark {
    /// Lets the waiting response send headers.
    pub fn release(self) {
        let _ = self.tx.send(true);
    }
}

/// A park handle and gate for HTTP/1 or HTTP/2 response headers.
#[must_use]
pub fn header_park() -> (HeaderPark, watch::Receiver<bool>) {
    let (tx, rx) = watch::channel(false);
    (HeaderPark { tx }, rx)
}

/// A scripted response to one HTTP/2 stream.
#[derive(Clone, Debug)]
pub struct Http2Response {
    status: u16,
    headers: Vec<(String, String)>,
    data: Vec<Vec<u8>>,
    header_delay: Duration,
    data_delay: Duration,
    header_gate: Option<watch::Receiver<bool>>,
    refused: bool,
    hold_open: bool,
}

impl Http2Response {
    /// A response with no DATA frames.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            data: Vec::new(),
            header_delay: Duration::ZERO,
            data_delay: Duration::ZERO,
            header_gate: None,
            refused: false,
            hold_open: false,
        }
    }

    /// Adds a response header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Adds one outbound DATA payload.
    #[must_use]
    pub fn data(mut self, data: Vec<u8>) -> Self {
        self.data.push(data);
        self
    }

    /// Delays response HEADERS.
    #[must_use]
    pub fn header_delay(mut self, delay: Duration) -> Self {
        self.header_delay = delay;
        self
    }

    /// Waits before each outbound DATA payload.
    #[must_use]
    pub fn data_delay(mut self, delay: Duration) -> Self {
        self.data_delay = delay;
        self
    }

    /// Parks response HEADERS until `gate` becomes `true`.
    ///
    /// A dropped sender without a release never sends HEADERS, so a header
    /// timeout can be driven by the test clock without a delayed sleep that
    /// later completes the stream as an empty 200.
    #[must_use]
    pub fn park_headers(mut self, gate: watch::Receiver<bool>) -> Self {
        self.header_gate = Some(gate);
        self
    }

    /// Sends REFUSED_STREAM instead of response HEADERS.
    #[must_use]
    pub fn refused(mut self) -> Self {
        self.refused = true;
        self
    }

    /// Leaves the response stream open until server shutdown.
    #[must_use]
    pub fn hold_open(mut self) -> Self {
        self.hold_open = true;
        self
    }
}

/// A cleartext, prior-knowledge HTTP/2 server bound to IPv4 loopback.
pub struct Http2Server {
    base_url: String,
    port: u16,
    requests: Arc<Mutex<Vec<RecordedHttp2Request>>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Http2Server {
    /// Binds an ephemeral loopback port and serves `responses` in stream order.
    ///
    /// # Panics
    ///
    /// Panics when the loopback listener cannot be bound.
    pub async fn start(responses: Vec<Http2Response>) -> Self {
        Self::start_on(0, responses).await
    }

    /// Binds `port` on IPv4 loopback. Port zero selects an ephemeral port.
    ///
    /// # Panics
    ///
    /// Panics when the loopback listener cannot be bound.
    pub async fn start_on(port: u16, responses: Vec<Http2Response>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind loopback HTTP/2 test server");
        let address = listener.local_addr().expect("loopback address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let scripts = Arc::new(Mutex::new(VecDeque::from(responses)));
        let (shutdown, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let seen = Arc::clone(&seen);
                        let scripts = Arc::clone(&scripts);
                        let stopped = stopped.clone();
                        tokio::spawn(async move {
                            serve_http2_connection(stream, seen, scripts, stopped).await;
                        });
                    }
                }
            }
        });
        Self {
            base_url: format!("http://{address}"),
            port: address.port(),
            requests,
            shutdown,
            task: Some(task),
        }
    }

    /// Base URL with no trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Bound loopback port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Streams recorded so far.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedHttp2Request> {
        self.requests.lock().expect("stream records").clone()
    }

    /// Stops accepting connections and closes held streams.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Http2Server {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve_http2_connection(
    stream: TcpStream,
    requests: Arc<Mutex<Vec<RecordedHttp2Request>>>,
    scripts: Arc<Mutex<VecDeque<Http2Response>>>,
    mut stopped: watch::Receiver<bool>,
) {
    let Ok(mut connection) = h2::server::handshake(stream).await else {
        return;
    };
    loop {
        tokio::select! {
            _ = stopped.changed() => break,
            accepted = connection.accept() => {
                let Some(accepted) = accepted else { break };
                let Ok((request, respond)) = accepted else { continue };
                let requests = Arc::clone(&requests);
                let scripts = Arc::clone(&scripts);
                let stopped = stopped.clone();
                tokio::spawn(async move {
                    serve_http2_stream(request, respond, requests, scripts, stopped).await;
                });
            }
        }
    }
}

async fn serve_http2_stream(
    request: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    requests: Arc<Mutex<Vec<RecordedHttp2Request>>>,
    scripts: Arc<Mutex<VecDeque<Http2Response>>>,
    mut stopped: watch::Receiver<bool>,
) {
    let recorded = RecordedHttp2Request {
        method: request.method().to_string(),
        path: request.uri().path().to_owned(),
        headers: request
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        data: Vec::new(),
    };
    let index = {
        let mut records = requests.lock().expect("stream records");
        records.push(recorded);
        records.len() - 1
    };
    let response = scripts
        .lock()
        .expect("response scripts")
        .pop_front()
        .unwrap_or_else(|| Http2Response::new(200));
    if response.refused {
        respond.send_reset(h2::Reason::REFUSED_STREAM);
        return;
    }
    if let Some(mut gate) = response.header_gate {
        loop {
            if *gate.borrow() {
                break;
            }
            tokio::select! {
                _ = stopped.changed() => return,
                changed = gate.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    } else if !response.header_delay.is_zero() {
        tokio::time::sleep(response.header_delay).await;
    }
    let mut builder = http::Response::builder().status(response.status);
    for (name, value) in response.headers {
        builder = builder.header(name, value);
    }
    let end_stream = response.data.is_empty() && !response.hold_open;
    let Ok(head) = builder.body(()) else {
        return;
    };
    let Ok(mut send) = respond.send_response(head, end_stream) else {
        return;
    };
    let data_len = response.data.len();
    let end_after_request =
        !response.hold_open && !response.data.is_empty() && (200..300).contains(&response.status);
    for (position, data) in response.data.into_iter().enumerate() {
        if !response.data_delay.is_zero() {
            tokio::time::sleep(response.data_delay).await;
        }
        let end = !end_after_request && !response.hold_open && position + 1 == data_len;
        if send.send_data(Bytes::from(data), end).is_err() {
            return;
        }
    }
    let mut body = request.into_body();
    loop {
        tokio::select! {
            _ = stopped.changed() => break,
            chunk = body.data() => {
                match chunk {
                    Some(Ok(chunk)) => {
                        requests.lock().expect("stream records")[index]
                            .data
                            .push(chunk.to_vec());
                        let _ = body.flow_control().release_capacity(chunk.len());
                        if end_after_request {
                            let _ = send.send_data(Bytes::new(), true);
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
    if response.hold_open {
        let _ = stopped.changed().await;
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::{Http1Response, Http1Server, Http2Response, Http2Server};

    #[tokio::test]
    async fn http1_records_a_request_and_returns_the_scripted_body() {
        let server = Http1Server::start(vec![
            Http1Response::new(201)
                .header("x-test-reply", "yes")
                .body(b"pong".to_vec()),
        ])
        .await;
        let mut stream = TcpStream::connect(server.base_url().trim_start_matches("http://"))
            .await
            .expect("connect");
        stream
            .write_all(
                b"POST /v1/test HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-length: 4\r\nx-test-request: seen\r\n\r\nping",
            )
            .await
            .expect("request");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.expect("reply");
        let text = String::from_utf8(reply).expect("text reply");
        assert!(text.starts_with("HTTP/1.1 201"), "{text}");
        assert!(text.contains("x-test-reply: yes\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\npong"), "{text}");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method(), "POST");
        assert_eq!(requests[0].path(), "/v1/test");
        assert_eq!(requests[0].header("x-test-request"), Some("seen"));
        assert_eq!(requests[0].body(), b"ping");
    }

    #[tokio::test]
    async fn http2_records_all_client_data_after_a_success_response() {
        let server =
            Http2Server::start(vec![Http2Response::new(200).data(b"reply".to_vec())]).await;
        let tcp = TcpStream::connect(server.base_url().trim_start_matches("http://"))
            .await
            .expect("connect");
        let (mut client, connection) = h2::client::handshake(tcp).await.expect("handshake");
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("{}/v1/bidi", server.base_url()))
            .body(())
            .expect("request");
        let (response, mut send) = client.send_request(request, false).expect("send request");
        send.send_data(Bytes::from_static(b"first"), false)
            .expect("first data");
        send.send_data(Bytes::from_static(b"second"), true)
            .expect("second data");
        let response = response.await.expect("response");
        assert_eq!(response.status(), 200);
        let mut body = response.into_body();
        while body.data().await.transpose().expect("body").is_some() {}

        assert_eq!(
            server.requests()[0].data(),
            [b"first".to_vec(), b"second".to_vec()]
        );
    }

    #[tokio::test]
    async fn http2_records_bidirectional_data() {
        let server =
            Http2Server::start(vec![Http2Response::new(200).data(b"reply".to_vec())]).await;
        let tcp = TcpStream::connect(server.base_url().trim_start_matches("http://"))
            .await
            .expect("connect");
        let (mut client, connection) = h2::client::handshake(tcp).await.expect("handshake");
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("{}/v1/bidi", server.base_url()))
            .header("x-test-request", "seen")
            .body(())
            .expect("request");
        let (response, mut send) = client.send_request(request, false).expect("send request");
        send.send_data(Bytes::from_static(b"ping"), true)
            .expect("send data");
        let response = response.await.expect("response");
        assert_eq!(response.status(), 200);
        let mut body = response.into_body();
        let received = body.data().await.expect("data").expect("data");
        assert_eq!(received, Bytes::from_static(b"reply"));

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method(), "POST");
        assert_eq!(requests[0].path(), "/v1/bidi");
        assert_eq!(requests[0].header("x-test-request"), Some("seen"));
        assert_eq!(requests[0].data(), [b"ping".to_vec()]);
    }
}
