//! Compiled catalog composition and the shared editing engine.
//!
//! Paths and keys were checked against each tool's own docs on 2026-10-24.
//! The citations sit on each shipped description.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use hofund_plugin_api::{
    AgentDescription, AgentFile, AgentRoute, FileKind as ApiFileKind, ModelField, ModelValue,
    Slot as ApiSlot, Visible,
};

use crate::edit::{Assignment, FileKind, get, is_empty_document, remove_keys, render, string_at};
use crate::store::{
    backup_once, clear_markers, created_here, delete_verified, marker_present, read_optional,
    remove_empty_parents, revert_path, unified_diff, write_atomic, write_verified,
};
use crate::{
    AgentAdapter, AgentError, ConfigFile, ConnectionStatus, FileChange, GatewayTarget, Plan,
    Session, TargetOs,
};
use crate::{DisconnectOutcome, usable_override};

#[derive(Clone, Copy)]
struct ModelKey {
    file: &'static str,
    key: &'static str,
}

/// An adapter backed by a compiled static agent description.
#[derive(Clone, Copy, Debug)]
pub struct Compiled {
    description: &'static AgentDescription,
    os: TargetOs,
}

impl Compiled {
    /// Uses the path templates for the current operating system.
    #[must_use]
    pub const fn new(description: &'static AgentDescription) -> Self {
        Self {
            description,
            os: TargetOs::current(),
        }
    }

    /// Selects path templates for `os`, independently of the host system.
    #[must_use]
    pub const fn with_os(mut self, os: TargetOs) -> Self {
        self.os = os;
        self
    }

    fn engine(&self, session: &Session) -> Result<Engine, AgentError> {
        Engine::compiled(self.description, self.os, session)
    }
}

const BUILTIN_ORDER: [&str; 4] = ["claude-code", "codex", "gemini-cli", "opencode"];

/// Compiled adapters in catalog order.
pub struct Catalog {
    adapters: Vec<Box<dyn AgentAdapter>>,
}

impl Catalog {
    /// All adapters in catalog order.
    #[must_use]
    pub fn adapters(&self) -> Vec<&dyn AgentAdapter> {
        self.adapters.iter().map(Box::as_ref).collect()
    }

    /// Finds an adapter by its stable id.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&dyn AgentAdapter> {
        self.adapters()
            .into_iter()
            .find(|adapter| adapter.id() == name)
    }
}

/// Composes compiled adapters in CLI order and rejects duplicate ids.
///
/// Every built-in (claude-code, codex, gemini-cli, opencode) comes from the
/// compiled list supplied by the daemon's `agent_descriptions()`. Other
/// compiled ids follow the four built-in slots in input order.
///
/// # Errors
///
/// Returns an error when two compiled descriptions share an id, or when a
/// built-in has no compiled description.
pub fn catalog(compiled: &'static [&'static AgentDescription]) -> Result<Catalog, AgentError> {
    compose(compiled, builtin_adapter)
}

fn compose(
    compiled: &'static [&'static AgentDescription],
    fallback: fn(&str) -> Option<Box<dyn AgentAdapter>>,
) -> Result<Catalog, AgentError> {
    let mut ids = BTreeSet::new();
    for description in compiled {
        if !ids.insert(description.id().as_str().to_owned()) {
            return Err(AgentError::message(format!(
                "agent id `{}` collides with another catalog entry",
                description.id().as_str()
            )));
        }
    }

    let mut composed: Vec<Box<dyn AgentAdapter>> = Vec::new();
    for id in BUILTIN_ORDER {
        if let Some(description) = compiled
            .iter()
            .find(|description| description.id().as_str() == id)
        {
            composed.push(Box::new(Compiled::new(description)));
            continue;
        }
        let Some(adapter) = fallback(id) else {
            return Err(AgentError::message(format!(
                "built-in agent `{id}` has no compiled description"
            )));
        };
        composed.push(adapter);
    }
    for description in compiled
        .iter()
        .filter(|description| !BUILTIN_ORDER.contains(&description.id().as_str()))
    {
        composed.push(Box::new(Compiled::new(description)));
    }
    Ok(Catalog { adapters: composed })
}

fn builtin_adapter(_id: &str) -> Option<Box<dyn AgentAdapter>> {
    None
}

/// Resolves one compiled file location against a session.
///
/// Empty and relative environment overrides are treated as unset, so the
/// default template is used.
#[must_use]
pub fn resolve(file: &AgentFile, os: TargetOs, session: &Session) -> PathBuf {
    let template = match os {
        TargetOs::Linux => file.linux(),
        TargetOs::Macos => file.macos(),
        TargetOs::Windows => file.windows(),
    };
    let override_value = file.env_override().and_then(|name| {
        session
            .env(name)
            .filter(|value| usable_override(value))
            .map(PathBuf::from)
    });
    let (root, remainder) = if let Some(value) = override_value {
        let prefix = file.env_replaces().unwrap_or_else(|| {
            template
                .rsplit_once('/')
                .map_or("~", |(directory, _)| directory)
        });
        let remainder = template
            .strip_prefix(prefix)
            .unwrap_or(template)
            .trim_start_matches('/');
        (value, remainder)
    } else {
        (
            session.home.clone(),
            template
                .strip_prefix("~")
                .unwrap_or(template)
                .trim_start_matches('/'),
        )
    };
    remainder
        .split('/')
        .filter(|segment| !segment.is_empty())
        .fold(root, |path, segment| path.join(segment))
}

struct EngineLocation {
    file: &'static str,
    path: PathBuf,
    kind: FileKind,
    create: bool,
}

struct EngineSet {
    file: &'static str,
    key: &'static str,
    slot: ApiSlot,
}

