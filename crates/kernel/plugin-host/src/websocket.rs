//! Outbound WebSocket dialing on the host. Plugins do not open sockets.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hofund_plugin_api::kernel::WebSocketIo;
use hofund_plugin_api::{PluginError, WebSocketMessage, WebSocketSession};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Message, http};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

/// Largest accepted frame or message. A peer that exceeds it is a transport error.
const MAX_FRAME: usize = 8 * 1024 * 1024;
/// How long a handshake may take before the caller sees a timeout.
const HANDSHAKE: Duration = Duration::from_secs(15);

/// Dials `uri` and returns an open session.
///
/// `uri` may be `ws`, `wss`, `http`, or `https`. `http` and `https` are
/// upgraded. Header values marked sensitive stay sensitive on the handshake.
///
/// # Errors
///
/// Returns [`PluginError`] when the URI is not a handshake target, the dial
/// fails, or the peer refuses the handshake. A refusal keeps its HTTP status.
pub(crate) async fn connect(
    uri: &str,
    headers: Vec<(&str, &str, bool)>,
) -> Result<WebSocketSession, PluginError> {
    let uri = websocket_uri(uri)?;
    let mut request = uri
        .into_client_request()
        .map_err(|_| PluginError::config("the websocket URI is not a handshake target"))?;
    for (name, value, sensitive) in headers {
        let header_name = http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| PluginError::config("a websocket header name is not valid"))?;
        let mut header_value = http::HeaderValue::from_str(value)
            .map_err(|_| PluginError::config("a websocket header value is not valid"))?;
        header_value.set_sensitive(sensitive);
        request.headers_mut().insert(header_name, header_value);
    }
    let mut config = WebSocketConfig::default();
    config.read_buffer_size = 8 * 1024;
    config.write_buffer_size = 8 * 1024;
    config.max_write_buffer_size = MAX_FRAME;
    config.max_message_size = Some(MAX_FRAME);
    config.max_frame_size = Some(MAX_FRAME);
    let dial = connect_async_with_config(request, Some(config), true);
    let (stream, _) = match tokio::time::timeout(HANDSHAKE, dial).await {
        Ok(Ok(opened)) => opened,
        Ok(Err(error)) => return Err(map_handshake(error)),
        Err(_) => {
            return Err(PluginError::timeout("websocket handshake timed out"));
        }
    };
    Ok(WebSocketSession::kernel_new(Box::new(LiveSocket {
        stream,
    })))
}

/// `uri` with the origin replaced by loopback `origin`, keeping the path.
///
/// `origin` must be an `http` or `https` URL on `127.0.0.1`, `localhost`, or `[::1]`.
///
/// # Errors
///
/// Returns [`PluginError`] when `origin` is not that kind of URL.
pub(crate) fn loopback_uri(uri: &str, origin: &str) -> Result<String, PluginError> {
    if !loopback_origin(origin) {
        return Err(PluginError::unsupported(
            "this HTTP backend has no websocket client",
        ));
    }
    Ok(format!(
        "{}{}",
        origin.trim_end_matches('/'),
        path_and_query(uri)
    ))
}

fn path_and_query(uri: &str) -> &str {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    rest.find('/').map_or("/", |index| &rest[index..])
}

/// Whether `origin` is an HTTP URL on a loopback host.
#[must_use]
pub(crate) fn loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(name, _)| name);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn websocket_uri(uri: &str) -> Result<String, PluginError> {
    if let Some(rest) = uri.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = uri.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else if uri.starts_with("ws://") || uri.starts_with("wss://") {
        Ok(uri.to_owned())
    } else {
        Err(PluginError::config(
            "the websocket URI is not a handshake target",
        ))
    }
}

fn map_handshake(error: tokio_tungstenite::tungstenite::Error) -> PluginError {
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => PluginError::status_error(
            response.status().as_u16(),
            "the websocket handshake was refused",
        ),
        tokio_tungstenite::tungstenite::Error::Io(error) => match error.kind() {
            std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted => {
                PluginError::connect("websocket connect failed")
            }
            _ => PluginError::transport("websocket connection failed"),
        },
        tokio_tungstenite::tungstenite::Error::Url(_) => {
            PluginError::config("the websocket URI is not a handshake target")
        }
        _ => PluginError::transport("websocket handshake failed"),
    }
}

struct LiveSocket {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

#[async_trait]
impl WebSocketIo for LiveSocket {
    async fn send_text(&mut self, text: String) -> Result<(), PluginError> {
        self.stream
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| PluginError::transport("websocket write failed"))
    }

    async fn recv(&mut self) -> Result<Option<WebSocketMessage>, PluginError> {
        loop {
            match self.stream.next().await {
                None => return Ok(None),
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(WebSocketMessage::Text(text.to_string())));
                }
                Some(Ok(Message::Binary(bytes))) => {
                    return Ok(Some(WebSocketMessage::Binary(bytes.to_vec())));
                }
                Some(Ok(Message::Ping(payload))) => {
                    self.stream
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|_| PluginError::transport("websocket write failed"))?;
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Err(_)) => {
                    return Err(PluginError::transport("websocket read failed"));
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), PluginError> {
        self.stream
            .close(None)
            .await
            .map_err(|_| PluginError::transport("websocket close failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::{loopback_origin, loopback_uri, websocket_uri};

    #[test]
    fn http_origins_become_websocket_uris() {
        assert_eq!(
            websocket_uri("https://example.test/responses").expect("wss"),
            "wss://example.test/responses"
        );
        assert_eq!(
            websocket_uri("http://127.0.0.1:9/responses").expect("ws"),
            "ws://127.0.0.1:9/responses"
        );
        assert!(websocket_uri("ftp://example.test").is_err());
    }

    #[test]
    fn loopback_rewrite_keeps_the_path() {
        assert!(loopback_origin("http://127.0.0.1:9"));
        assert!(!loopback_origin("https://example.test"));
        assert_eq!(
            loopback_uri("wss://api.example.test/responses?x=1", "http://127.0.0.1:9")
                .expect("rewrite"),
            "http://127.0.0.1:9/responses?x=1"
        );
        assert!(loopback_uri("wss://api.example.test/responses", "https://example.test").is_err());
    }
}
