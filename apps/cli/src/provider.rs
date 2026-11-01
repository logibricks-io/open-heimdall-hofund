//! `hofund provider` and `hofund model`: provider reads, connectivity tests,
//! model-list refetches, and revision-checked pick edits.
//!
//! Every command goes through [`crate::commands::client`], so it talks to a
//! running daemon or, with none, to one-shot `hofundd`. One-shot pick edits
//! and refetches take the per-provider host-state lock in `hofundd`.

use std::path::Path;

use hofund_control_client::hofund_control_proto::{
    ModelListResponse, PicksResponse, PicksSetRequest, ProviderKeyAction, ProviderKeyRequest,
    ProviderKeysResponse, ProviderSummary, ProviderTestRequest,
};
use hofund_control_client::{Client, ClientError};

use crate::commands::{client, failure};

/// How many times a pick edit is sent before a revision conflict is final.
pub(crate) const PICKS_ATTEMPTS: usize = 3;

/// `hofund provider list`.
pub(crate) async fn list(config: &Path) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.providers().await.map_err(failure)?;
    let mut out = String::from("id\tkind\tprotocol\tready\tmodels");
    for provider in &response.providers {
        out.push_str(&format!(
            "\n{}\t{}\t{}\t{}\t{}",
            provider.id,
            provider.kind,
            provider.protocol,
            yes_no(provider.ready),
            provider.model_count
        ));
    }
    Ok(out)
}

/// `hofund provider keys <id>`.
pub(crate) async fn keys(config: &Path, id: &str) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.provider_keys(id).await.map_err(failure)?;
    Ok(render_keys(&response))
}

/// `hofund provider key-add <id> <keys...>`.
pub(crate) async fn key_add(
    config: &Path,
    id: &str,
    keys: &[String],
    protocol: &str,
) -> Result<String, String> {
    let client = client(config).await?;
    let mut request = ProviderKeyRequest::new(ProviderKeyAction::Add);
    request.keys = keys.to_vec();
    protocol.clone_into(&mut request.protocol);
    let response = client
        .change_provider_key(id, request)
        .await
        .map_err(failure)?;
    Ok(render_keys(&response))
}

/// A key on/off/use/rename/replace/weight/protocol/remove operation.
pub(crate) async fn key_edit(
    config: &Path,
    id: &str,
    action: ProviderKeyAction,
    key_id: &str,
    value: &str,
) -> Result<String, String> {
    let client = client(config).await?;
    let mut request = ProviderKeyRequest::new(action).for_id(key_id);
    match action {
        ProviderKeyAction::Rename => request.name = value.to_owned(),
        ProviderKeyAction::Replace | ProviderKeyAction::RemoveMany => {
            request.keys = vec![value.to_owned()];
        }
        ProviderKeyAction::Weight => {
            request.weight = value
                .parse()
                .map_err(|_| format!("a key's weight is a whole number, not `{value}`"))?;
        }
        ProviderKeyAction::Protocol => value.clone_into(&mut request.protocol),
        _ => {}
    }
    let response = client
        .change_provider_key(id, request)
        .await
        .map_err(failure)?;
    Ok(render_keys(&response))
}

fn render_keys(response: &ProviderKeysResponse) -> String {
    let mut out = String::new();
    if response.removed > 0 {
        out.push_str(&format!("removed: {}\n", response.removed));
    }
    if response.added > 0 || response.already_had > 0 {
        out.push_str(&format!(
            "added: {}\nalready had: {}\n",
            response.added, response.already_had
        ));
    }
    if !response.before.is_empty() {
        out.push_str(&format!(
            "before: {}\nafter: {}\n",
            response.before, response.after
        ));
    }
    out.push_str("id\tname\tmasked\ton\tprotocol\tweight\trest");
    for key in &response.keys {
        let rest = key.rest.as_ref().map_or_else(String::new, |rest| {
            format!(
                "{} until {} (status {:?})",
                rest.why, rest.until_ms, rest.status
            )
        });
        out.push_str(&format!(
            "\n{}\t{}\t{}\t{}\t{}\t{}\t{}",
            key.id,
            key.name,
            key.masked,
            yes_no(key.on),
            key.protocol,
            key.weight,
            rest
        ));
    }
    out
}

/// `hofund provider show <id>`: the summary, the cached model list, and the
/// picks.
pub(crate) async fn show(config: &Path, id: &str) -> Result<String, String> {
    let client = client(config).await?;
    show_with(&client, id).await
}