struct Engine {
    id: &'static str,
    display_name: &'static str,
    binary: &'static str,
    locations: Vec<EngineLocation>,
    sets: Vec<EngineSet>,
    model: Option<ModelKey>,
    model_prefix: Option<&'static str>,
    visible: Visible,
    route: AgentRoute,
}

/// What an apply does with the agent's model setting.
#[derive(Clone, Copy)]
enum Choice<'a> {
    /// Keep a model the user picked; fill it only when it is unset.
    Keep,
    /// Write this route model.
    Set(&'a str),
    /// Drop the model so the connect default applies.
    Clear,
}

/// One managed file as an apply would write it.
struct Rendered {
    path: PathBuf,
    kind: FileKind,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
    assignments: Vec<Assignment>,
    /// Model keys whose snapshot value must match the written file.
    pinned: Vec<(&'static str, Option<String>)>,
}

impl Engine {
    fn compiled(
        description: &'static AgentDescription,
        os: TargetOs,
        session: &Session,
    ) -> Result<Self, AgentError> {
        let locations = description
            .files()
            .iter()
            .map(|file| {
                let kind = match file.kind() {
                    ApiFileKind::Json => FileKind::Json,
                    ApiFileKind::Toml => FileKind::Toml,
                    ApiFileKind::Env => FileKind::Env,
                    ApiFileKind::Yaml => FileKind::Yaml,
                    _ => {
                        return Err(AgentError::message(format!(
                            "agent `{}` file `{}` has an unsupported file kind",
                            description.id().as_str(),
                            file.id()
                        )));
                    }
                };
                Ok(EngineLocation {
                    file: file.id(),
                    path: resolve(file, os, session),
                    kind,
                    create: file.create(),
                })
            })
            .collect::<Result<Vec<_>, AgentError>>()?;
        let sets = description
            .sets()
            .iter()
            .map(|set| EngineSet {
                file: set.file(),
                key: set.key(),
                slot: set.slot().clone(),
            })
            .collect();
        let model_file = description.model_file().or_else(|| {
            let mut ids = description.files().iter().map(AgentFile::id);
            let first = ids.next()?;
            ids.all(|id| id == first).then_some(first)
        });
        let model = description
            .model_key()
            .zip(model_file)
            .map(|(key, file)| ModelKey { file, key });
        Ok(Self {
            id: description.id().as_str(),
            display_name: description.display_name(),
            binary: description.binary(),
            locations,
            sets,
            model,
            model_prefix: description.model_prefix(),
            visible: description.visible(),
            route: description.route(),
        })
    }

    fn detect(&self, session: &Session) -> bool {
        binary_on_path(session, self.binary)
            || self
                .locations
                .iter()
                .any(|location| location.path.is_file())
    }

    fn read(&self) -> Result<Vec<ConfigFile>, AgentError> {
        let mut files = Vec::new();
        for chosen in self.chosen()? {
            let bytes = read_optional(&chosen.path)?;
            files.push(ConfigFile {
                path: chosen.path,
                bytes,
            });
        }
        Ok(files)
    }

    fn plan(&self, target: &GatewayTarget) -> Result<Plan, AgentError> {
        Ok(plan_of(&self.render_all(target, Choice::Keep)?))
    }

    fn apply(&self, target: &GatewayTarget) -> Result<Plan, AgentError> {
        self.apply_with(target, Choice::Keep)
    }

    fn apply_with(&self, target: &GatewayTarget, choice: Choice<'_>) -> Result<Plan, AgentError> {
        let rendered = self.render_all(target, choice)?;
        let plan = plan_of(&rendered);
        for change in plan.changes() {
            backup_once(change.path())?;
        }
        for change in plan.changes() {
            write_atomic(change.path(), change.after())?;
        }
        for file in rendered {
            if marker_present(&file.path) {
                snapshot_wrote(&file.path, file.kind, &file.assignments, &file.pinned)?;
            }
        }
        Ok(plan)
    }

    fn set_model(&self, target: &GatewayTarget, model: Option<&str>) -> Result<Plan, AgentError> {
        let Some(key) = self.model else {
            return Err(AgentError::rejected(format!(
                "`{}` has no model setting",
                self.id
            )));
        };
        let Some(id) = model else {
            return self.clear_model(target, key);
        };
        if !self.models(target).iter().any(|route| route == id) {
            return Err(AgentError::rejected(format!(
                "`{id}` is not a route model `{}` can use; `hofund model list` shows them",
                self.id
            )));
        }
        self.apply_with(target, Choice::Set(id))
    }

    /// Drops the model key. A connected agent is re-applied so the snapshot
    /// follows; a disconnected one is edited in place without a backup.
    fn clear_model(&self, target: &GatewayTarget, model: ModelKey) -> Result<Plan, AgentError> {
        let chosen = self
            .chosen()?
            .into_iter()
            .find(|chosen| chosen.file == model.file)
            .ok_or_else(|| {
                AgentError::message(format!("`{}` has no config location", model.file))
            })?;
        if marker_present(&chosen.path) {
            return self.apply_with(target, Choice::Clear);
        }
        let Some(before) = read_optional(&chosen.path)? else {
            return Ok(Plan::new(Vec::new()));
        };
        let text = utf8(&chosen.path, &before)?;
        let current =
            get(chosen.kind, text, model.key).map_err(|error| annotate(&chosen.path, error))?;
        if current.is_none() {
            return Ok(Plan::new(Vec::new()));
        }
        let after = remove_keys(chosen.kind, text, &[model.key])
            .map_err(|error| annotate(&chosen.path, error))?;
        write_atomic(&chosen.path, &after)?;
        let diff = unified_diff(&chosen.path, &before, &after);
        Ok(Plan::new(vec![FileChange::new(
            chosen.path,
            before,
            after,
            diff,
        )]))
    }

