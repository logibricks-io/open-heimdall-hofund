//! `hofund provider add/set/key/rm/refresh-every/auto-include`: provider
//! config surgery.
//!
//! Each command reads the config text and its revision, edits the
//! `[[providers]]` entry with `toml_edit` so comments and layout survive, and
//! applies the result against that revision. A revision conflict rebuilds
//! the edit on a fresh read, up to [`EDIT_ATTEMPTS`] sends in all. Without a
//! daemon the apply runs in one-shot `hofundd`, which takes the config
//! sidecar lock and rechecks the revision before it writes.
//!
//! `models=` is not a config field: it edits the provider's picks through
//! the picks route, after any config change.

use std::path::Path;

use hofund_config::ModelList;
use hofund_control_client::hofund_control_proto::{ConfigApplyRequest, ConfigResponse};
use hofund_control_client::{Client, ClientError};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, TableLike, value};

use crate::commands::{client, failure, push_issues};
use crate::provider::{edit_picks, picks_text};

/// How many times a config edit is sent before a revision conflict is final.
pub(crate) const EDIT_ATTEMPTS: usize = 3;

/// The error for `id=` on `provider set`, raised before any I/O.
pub(crate) const RENAME_UNSUPPORTED: &str =
    "renaming a provider is not supported yet; see feature-parity row 11.6";

/// One provider config field a `k=v` pair names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FieldKey {
    Preset,
    Protocol,
    BaseUrl,
    ApiKey,
    Quirks,
    MaxOutputTokens,
    Header(String),
    Option(String),
    Refresh,
    AutoInclude,
    /// Whole auto-include rules, as `provider auto-include` takes them, so a
    /// rule may hold a comma.
    AutoIncludeRules(Vec<String>),
    ModelsUrl,
}

/// One parsed `k=v` pair. An empty value removes the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) key: FieldKey,
    pub(crate) value: String,
}

/// The pairs of an `add` or `set`: config fields, and the picks `models=`
/// gives, split on commas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pairs {
    pub(crate) fields: Vec<Field>,
    pub(crate) models: Option<Vec<String>>,
}

const FIELDS: &str = "preset, protocol, base_url (url), api_key (key), quirks, \
max_output_tokens, header.<name>, option.<name>, refresh, auto_include, \
models.url, models";

/// What the key of a `k=v` pair names.
enum Name {
    Id,
    Models,
    Field(FieldKey),
}

fn name(key: &str) -> Option<Name> {
    let key = key.trim();
    let field = match key.to_ascii_lowercase().as_str() {
        "id" => return Some(Name::Id),
        "models" => return Some(Name::Models),
        "preset" => FieldKey::Preset,
        "protocol" => FieldKey::Protocol,
        "base_url" | "url" => FieldKey::BaseUrl,
        "api_key" | "key" => FieldKey::ApiKey,
        "quirks" => FieldKey::Quirks,
        "max_output_tokens" => FieldKey::MaxOutputTokens,
        "refresh" | "models.refresh" => FieldKey::Refresh,
        "auto_include" | "auto-include" | "models.auto_include" => FieldKey::AutoInclude,
        "models.url" => FieldKey::ModelsUrl,
        _ => match named(key, &["header.", "headers."]) {
            Some(name) => FieldKey::Header(name),
            None => FieldKey::Option(named(key, &["option.", "options."])?),
        },
    };
    Some(Name::Field(field))
}

/// Whether `arg` is a `k=v` pair whose key names a field. `provider add`
/// takes any other first argument as the API key, so a key holding `=`
/// (base64 padding) is not mistaken for a pair or echoed in an error.
pub(crate) fn is_pair(arg: &str) -> bool {
    arg.split_once('=')
        .is_some_and(|(key, _)| name(key).is_some())
}

/// Parses `k=v` pairs. `id=` is refused: `set` cannot rename, and `add`
/// takes the id as its first argument.
pub(crate) fn parse_pairs(pairs: &[&str], verb: &str) -> Result<Pairs, String> {
    let mut parsed = Pairs::default();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("expected key=value, got `{pair}`"));
        };
        let value = value.trim().to_owned();
        let key = match name(key) {
            Some(Name::Id) if verb == "set" => return Err(RENAME_UNSUPPORTED.to_owned()),
            Some(Name::Id) => {
                return Err("the provider id is the first argument of provider add".to_owned());
            }
            Some(Name::Models) => {
                parsed.models = Some(split_list(&value));
                continue;
            }
            Some(Name::Field(key)) => key,
            None => return Err(format!("unknown provider field `{key}`; fields: {FIELDS}")),
        };
        if key == FieldKey::MaxOutputTokens
            && !value.is_empty()
            && value.parse::<i64>().map_or(true, |number| number < 0)
        {
            return Err(format!(
                "max_output_tokens must be a whole number, got `{value}`"
            ));
        }
        parsed.fields.push(Field { key, value });
    }
    Ok(parsed)
}

/// The name after one of `prefixes`, keeping its case (header names and
/// option keys are case-sensitive in the file).
fn named(key: &str, prefixes: &[&str]) -> Option<String> {
    prefixes.iter().find_map(|prefix| {
        let head = key.get(..prefix.len())?;
        let name = &key[prefix.len()..];
        (head.eq_ignore_ascii_case(prefix) && !name.is_empty()).then(|| name.to_owned())
    })
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One edit of the config text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    /// A new `[[providers]]` entry.
    Add { id: String, fields: Vec<Field> },
    /// Fields of an existing entry.
    Set { id: String, fields: Vec<Field> },
    /// The entry removed.
    Remove { id: String },
    /// Accounts-at-once override; `None` restores the plugin default.
    AtOnce { id: String, value: Option<u64> },
}

