//! Localhost TLS fixtures for http1_only guard tests.

use std::sync::{Arc, Mutex};

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;

const CERT_DER: &[u8] = include_bytes!("../testdata/localhost.der");
const KEY_DER: &[u8] = include_bytes!("../testdata/localhost.key.der");

pub(crate) fn localhost_server_config(alpn: Vec<Vec<u8>>) -> Arc<ServerConfig> {
    let cert = CertificateDer::from(CERT_DER.to_vec());
    let key = PrivateKeyDer::try_from(KEY_DER.to_vec()).expect("private key der");
    let mut config = ServerConfig::builder_with_protocol_versions(&[
        &rustls::version::TLS12,
        &rustls::version::TLS13,
    ])
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .expect("server config");
    config.alpn_protocols = alpn;
    Arc::new(config)
}

/// Binds a loopback TLS listener. The first application-data bytes of each
/// connection are appended to `captured`.
pub(crate) async fn spawn_tls_capture(alpn: Vec<Vec<u8>>, captured: Arc<Mutex<Vec<u8>>>) -> u16 {
    let acceptor = TlsAcceptor::from(localhost_server_config(alpn));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buf = vec![0_u8; 512];
                let Ok(read) = tls.read(&mut buf).await else {
                    return;
                };
                captured
                    .lock()
                    .expect("capture lock")
                    .extend_from_slice(&buf[..read]);
                let _ = tls
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    port
}