    fn render_all(
        &self,
        target: &GatewayTarget,
        choice: Choice<'_>,
    ) -> Result<Vec<Rendered>, AgentError> {
        let mut rendered = Vec::new();
        for Chosen {
            file,
            path,
            kind,
            sets,
        } in self.chosen()?
        {
            let before = read_optional(&path)?;
            let text = match &before {
                Some(bytes) => Some(utf8(&path, bytes)?),
                None => None,
            };
            let model = self.model.filter(|model| model.file == file);
            let stripped = match (choice, model, text) {
                (Choice::Clear, Some(model), Some(text)) => {
                    let keys = self.model_keys(model);
                    let bytes =
                        remove_keys(kind, text, &keys).map_err(|error| annotate(&path, error))?;
                    Some(utf8(&path, &bytes)?.to_owned())
                }
                _ => None,
            };
            let base = stripped.as_deref().or(text);
            let mut assignments = self.assignments_for(&sets, kind, target, base, choice);
            if let (Choice::Set(id), Some(model)) = (choice, model)
                && !assignments
                    .iter()
                    .any(|assignment| assignment.key == model.key)
            {
                assignments.push(Assignment {
                    key: model.key,
                    value: self.prefixed(id),
                    raw_json: false,
                });
            }
            // Clear renders over the original text so the keys it keeps stay
            // in place, then drops the model keys no set wrote back.
            let mut after =
                render(kind, text, &assignments).map_err(|error| annotate(&path, error))?;
            if let (Choice::Clear, Some(model)) = (choice, model) {
                let leftover: Vec<&'static str> = self
                    .model_keys(model)
                    .into_iter()
                    .filter(|key| !assignments.iter().any(|assignment| assignment.key == *key))
                    .collect();
                let written = utf8(&path, &after)?.to_owned();
                after = remove_keys(kind, &written, &leftover)
                    .map_err(|error| annotate(&path, error))?;
            }
            let pinned = match model {
                Some(model) => self.pinned(&path, kind, text, &after, model, choice)?,
                None => Vec::new(),
            };
            rendered.push(Rendered {
                path,
                kind,
                before,
                after,
                assignments,
                pinned,
            });
        }
        Ok(rendered)
    }

    /// The model keys whose snapshot value follows the written file.
    ///
    /// Set and clear decide the model and its tiers. A plain connect keeps a
    /// model an earlier set or clear decided, which is when the live value
    /// still matches the previous snapshot.
    fn pinned(
        &self,
        path: &Path,
        kind: FileKind,
        text: Option<&str>,
        after: &[u8],
        model: ModelKey,
        choice: Choice<'_>,
    ) -> Result<Vec<(&'static str, Option<String>)>, AgentError> {
        let keys = match choice {
            Choice::Set(_) | Choice::Clear => self.model_keys(model),
            Choice::Keep => {
                let Some(earlier) = read_optional(&sibling_wrote(path))? else {
                    return Ok(Vec::new());
                };
                let earlier = utf8(path, &earlier)?;
                let current = match text {
                    Some(text) => {
                        get(kind, text, model.key).map_err(|error| annotate(path, error))?
                    }
                    None => None,
                };
                if get(kind, earlier, model.key).map_err(|error| annotate(path, error))? != current
                {
                    return Ok(Vec::new());
                }
                vec![model.key]
            }
        };
        let after = utf8(path, after)?;
        keys.into_iter()
            .map(|key| {
                let value = get(kind, after, key).map_err(|error| annotate(path, error))?;
                Ok((key, value))
            })
            .collect()
    }

    /// The model key and the tier keys that follow it in the model file.
    fn model_keys(&self, model: ModelKey) -> Vec<&'static str> {
        let mut keys = vec![model.key];
        keys.extend(
            self.sets
                .iter()
                .filter(|set| {
                    set.file == model.file && matches!(set.slot, ApiSlot::ModelTier { .. })
                })
                .map(|set| set.key),
        );
        keys
    }

    fn prefixed(&self, id: &str) -> String {
        format!("{}{id}", self.model_prefix.unwrap_or(""))
    }

    fn revert(&self) -> Result<(), AgentError> {
        let mut restored = false;
        for location in &self.locations {
            if revert_path(&location.path)? {
                restored = true;
            }
        }
        if restored {
            Ok(())
        } else {
            Err(AgentError::message(format!(
                "nothing to restore for `{}`",
                self.id
            )))
        }
    }

