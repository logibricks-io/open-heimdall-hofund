//! Blocking HTTP fetcher for catalog sync.

use std::io;

use reqwest::blocking::Client;
use reqwest::header::IF_NONE_MATCH;

use crate::sync::{CatalogFetcher, FetchResponse};

const USER_AGENT_VALUE: &str = "hofund";

/// Production fetcher using reqwest.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReqwestFetcher;

impl ReqwestFetcher {
    /// # Errors
    ///
    /// Always succeeds; the HTTP client is built per fetch so it is never dropped on a Tokio worker.
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }
}

fn client() -> io::Result<Client> {
    Client::builder()
        .http1_only()
        .user_agent(USER_AGENT_VALUE)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(io::Error::other)
}

impl CatalogFetcher for ReqwestFetcher {
    fn fetch(&self, url: &str, if_none_match: Option<&str>) -> Result<FetchResponse, io::Error> {
        let client = client()?;
        let mut request = client.get(url);
        if let Some(etag) = if_none_match {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().map_err(io::Error::other)?;
        let status = response.status().as_u16();
        if status == 304 {
            return Ok(FetchResponse::NotModified);
        }
        if status != 200 {
            return Ok(FetchResponse::Status(status));
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().map_err(io::Error::other)?.to_vec();
        Ok(FetchResponse::Ok { body, etag })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::client;

    #[test]
    fn catalog_client_sends_an_http11_request_line() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (request_tx, request_rx) = mpsc::sync_channel::<Vec<u8>>(1);
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            while let Ok(read) = stream.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = request_tx.send(request);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        });
        let client = client().expect("client");
        let _ = client
            .get(format!("http://127.0.0.1:{port}/catalog"))
            .send()
            .expect("send");
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("request bytes");
        let first_line = request
            .split(|byte| *byte == b'\n')
            .next()
            .expect("request line");
        assert!(
            first_line.ends_with(b" HTTP/1.1\r"),
            "request line was {:?}",
            String::from_utf8_lossy(first_line)
        );
    }

    #[test]
    fn catalog_client_sends_an_http11_request_line_over_https_when_alpn_offers_h2_first() {
        use std::sync::{Arc, Mutex};

        use rustls::ServerConfig;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::TlsAcceptor;

        const CERT_DER: &[u8] = include_bytes!("../testdata/localhost.der");
        const KEY_DER: &[u8] = include_bytes!("../testdata/localhost.key.der");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let port = rt.block_on(async {
            let cert = CertificateDer::from(CERT_DER.to_vec());
            let key = PrivateKeyDer::try_from(KEY_DER.to_vec()).expect("private key der");
            let mut server_config = ServerConfig::builder_with_protocol_versions(&[
                &rustls::version::TLS12,
                &rustls::version::TLS13,
            ])
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .expect("server config");
            server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let acceptor = TlsAcceptor::from(Arc::new(server_config));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let captured_server = Arc::clone(&captured);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let acceptor = acceptor.clone();
                    let captured_server = Arc::clone(&captured_server);
                    tokio::spawn(async move {
                        let Ok(mut tls) = acceptor.accept(stream).await else {
                            return;
                        };
                        let mut buf = vec![0_u8; 512];
                        let Ok(read) = tls.read(&mut buf).await else {
                            return;
                        };
                        captured_server
                            .lock()
                            .expect("capture")
                            .extend_from_slice(&buf[..read]);
                        let _ = tls
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            )
                            .await;
                    });
                }
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            port
        });
        let client = reqwest::blocking::Client::builder()
            .http1_only()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client");
        let _ = client
            .get(format!("https://127.0.0.1:{port}/catalog"))
            .send()
            .expect("send");
        let bytes = captured.lock().expect("capture").clone();
        assert!(
            !bytes.starts_with(b"PRI * HTTP/2.0"),
            "client must not send the HTTP/2 connection preface: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        let first_line = bytes
            .split(|byte| *byte == b'\n')
            .next()
            .expect("request line");
        assert!(
            first_line.ends_with(b" HTTP/1.1\r"),
            "request line was {:?}",
            String::from_utf8_lossy(first_line)
        );
    }
}