impl Edit {
    fn id(&self) -> &str {
        match self {
            Self::Add { id, .. }
            | Self::Set { id, .. }
            | Self::Remove { id }
            | Self::AtOnce { id, .. } => id,
        }
    }
}

/// Applies `edit` to `text`, keeping everything it does not touch.
pub(crate) fn edit_text(text: &str, edit: &Edit) -> Result<String, String> {
    if let Edit::AtOnce { id, value } = edit {
        return hofund_config::edit_at_once(text, id, *value).map_err(|error| error.to_string());
    }
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| format!("the config is not valid TOML: {error}"))?;
    let providers = providers_mut(&mut document, matches!(edit, Edit::Add { .. }))?;
    let id = edit.id();
    let index = providers
        .iter()
        .position(|table| table.get("id").and_then(Item::as_str) == Some(id));
    match (edit, index) {
        (Edit::Add { .. }, Some(_)) => {
            return Err(format!(
                "provider `{id}` already exists; hofund provider set {id} k=v... edits it"
            ));
        }
        (Edit::Add { fields, .. }, None) => {
            let mut table = Table::new();
            table.insert("id", value(id));
            apply_fields(&mut table, fields);
            providers.push(table);
        }
        (Edit::Set { fields, .. }, Some(index)) => {
            let table = providers.get_mut(index).ok_or_else(|| unknown(id))?;
            apply_fields(table, fields);
        }
        (Edit::Remove { .. }, Some(index)) => {
            providers.remove(index);
        }
        (Edit::Set { .. } | Edit::Remove { .. }, None) => return Err(unknown(id)),
        (Edit::AtOnce { .. }, _) => unreachable!("at-once returned above"),
    }
    Ok(document.to_string())
}

fn unknown(id: &str) -> String {
    format!("unknown provider `{id}`")
}

/// The `[[providers]]` array, created for an add when the file has none.
fn providers_mut(document: &mut DocumentMut, create: bool) -> Result<&mut ArrayOfTables, String> {
    if create && !document.contains_key("providers") {
        document.insert("providers", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    match document.get_mut("providers") {
        Some(Item::ArrayOfTables(providers)) => Ok(providers),
        Some(_) => Err(
            "`providers` is not written as [[providers]] tables; edit the file to use them"
                .to_owned(),
        ),
        None => Err("the config has no [[providers]]".to_owned()),
    }
}

fn apply_fields(table: &mut Table, fields: &[Field]) {
    for field in fields {
        let text = field.value.as_str();
        match &field.key {
            FieldKey::Preset => set_str(table, "preset", text),
            FieldKey::Protocol => set_str(table, "protocol", text),
            FieldKey::BaseUrl => set_str(table, "base_url", text),
            FieldKey::ApiKey => set_str(table, "api_key", text),
            FieldKey::Quirks => set_str(table, "quirks", text),
            FieldKey::MaxOutputTokens => match text.parse::<i64>() {
                Ok(number) => put(table, "max_output_tokens", value(number)),
                Err(_) => {
                    table.remove("max_output_tokens");
                }
            },
            FieldKey::Header(name) => {
                set_nested(
                    table,
                    "headers",
                    name,
                    (!text.is_empty()).then(|| value(text)),
                );
            }
            FieldKey::Option(name) => set_nested(table, "options", name, option_value(text)),
            FieldKey::Refresh => {
                set_nested(
                    table,
                    "models",
                    "refresh",
                    (!text.is_empty()).then(|| value(text)),
                );
            }
            FieldKey::AutoInclude => set_rules(table, split_list(text)),
            FieldKey::AutoIncludeRules(rules) => set_rules(table, rules.clone()),
            FieldKey::ModelsUrl => {
                set_nested(
                    table,
                    "models",
                    "url",
                    (!text.is_empty()).then(|| value(text)),
                );
            }
        }
    }
}

/// Sets the auto-include rules; none, or a sole `none` or `off`, removes
/// them.
fn set_rules(table: &mut Table, rules: Vec<String>) {
    let none = match rules.as_slice() {
        [] => true,
        [only] => only == "none" || only == "off",
        _ => false,
    };
    let rules = (!none).then(|| value(rules.into_iter().collect::<Array>()));
    set_nested(table, "models", "auto_include", rules);
}

fn set_str(table: &mut Table, key: &str, text: &str) {
    if text.is_empty() {
        table.remove(key);
    } else {
        put(table, key, value(text));
    }
}

/// Inserts `item` at `key`. A value replacing a value keeps the old one's
/// surrounding whitespace and trailing comment.
fn put(table: &mut dyn TableLike, key: &str, mut item: Item) {
    if let (Some(Item::Value(old)), Item::Value(new)) = (table.get(key), &mut item) {
        *new.decor_mut() = old.decor().clone();
    }
    table.insert(key, item);
}

/// An option value: a whole number, `true` or `false`, or text. Empty
/// removes the option.
fn option_value(text: &str) -> Option<Item> {
    if text.is_empty() {
        None
    } else if let Ok(number) = text.parse::<i64>() {
        Some(value(number))
    } else if let Ok(flag) = text.parse::<bool>() {
        Some(value(flag))
    } else {
        Some(value(text))
    }
}

/// Sets or removes `key` in the `section` sub-table, which may be written
/// as a table or inline. A section left empty by a removal is dropped.
fn set_nested(table: &mut Table, section: &str, key: &str, item: Option<Item>) {
    if item.is_some() && !table.contains_key(section) {
        table.insert(section, Item::Table(Table::new()));
    }
    let Some(inner) = table.get_mut(section).and_then(Item::as_table_like_mut) else {
        return;
    };
    match item {
        Some(item) => put(inner, key, item),
        None => {
            inner.remove(key);
        }
    }
    if TableLike::is_empty(inner) {
        table.remove(section);
    }
}

/// The config routes, so the conflict loop can be tested without a server.
pub(crate) trait ConfigApi {
    /// GET the config text and its revision.
    async fn get_config(&self) -> Result<ConfigResponse, ClientError>;
    /// POST a revision-checked apply.
    async fn post_apply(&self, request: ConfigApplyRequest) -> Result<ConfigResponse, ClientError>;
}

impl ConfigApi for Client {
    async fn get_config(&self) -> Result<ConfigResponse, ClientError> {
        self.config().await
    }

    async fn post_apply(&self, request: ConfigApplyRequest) -> Result<ConfigResponse, ClientError> {
        self.apply_config(request).await
    }
}

/// Why an edit failed: the edit itself, or the server.
#[derive(Debug)]
pub(crate) enum EditError {
    Edit(String),
    Client(ClientError),
}

impl From<EditError> for String {
    fn from(error: EditError) -> Self {
        match error {
            EditError::Edit(message) => message,
            EditError::Client(error) => failure(error),
        }
    }
}

/// What an applied edit did: `None` when the text was already as asked,
/// so nothing was sent.
pub(crate) type Applied = Option<ConfigResponse>;

/// GETs the config, edits it, and applies it against that revision. On a
/// revision conflict the edit is rebuilt on a fresh GET, up to
/// [`EDIT_ATTEMPTS`] sends in all.
pub(crate) async fn apply_edit(api: &impl ConfigApi, edit: &Edit) -> Result<Applied, EditError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let current = api.get_config().await.map_err(EditError::Client)?;
        let text = edit_text(&current.text, edit).map_err(EditError::Edit)?;
        if text == current.text {
            return Ok(None);
        }
        match api
            .post_apply(ConfigApplyRequest::new(current.disk_revision, text))
            .await
        {
            Err(error) if attempt < EDIT_ATTEMPTS && is_conflict(&error) => {}
            Err(error) => return Err(EditError::Client(error)),
            Ok(response) => return Ok(Some(response)),
        }
    }
}

