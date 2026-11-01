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
