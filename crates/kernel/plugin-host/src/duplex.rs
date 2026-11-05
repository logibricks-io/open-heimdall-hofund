//! Duplex HTTP on the process-wide pool.

use async_trait::async_trait;
use bytes::Bytes;
use hofund_plugin_api::kernel::DuplexWriterBackend;
use hofund_plugin_api::{DuplexRequest, DuplexSession, DuplexWriter, HttpVersionPin, PluginError};
use tokio::sync::mpsc;
use url::Url;

use crate::pool::{self, DuplexDial, HttpLimits, PooledHttp};

pub(crate) const HEADERS_TIMED_OUT: &str = "duplex headers timed out";
pub(crate) const HTTP2_UNAVAILABLE: &str = "http2 unavailable";
pub(crate) const STREAM_REFUSED: &str = "stream refused";

const DUPLEX_READ: std::time::Duration = std::time::Duration::from_secs(5 * 60);

struct ChannelWriter {
    sender: Option<mpsc::Sender<Bytes>>,
}

#[async_trait]
impl DuplexWriterBackend for ChannelWriter {
    async fn write(&mut self, bytes: Bytes) -> Result<(), PluginError> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(PluginError::transport("duplex request body is finished"));
        };
        sender
            .send(bytes)
            .await
            .map_err(|_| PluginError::transport("duplex request body is closed"))
    }

    async fn finish(&mut self) -> Result<(), PluginError> {
        self.sender.take();
        Ok(())
    }
}

pub(crate) async fn open_duplex(
    http: &PooledHttp,
    request: DuplexRequest,
) -> Result<DuplexSession, PluginError> {
    let dial = dial_for(request.kernel_uri(), request.kernel_version_pin())?;
    validate_duplex_uri(request.kernel_uri())?;
    let client = pool::duplex_client_for(&http.pool_key().profile, dial)?;
    let method = reqwest::Method::from_bytes(request.kernel_method().as_bytes())
        .map_err(|error| PluginError::internal(error.to_string()))?;
    let (body_tx, body_rx) = mpsc::channel::<Bytes>(16);
    let body_stream = futures_util::stream::unfold(body_rx, |mut receiver| async move {
        receiver.recv().await.map(|bytes| {
            (
                Ok::<Bytes, Box<dyn std::error::Error + Send + Sync>>(bytes),
                receiver,
            )
        })
    });
    let mut outbound = client
        .request(method, request.kernel_uri())
        .body(reqwest::Body::wrap_stream(body_stream));
    for (name, value, sensitive) in request.kernel_header_triples() {
        let mut header_value = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_| PluginError::internal(format!("invalid header {name}")))?;
        header_value.set_sensitive(sensitive);
        outbound = outbound.header(name, header_value);
    }
    let header_wait = request.kernel_header_wait();
    let send = outbound.send();
    let response = match tokio::time::timeout(header_wait, send).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(map_duplex_open_error(error)),
        Err(_) => {
            return Err(PluginError::timeout(format!(
                "{HEADERS_TIMED_OUT}: no response headers within {header_wait:?}"
            )));
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let reader = pool::live_body(response);
    let writer = DuplexWriter::kernel_new(Box::new(ChannelWriter {
        sender: Some(body_tx),
    }));
    Ok(DuplexSession::kernel_new(status, headers, reader, writer))
}

fn dial_for(uri: &str, pin: HttpVersionPin) -> Result<DuplexDial, PluginError> {
    let parsed = Url::parse(uri).map_err(|error| PluginError::config(error.to_string()))?;
    match (parsed.scheme(), pin) {
        ("https", HttpVersionPin::Http2) => Ok(DuplexDial::Alpn),
        ("http", HttpVersionPin::Http2) => Ok(DuplexDial::PriorKnowledge),
        ("https", HttpVersionPin::Http1) | ("http", HttpVersionPin::Http1) => Err(
            PluginError::config("duplex calls require HttpVersionPin::Http2"),
        ),
        (scheme, _) => Err(PluginError::config(format!(
            "duplex URI scheme {scheme} is not supported"
        ))),
    }
}