fn is_conflict(error: &ClientError) -> bool {
    matches!(error, ClientError::Server { status, .. } if status.as_u16() == 409)
}

fn report(done: &str, id: &str, applied: &Applied) -> String {
    match applied {
        None => format!("provider `{id}` unchanged"),
        Some(response) => {
            let mut out = format!(
                "{done} provider `{id}` (revision {})",
                response.disk_revision
            );
            for field in &response.pending_restart {
                out.push_str(&format!("\n  {field}: takes effect after a restart"));
            }
            push_issues(&mut out, &response.not_applied);
            out
        }
    }
}

async fn picks_after(
    client: &Client,
    id: &str,
    models: Option<&[String]>,
) -> Result<String, String> {
    let Some(models) = models else {
        return Ok(String::new());
    };
    let args = if models.is_empty() {
        vec!["-".to_owned()]
    } else {
        models.to_vec()
    };
    let (before, after) = edit_picks(client, id, &args).await.map_err(failure)?;
    Ok(format!(
        "\npicks: {} -> {}",
        picks_text(&before.picks),
        picks_text(&after.picks)
    ))
}

/// `hofund provider add <id> [<key>] [k=v...]`.
pub(crate) async fn add(config: &Path, id: &str, pairs: &Pairs) -> Result<String, String> {
    let client = client(config).await?;
    let edit = Edit::Add {
        id: id.to_owned(),
        fields: pairs.fields.clone(),
    };
    let applied = apply_edit(&client, &edit).await?;
    let mut out = report("added", id, &applied);
    out.push_str(&picks_after(&client, id, pairs.models.as_deref()).await?);
    out.push_str(&format!(
        "\nhofund provider refresh {id} fetches its model list"
    ));
    Ok(out)
}

/// `hofund provider set <id> k=v...`. `id=` never gets here: parsing
/// refuses it.
pub(crate) async fn set(config: &Path, id: &str, pairs: &Pairs) -> Result<String, String> {
    let client = client(config).await?;
    let mut out = String::new();
    if !pairs.fields.is_empty() || pairs.models.is_none() {
        let edit = Edit::Set {
            id: id.to_owned(),
            fields: pairs.fields.clone(),
        };
        out = report("saved", id, &apply_edit(&client, &edit).await?);
    }
    let picks = picks_after(&client, id, pairs.models.as_deref()).await?;
    out.push_str(if out.is_empty() {
        picks.trim_start_matches('\n')
    } else {
        &picks
    });
    Ok(out)
}

