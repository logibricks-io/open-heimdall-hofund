//! OpenAI Chat upstream with WorkBuddy client headers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, RawAnswer, RawBody, Request, Role, UpstreamProvider,
};
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::wire::{self, Target};

use crate::Endpoints;
use crate::creds;
use crate::refresh;
use crate::site::Site;
use crate::tools;

const SMALL_BUDGET: u64 = 1024;

fn names_effort(request: &Request) -> bool {
    request.params.effort.is_some()
        || request.extensions.get("reasoning_effort").is_some()
        || request
            .extensions
            .get("reasoning")
            .and_then(Value::as_object)
            .is_some_and(|reasoning| reasoning.contains_key("effort"))
}

fn apply_default_effort(body: &mut Map<String, Value>, default: Option<&str>) {
    let names_effort = body.contains_key("reasoning_effort")
        || body
            .get("reasoning")
            .and_then(Value::as_object)
            .is_some_and(|reasoning| reasoning.contains_key("effort"));
    if names_effort {
        return;
    }
    let limit = body
        .get("max_tokens")
        .filter(|limit| !limit.is_null())
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(Value::as_u64);
    if limit.is_some_and(|limit| limit > 0 && limit <= SMALL_BUDGET) {
        return;
    }
    if let Some(default) = default {
        body.insert(
            "reasoning_effort".to_owned(),
            Value::String(default.to_owned()),
        );
    }
}

pub(crate) struct WorkBuddyUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    after_tools: Arc<Mutex<HashSet<String>>>,
    effort_defaults: Arc<crate::models::EffortDefaults>,
}