fn validate_duplex_uri(uri: &str) -> Result<(), PluginError> {
    let parsed = Url::parse(uri).map_err(|error| PluginError::config(error.to_string()))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = parsed.host_str().unwrap_or("");
            if host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") {
                Ok(())
            } else {
                Err(PluginError::config(
                    "cleartext duplex dials are limited to loopback hosts",
                ))
            }
        }
        scheme => Err(PluginError::config(format!(
            "duplex URI scheme {scheme} is not supported"
        ))),
    }
}

/// Maps a failed duplex open to the stable plugin-facing classes in the design doc.
#[must_use]
pub(crate) fn map_duplex_open_error(error: reqwest::Error) -> PluginError {
    classify_duplex_failure(error.is_timeout(), error.is_connect(), &error.to_string())
}

pub(crate) fn classify_duplex_failure(
    is_timeout: bool,
    is_connect: bool,
    message: &str,
) -> PluginError {
    let lower = message.to_ascii_lowercase();
    if is_timeout {
        return PluginError::timeout(format!("{HEADERS_TIMED_OUT}: {message}"));
    }
    if is_connect {
        return PluginError::connect(message.to_owned());
    }
    if lower.contains("stream refused") || lower.contains("refused_stream") {
        return PluginError::transport(format!("{STREAM_REFUSED}: {message}"));
    }
    if lower.contains("stream error") && !lower.contains("refused") {
        return PluginError::transport(format!("{STREAM_REFUSED}: {message}"));
    }
    if lower.starts_with("err_http2")
        || lower.contains("h2 is not supported")
        || lower.contains("alpn")
        || lower.contains("nghttp2")
        || lower.contains("protocol error")
    {
        return PluginError::transport(format!("{HTTP2_UNAVAILABLE}: {message}"));
    }
    PluginError::transport(message.to_owned())
}

pub(crate) fn duplex_limits() -> HttpLimits {
    HttpLimits::new(HttpLimits::default().connect(), DUPLEX_READ).with_max_idle_per_host(0)
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginErrorKind;

    use super::{HEADERS_TIMED_OUT, HTTP2_UNAVAILABLE, STREAM_REFUSED, classify_duplex_failure};

    #[test]
    fn header_timeout_uses_the_duplex_prefix() {
        let mapped = classify_duplex_failure(true, false, "operation timed out");
        assert_eq!(mapped.kind(), PluginErrorKind::Timeout);
        assert!(
            mapped.message().starts_with(HEADERS_TIMED_OUT),
            "{}",
            mapped.message()
        );
    }

    #[test]
    fn alpn_failure_uses_http2_unavailable_prefix() {
        let error = classify_duplex_failure(false, false, "h2 is not supported by proxy");
        assert_eq!(error.kind(), PluginErrorKind::Transport);
        assert!(
            error.message().starts_with(HTTP2_UNAVAILABLE),
            "{}",
            error.message()
        );
    }

    #[test]
    fn stream_refused_uses_its_prefix() {
        let error = classify_duplex_failure(false, false, "REFUSED_STREAM before headers");
        assert_eq!(error.kind(), PluginErrorKind::Transport);
        assert!(
            error.message().starts_with(STREAM_REFUSED),
            "{}",
            error.message()
        );
    }

    #[test]
    fn inverted_prefix_branches_fail() {
        let error = classify_duplex_failure(false, false, "h2 is not supported");
        assert!(
            !error.message().starts_with(STREAM_REFUSED),
            "http2 unavailable must not look like stream refused"
        );
        let error = classify_duplex_failure(false, false, "REFUSED_STREAM");
        assert!(
            !error.message().starts_with(HTTP2_UNAVAILABLE),
            "stream refused must not look like http2 unavailable"
        );
    }
}