async fn show_with(client: &Client, id: &str) -> Result<String, String> {
    let providers = client.providers().await.map_err(failure)?;
    let summary = find(&providers.providers, id)?;
    let models = client.provider_models(id).await.map_err(failure)?;
    let picks = client.picks(id).await.map_err(failure)?;
    Ok(render_show(summary, &models, &picks))
}

fn find<'a>(providers: &'a [ProviderSummary], id: &str) -> Result<&'a ProviderSummary, String> {
    providers
        .iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| format!("unknown provider `{id}`"))
}

fn render_show(
    summary: &ProviderSummary,
    models: &ModelListResponse,
    picks: &PicksResponse,
) -> String {
    let mut out = format!(
        "id: {}\nkind: {}\nprotocol: {}\nready: {}\nlisted: {}",
        summary.id,
        summary.kind,
        summary.protocol,
        yes_no(summary.ready),
        models.models.len()
    );
    out.push_str(&format!("\npicks: {}", picks_text(&picks.picks)));
    if !picks.delisted.is_empty() {
        out.push_str(&format!("\ndelisted: {}", picks.delisted.join(", ")));
    }
    for model in &models.models {
        let mark = if picks.picks.contains(&model.id) {
            "*"
        } else {
            " "
        };
        out.push_str(&format!("\n  {mark} {}", model.id));
    }
    out
}

pub(crate) fn picks_text(picks: &[String]) -> String {
    if picks.is_empty() {
        "default".to_owned()
    } else {
        picks.join(", ")
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// `hofund provider test <id> [<model>...]`: one line per probed model. Any
/// failed probe makes the command fail after the lines are printed.
pub(crate) async fn test(config: &Path, id: &str, models: &[String]) -> Result<String, String> {
    let client = client(config).await?;
    let request = ProviderTestRequest::new((!models.is_empty()).then(|| models.to_vec()));
    let response = client.test_provider(id, request).await.map_err(failure)?;
    let mut out = String::new();
    let mut failed = 0_usize;
    for result in &response.results {
        if result.ok {
            out.push_str(&format!(
                "ok\t{}\t{} ms\n",
                result.model,
                result.latency_ms.unwrap_or_default()
            ));
        } else {
            failed += 1;
            let message = result
                .error
                .as_ref()
                .map_or("probe failed", |error| error.message.as_str());
            let detail = match result.status {
                Some(status) => format!("{status} {message}"),
                None => message.to_owned(),
            };
            out.push_str(&format!("fail\t{}\t{detail}\n", result.model));
        }
    }
    let out = out.trim_end_matches('\n').to_owned();
    if response.results.is_empty() {
        return Err(format!("{id}: no model was probed"));
    }
    if failed == 0 {
        return Ok(out);
    }
    println!("{out}");
    Err(format!(
        "{id}: {failed} of {} probes failed",
        response.results.len()
    ))
}

/// `hofund provider refresh <id>` (also `fetch`, and `models <id>` with no
/// ids): refetch the list, which drops gone picks and applies auto-include,
/// then show the provider.
pub(crate) async fn refresh(config: &Path, id: &str) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.refresh_models(id).await.map_err(failure)?;
    let mut out = refresh_summary(id, &response);
    out.push('\n');
    out.push_str(&show_with(&client, id).await?);
    Ok(out)
}

fn refresh_summary(id: &str, response: &ModelListResponse) -> String {
    let mut out = format!("{} models from {id}", response.models.len());
    if !response.added.is_empty() {
        out.push_str(&format!(
            "\n+ newly listed, taken by its auto-include rules: {}",
            response.added.join(", ")
        ));
    }
    if !response.removed.is_empty() {
        out.push_str(&format!(
            "\n! gone from its list, so no longer picked: {}",
            response.removed.join(", ")
        ));
    }
    out
}

/// `hofund provider models <id> <ids...>`: edit the picks and show the
/// result.
pub(crate) async fn picks(config: &Path, id: &str, args: &[String]) -> Result<String, String> {
    let client = client(config).await?;
    let (before, after) = edit_picks(&client, id, args).await.map_err(failure)?;
    Ok(format!(
        "picks: {} -> {}\nrevision: {}",
        picks_text(&before.picks),
        picks_text(&after.picks),
        after.picks_revision
    ))
}

/// The pick routes, so the conflict loop can be tested without a server.
pub(crate) trait PicksApi {
    /// GET the provider's picks and revision.
    async fn get_picks(&self, id: &str) -> Result<PicksResponse, ClientError>;
    /// PUT a revision-checked pick edit.
    async fn put_picks(
        &self,
        id: &str,
        request: PicksSetRequest,
    ) -> Result<PicksResponse, ClientError>;
}

impl PicksApi for Client {
    async fn get_picks(&self, id: &str) -> Result<PicksResponse, ClientError> {
        self.picks(id).await
    }

    async fn put_picks(
        &self,
        id: &str,
        request: PicksSetRequest,
    ) -> Result<PicksResponse, ClientError> {
        self.set_picks(id, request).await
    }
}

/// Applies `args` to the latest picks: GET, then PUT against that revision.
/// On a revision conflict the edit is rebuilt on a fresh GET, up to
/// [`PICKS_ATTEMPTS`] sends in all. Returns the picks before the applied
/// edit and after it.
pub(crate) async fn edit_picks(
    api: &impl PicksApi,
    id: &str,
    args: &[String],
) -> Result<(PicksResponse, PicksResponse), ClientError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let before = api.get_picks(id).await?;
        match api
            .put_picks(id, picks_request(&before.picks_revision, args))
            .await
        {
            Err(error) if attempt < PICKS_ATTEMPTS && is_conflict(&error) => {}
            Err(error) => return Err(error),
            Ok(after) => return Ok((before, after)),
        }
    }
}

