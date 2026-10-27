//! One minimal request on a provider's configured wire.

use std::time::Duration;

use hofund_plugin_api::{AuthChoice, PluginError, ProbeQuery, ProbeReport};
use serde_json::{Map, Value};

use crate::models::{ModelClient, Page, both_auth_refused, probe_error_message};
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
            body[if self.is_azure() || self.is_bedrock() {
                "max_completion_tokens"
            } else {
                "max_tokens"
            }] = Value::from(16);
        }
        merge_extra(&mut body, query)?;
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| PluginError::decode("could not encode probe body"))?;
        let headers = if self.is_azure() {
            self.azure_headers()?
        } else {
            self.bearer_headers()?
        };
        let base = if self.is_mantle() {
            crate::mantle::mantle_base(&self.endpoint.base_url, model, false)
        } else {
            self.endpoint.base_url.clone()
        };
        let mut url = format!("{}/{}", base.trim_end_matches('/'), path);
        let mut page = self
            .post_probe(&url, headers.clone(), bytes.clone(), stream)
            .await?;
        if self.is_mantle()
            && !model.is_empty()
            && crate::mantle::wrong_route(Some(page.status), &probe_error(&page))
        {
            let base = crate::mantle::mantle_base(&self.endpoint.base_url, model, true);
            url = format!("{}/{}", base.trim_end_matches('/'), path);
            page = self.post_probe(&url, headers, bytes, stream).await?;
        }
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
        let initial = if self.is_azure() || self.is_bedrock() || self.is_official_anthropic() {
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

    pub async fn vertex_probe(
        &self,
        query: &ProbeQuery,
        token: &str,
    ) -> Result<ProbeReport, PluginError> {
        tokio::time::timeout(PROBE_WAIT, self.vertex_probe_inner(query, token))
            .await
            .map_err(|_| PluginError::timeout("provider probe exceeded 20 seconds"))?
    }

    async fn vertex_probe_inner(
        &self,
        query: &ProbeQuery,
        token: &str,
    ) -> Result<ProbeReport, PluginError> {
        let model = crate::generative::escape_model(query.model())
            .map_err(|error| PluginError::decode(error.message))?;
        let url = format!(
            "{}/publishers/google/models/{model}:generateContent",
            self.endpoint.base_url.trim_end_matches('/')
        );
        let mut generation = Map::new();
        generation.insert("maxOutputTokens".to_owned(), Value::from(16));
        let lower = query.model().to_ascii_lowercase();
        if lower.starts_with("gemini-2.5-pro") {
            generation.insert(
                "thinkingConfig".to_owned(),
                serde_json::json!({"thinkingBudget": 128}),
            );
        } else if lower.starts_with("gemini-2.5") {
            generation.insert(
                "thinkingConfig".to_owned(),
                serde_json::json!({"thinkingBudget": 0}),
            );
        } else if vertex_generation(&lower) >= 3 {
            generation.insert(
                "thinkingConfig".to_owned(),
                serde_json::json!({"thinkingLevel": "low"}),
            );
        }
        let mut body = serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": generation,
        });
        merge_extra(&mut body, query)?;
        let mut headers = self.user_headers(reqwest::header::HeaderMap::new())?;
        let mut bearer = reqwest::header::HeaderValue::try_from(format!("Bearer {token}"))
            .map_err(|_| PluginError::config("access token is not valid header text"))?;
        bearer.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, bearer);
        self.post_and_report(&url, headers, body).await
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

fn merge_extra(body: &mut Value, query: &ProbeQuery) -> Result<(), PluginError> {
    let Some(extra) = query.extra_body() else {
        return Ok(());
    };
    let extra: Value = serde_json::from_slice(extra)
        .map_err(|_| PluginError::decode("probe extra body is not JSON"))?;
    let Some(extra) = extra.as_object() else {
        return Err(PluginError::decode("probe extra body is not a JSON object"));
    };
    let Some(object) = body.as_object_mut() else {
        return Err(PluginError::decode("probe body is not a JSON object"));
    };
    for (key, value) in extra {
        object.insert(key.clone(), value.clone());
    }
    Ok(())
}

// reference: internal/provider/test.go:580-631
fn stream_report(body: &[u8]) -> Result<ProbeReport, PluginError> {
    for line in String::from_utf8_lossy(body).lines() {
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "error" || event.get("error").is_some_and(|error| !error.is_null()) {
            return Err(PluginError::decode(format!(
                "error in the stream: {}",
                probe_error_message(data.as_bytes())
            )));
        }
        if kind == "response.failed" {
            let detail = event.pointer("/response/error").map_or_else(
                || "response failed".to_owned(),
                |error| probe_error_message(error.to_string().as_bytes()),
            );
            return Err(PluginError::decode(detail));
        }
        if matches!(
            kind,
            "content_block_start" | "content_block_delta" | "response.output_item.added"
        ) || (kind.starts_with("response.") && kind.ends_with(".delta"))
        {
            return Ok(ProbeReport::new(data));
        }
        if event
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("delta")
                        .and_then(Value::as_object)
                        .is_some_and(|delta| {
                            ["content", "reasoning", "reasoning_content", "tool_calls"]
                                .iter()
                                .any(|key| {
                                    delta.get(*key).is_some_and(|value| {
                                        !value.is_null() && value.as_str() != Some("")
                                    })
                                })
                        })
                })
            })
            || event
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts.iter().any(|part| {
                        ["text", "thought", "functionCall"]
                            .iter()
                            .any(|key| part.get(*key).is_some_and(|value| !value.is_null()))
                    })
                })
        {
            return Ok(ProbeReport::new(data));
        }
    }
    Ok(ProbeReport::new(""))
}

fn probe_error(page: &Page) -> String {
    String::from_utf8_lossy(&page.body).into_owned()
}

fn vertex_generation(model: &str) -> u32 {
    let Some(rest) = model.strip_prefix("gemini-") else {
        return 0;
    };
    rest.split(['-', '.'])
        .next()
        .and_then(|generation| generation.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::vertex_generation;

    #[test]
    fn vertex_generation_requires_a_number() {
        assert_eq!(vertex_generation("gemini-3-flash"), 3);
        assert_eq!(vertex_generation("gemini-2.5-pro"), 2);
        assert_eq!(vertex_generation("gemini-flash-latest"), 0);
    }
}
