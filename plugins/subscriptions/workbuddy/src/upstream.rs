//! OpenAI Chat upstream with WorkBuddy client headers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, Role, UpstreamProvider,
};
use hofund_plugin_sdk::wire::{self, Target};

use crate::Endpoints;
use crate::creds;
use crate::refresh;
use crate::site::Site;
use crate::tools;

pub(crate) struct WorkBuddyUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    after_tools: Arc<Mutex<HashSet<String>>>,
}

impl WorkBuddyUpstream {
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        after_tools: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            after_tools,
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