    fn status(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<ConnectionStatus, AgentError> {
        let mut connected = true;
        let mut paths = Vec::new();
        for chosen in self.chosen()? {
            paths.push(chosen.path.clone());
            let Chosen {
                path, kind, sets, ..
            } = chosen;
            if !connected {
                continue;
            }
            let Some(bytes) = read_optional(&path)? else {
                connected = false;
                continue;
            };
            let text = utf8(&path, &bytes)?;
            for set in sets {
                let Some(expected) = self.slot_value(set, kind, target, Some(text), Choice::Keep)
                else {
                    continue;
                };
                let current = get(kind, text, set.key).map_err(|error| annotate(&path, error))?;
                if current.as_deref() != Some(expected.as_str()) {
                    connected = false;
                    break;
                }
            }
        }
        Ok(ConnectionStatus::new(
            self.id,
            self.display_name,
            self.detect(session),
            connected,
            paths,
        ))
    }

    fn disconnect(&self, session: &Session, force: bool) -> Result<DisconnectOutcome, AgentError> {
        let mut changed = false;
        let mut warnings = Vec::new();
        let preserve = preserve_dirs(session);
        for location in &self.locations {
            let path = &location.path;
            if !marker_present(path) {
                continue;
            }
            if force {
                if revert_path(path)? {
                    changed = true;
                    if !path.exists() {
                        remove_empty_parents(path, &preserve)?;
                    }
                }
                continue;
            }
            warnings.extend(merge_owned_keys(
                path,
                location.kind,
                self.owned_keys(location.file),
                self.tracked_key(location.file),
                &preserve,
                self.id,
            )?);
            changed = true;
        }
        Ok(DisconnectOutcome::new(changed, warnings))
    }

    fn refresh_token(&self, target: &GatewayTarget) -> Result<Vec<PathBuf>, AgentError> {
        let mut updated = Vec::new();
        for chosen in self.chosen()? {
            let token_sets: Vec<&EngineSet> = chosen
                .sets
                .iter()
                .copied()
                .filter(|set| matches!(set.slot, ApiSlot::Token))
                .collect();
            if token_sets.is_empty() {
                continue;
            }
            let Some(before) = read_optional(&chosen.path)? else {
                continue;
            };
            let text = utf8(&chosen.path, &before)?;
            let assignments =
                self.assignments_for(&token_sets, chosen.kind, target, Some(text), Choice::Keep);
            let after = render(chosen.kind, Some(text), &assignments)
                .map_err(|error| annotate(&chosen.path, error))?;
            if before != after {
                write_atomic(&chosen.path, &after)?;
                refresh_wrote(&chosen.path, chosen.kind, &assignments)?;
                updated.push(chosen.path);
            }
        }
        Ok(updated)
    }

    fn configured_model(&self) -> Result<Option<String>, AgentError> {
        let Some(model) = self.model else {
            return Ok(None);
        };
        for chosen in self.chosen()? {
            if chosen.file != model.file {
                continue;
            }
            let Some(bytes) = read_optional(&chosen.path)? else {
                return Ok(None);
            };
            let text = utf8(&chosen.path, &bytes)?;
            return string_at(chosen.kind, text, model.key)
                .map_err(|error| annotate(&chosen.path, error));
        }
        Ok(None)
    }

    fn chosen(&self) -> Result<Vec<Chosen<'_>>, AgentError> {
        let mut chosen = Vec::new();
        for file in self.file_ids() {
            let location = self
                .choose(file)
                .ok_or_else(|| AgentError::message(format!("`{file}` has no config location")))?;
            let sets = self.sets.iter().filter(|set| set.file == file).collect();
            chosen.push(Chosen {
                file,
                path: location.path.clone(),
                kind: location.kind,
                sets,
            });
        }
        Ok(chosen)
    }

    fn file_ids(&self) -> Vec<&'static str> {
        let mut ids = Vec::new();
        for location in &self.locations {
            if !ids.contains(&location.file) {
                ids.push(location.file);
            }
        }
        ids
    }

    fn choose(&self, file: &str) -> Option<&EngineLocation> {
        let mut creator = None;
        let mut fallback = None;
        for location in self
            .locations
            .iter()
            .filter(|location| location.file == file)
        {
            if location.path.is_file() {
                return Some(location);
            }
            if location.create && creator.is_none() {
                creator = Some(location);
            }
            if fallback.is_none() {
                fallback = Some(location);
            }
        }
        creator.or(fallback)
    }

    fn assignments_for(
        &self,
        sets: &[&EngineSet],
        kind: FileKind,
        target: &GatewayTarget,
        existing: Option<&str>,
        choice: Choice<'_>,
    ) -> Vec<Assignment> {
        sets.iter()
            .filter_map(|set| {
                let value = self.slot_value(set, kind, target, existing, choice)?;
                Some(Assignment {
                    key: set.key,
                    value,
                    raw_json: matches!(
                        set.slot,
                        ApiSlot::CatalogModels | ApiSlot::Json(_) | ApiSlot::ModelArray { .. }
                    ),
                })
            })
            .collect()
    }

    fn slot_value(
        &self,
        set: &EngineSet,
        kind: FileKind,
        target: &GatewayTarget,
        existing: Option<&str>,
        choice: Choice<'_>,
    ) -> Option<String> {
        let models = self.models(target);
        let chosen = match choice {
            Choice::Set(id) => Some(id),
            Choice::Keep | Choice::Clear => None,
        };
        let gateway = self.route == AgentRoute::Gateway;
        match &set.slot {
            ApiSlot::Origin if gateway => Some(origin(target.base_url())),
            ApiSlot::V1 if gateway => Some(format!("{}/v1", origin(target.base_url()))),
            ApiSlot::Token if gateway => Some(target.token().to_owned()),
            ApiSlot::Literal(text) => Some((*text).to_owned()),
            ApiSlot::ModelIfUnset { from } => match chosen {
                Some(id) if self.model.is_some_and(|model| model.key == set.key) => {
                    Some(self.prefixed(id))
                }
                _ => model_if_unset(kind, existing, set.key, *from, &models),
            },
            ApiSlot::ModelTier {
                when_model_matches, ..
            } => {
                let main = match chosen {
                    Some(id) => id.to_owned(),
                    None => self.tier_model(set, kind, existing, &models)?,
                };
                if when_model_matches
                    .is_some_and(|needle| !main.to_ascii_lowercase().contains(needle.as_str()))
                {
                    return None;
                }
                Some(main)
            }
            ApiSlot::CatalogModels if gateway => Some(catalog_models_json(&models)),
            ApiSlot::Json(text) => Some((*text).to_owned()),
            ApiSlot::ModelArray { fields } if gateway => Some(model_array_json(&models, fields)),
            _ => None,
        }
    }

    fn models(&self, target: &GatewayTarget) -> Vec<String> {
        match self.visible {
            Visible::All => target.models().to_vec(),
            Visible::Only(only) => target
                .models()
                .iter()
                .filter(|id| only.contains(&id.as_str()))
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn tier_model(
        &self,
        set: &EngineSet,
        kind: FileKind,
        existing: Option<&str>,
        models: &[String],
    ) -> Option<String> {
        if let Some(model) = self.model
            && model.file == set.file
            && let Some(text) = existing
            && let Some(value) = string_value(kind, text, model.key)
        {
            return Some(value);
        }
        single_model(models)
    }

    fn owned_keys(&self, file: &str) -> Vec<&'static str> {
        self.sets
            .iter()
            .filter(|set| set.file == file)
            .map(|set| set.key)
            .collect()
    }

    /// The model key when no set owns it, so disconnect restores it only
    /// when hofund changed it.
    fn tracked_key(&self, file: &str) -> Option<&'static str> {
        let model = self.model?;
        let owned = self
            .sets
            .iter()
            .any(|set| set.file == file && set.key == model.key);
        (model.file == file && !owned).then_some(model.key)
    }
}