fn is_conflict(error: &ClientError) -> bool {
    matches!(error, ClientError::Server { status, .. } if status.as_u16() == 409)
}

/// The request for `args`, as the reference reads them: a sole `all`, `-`,
/// or `none`, or any `+id`/`-id` change, is an edit; a list of bare ids
/// replaces the picks. A mix of changes and bare ids goes as an edit, which
/// the server rejects as mixed forms.
pub(crate) fn picks_request(revision: &str, args: &[String]) -> PicksSetRequest {
    let sole = matches!(args, [only] if matches!(only.as_str(), "all" | "-" | "none"));
    let change = |arg: &String| arg.len() > 1 && (arg.starts_with('+') || arg.starts_with('-'));
    if sole || args.iter().any(change) {
        PicksSetRequest::edit(revision, args.to_vec())
    } else {
        PicksSetRequest::replace(revision, args.to_vec())
    }
}

/// `hofund model list`: the client-facing route models.
pub(crate) async fn model_list(config: &Path) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.route_models().await.map_err(failure)?;
    Ok(response
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<Vec<_>>()
        .join("\n"))
}

/// `hofund model show <id>`: the route model, or an error when no route
/// serves it.
pub(crate) async fn model_show(config: &Path, id: &str) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.route_models().await.map_err(failure)?;
    if response.models.iter().any(|model| model.id == id) {
        Ok(format!("model: {id}\nrouted: yes"))
    } else {
        Err(format!("unknown model `{id}`: no route serves it"))
    }
}

/// `hofund routes`: configured route models and their strategy names.
pub(crate) async fn routes(config: &Path) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.route_models().await.map_err(failure)?;
    Ok(render_routes(&response.models))
}