/// `hofund provider key <id> <key>`. The key is never printed.
pub(crate) async fn key(config: &Path, id: &str, key: &str) -> Result<String, String> {
    let client = client(config).await?;
    let edit = Edit::Set {
        id: id.to_owned(),
        fields: vec![Field {
            key: FieldKey::ApiKey,
            value: key.trim().to_owned(),
        }],
    };
    let applied = apply_edit(&client, &edit).await?;
    Ok(report("saved the key of", id, &applied))
}

/// `hofund provider rm <id>` (also `remove`, `delete`). Agents set to its
/// models are not moved: a route that still names it fails the apply.
pub(crate) async fn remove(config: &Path, id: &str) -> Result<String, String> {
    let client = client(config).await?;
    let applied = apply_edit(&client, &Edit::Remove { id: id.to_owned() }).await?;
    Ok(report("removed", id, &applied))
}

/// The provider table `id` names in `text`.
fn provider_table<'a>(document: &'a DocumentMut, id: &str) -> Result<&'a Table, String> {
    document
        .get("providers")
        .and_then(Item::as_array_of_tables)
        .and_then(|providers| {
            providers
                .iter()
                .find(|table| table.get("id").and_then(Item::as_str) == Some(id))
        })
        .ok_or_else(|| unknown(id))
}

fn models_field<'a>(table: &'a Table, key: &str) -> Option<&'a Item> {
    table.get("models")?.as_table_like()?.get(key)
}

async fn read_document(client: &Client) -> Result<DocumentMut, String> {
    let current = client.config().await.map_err(failure)?;
    current
        .text
        .parse::<DocumentMut>()
        .map_err(|error| format!("the config is not valid TOML: {error}"))
}

async fn read_table(client: &Client, id: &str) -> Result<DocumentMut, String> {
    let document = read_document(client).await?;
    provider_table(&document, id)?;
    Ok(document)
}

fn entry_table<'a>(document: &'a DocumentMut, id: &str) -> Result<&'a Table, String> {
    ["providers", "subscriptions"]
        .into_iter()
        .find_map(|kind| {
            document
                .get(kind)
                .and_then(Item::as_array_of_tables)
                .and_then(|entries| {
                    entries
                        .iter()
                        .find(|table| table.get("id").and_then(Item::as_str) == Some(id))
                })
        })
        .ok_or_else(|| unknown(id))
}

/// `hofund provider at-once <id> [n|off|default]`: show or set how many
/// accounts or keys take requests, with the rest in reserve.
pub(crate) async fn at_once(
    config: &Path,
    id: &str,
    value: Option<&str>,
) -> Result<String, String> {
    let value = value.map(parse_at_once).transpose()?;
    let client = client(config).await?;
    let mut out = String::new();
    if let Some(value) = value {
        let edit = Edit::AtOnce {
            id: id.to_owned(),
            value,
        };
        out = report("saved", id, &apply_edit(&client, &edit).await?);
        out.push('\n');
    }
    let document = read_document(&client).await?;
    let configured = entry_table(&document, id)?
        .get("at_once")
        .and_then(Item::as_integer)
        .and_then(|value| u64::try_from(value).ok());
    out.push_str(&at_once_line(id, configured));
    Ok(out)
}

fn parse_at_once(value: &str) -> Result<Option<u64>, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "default" | "plugin" => Ok(None),
        "off" | "none" | "no" | "-" => Ok(Some(0)),
        _ => {
            let limit = value.trim().parse::<u64>().map_err(|_| {
                format!("accounts at once is a whole number, off or default, not `{value}`")
            })?;
            if limit > hofund_config::MAX_AT_ONCE {
                return Err(format!(
                    "accounts at once is 0 to {}, not {limit}",
                    hofund_config::MAX_AT_ONCE
                ));
            }
            Ok(Some(limit))
        }
    }
}

fn at_once_line(id: &str, configured: Option<u64>) -> String {
    match configured {
        Some(0) => format!("{id}: every account or key takes requests"),
        Some(limit) => format!(
            "{id}: {limit} accounts or keys take requests at once; the others wait in reserve"
        ),
        None => format!("{id}: accounts at once follows the plugin default"),
    }
}

/// `hofund provider refresh-every <id> [<interval>|off]`: show or set how
/// often the daemon refetches the model list.
pub(crate) async fn refresh_every(
    config: &Path,
    id: &str,
    interval: Option<&str>,
) -> Result<String, String> {
    let client = client(config).await?;
    let mut out = String::new();
    if let Some(interval) = interval {
        let edit = Edit::Set {
            id: id.to_owned(),
            fields: vec![Field {
                key: FieldKey::Refresh,
                value: interval.trim().to_owned(),
            }],
        };
        out = report("saved", id, &apply_edit(&client, &edit).await?);
        out.push('\n');
    }
    let document = read_table(&client, id).await?;
    let table = provider_table(&document, id)?;
    let refresh = models_field(table, "refresh")
        .and_then(Item::as_str)
        .unwrap_or_default();
    out.push_str(&refresh_line(id, refresh));
    Ok(out)
}

fn refresh_line(id: &str, refresh: &str) -> String {
    let list = ModelList {
        refresh: refresh.to_owned(),
        ..ModelList::default()
    };
    if list.refresh_interval().is_some() {
        format!("{id}: the daemon refetches its model list every {refresh}")
    } else {
        format!("{id}: its model list is fetched only when asked (hofund provider refresh {id})")
    }
}