struct Chosen<'a> {
    file: &'static str,
    path: PathBuf,
    kind: FileKind,
    sets: Vec<&'a EngineSet>,
}

impl AgentAdapter for Compiled {
    fn id(&self) -> &str {
        self.description.id().as_str()
    }

    fn display_name(&self) -> &str {
        self.description.display_name()
    }

    fn detect(&self, session: &Session) -> bool {
        self.engine(session)
            .is_ok_and(|engine| engine.detect(session))
    }

    fn read(&self, session: &Session) -> Result<Vec<ConfigFile>, AgentError> {
        self.engine(session)?.read()
    }

    fn plan(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, AgentError> {
        self.engine(session)?.plan(target)
    }

    fn apply(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, AgentError> {
        self.engine(session)?.apply(target)
    }

    fn revert(&self, session: &Session) -> Result<(), AgentError> {
        self.engine(session)?.revert()
    }

    fn disconnect(&self, session: &Session, force: bool) -> Result<DisconnectOutcome, AgentError> {
        self.engine(session)?.disconnect(session, force)
    }

    fn refresh_token(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<Vec<PathBuf>, AgentError> {
        self.engine(session)?.refresh_token(target)
    }

    fn set_model(
        &self,
        session: &Session,
        target: &GatewayTarget,
        model: Option<&str>,
    ) -> Result<Plan, AgentError> {
        self.engine(session)?.set_model(target, model)
    }

    fn configured_model(&self, session: &Session) -> Result<Option<String>, AgentError> {
        self.engine(session)?.configured_model()
    }

    fn model_prefix(&self) -> Option<&str> {
        self.description.model_prefix()
    }

    fn status(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<ConnectionStatus, AgentError> {
        self.engine(session)?.status(session, target)
    }
}

fn model_if_unset(
    kind: FileKind,
    existing: Option<&str>,
    key: &str,
    from: Option<&str>,
    models: &[String],
) -> Option<String> {
    if let Some(text) = existing
        && string_value(kind, text, key).is_some()
    {
        return None;
    }
    if let Some(source) = from
        && let Some(text) = existing
        && let Some(value) = string_value(kind, text, source)
    {
        return Some(value);
    }
    single_model(models)
}

fn string_value(kind: FileKind, text: &str, key: &str) -> Option<String> {
    string_at(kind, text, key)
        .ok()
        .flatten()
        .filter(|value| !value.is_empty())
}

fn single_model(models: &[String]) -> Option<String> {
    let mut ids = models.iter().filter(|id| !id.is_empty()).cloned();
    let first = ids.next()?;
    ids.next().is_none().then_some(first)
}

fn catalog_models_json(models: &[String]) -> String {
    let mut root = serde_json::Map::new();
    for id in models {
        if id.is_empty() || root.contains_key(id) {
            continue;
        }
        let mut entry = serde_json::Map::new();
        entry.insert("name".to_owned(), serde_json::Value::String(id.clone()));
        entry.insert(
            "variants".to_owned(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
        root.insert(id.clone(), serde_json::Value::Object(entry));
    }
    serde_json::Value::Object(root).to_string()
}

/// One object per distinct non-empty model, in route order, holding `fields`.
fn model_array_json(models: &[String], fields: &[ModelField]) -> String {
    let mut seen = BTreeSet::new();
    let mut items = Vec::new();
    for id in models {
        if id.is_empty() || !seen.insert(id.as_str()) {
            continue;
        }
        let mut entry = serde_json::Map::new();
        for field in fields {
            let value = match field.value() {
                ModelValue::Id => serde_json::Value::String(id.clone()),
                ModelValue::Literal(text) => serde_json::Value::String(text.to_owned()),
                ModelValue::Json(text) => {
                    serde_json::from_str(text).unwrap_or(serde_json::Value::Null)
                }
                _ => serde_json::Value::Null,
            };
            entry.insert(field.key().to_owned(), value);
        }
        items.push(serde_json::Value::Object(entry));
    }
    serde_json::Value::Array(items).to_string()
}

fn origin(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_owned()
}

fn plan_of(rendered: &[Rendered]) -> Plan {
    let mut changes = Vec::new();
    for file in rendered {
        let before = file.before.clone().unwrap_or_default();
        if before != file.after {
            let diff = unified_diff(&file.path, &before, &file.after);
            changes.push(FileChange::new(
                file.path.clone(),
                before,
                file.after.clone(),
                diff,
            ));
        }
    }
    Plan::new(changes)
}

/// Records the bytes apply would write onto the pre-connect file.
///
/// User edits are not part of this snapshot. Disconnect compares the live file
/// with it, per key, so a later edit is not treated as something hofund wrote.
/// Each `pinned` key then takes the value apply wrote, or is removed, so a
/// model hofund set or cleared is restored on disconnect.
fn snapshot_wrote(
    path: &Path,
    kind: FileKind,
    assignments: &[Assignment],
    pinned: &[(&'static str, Option<String>)],
) -> Result<(), AgentError> {
    let backup = read_optional(&sibling_bak(path))?;
    let text = match &backup {
        Some(bytes) => Some(utf8(path, bytes)?),
        None => None,
    };
    let mut wrote = render(kind, text, assignments).map_err(|error| annotate(path, error))?;
    if !pinned.is_empty() {
        let values: Vec<Assignment> = pinned
            .iter()
            .filter_map(|(key, value)| {
                let value = value.clone()?;
                Some(Assignment {
                    key,
                    raw_json: is_json_value(&value),
                    value,
                })
            })
            .collect();
        let removed: Vec<&str> = pinned
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| *key)
            .collect();
        let set = render(kind, Some(utf8(path, &wrote)?), &values)
            .map_err(|error| annotate(path, error))?;
        wrote = remove_keys(kind, utf8(path, &set)?, &removed)
            .map_err(|error| annotate(path, error))?;
    }
    write_atomic(&sibling_wrote(path), &wrote)
}

/// Whether a value read back with `get` is JSON text rather than a string.
fn is_json_value(value: &str) -> bool {
    let value = value.trim_start();
    value.starts_with('{') || value.starts_with('[')
}

/// Applies a token refresh to the connect snapshot, not to a user-edited copy.
fn refresh_wrote(
    path: &Path,
    kind: FileKind,
    assignments: &[Assignment],
) -> Result<(), AgentError> {
    let Some(bytes) = read_optional(&sibling_wrote(path))? else {
        return Ok(());
    };
    let text = utf8(path, &bytes)?;
    let wrote = render(kind, Some(text), assignments).map_err(|error| annotate(path, error))?;
    write_atomic(&sibling_wrote(path), &wrote)
}

fn sibling_bak(path: &Path) -> PathBuf {
    sibling_named(path, ".hofund.bak")
}

fn sibling_wrote(path: &Path) -> PathBuf {
    sibling_named(path, ".hofund.wrote")
}

fn sibling_named(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("hofund"), ToOwned::to_owned);
    name.push(suffix);
    path.with_file_name(name)
}

fn utf8<'a>(path: &Path, bytes: &'a [u8]) -> Result<&'a str, AgentError> {
    std::str::from_utf8(bytes)
        .map_err(|_| AgentError::message(format!("{} is not UTF-8", path.display())))
}

fn merge_owned_keys(
    path: &Path,
    kind: FileKind,
    keys: Vec<&'static str>,
    tracked: Option<&'static str>,
    preserve: &[PathBuf],
    id: &str,
) -> Result<Vec<String>, AgentError> {
    let wrote = read_optional(&sibling_wrote(path))?
        .ok_or_else(|| refused(id, path, "connect snapshot is missing"))?;
    let current = read_optional(path)?;
    let backup = read_optional(&sibling_bak(path))?;
    if current.as_ref() == Some(&wrote) {
        restore_exact(path, backup, preserve, id)?;
        return Ok(Vec::new());
    }
    let Some(current_bytes) = current else {
        restore_exact(path, backup, preserve, id)?;
        return Ok(Vec::new());
    };
    let current_text =
        std::str::from_utf8(&current_bytes).map_err(|_| refused(id, path, "file is not UTF-8"))?;
    let wrote_text = std::str::from_utf8(&wrote)
        .map_err(|_| refused(id, path, "connect snapshot is not UTF-8"))?;
    let backup_text = match &backup {
        Some(bytes) => {
            Some(std::str::from_utf8(bytes).map_err(|_| refused(id, path, "backup is not UTF-8"))?)
        }
        None => None,
    };
    let mut restore = Vec::new();
    let mut remove = Vec::new();
    let mut warnings = Vec::new();
    for key in keys.into_iter().chain(tracked) {
        let current_value =
            get(kind, current_text, key).map_err(|error| refused(id, path, &error.to_string()))?;
        let wrote_value =
            get(kind, wrote_text, key).map_err(|error| refused(id, path, &error.to_string()))?;
        let original = match backup_text {
            Some(text) => {
                get(kind, text, key).map_err(|error| refused(id, path, &error.to_string()))?
            }
            None => None,
        };
        if tracked == Some(key) && wrote_value == original {
            continue;
        }
        if current_value == wrote_value {
            match original {
                Some(value) => restore.push(Assignment {
                    key,
                    raw_json: is_json_value(&value),
                    value,
                }),
                None => remove.push(key),
            }
        } else {
            warnings.push(format!(
                "warning: {id} kept `{key}` in {}; it no longer matches the value hofund wrote",
                path.display()
            ));
        }
    }
    if restore.is_empty() && remove.is_empty() {
        clear_markers(path).map_err(|error| refused(id, path, &error.to_string()))?;
        return Ok(warnings);
    }
    let restored = render(kind, Some(current_text), &restore)
        .map_err(|error| refused(id, path, &error.to_string()))?;
    let restored_text = std::str::from_utf8(&restored)
        .map_err(|_| refused(id, path, "merged file is not UTF-8"))?;
    let after = remove_keys(kind, restored_text, &remove)
        .map_err(|error| refused(id, path, &error.to_string()))?;
    let remove_file = created_here(path)
        && is_empty_document(kind, &after)
            .map_err(|error| refused(id, path, &error.to_string()))?;
    if remove_file {
        delete_verified(path).map_err(|error| refused(id, path, &error.to_string()))?;
    } else {
        write_verified(path, &after).map_err(|error| refused(id, path, &error.to_string()))?;
    }
    clear_markers(path).map_err(|error| refused(id, path, &error.to_string()))?;
    if remove_file {
        remove_empty_parents(path, preserve)?;
    }
    Ok(warnings)
}

fn restore_exact(
    path: &Path,
    backup: Option<Vec<u8>>,
    preserve: &[PathBuf],
    id: &str,
) -> Result<(), AgentError> {
    if created_here(path) {
        delete_verified(path).map_err(|error| refused(id, path, &error.to_string()))?;
        clear_markers(path).map_err(|error| refused(id, path, &error.to_string()))?;
        remove_empty_parents(path, preserve)?;
        return Ok(());
    }
    let Some(bytes) = backup else {
        return Err(refused(id, path, "connect backup is missing"));
    };
    write_verified(path, &bytes).map_err(|error| refused(id, path, &error.to_string()))?;
    clear_markers(path).map_err(|error| refused(id, path, &error.to_string()))?;
    Ok(())
}

fn refused(id: &str, path: &Path, detail: &str) -> AgentError {
    AgentError::message(format!(
        "{detail}: {}. Re-run `hofund agent disconnect {id} --force` to restore the connect-time backup.",
        path.display()
    ))
}

fn preserve_dirs(session: &Session) -> Vec<PathBuf> {
    let mut dirs = vec![session.home.clone()];
    if let Some(path) = &session.xdg_config_home {
        dirs.push(path.clone());
    }
    dirs
}

fn annotate(path: &Path, error: AgentError) -> AgentError {
    AgentError::message(format!("{}: {error}", path.display()))
}

fn binary_on_path(session: &Session, name: &str) -> bool {
    let Some(path) = session.path.as_deref() else {
        return false;
    };
    for directory in std::env::split_paths(path) {
        if directory.join(name).is_file() {
            return true;
        }
        #[cfg(windows)]
        if directory.join(format!("{name}.exe")).is_file() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn missing_compiled_builtin_is_an_error() {
        let error = super::catalog(&[]).err().expect("missing built-in");
        assert_eq!(
            error.to_string(),
            "built-in agent `claude-code` has no compiled description"
        );
    }

    #[derive(Default)]
    struct GoldenCase {
        only: Option<Vec<String>>,
    }

    type GoldenLayout = (Vec<PathBuf>, [(&'static str, Vec<PathBuf>); 3]);

    fn golden_layout(id: &str, fixtures: &Path) -> Result<GoldenLayout, String> {
        let shared = golden_cases(&fixtures.join("all"))?;
        let per_os = [
            ("linux", golden_cases(&fixtures.join("linux"))?),
            ("macos", golden_cases(&fixtures.join("macos"))?),
            ("windows", golden_cases(&fixtures.join("windows"))?),
        ];
        validate_fixture_layout(id, fixtures, &shared, &per_os)?;
        Ok((shared, per_os))
    }

    fn golden_cases(root: &Path) -> Result<Vec<PathBuf>, String> {
        let mut cases = Vec::new();
        match fs::read_dir(root) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry
                        .map_err(|error| format!("read {}: {error}", root.display()))?
                        .path();
                    if !path.join("case.toml").is_file() {
                        return Err(format!(
                            "golden case directory {} must contain case.toml",
                            path.display()
                        ));
                    }
                    cases.push(path);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("read {}: {error}", root.display())),
        }
        cases.sort();
        Ok(cases)
    }

    fn validate_fixture_layout(
        id: &str,
        fixtures: &Path,
        shared: &[PathBuf],
        per_os: &[(&str, Vec<PathBuf>); 3],
    ) -> Result<(), String> {
        match fs::read_dir(fixtures) {
            Ok(entries) => {
                for entry in entries {
                    let entry =
                        entry.map_err(|error| format!("read {}: {error}", fixtures.display()))?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !["all", "linux", "macos", "windows"].contains(&name.as_str()) {
                        return Err(format!(
                            "unknown top-level fixture entry `{name}` at {}",
                            entry.path().display()
                        ));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("read {}: {error}", fixtures.display())),
        }
        for (directory, cases) in per_os {
            if shared.is_empty() && cases.is_empty() {
                return Err(format!("{id}: {directory} has no golden cases"));
            }
        }
        for case in shared {
            if case_targets(case)?.is_some() {
                return Err(format!(
                    "shared case `{}` must not declare only",
                    case.display()
                ));
            }
            let name = case_name(case);
            for (directory, cases) in per_os {
                if cases.iter().any(|candidate| case_name(candidate) == name) {
                    return Err(format!(
                        "{id}: case `{name}` occurs in both all/ and {directory}/"
                    ));
                }
            }
        }

        let mut by_name: BTreeMap<&str, Vec<(&str, &Path)>> = BTreeMap::new();
        for (directory, cases) in per_os {
            for case in cases {
                by_name
                    .entry(case_name(case))
                    .or_default()
                    .push((directory, case));
            }
        }
        for (name, cases) in by_name {
            let targets = case_targets(cases[0].1)?;
            for (_, case) in cases.iter().skip(1) {
                if case_targets(case)? != targets {
                    return Err(format!("case `{name}` has inconsistent only declarations"));
                }
            }
            let expected = targets.as_ref().map_or_else(
                || {
                    ["linux", "macos", "windows"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect::<std::collections::BTreeSet<_>>()
                },
                |targets| targets.clone(),
            );
            let actual = cases
                .iter()
                .map(|(directory, _)| (*directory).to_owned())
                .collect::<std::collections::BTreeSet<_>>();
            if actual != expected {
                return Err(format!(
                    "case `{name}` occurs in {actual:?}, but its layout requires {expected:?}"
                ));
            }
            if targets.is_none() {
                let first = tree(cases[0].1);
                if cases.iter().skip(1).all(|(_, case)| tree(case) == first) {
                    return Err(format!(
                        "{id}: case `{name}` has byte-identical per-OS trees in {}; the trees must differ or the case must be shared via all/ for all three OS",
                        fixtures.display()
                    ));
                }
            }
        }
        Ok(())
    }

    fn case_name(case: &Path) -> &str {
        case.file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 golden case name")
    }

    fn case_targets(case: &Path) -> Result<Option<std::collections::BTreeSet<String>>, String> {
        let parsed = read_case(&case.join("case.toml"));
        let Some(only) = parsed.only else {
            return Ok(None);
        };
        if only.is_empty() {
            return Err(format!(
                "case `{}` only must list at least one OS",
                case.display()
            ));
        }
        let mut targets = std::collections::BTreeSet::new();
        for directory in only {
            if !["linux", "macos", "windows"].contains(&directory.as_str()) {
                return Err(format!(
                    "case `{}` declares unknown OS `{directory}` in only",
                    case.display()
                ));
            }
            if !targets.insert(directory.clone()) {
                return Err(format!(
                    "case `{}` declares duplicate OS `{directory}` in only",
                    case.display()
                ));
            }
        }
        if targets.len() == 3 {
            return Err(format!(
                "case `{}` only lists every OS; use all/ instead",
                case.display()
            ));
        }
        Ok(Some(targets))
    }

    fn write_layout_case(root: &Path, directory: &str, name: &str, only: Option<&[&str]>) {
        let only = only.map_or_else(String::new, |directories| {
            let directories = directories
                .iter()
                .map(|directory| format!("\"{directory}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("only = [{directories}]\n")
        });
        let case = root.join(directory).join(name);
        fs::create_dir_all(case.join("after").join(directory)).expect("layout case");
        fs::write(
            case.join("case.toml"),
            format!(
                "routes = []\nbase_url = \"http://127.0.0.1:3425/\"\ntoken = \"old-token\"\n{only}"
            ),
        )
        .expect("layout case.toml");
        fs::write(
            case.join("after").join(directory).join("fixture.env"),
            "FIXTURE_TOKEN=old-token\n",
        )
        .expect("layout golden");
    }

    fn layout_error(root: &Path) -> String {
        golden_layout("fixture-test", root)
            .map(|_| ())
            .expect_err("invalid fixture layout")
    }

    #[test]
    fn fixture_layout_rejects_unknown_top_level_entry() {
        let root = scratch();
        for directory in ["linux", "macOS", "windows"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("macOS"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_layout_requires_every_os() {
        let root = scratch();
        for directory in ["linux", "macos"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("windows has no golden cases"), "{error}");
        let _ = fs::remove_dir_all(&root);

        let root = scratch();
        for directory in ["linux", "windows"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("macos has no golden cases"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_layout_requires_complete_per_os_cases() {
        let root = scratch();
        write_layout_case(&root, "all", "shared", None);
        for directory in ["macos", "windows"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("layout requires"), "{error}");
        assert!(error.contains("linux"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_layout_requires_case_toml() {
        let root = scratch();
        fs::create_dir_all(root.join("all").join("broken")).expect("case directory");
        let error = layout_error(&root);
        assert!(error.contains("must contain case.toml"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_layout_enforces_only() {
        let root = scratch();
        write_layout_case(&root, "all", "shared", None);
        for directory in ["linux", "windows"] {
            write_layout_case(&root, directory, "selected", Some(&["linux", "windows"]));
        }
        golden_layout("fixture-test", &root).expect("only layout");

        write_layout_case(&root, "windows", "selected", Some(&["linux"]));
        let error = layout_error(&root);
        assert!(error.contains("inconsistent only"), "{error}");

        write_layout_case(&root, "all", "shared", Some(&["linux"]));
        let error = layout_error(&root);
        assert!(error.contains("must not declare only"), "{error}");
        let _ = fs::remove_dir_all(&root);

        for (only, expected) in [
            (&[][..], "must list at least one OS"),
            (&["linux", "linux"][..], "duplicate OS `linux`"),
            (&["linux", "macos", "windows"][..], "use all/ instead"),
        ] {
            let root = scratch();
            write_layout_case(&root, "all", "shared", None);
            write_layout_case(&root, "linux", "selected", Some(only));
            let error = layout_error(&root);
            assert!(error.contains(expected), "{error}");
            let _ = fs::remove_dir_all(root);
        }

        let root = scratch();
        write_layout_case(&root, "all", "shared", None);
        write_layout_case(&root, "linux", "selected", Some(&["plan9"]));
        let error = layout_error(&root);
        assert!(error.contains("unknown OS `plan9`"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn real_fixture_trees_pass_layout_validation() {
        let agents = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../plugins/registry/tests/fixtures/agents");
        for id in [
            "fixture-toml-agent",
            "fixture-json-agent",
            "fixture-yaml-agent",
        ] {
            golden_layout(id, &agents.join(id).join("fixtures"))
                .unwrap_or_else(|error| panic!("{id}: {error}"));
        }
    }

    fn read_case(path: &Path) -> GoldenCase {
        let text = fs::read_to_string(path).expect("case.toml");
        let mut case = GoldenCase::default();
        for line in text.lines() {
            let line = line.trim();
            let Some((key, raw)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "only" {
                case.only = Some(serde_json::from_str(raw.trim()).expect("case only"));
            }
        }
        case
    }

    fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        collect_files(root, root, &mut files);
        files
    }

    fn collect_files(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("golden entry").path();
            if path.is_dir() {
                collect_files(root, &path, files);
            } else if path.is_file() {
                files.insert(
                    path.strip_prefix(root)
                        .expect("golden relative")
                        .to_path_buf(),
                    fs::read(path).expect("golden file"),
                );
            }
        }
    }

    struct Scratch(tempfile::TempDir);

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            self.0.path()
        }
    }

    impl AsRef<Path> for Scratch {
        fn as_ref(&self) -> &Path {
            self.0.path()
        }
    }

    fn scratch() -> Scratch {
        Scratch(tempfile::tempdir().expect("golden layout temp dir"))
    }
}
