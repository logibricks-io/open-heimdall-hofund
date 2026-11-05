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
}