fn render_routes(models: &[hofund_control_client::hofund_control_proto::RouteModel]) -> String {
    let mut out = String::from("model\trouting");
    for model in models {
        out.push_str(&format!("\n{}\t{}", model.id, model.routing));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hofund_control_client::ClientError;
    use hofund_control_client::hofund_control_proto::{
        ControlError, ControlErrorKind, ErrorEnvelope, ModelEntry, ModelListResponse, PickEdit,
        PicksForm, PicksResponse, PicksSetRequest, ProviderSummary, RouteModel,
    };

    use super::{
        PICKS_ATTEMPTS, PicksApi, edit_picks, picks_request, refresh_summary, render_routes,
        render_show,
    };

    /// A picks server whose state another writer changes before each of the
    /// first `races` PUTs land.
    struct Racing {
        state: Mutex<(Vec<String>, u32)>,
        races: Mutex<usize>,
        gets: Mutex<usize>,
        puts: Mutex<Vec<PicksSetRequest>>,
    }

    impl Racing {
        fn new(picks: &[&str], races: usize) -> Self {
            Self {
                state: Mutex::new((picks.iter().map(|id| (*id).to_owned()).collect(), 1)),
                races: Mutex::new(races),
                gets: Mutex::new(0),
                puts: Mutex::new(Vec::new()),
            }
        }

        fn response(state: &(Vec<String>, u32)) -> PicksResponse {
            let mut response = PicksResponse::default();
            response.picks = state.0.clone();
            response.picks_revision = format!("r{}", state.1);
            response
        }
    }

    fn conflict() -> ClientError {
        let mut error = ErrorEnvelope::default();
        error.error = ControlError::new(ControlErrorKind::Conflict, "stale picks revision", "req");
        ClientError::Server {
            status: http::StatusCode::CONFLICT,
            error,
        }
    }

    impl PicksApi for Racing {
        async fn get_picks(&self, _id: &str) -> Result<PicksResponse, ClientError> {
            *self.gets.lock().expect("gets") += 1;
            Ok(Self::response(&self.state.lock().expect("state")))
        }

        async fn put_picks(
            &self,
            _id: &str,
            request: PicksSetRequest,
        ) -> Result<PicksResponse, ClientError> {
            self.puts.lock().expect("puts").push(request.clone());
            let mut state = self.state.lock().expect("state");
            {
                let mut races = self.races.lock().expect("races");
                if *races > 0 {
                    *races -= 1;
                    // Another writer adds a pick first.
                    let next = format!("other{}", state.1);
                    state.0.push(next);
                    state.1 += 1;
                }
            }
            if request.expected_picks_revision != format!("r{}", state.1) {
                return Err(conflict());
            }
            match request.form().expect("valid form") {
                PicksForm::Replace(ids) => state.0 = ids,
                PicksForm::Edit(changes) => {
                    for change in changes {
                        match change {
                            PickEdit::Add(id) => {
                                if !state.0.contains(&id) {
                                    state.0.push(id);
                                }
                            }
                            PickEdit::Remove(id) => state.0.retain(|pick| *pick != id),
                            _ => unreachable!("no other edit"),
                        }
                    }
                }
                PicksForm::Reset => state.0.clear(),
                _ => unreachable!("no other form"),
            }
            state.1 += 1;
            Ok(Self::response(&state))
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn provider_picks_409_refetches_and_reapplies() {
        // Two conflicts: the third send applies the delta to the newest picks.
        let api = Racing::new(&["a", "b"], 2);
        let (before, after) = block_on(edit_picks(&api, "p", &args(&["+c", "-a"]))).expect("edit");
        assert_eq!(before.picks, ["a", "b", "other1", "other2"]);
        assert_eq!(after.picks, ["b", "other1", "other2", "c"]);
        assert_eq!(*api.gets.lock().expect("gets"), 3);
        let puts = api.puts.lock().expect("puts");
        let revisions = puts
            .iter()
            .map(|put| put.expected_picks_revision.as_str())
            .collect::<Vec<_>>();
        assert_eq!(revisions, ["r1", "r2", "r3"]);
        assert!(
            puts.iter()
                .all(|put| put.edit.as_deref() == Some(&args(&["+c", "-a"])[..]))
        );
        drop(puts);

        // A conflict on every send is final after the bound.
        let api = Racing::new(&["a"], PICKS_ATTEMPTS);
        let error = block_on(edit_picks(&api, "p", &args(&["x"]))).expect_err("conflict");
        assert!(super::is_conflict(&error), "{error}");
        assert_eq!(api.puts.lock().expect("puts").len(), PICKS_ATTEMPTS);
        assert_eq!(*api.gets.lock().expect("gets"), PICKS_ATTEMPTS);

        // One conflict short of the bound still lands.
        let api = Racing::new(&["a"], PICKS_ATTEMPTS - 1);
        let (_, after) = block_on(edit_picks(&api, "p", &args(&["x"]))).expect("edit");
        assert_eq!(after.picks, ["x"]);
    }

    #[test]
    fn picks_errors_other_than_conflict_are_not_retried() {
        struct Refusing(Mutex<usize>);
        impl PicksApi for Refusing {
            async fn get_picks(&self, _id: &str) -> Result<PicksResponse, ClientError> {
                Ok(PicksResponse::default())
            }
            async fn put_picks(
                &self,
                _id: &str,
                _request: PicksSetRequest,
            ) -> Result<PicksResponse, ClientError> {
                *self.0.lock().expect("puts") += 1;
                let mut error = ErrorEnvelope::default();
                error.error = ControlError::new(ControlErrorKind::Unprocessable, "no", "req");
                Err(ClientError::Server {
                    status: http::StatusCode::UNPROCESSABLE_ENTITY,
                    error,
                })
            }
        }
        let api = Refusing(Mutex::new(0));
        let error = block_on(edit_picks(&api, "p", &args(&["a"]))).expect_err("refused");
        assert!(!super::is_conflict(&error));
        assert_eq!(*api.0.lock().expect("puts"), 1);
    }

    #[test]
    fn picks_request_classifies_the_reference_forms() {
        let replace = picks_request("r", &args(&["a", "b"]));
        assert_eq!(replace.ids, Some(args(&["a", "b"])));
        assert_eq!(replace.edit, None);
        assert_eq!(replace.expected_picks_revision, "r");
        for sole in ["all", "-", "none"] {
            let request = picks_request("r", &args(&[sole]));
            assert_eq!(request.edit, Some(args(&[sole])), "{sole}");
            assert_eq!(request.ids, None);
        }
        let delta = picks_request("r", &args(&["+a", "-b"]));
        assert_eq!(delta.edit, Some(args(&["+a", "-b"])));
        let mixed = picks_request("r", &args(&["a", "+b"]));
        assert_eq!(mixed.edit, Some(args(&["a", "+b"])));
        assert!(mixed.form().is_err());
        // A lone `+` or `-` inside a list is an id, not a change.
        let bare = picks_request("r", &args(&["a", "+"]));
        assert_eq!(bare.ids, Some(args(&["a", "+"])));
        let none_in_list = picks_request("r", &args(&["none", "a"]));
        assert_eq!(none_in_list.ids, Some(args(&["none", "a"])));
    }

    #[test]
    fn refresh_summary_names_taken_and_dropped_picks() {
        let mut response = ModelListResponse::default();
        let mut entry = ModelEntry::default();
        entry.id = "a".to_owned();
        response.models.push(entry);
        assert_eq!(refresh_summary("p", &response), "1 models from p");
        response.added = args(&["n"]);
        response.removed = args(&["g", "h"]);
        assert_eq!(
            refresh_summary("p", &response),
            "1 models from p\n+ newly listed, taken by its auto-include rules: n\n! gone from its list, so no longer picked: g, h"
        );
    }

    #[test]
    fn a9_routes_print_the_routing_name() {
        let mut first = RouteModel::default();
        first.id = "first".to_owned();
        first.routing = "weight".to_owned();
        let mut second = RouteModel::default();
        second.id = "second".to_owned();
        let models = vec![first, second];
        assert_eq!(
            render_routes(&models),
            "model\trouting\nfirst\tweight\nsecond\tsmart"
        );
    }

    #[test]
    fn k6_key_table_shows_counts_masks_weights_and_rests() {
        let mut response =
            hofund_control_client::hofund_control_proto::ProviderKeysResponse::default();
        response.added = 2;
        response.already_had = 1;
        response.before = "before-id".to_owned();
        response.after = "after-id".to_owned();
        let mut key = hofund_control_client::hofund_control_proto::ProviderKey::default();
        key.id = "key-id".to_owned();
        key.name = "Team".to_owned();
        key.masked = "…-key".to_owned();
        key.on = true;
        key.protocol = "openai-chat".to_owned();
        key.weight = 3;
        let mut rest = hofund_control_client::hofund_control_proto::ProviderKeyRest::default();
        rest.why = "rate_limit".to_owned();
        rest.status = Some(429);
        rest.until_ms = 1234;
        key.rest = Some(rest);
        response.keys.push(key);
        let rendered = super::render_keys(&response);
        assert!(rendered.contains("added: 2\nalready had: 1"), "{rendered}");
        assert!(
            rendered.contains("before: before-id\nafter: after-id"),
            "{rendered}"
        );
        assert!(
            rendered.contains("key-id\tTeam\t…-key\tyes\topenai-chat\t3"),
            "{rendered}"
        );
        assert!(rendered.contains("rate_limit until 1234"), "{rendered}");
        assert!(!rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn show_marks_picked_models() {
        let mut summary = ProviderSummary::default();
        summary.id = "p".to_owned();
        summary.kind = "openai".to_owned();
        summary.protocol = "openai-chat".to_owned();
        summary.ready = true;
        let mut models = ModelListResponse::default();
        for id in ["a", "b"] {
            let mut entry = ModelEntry::default();
            entry.id = id.to_owned();
            models.models.push(entry);
        }
        let mut picks = PicksResponse::default();
        assert!(render_show(&summary, &models, &picks).contains("\npicks: default\n"));
        picks.picks = args(&["b"]);
        picks.delisted = args(&["z"]);
        assert_eq!(
            render_show(&summary, &models, &picks),
            "id: p\nkind: openai\nprotocol: openai-chat\nready: yes\nlisted: 2\npicks: b\ndelisted: z\n    a\n  * b"
        );
    }
}