/// `hofund provider auto-include <id> [<rule>...|none]`: show or set the
/// rules newly listed models are picked by.
pub(crate) async fn auto_include(
    config: &Path,
    id: &str,
    rules: &[String],
) -> Result<String, String> {
    let client = client(config).await?;
    let mut out = String::new();
    if !rules.is_empty() {
        let edit = Edit::Set {
            id: id.to_owned(),
            fields: vec![Field {
                key: FieldKey::AutoIncludeRules(rules.to_vec()),
                value: String::new(),
            }],
        };
        out = report("saved", id, &apply_edit(&client, &edit).await?);
        out.push('\n');
    }
    let document = read_table(&client, id).await?;
    let table = provider_table(&document, id)?;
    let rules = models_field(table, "auto_include")
        .and_then(Item::as_array)
        .map(|rules| {
            rules
                .iter()
                .filter_map(|rule| rule.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    out.push_str(&auto_include_line(id, &rules));
    Ok(out)
}

fn auto_include_line(id: &str, rules: &[&str]) -> String {
    if rules.is_empty() {
        format!("{id}: no auto-include rules; a newly listed model is picked only by hand")
    } else {
        format!(
            "{id}: a newly listed model is picked when one of these matches its id: {}",
            rules.join(" ")
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hofund_control_client::ClientError;
    use hofund_control_client::hofund_control_proto::{
        ConfigApplyRequest, ConfigResponse, ControlError, ControlErrorKind, ErrorEnvelope,
    };

    use super::{
        ConfigApi, EDIT_ATTEMPTS, Edit, EditError, Field, FieldKey, RENAME_UNSUPPORTED, apply_edit,
        at_once_line, auto_include_line, edit_text, parse_at_once, parse_pairs, refresh_line,
    };

    const BASE: &str = "# top\nlisten = \"127.0.0.1:0\"\n\n# the chat one\n[[providers]]\n\
id = \"chat\" # main\nbase_url = \"http://x\"\n[providers.models]\nrefresh = \"off\" # by hand\n\n\
[[providers]]\nid = \"other\"\nmodels = { refresh = \"6h\" }\n\n[[routes]]\nmodel = \"m\"\n";

    fn field(key: FieldKey, value: &str) -> Field {
        Field {
            key,
            value: value.to_owned(),
        }
    }

    fn set(id: &str, fields: Vec<Field>) -> Edit {
        Edit::Set {
            id: id.to_owned(),
            fields,
        }
    }

    #[test]
    fn d26_7_at_once_cmd_parses_and_writes_number_off_and_default() {
        assert_eq!(parse_at_once("2"), Ok(Some(2)));
        assert_eq!(parse_at_once("off"), Ok(Some(0)));
        assert_eq!(parse_at_once("default"), Ok(None));
        assert_eq!(parse_at_once("plugin"), Ok(None));
        for bad in ["-1", "1001", "lots"] {
            assert!(parse_at_once(bad).is_err(), "{bad}");
        }
        let text = "# keep\n[[providers]]\nid = \"p\"\npreset = \"openai\"\n[[subscriptions]]\nid = \"sub\"\naccounts = \"*\"\n";
        let numbered = edit_text(
            text,
            &Edit::AtOnce {
                id: "p".to_owned(),
                value: Some(2),
            },
        )
        .expect("number");
        assert!(numbered.contains("at_once = 2"), "{numbered}");
        let off = edit_text(
            &numbered,
            &Edit::AtOnce {
                id: "p".to_owned(),
                value: Some(0),
            },
        )
        .expect("off");
        assert!(off.contains("at_once = 0"), "{off}");
        assert_eq!(
            at_once_line("p", Some(0)),
            "p: every account or key takes requests"
        );
        let subscription = edit_text(
            &off,
            &Edit::AtOnce {
                id: "sub".to_owned(),
                value: Some(1),
            },
        )
        .expect("subscription");
        assert_eq!(subscription.matches("at_once = 1").count(), 1);
        let default = edit_text(
            &subscription,
            &Edit::AtOnce {
                id: "p".to_owned(),
                value: None,
            },
        )
        .expect("default");
        assert!(!default.contains("at_once = 0"), "{default}");
        assert!(default.contains("at_once = 1"), "{default}");
        assert!(default.starts_with("# keep\n"), "{default}");
        assert_eq!(
            at_once_line("p", None),
            "p: accounts at once follows the plugin default"
        );
    }

    #[test]
    fn provider_edit_preserves_comments() {
        let edited = edit_text(
            BASE,
            &set(
                "chat",
                vec![
                    field(FieldKey::Quirks, "deepseek"),
                    field(FieldKey::Refresh, "1d"),
                ],
            ),
        )
        .expect("edit");
        assert_eq!(
            edited,
            BASE.replace(
                "base_url = \"http://x\"\n",
                "base_url = \"http://x\"\nquirks = \"deepseek\"\n"
            )
            .replace("refresh = \"off\" # by hand", "refresh = \"1d\" # by hand")
        );
        // Removing the other provider keeps the first one's comments.
        let removed = edit_text(
            BASE,
            &Edit::Remove {
                id: "other".to_owned(),
            },
        )
        .expect("remove");
        assert_eq!(
            removed,
            BASE.replace(
                "[[providers]]\nid = \"other\"\nmodels = { refresh = \"6h\" }\n\n",
                ""
            )
        );
    }

    #[test]
    fn edits_cover_every_field_and_remove_emptied_sections() {
        let fields = vec![
            field(FieldKey::Preset, "openai"),
            field(FieldKey::Protocol, "openai-chat"),
            field(FieldKey::BaseUrl, ""),
            field(FieldKey::ApiKey, "env:K"),
            field(FieldKey::MaxOutputTokens, "64"),
            field(FieldKey::Header("X-A".to_owned()), "1"),
            field(FieldKey::Option("n".to_owned()), "5"),
            field(FieldKey::Option("b".to_owned()), "true"),
            field(FieldKey::Option("s".to_owned()), "text"),
            field(FieldKey::AutoInclude, "a*, re:b"),
            field(FieldKey::ModelsUrl, "http://list"),
        ];
        let edited = edit_text(BASE, &set("chat", fields)).expect("edit");
        let expected = "# top\nlisten = \"127.0.0.1:0\"\n\n# the chat one\n[[providers]]\n\
id = \"chat\" # main\npreset = \"openai\"\nprotocol = \"openai-chat\"\napi_key = \"env:K\"\n\
max_output_tokens = 64\n[providers.models]\nrefresh = \"off\" # by hand\nauto_include = [\"a*\", \"re:b\"]\n\
url = \"http://list\"\n\n[providers.headers]\nX-A = \"1\"\n\n[providers.options]\nn = 5\nb = true\ns = \"text\"\n\n\
[[providers]]\nid = \"other\"\nmodels = { refresh = \"6h\" }\n\n[[routes]]\nmodel = \"m\"\n";
        assert_eq!(edited, expected);

        // Clearing every added field gives the original text back.
        let cleared = vec![
            field(FieldKey::Preset, ""),
            field(FieldKey::Protocol, ""),
            field(FieldKey::BaseUrl, "http://x"),
            field(FieldKey::ApiKey, ""),
            field(FieldKey::MaxOutputTokens, ""),
            field(FieldKey::Header("X-A".to_owned()), ""),
            field(FieldKey::Option("n".to_owned()), ""),
            field(FieldKey::Option("b".to_owned()), ""),
            field(FieldKey::Option("s".to_owned()), ""),
            field(FieldKey::AutoInclude, "none"),
            field(FieldKey::ModelsUrl, ""),
        ];
        let back = edit_text(&edited, &set("chat", cleared)).expect("clear");
        assert!(
            !back.contains("headers") && !back.contains("options"),
            "{back}"
        );
        assert!(
            !back.contains("preset") && !back.contains("auto_include"),
            "{back}"
        );
        assert!(back.contains("base_url = \"http://x\"\n"), "{back}");

        // An inline models table is edited in place, and dropped when empty.
        let inline =
            edit_text(BASE, &set("other", vec![field(FieldKey::Refresh, "1d")])).expect("inline");
        assert!(
            inline.contains("models = { refresh = \"1d\" }\n"),
            "{inline}"
        );
        let inline =
            edit_text(BASE, &set("other", vec![field(FieldKey::Refresh, "")])).expect("inline");
        assert!(
            inline.contains("[[providers]]\nid = \"other\"\n\n[[routes]]"),
            "{inline}"
        );
        // Removing a field that is absent changes nothing.
        assert_eq!(
            edit_text(
                BASE,
                &set("other", vec![field(FieldKey::Header("H".to_owned()), "")])
            )
            .expect("noop"),
            BASE
        );
        // Whole rules keep their commas.
        let rules = edit_text(
            BASE,
            &set(
                "chat",
                vec![field(
                    FieldKey::AutoIncludeRules(vec!["re:a{1,2}".to_owned(), "b".to_owned()]),
                    "",
                )],
            ),
        )
        .expect("rules");
        assert!(
            rules.contains("auto_include = [\"re:a{1,2}\", \"b\"]\n"),
            "{rules}"
        );
        for none in [&[][..], &["off".to_owned()], &["none".to_owned()]] {
            let cleared = edit_text(
                &rules,
                &set(
                    "chat",
                    vec![field(FieldKey::AutoIncludeRules(none.to_vec()), "")],
                ),
            )
            .expect("cleared");
            assert!(!cleared.contains("auto_include"), "{none:?}: {cleared}");
        }
    }

    #[test]
    fn add_and_remove_find_the_entry_by_id() {
        let added = edit_text(
            BASE,
            &Edit::Add {
                id: "new".to_owned(),
                fields: vec![field(FieldKey::Preset, "openai")],
            },
        )
        .expect("add");
        // The new entry follows the last provider, before the routes.
        assert_eq!(
            added,
            BASE.replace(
                "[[routes]]",
                "[[providers]]\nid = \"new\"\npreset = \"openai\"\n\n[[routes]]"
            )
        );
        let duplicate = Edit::Add {
            id: "chat".to_owned(),
            fields: Vec::new(),
        };
        assert_eq!(
            edit_text(BASE, &duplicate).expect_err("duplicate"),
            "provider `chat` already exists; hofund provider set chat k=v... edits it"
        );
        // An add creates the array in a file with none.
        let first = edit_text(
            "listen = \"x\"\n",
            &Edit::Add {
                id: "a".to_owned(),
                fields: Vec::new(),
            },
        )
        .expect("first");
        assert_eq!(first, "listen = \"x\"\n\n[[providers]]\nid = \"a\"\n");
        for edit in [
            set("ghost", Vec::new()),
            Edit::Remove {
                id: "ghost".to_owned(),
            },
        ] {
            assert_eq!(
                edit_text(BASE, &edit).expect_err("ghost"),
                "unknown provider `ghost`"
            );
        }
        assert_eq!(
            edit_text("listen = \"x\"\n", &set("a", Vec::new())).expect_err("none"),
            "the config has no [[providers]]"
        );
        assert!(
            edit_text("providers = [{ id = \"a\" }]\n", &set("a", Vec::new()))
                .expect_err("inline")
                .contains("not written as [[providers]] tables")
        );
        assert!(
            edit_text("listen = ", &set("a", Vec::new()))
                .expect_err("toml")
                .starts_with("the config is not valid TOML")
        );
    }

    #[test]
    fn pairs_parse_fields_models_and_refuse_id() {
        let pairs = parse_pairs(
            &[
                "URL=http://x",
                "key=env:K",
                "Header.X-Case=v",
                "options.Mode=fast",
                "models.refresh=6h",
                "auto-include=a*,b",
                "models.url=http://l",
                "max_output_tokens=7",
                "models=a, b,",
                "quirks= q ",
                "preset=p",
                "protocol=openai-chat",
                "headers.Y=1",
                "option.z=2",
                "refresh=1d",
                "auto_include=c",
                "models.auto_include=d",
            ],
            "set",
        )
        .expect("pairs");
        assert_eq!(pairs.models, Some(vec!["a".to_owned(), "b".to_owned()]));
        assert_eq!(
            pairs.fields,
            vec![
                field(FieldKey::BaseUrl, "http://x"),
                field(FieldKey::ApiKey, "env:K"),
                field(FieldKey::Header("X-Case".to_owned()), "v"),
                field(FieldKey::Option("Mode".to_owned()), "fast"),
                field(FieldKey::Refresh, "6h"),
                field(FieldKey::AutoInclude, "a*,b"),
                field(FieldKey::ModelsUrl, "http://l"),
                field(FieldKey::MaxOutputTokens, "7"),
                field(FieldKey::Quirks, "q"),
                field(FieldKey::Preset, "p"),
                field(FieldKey::Protocol, "openai-chat"),
                field(FieldKey::Header("Y".to_owned()), "1"),
                field(FieldKey::Option("z".to_owned()), "2"),
                field(FieldKey::Refresh, "1d"),
                field(FieldKey::AutoInclude, "c"),
                field(FieldKey::AutoInclude, "d"),
            ]
        );
        assert_eq!(
            parse_pairs(&["models="], "set").expect("empty").models,
            Some(Vec::new())
        );
        for pairs in [
            &["id=x"][..],
            &["models=a", "id=x"],
            &["quirks=q", " Id =x"],
        ] {
            assert_eq!(
                parse_pairs(pairs, "set").expect_err("id"),
                RENAME_UNSUPPORTED
            );
        }
        assert_eq!(
            parse_pairs(&["id=x"], "add").expect_err("id"),
            "the provider id is the first argument of provider add"
        );
        assert!(
            parse_pairs(&["name=x"], "set")
                .expect_err("unknown")
                .starts_with("unknown provider field `name`; fields: preset")
        );
        for bad in ["header.=v", "option.=v"] {
            assert!(
                parse_pairs(&[bad], "set")
                    .expect_err(bad)
                    .starts_with("unknown provider field")
            );
        }
        assert_eq!(
            parse_pairs(&["quirks"], "set").expect_err("bare"),
            "expected key=value, got `quirks`"
        );
        for bad in [
            "max_output_tokens=-1",
            "max_output_tokens=x",
            "max_output_tokens=99999999999999999999",
        ] {
            assert!(
                parse_pairs(&[bad], "set")
                    .expect_err(bad)
                    .starts_with("max_output_tokens must be a whole number"),
                "{bad}"
            );
        }
        assert_eq!(
            parse_pairs(&["max_output_tokens="], "set")
                .expect("clear")
                .fields,
            vec![field(FieldKey::MaxOutputTokens, "")]
        );
        assert_eq!(
            parse_pairs(&["max_output_tokens=0"], "set")
                .expect("zero")
                .fields,
            vec![field(FieldKey::MaxOutputTokens, "0")]
        );
    }

    #[test]
    fn show_lines_read_the_refresh_and_rules() {
        assert_eq!(
            refresh_line("p", "6h"),
            "p: the daemon refetches its model list every 6h"
        );
        for off in ["", "off", "1s"] {
            assert_eq!(
                refresh_line("p", off),
                "p: its model list is fetched only when asked (hofund provider refresh p)"
            );
        }
        assert_eq!(
            auto_include_line("p", &[]),
            "p: no auto-include rules; a newly listed model is picked only by hand"
        );
        assert_eq!(
            auto_include_line("p", &["a", "b"]),
            "p: a newly listed model is picked when one of these matches its id: a b"
        );
    }

    /// A config server another writer edits before each of the first
    /// `races` applies land.
    struct Racing {
        state: Mutex<(String, u32)>,
        races: Mutex<usize>,
        gets: Mutex<usize>,
        applies: Mutex<Vec<ConfigApplyRequest>>,
    }

    impl Racing {
        fn new(races: usize) -> Self {
            Self {
                state: Mutex::new((BASE.to_owned(), 1)),
                races: Mutex::new(races),
                gets: Mutex::new(0),
                applies: Mutex::new(Vec::new()),
            }
        }

        fn response(state: &(String, u32)) -> ConfigResponse {
            let mut response = ConfigResponse::default();
            response.text = state.0.clone();
            response.disk_revision = format!("r{}", state.1);
            response
        }
    }

    fn conflict() -> ClientError {
        let mut error = ErrorEnvelope::default();
        error.error = ControlError::new(ControlErrorKind::Conflict, "stale revision", "req");
        ClientError::Server {
            status: http::StatusCode::CONFLICT,
            error,
        }
    }

    impl ConfigApi for Racing {
        async fn get_config(&self) -> Result<ConfigResponse, ClientError> {
            *self.gets.lock().expect("gets") += 1;
            Ok(Self::response(&self.state.lock().expect("state")))
        }

        async fn post_apply(
            &self,
            request: ConfigApplyRequest,
        ) -> Result<ConfigResponse, ClientError> {
            self.applies.lock().expect("applies").push(request.clone());
            let mut state = self.state.lock().expect("state");
            {
                let mut races = self.races.lock().expect("races");
                if *races > 0 {
                    *races -= 1;
                    // Another writer adds a comment by hand first.
                    let next = format!("{}# hand edit {}\n", state.0, state.1);
                    state.0 = next;
                    state.1 += 1;
                }
            }
            if request.expected_revision != format!("r{}", state.1) {
                return Err(conflict());
            }
            state.0 = request.text;
            state.1 += 1;
            Ok(Self::response(&state))
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    fn quirks() -> Edit {
        set("chat", vec![field(FieldKey::Quirks, "q")])
    }

    #[test]
    fn provider_edit_409_refetches_and_reapplies() {
        // Two conflicts: the third send applies the edit to the newest text.
        let api = Racing::new(2);
        let applied = block_on(apply_edit(&api, &quirks()))
            .expect("edit")
            .expect("applied");
        assert_eq!(applied.disk_revision, "r4");
        assert!(
            applied.text.contains("quirks = \"q\"\n"),
            "{}",
            applied.text
        );
        assert!(applied.text.contains("# hand edit 1\n# hand edit 2\n"));
        assert_eq!(*api.gets.lock().expect("gets"), 3);
        let applies = api.applies.lock().expect("applies");
        let revisions = applies
            .iter()
            .map(|apply| apply.expected_revision.as_str())
            .collect::<Vec<_>>();
        assert_eq!(revisions, ["r1", "r2", "r3"]);
        drop(applies);

        // A conflict on every send is final after the bound.
        let api = Racing::new(EDIT_ATTEMPTS);
        let error = block_on(apply_edit(&api, &quirks())).expect_err("conflict");
        assert!(
            matches!(&error, EditError::Client(error) if super::is_conflict(error)),
            "{error:?}"
        );
        assert_eq!(api.applies.lock().expect("applies").len(), EDIT_ATTEMPTS);
        assert_eq!(*api.gets.lock().expect("gets"), EDIT_ATTEMPTS);

        // One conflict short of the bound still lands.
        let api = Racing::new(EDIT_ATTEMPTS - 1);
        assert!(
            block_on(apply_edit(&api, &quirks()))
                .expect("edit")
                .is_some()
        );

        // An edit already in place sends nothing.
        let api = Racing::new(0);
        let unchanged = set("chat", vec![field(FieldKey::Refresh, "off")]);
        assert!(
            block_on(apply_edit(&api, &unchanged))
                .expect("noop")
                .is_none()
        );
        assert!(api.applies.lock().expect("applies").is_empty());

        // An edit that cannot be made sends nothing.
        let error = block_on(apply_edit(&api, &set("ghost", Vec::new()))).expect_err("ghost");
        assert_eq!(String::from(error), "unknown provider `ghost`");
        assert!(api.applies.lock().expect("applies").is_empty());
    }

    #[test]
    fn config_errors_other_than_conflict_are_not_retried() {
        struct Refusing(Mutex<usize>);
        impl ConfigApi for Refusing {
            async fn get_config(&self) -> Result<ConfigResponse, ClientError> {
                let mut response = ConfigResponse::default();
                response.text = BASE.to_owned();
                Ok(response)
            }
            async fn post_apply(
                &self,
                _request: ConfigApplyRequest,
            ) -> Result<ConfigResponse, ClientError> {
                *self.0.lock().expect("applies") += 1;
                let mut error = ErrorEnvelope::default();
                error.error =
                    ControlError::new(ControlErrorKind::Unprocessable, "bad field", "req");
                Err(ClientError::Server {
                    status: http::StatusCode::UNPROCESSABLE_ENTITY,
                    error,
                })
            }
        }
        let api = Refusing(Mutex::new(0));
        let error = block_on(apply_edit(&api, &quirks())).expect_err("refused");
        assert_eq!(String::from(error), "bad field");
        assert_eq!(*api.0.lock().expect("applies"), 1);
    }
}
