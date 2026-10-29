//! One minimal request on a provider's configured wire.

use std::time::Duration;

use hofund_plugin_api::{AuthChoice, PluginError, ProbeQuery, ProbeReport};
use serde_json::Value;

use hofund_plugin_sdk::probe::{merge_extra, stream_report};

use crate::models::{ModelClient, Page, both_auth_refused};
use crate::openai_compat::OpenAiProtocol;

pub(crate) const PROBE_WAIT: Duration = Duration::from_secs(20);
const STREAM_CAP: usize = 1024 * 1024;

impl ModelClient {
    pub async fn openai_probe(
        &self,
        protocol: OpenAiProtocol,
        query: &ProbeQuery,
    ) -> Result<ProbeReport, PluginError> {
        tokio::time::timeout(PROBE_WAIT, self.openai_probe_inner(protocol, query))
            .await
            .map_err(|_| PluginError::timeout("provider probe exceeded 20 seconds"))?
    }

    async fn openai_probe_inner(
        &self,
        protocol: OpenAiProtocol,
        query: &ProbeQuery,
    ) -> Result<ProbeReport, PluginError> {
        let model = query.model();
        let path = match protocol {
            OpenAiProtocol::Chat => "chat/completions",
            OpenAiProtocol::Responses => "responses",
        };
        let mut body = match protocol {
            OpenAiProtocol::Chat => serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}],
            }),
            OpenAiProtocol::Responses => serde_json::json!({
                "model": model,
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
                "max_output_tokens": 16,
            }),
        };
        if protocol == OpenAiProtocol::Chat {
            body["max_tokens"] = Value::from(16);
        }
        merge_extra(&mut body, query)?;
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| PluginError::decode("could not encode probe body"))?;
        let headers = self.bearer_headers()?;
        let url = format!("{}/{}", self.endpoint.base_url.trim_end_matches('/'), path);
        let page = self.post_probe(&url, headers, bytes, stream).await?;
        self.probe_report(&url, page, stream)
    }

    pub async fn anthropic_probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        tokio::time::timeout(PROBE_WAIT, self.anthropic_probe_inner(query))
            .await
            .map_err(|_| PluginError::timeout("provider probe exceeded 20 seconds"))?
    }

    async fn anthropic_probe_inner(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let url = format!(
            "{}/v1/messages",
            self.endpoint.base_url.trim_end_matches('/')
        );
        let mut body = serde_json::json!({
            "model": query.model(),
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        });
        merge_extra(&mut body, query)?;
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| PluginError::decode("could not encode probe body"))?;
        let initial = if self.is_azure_host() || self.is_official_anthropic() {
            AuthChoice::XApiKey
        } else {
            query.auth_choice().unwrap_or(AuthChoice::Both)
        };
        let learning = self.can_learn_auth();
        let mut learned = None;
        let mut page = self
            .post_probe(
                &url,
                self.auth_headers(initial, true, false)?,
                bytes.clone(),
                stream,
            )
            .await?;
        if learning && initial == AuthChoice::Both && both_auth_refused(page.status, &page.body) {
            for choice in [AuthChoice::XApiKey, AuthChoice::Bearer] {
                page = self
                    .post_probe(
                        &url,
                        self.auth_headers(choice, true, false)?,
                        bytes.clone(),
                        stream,
                    )
                    .await?;
                if page.status != 401 && page.status != 403 {
                    learned = Some(choice);
                    break;
                }
            }
        }
        let mut report = self.probe_report(&url, page, stream)?;
        if let Some(choice) = learned {
            report = report.with_auth_choice(choice);
        }
        Ok(report)
    }

    pub async fn generative_probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        tokio::time::timeout(PROBE_WAIT, self.generative_probe_inner(query))
            .await
            .map_err(|_| PluginError::timeout("provider probe exceeded 20 seconds"))?
    }

    async fn generative_probe_inner(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let model = crate::generative::escape_model(query.model())
            .map_err(|error| PluginError::decode(error.message))?;
        let url = format!(
            "{}/models/{model}:generateContent",
            self.endpoint.base_url.trim_end_matches('/')
        );
        let mut body = serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"maxOutputTokens": 16},
        });
        merge_extra(&mut body, query)?;
        self.post_and_report(
            &url,
            self.auth_headers(AuthChoice::Both, false, true)?,
            body,
        )
        .await
    }

    pub async fn post_and_report(
        &self,
        url: &str,
        headers: reqwest::header::HeaderMap,
        body: Value,
    ) -> Result<ProbeReport, PluginError> {
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| PluginError::decode("could not encode probe body"))?;
        let page = self.post_probe(url, headers, bytes, stream).await?;
        self.probe_report(url, page, stream)
    }

    async fn post_probe(
        &self,
        url: &str,
        headers: reqwest::header::HeaderMap,
        body: Vec<u8>,
        stream: bool,
    ) -> Result<Page, PluginError> {
        let response = self
            .client
            .post(url)
            .headers(headers)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    PluginError::timeout("provider probe exceeded its deadline")
                } else if error.is_connect() {
                    PluginError::connect("could not connect to the upstream")
                } else {
                    PluginError::transport("upstream transport failed")
                }
            })?;
        let status = response.status().as_u16();
        let cap = if (200..300).contains(&status) && stream {
            STREAM_CAP
        } else {
            4096
        };
        crate::models::read_page(response, cap, false).await
    }

    pub(crate) fn probe_report(
        &self,
        url: &str,
        page: Page,
        stream: bool,
    ) -> Result<ProbeReport, PluginError> {
        let body = self.page_bytes(url, page, true)?;
        if stream {
            return stream_report(&body).map_err(|error| self.redact_error(error));
        }
        Ok(ProbeReport::new(String::from_utf8_lossy(&body)))
    }
}
