//! The model gateway health probe behind `hofund healthcheck`.
//!
//! It talks to the model listener, not the control plane, so it needs no
//! token or discovery: GET the unauthenticated `/health` and require a 200
//! whose body names the hofund service.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use http::header;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::TcpStream;

use crate::{ClientError, connect_error, transport};

/// The largest model `/health` body the probe reads.
const MAX_HEALTH_BODY_BYTES: usize = 4096;

/// The address to probe for a model listener bound to `listen`: an
/// unspecified host becomes the loopback address of the same family.
#[must_use]
pub fn probe_address(listen: SocketAddr) -> SocketAddr {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, listen.port())
}

/// Probes the model gateway at `listen` (rewritten by [`probe_address`])
/// within `timeout`.
///
/// # Errors
///
/// Returns [`ClientError::ConnectionUnavailable`] when nothing listens,
/// [`ClientError::Transport`] on a timeout, a non-200 status, an oversized
/// body, or a body that does not identify hofund.
pub async fn probe_model_health(listen: SocketAddr, timeout: Duration) -> Result<(), ClientError> {
    let addr = probe_address(listen);
    match tokio::time::timeout(timeout, probe(addr)).await {
        Ok(result) => result,
        Err(_) => Err(ClientError::Transport(format!(
            "model gateway at {addr} did not answer /health within {} ms",
            timeout.as_millis()
        ))),
    }
}

async fn probe(addr: SocketAddr) -> Result<(), ClientError> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|error| connect_error(&format!("no model gateway at {addr}"), error))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| transport("model gateway HTTP handshake failed", error))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri("/health")
        .header(header::HOST, addr.to_string())
        .body(Full::new(std::io::Cursor::new(Vec::<u8>::new())))
        .map_err(|_| ClientError::Transport("cannot construct health request".to_owned()))?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| transport("model gateway /health request failed", error))?;
    let status = response.status();
    if status != http::StatusCode::OK {
        return Err(ClientError::Transport(format!(
            "model gateway at {addr} answered /health with {status}"
        )));
    }
    let body = Limited::new(response.into_body(), MAX_HEALTH_BODY_BYTES)
        .collect()
        .await
        .map_err(|error| transport("model gateway /health body failed", error))?
        .to_bytes();
    if is_hofund(&body) {
        Ok(())
    } else {
        Err(ClientError::Transport(format!(
            "the service at {addr} is not a hofund model gateway"
        )))
    }
}

fn is_hofund(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Health {
        service: Option<String>,
    }
    serde_json::from_slice::<Health>(body)
        .is_ok_and(|health| health.service.as_deref() == Some("hofund"))
}

#[cfg(test)]
mod tests {
    use super::{is_hofund, probe_address};

    #[test]
    fn probe_address_rewrites_only_unspecified_hosts() {
        for (listen, probe) in [
            ("0.0.0.0:3425", "127.0.0.1:3425"),
            ("[::]:3425", "[::1]:3425"),
            ("127.0.0.1:9", "127.0.0.1:9"),
            ("192.0.2.7:9", "192.0.2.7:9"),
            ("[::1]:9", "[::1]:9"),
        ] {
            assert_eq!(
                probe_address(listen.parse().expect("listen")),
                probe.parse().expect("probe"),
                "{listen}"
            );
        }
    }

    #[test]
    fn only_a_hofund_body_identifies_the_gateway() {
        assert!(is_hofund(br#"{"status":"ok","service":"hofund"}"#));
        assert!(!is_hofund(br#"{"status":"ok"}"#));
        assert!(!is_hofund(br#"{"status":"ok","service":"other"}"#));
        assert!(!is_hofund(b"ok"));
    }
}
