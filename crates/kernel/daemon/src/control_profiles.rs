//! Control-plane handlers for named agent setting profiles.

use std::sync::Arc;

use hofund_agent::{ProfileFields, apply_fields, catalog, in_use};
use hofund_control_proto::{
    ControlError, ControlErrorKind, ProfileDeleteRequest, ProfileDetailResponse,
    ProfileListResponse, ProfileSaveRequest, ProfileSummary, ProfileSwitchRequest,
    ProfileSwitchResponse,
};
use hofund_plugin_api::AgentDescription;

use super::profiles::ProfileStore;

pub(crate) struct ProfileOps {
    store: Arc<ProfileStore>,
    descriptions: &'static [&'static AgentDescription],
    session: Option<hofund_agent::Session>,
}

impl ProfileOps {
    pub(crate) fn new(
        config_path: &std::path::Path,
        descriptions: &'static [&'static AgentDescription],
    ) -> Self {
        Self {
            store: Arc::new(ProfileStore::new(config_path, descriptions)),
            descriptions,
            session: None,
        }
    }

    pub(crate) fn with_session(mut self, session: Option<hofund_agent::Session>) -> Self {
        self.session = session;
        self
    }

    pub(crate) async fn list(&self) -> Result<ProfileListResponse, ControlError> {
        self.blocking(|store, catalog, session| {
            let names = store.names().map_err(internal)?;
            let mut response = ProfileListResponse::default();
            for name in names {
                let fields = store.get(&name).map_err(internal)?.unwrap_or_default();
                let active = in_use(&fields, catalog, session).map_err(agent_failure)?;
                let mut summary = ProfileSummary::default();
                summary.name = name;
                summary.in_use = active;
                response.profiles.push(summary);
            }
            Ok(response)
        })
        .await
    }

    pub(crate) async fn detail(&self, name: String) -> Result<ProfileDetailResponse, ControlError> {
        self.blocking(move |store, catalog, session| {
            let fields = store.get(&name).map_err(internal)?;
            let Some(fields) = fields else {
                return Err(not_found(&name));
            };
            let active = in_use(&fields, catalog, session).map_err(agent_failure)?;
            let mut response = ProfileDetailResponse::default();
            response.name = name;
            response.in_use = active;
            response.fields = fields;
            Ok(response)
        })
        .await
    }

    pub(crate) async fn save(&self, request: ProfileSaveRequest) -> Result<(), ControlError> {
        request.validate()?;
        let name = request.name.trim().to_owned();
        self.blocking(move |store, _catalog, session| {
            let fields = if request.from_current {
                store.snapshot(session).map_err(internal)?
            } else {
                ProfileFields::from_iter(request.fields)
            };
            store.save(&name, fields).map_err(internal)
        })
        .await
    }

    pub(crate) async fn switch(
        &self,
        request: ProfileSwitchRequest,
    ) -> Result<ProfileSwitchResponse, ControlError> {
        request.validate()?;
        let name = request.name;
        self.blocking(move |store, catalog, session| {
            let fields = store.get(&name).map_err(internal)?;
            let Some(fields) = fields else {
                return Err(not_found(&name));
            };
            let mut notes = Vec::new();
            let outcome =
                apply_fields(catalog, session, &fields, &mut notes).map_err(agent_failure)?;
            let mut response =
                ProfileSwitchResponse::new(u64::try_from(outcome.changed).unwrap_or(u64::MAX));
            response.notes = notes;
            Ok(response)
        })
        .await
    }

    pub(crate) async fn delete(&self, request: ProfileDeleteRequest) -> Result<(), ControlError> {
        request.validate()?;
        let name = request.name;
        self.blocking(move |store, _catalog, _session| store.delete(&name).map_err(internal))
            .await
    }

    async fn blocking<T, F>(&self, work: F) -> Result<T, ControlError>
    where
        T: Send + 'static,
        F: FnOnce(
                &ProfileStore,
                &hofund_agent::Catalog,
                &hofund_agent::Session,
            ) -> Result<T, ControlError>
            + Send
            + 'static,
    {
        let store = Arc::clone(&self.store);
        let descriptions = self.descriptions;
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            let catalog = catalog(descriptions)
                .map_err(|error| internal(format!("agent catalog: {error}")))?;
            let session = session.unwrap_or_else(crate::agent_session);
            store.load().map_err(internal)?;
            work(&store, &catalog, &session)
        })
        .await
        .map_err(|_| internal("profile task failed".to_owned()))?
    }
}

fn not_found(name: &str) -> ControlError {
    ControlError::new(
        ControlErrorKind::NotFound,
        format!("no profile named `{name}`"),
        "profiles",
    )
}

fn internal(message: String) -> ControlError {
    ControlError::new(ControlErrorKind::Internal, message, "profiles")
}

fn agent_failure(error: hofund_agent::AgentError) -> ControlError {
    ControlError::new(
        ControlErrorKind::Unprocessable,
        error.to_string(),
        "profiles",
    )
}