impl WorkBuddyUpstream {
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        after_tools: Arc<Mutex<HashSet<String>>>,
        effort_defaults: Arc<crate::models::EffortDefaults>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            after_tools,
            effort_defaults,
        }
    }

    async fn stored(&self, force: bool) -> Result<creds::Stored, PluginError> {
        if force && !self.stored_is_desktop().await? {
            refresh::force(&self.host, Arc::clone(&self.endpoints), &self.account).await?;
        }
        refresh::ensure_fresh(&self.host, Arc::clone(&self.endpoints), &self.account).await
    }

    async fn stored_is_desktop(&self) -> Result<bool, PluginError> {
        Ok(creds::Stored::load(&self.host, &self.account)
            .await?
            .credential
            .desktop)
    }

    fn model_key(site: Site, model: &str) -> String {
        format!("{}:{model}", site.plugin_id())
    }

    fn learned_after_tools(&self, site: Site, model: &str) -> bool {
        self.after_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&Self::model_key(site, model))
    }

    fn learn_after_tools(&self, site: Site, model: &str) {
        self.after_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(Self::model_key(site, model));
    }

    async fn send(
        &self,
        stored: &creds::Stored,
        request: Request,
        inbound: ProtocolId,
        force_after_tools: bool,
    ) -> Result<Completion, PluginError> {
        let base = self.endpoints.base_for(stored.credential.site);
        let url = format!("{base}/v2");
        let default_effort = if names_effort(&request) {
            None
        } else {
            self.effort_defaults
                .get(
                    &self.host,
                    Arc::clone(&self.endpoints),
                    stored,
                    &self.account,
                    &request.model,
                )
                .await
        };
        let mut header_rows: Vec<(String, String)> =
            creds::signed_headers(&stored.credential, base)
                .into_iter()
                .filter(|(name, _)| name != "Authorization")
                .collect();
        header_rows.extend(creds::chat_headers());
        let after_tools =
            force_after_tools || self.learned_after_tools(stored.credential.site, &request.model);
        let names = Arc::new(Mutex::new(HashMap::new()));
        let fitted = Arc::clone(&names);
        let restore = Arc::clone(&names);
        let target = Target::new(ProtocolId::OPENAI_CHAT, url)
            .credential(CredentialStyle::Bearer)
            .headers(header_rows)
            .request_body_gzip(true)
            .rewrite(Arc::new(move |body| {
                apply_default_effort(body, default_effort.as_deref());
                let fitted_names = tools::prepare(body, after_tools);
                *fitted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = fitted_names;
                Vec::new()
            }))
            .unwrap(Arc::new(move |mut value| {
                let names = restore
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                tools::restore(&mut value, &names);
                value
            }));
        wire::execute(
            self.host.http(),
            &target,
            Some(&stored.credential.access),
            request,
            inbound,
        )
        .await
        .map_err(crate::errors::classify)
    }

    async fn send_forward(
        &self,
        stored: &creds::Stored,
        path: &str,
        body: RawBody,
    ) -> Result<RawAnswer, PluginError> {
        let body = self.image_body(stored, path, body).await?;
        let base = self.endpoints.base_for(stored.credential.site);
        let mut header_rows: Vec<(String, String)> =
            creds::signed_headers(&stored.credential, base)
                .into_iter()
                .filter(|(name, _)| name != "Authorization")
                .collect();
        header_rows.extend(creds::chat_headers());
        let target = Target::new(ProtocolId::OPENAI_CHAT, format!("{base}/v2"))
            .credential(CredentialStyle::Bearer)
            .headers(header_rows);
        let answer = wire::forward(
            self.host.http(),
            &target,
            Some(&stored.credential.access),
            path,
            body,
        )
        .await
        .map_err(crate::errors::classify)?;
        image_answer(answer)
    }

    async fn image_body(
        &self,
        stored: &creds::Stored,
        path: &str,
        body: RawBody,
    ) -> Result<RawBody, PluginError> {
        let mut object = match hofund_plugin_sdk::json::from_slice::<Value>(body.bytes()) {
            Ok(Value::Object(object)) => object,
            _ => {
                return Err(PluginError::status_error(
                    400,
                    "the image body must be a JSON object",
                ));
            }
        };
        if path == "/images/edits" {
            let model = object
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let editor =
                crate::models::editor(&self.host, Arc::clone(&self.endpoints), stored, &model)
                    .await?;
            object.insert("model".to_owned(), Value::String(editor));
        }
        shape_image(&mut object, path == "/images/edits");
        Ok(RawBody::new(
            "application/json",
            Value::Object(object).to_string(),
        ))
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for WorkBuddyUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let stored = self.stored(false).await?;
        match self.send(&stored, request.clone(), inbound, false).await {
            Err(error) if error.status() == Some(401) => {
                let renewed = self.stored(true).await?;
                self.send(&renewed, request, inbound, false).await
            }
            Err(error)
                if error.status() == Some(400)
                    && tools::after_tools_refusal(error.message())
                    && needs_reply_after_tools(&request)
                    && !self.learned_after_tools(stored.credential.site, &request.model) =>
            {
                self.learn_after_tools(stored.credential.site, &request.model);
                self.send(&stored, request, inbound, true).await
            }
            other => other,
        }
    }

    async fn forward(&self, path: &str, body: RawBody) -> Result<RawAnswer, PluginError> {
        if !matches!(path, "/images/generations" | "/images/edits") {
            return Err(PluginError::unsupported(format!(
                "this upstream does not serve {path}"
            )));
        }
        let stored = self.stored(false).await?;
        match self.send_forward(&stored, path, body.clone()).await {
            Err(error) if error.status() == Some(401) => {
                let renewed = self.stored(true).await?;
                self.send_forward(&renewed, path, body).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}

fn needs_reply_after_tools(request: &Request) -> bool {
    let mut messages = request.messages.iter().rev();
    let mut users = false;
    for message in &mut messages {
        if message.role == Role::User {
            users = true;
            continue;
        }
        return users && message.role == Role::Tool;
    }
    false
}

fn shape_image(object: &mut Map<String, Value>, edit: bool) {
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let gpt = model.to_ascii_lowercase().contains("gpt-image");
    let mut shaped = Map::new();
    for key in ["model", "prompt", "n"] {
        if let Some(value) = object.get(key) {
            shaped.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(size) = object.get("size").and_then(Value::as_str)
        && (gpt || size != "auto")
    {
        shaped.insert("size".to_owned(), Value::String(size.to_owned()));
    }
    if gpt {
        for key in ["quality", "background", "output_format"] {
            if let Some(value) = object.get(key).and_then(Value::as_str)
                && !value.is_empty()
            {
                shaped.insert(key.to_owned(), Value::String(value.to_owned()));
            }
        }
        shaped.insert(
            "response_format".to_owned(),
            Value::String("b64_json".to_owned()),
        );
    }
    if edit {
        if let Some(images) = object.get("image") {
            let images = match images {
                Value::String(image) if image.starts_with("data:") => {
                    Value::Array(vec![Value::String(image.clone())])
                }
                images => images.clone(),
            };
            shaped.insert("image".to_owned(), images);
        }
        if gpt && let Some(mask) = object.get("mask") {
            shaped.insert("mask".to_owned(), mask.clone());
        }
    }
    *object = shaped;
}

fn image_answer(answer: RawAnswer) -> Result<RawAnswer, PluginError> {
    let Ok(Value::Object(object)) = hofund_plugin_sdk::json::from_slice::<Value>(answer.body())
    else {
        return Ok(answer);
    };
    let Some(code) = object.get("code") else {
        return Ok(answer);
    };
    let code_text = match code {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => return Ok(answer),
    };
    if code_text != "0" {
        let message = object.get("msg").and_then(Value::as_str).unwrap_or("error");
        return Err(PluginError::status_error(
            502,
            format!("{message} ({code_text})"),
        ));
    }
    let Some(data) = object.get("data").filter(|data| data.is_object()) else {
        return Ok(answer);
    };
    Ok(RawAnswer::new(
        answer.status(),
        answer.content_type().map(str::to_owned),
        data.to_string(),
    ))
}
