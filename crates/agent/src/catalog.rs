//! Compiled catalog composition and the const engine kept for the legacy oracles.
//!
//! Paths and keys were checked against each tool's own docs on 2026-10-24.
//! The citations sit on each description below.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[cfg(any(test, feature = "legacy-agent-consts"))]
use hofund_plugin_api::TierId;
use hofund_plugin_api::{
    AgentDescription, AgentFile, AgentRoute, FileKind as ApiFileKind, Slot as ApiSlot, Visible,
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

#[cfg(any(test, feature = "legacy-agent-consts"))]
pub(crate) struct Description {
    pub(crate) id: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) binary: &'static str,
    pub(crate) locations: &'static [Location],
    pub(crate) sets: &'static [Set],
    /// File id and key holding the tool's selected model, when it has one.
    pub(crate) model: Option<ModelKey>,
}

#[derive(Clone, Copy)]
pub(crate) struct ModelKey {
    pub(crate) file: &'static str,
    pub(crate) key: &'static str,
}

#[cfg(any(test, feature = "legacy-agent-consts"))]
pub(crate) struct Location {
    pub(crate) file: &'static str,
    pub(crate) kind: FileKind,
    pub(crate) create: bool,
    pub(crate) resolve: fn(&Session) -> PathBuf,
}

#[cfg(any(test, feature = "legacy-agent-consts"))]
pub(crate) struct Set {
    pub(crate) file: &'static str,
    pub(crate) key: &'static str,
    pub(crate) slot: Slot,
}

#[cfg(any(test, feature = "legacy-agent-consts"))]
pub(crate) enum Slot {
    Origin,
    V1,
    Token,
    Literal(&'static str),
    /// Gemini CLI `model.name`, when this connect has a model to write.
    GeminiModel,
    /// OpenCode `models` object built from [`GatewayTarget::models`].
    OpenCodeModels,
    /// Claude Code `model`, when the file has none and one route is configured.
    ClaudeModel,
    /// Claude Code tier env value. `fable` is skipped unless the model id is Fable.
    ClaudeTier(&'static str),
    /// Test-only API `ModelIfUnset` with an arbitrary source key. The fixture's
    /// `legacy_model` source is not representable by `GeminiModel`.
    #[cfg(test)]
    ModelIfUnset(Option<&'static str>),
    /// Test-only API `ModelTier` with an arbitrary needle. The fixture's
    /// `fixture-fable-` needle is not representable by `ClaudeTier`.
    #[cfg(test)]
    ModelTier {
        tier: &'static str,
        when_model_matches: Option<&'static str>,
    },
}

#[cfg(any(test, feature = "legacy-agent-consts"))]
pub(crate) struct Described {
    pub(crate) description: &'static Description,
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

/// Const built-in adapters. Every built-in is now compiled, so this is empty.
#[must_use]
pub fn adapters() -> [&'static dyn AgentAdapter; 0] {
    []
}

/// Finds a const built-in adapter. Every built-in is now compiled, so this
/// always returns `None`; use [`Catalog::find`].
#[must_use]
pub fn find(name: &str) -> Option<&'static dyn AgentAdapter> {
    adapters().into_iter().find(|adapter| adapter.id() == name)
}

/// Const and compiled adapters in catalog order.
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

/// Composes const and compiled adapters in CLI order and rejects duplicate ids.
///
/// Every built-in (claude-code, codex, gemini-cli, opencode) comes from the
/// compiled list supplied by the daemon's `agent_descriptions()`. Other
/// compiled ids follow the four built-in slots in input order.
///
/// # Errors
///
/// Returns an error when a compiled id duplicates a const built-in or another
/// compiled id, or when a built-in has no compiled description or enabled
/// legacy fallback.
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

fn builtin_adapter(id: &str) -> Option<Box<dyn AgentAdapter>> {
    #[cfg(feature = "legacy-agent-consts")]
    let adapter = match id {
        "claude-code" => Some(crate::legacy::CLAUDE),
        "codex" => Some(crate::legacy::CODEX_ADAPTER),
        "gemini-cli" => Some(crate::legacy::GEMINI_ADAPTER),
        "opencode" => Some(crate::legacy::OPENCODE_ADAPTER),
        _ => None,
    }
    .map(|adapter| Box::new(adapter) as Box<dyn AgentAdapter>);
    #[cfg(not(feature = "legacy-agent-consts"))]
    let adapter = {
        let _ = id;
        None
    };
    adapter
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
    visible: Visible,
    route: AgentRoute,
}

impl Engine {
    #[cfg(any(test, feature = "legacy-agent-consts"))]
    fn described(description: &'static Description, session: &Session) -> Self {
        let locations = description
            .locations
            .iter()
            .map(|location| EngineLocation {
                file: location.file,
                path: (location.resolve)(session),
                kind: location.kind,
                create: location.create,
            })
            .collect();
        let sets = description
            .sets
            .iter()
            .map(|set| EngineSet {
                file: set.file,
                key: set.key,
                slot: match set.slot {
                    Slot::Origin => ApiSlot::Origin,
                    #[cfg(any(test, feature = "legacy-agent-consts"))]
                    Slot::V1 => ApiSlot::V1,
                    Slot::Token => ApiSlot::Token,
                    Slot::Literal(value) => ApiSlot::Literal(value),
                    Slot::GeminiModel => ApiSlot::model_if_unset(Some("model")),
                    #[cfg(any(test, feature = "legacy-agent-consts"))]
                    Slot::OpenCodeModels => ApiSlot::CatalogModels,
                    #[cfg(any(test, feature = "legacy-agent-consts"))]
                    Slot::ClaudeModel => ApiSlot::model_if_unset(None),
                    #[cfg(any(test, feature = "legacy-agent-consts"))]
                    Slot::ClaudeTier(tier) => ApiSlot::ModelTier {
                        tier: TierId::from_static(tier),
                        when_model_matches: (tier == "fable")
                            .then(|| hofund_plugin_api::MatchNeedle::new("claude-fable-")),
                    },
                    #[cfg(test)]
                    Slot::ModelIfUnset(from) => ApiSlot::model_if_unset(from),
                    #[cfg(test)]
                    Slot::ModelTier {
                        tier,
                        when_model_matches,
                    } => ApiSlot::ModelTier {
                        tier: TierId::from_static(tier),
                        when_model_matches: when_model_matches
                            .map(hofund_plugin_api::MatchNeedle::new),
                    },
                },
            })
            .collect();
        Self {
            id: description.id,
            display_name: description.display_name,
            binary: description.binary,
            locations,
            sets,
            model: description.model,
            visible: Visible::All,
            route: AgentRoute::Gateway,
        }
    }

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
        let mut changes = Vec::new();
        for Chosen {
            path, kind, sets, ..
        } in self.chosen()?
        {
            let before = read_optional(&path)?;
            let text = match &before {
                Some(bytes) => Some(utf8(&path, bytes)?),
                None => None,
            };
            let assignments = self.assignments_for(&sets, kind, target, text);
            let after = render(kind, text, &assignments).map_err(|error| annotate(&path, error))?;
            let before_bytes = before.unwrap_or_default();
            if before_bytes != after {
                let diff = unified_diff(&path, &before_bytes, &after);
                changes.push(FileChange::new(path, before_bytes, after, diff));
            }
        }
        Ok(Plan::new(changes))
    }

    fn apply(&self, target: &GatewayTarget) -> Result<Plan, AgentError> {
        let plan = self.plan(target)?;
        for change in plan.changes() {
            backup_once(change.path())?;
        }
        let snapshots = self.snapshot_inputs(target)?;
        for change in plan.changes() {
            write_atomic(change.path(), change.after())?;
        }
        for (path, kind, assignments) in snapshots {
            if marker_present(&path) {
                snapshot_wrote(&path, kind, &assignments)?;
            }
        }
        Ok(plan)
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
                let Some(expected) = self.slot_value(set, kind, target, Some(text)) else {
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
            let assignments = self.assignments_for(&token_sets, chosen.kind, target, Some(text));
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

    fn snapshot_inputs(
        &self,
        target: &GatewayTarget,
    ) -> Result<Vec<(PathBuf, FileKind, Vec<Assignment>)>, AgentError> {
        let mut snapshots = Vec::new();
        for chosen in self.chosen()? {
            let bytes = read_optional(&chosen.path)?;
            let text = match &bytes {
                Some(bytes) => Some(utf8(&chosen.path, bytes)?),
                None => None,
            };
            let assignments = self.assignments_for(&chosen.sets, chosen.kind, target, text);
            snapshots.push((chosen.path, chosen.kind, assignments));
        }
        Ok(snapshots)
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
    ) -> Vec<Assignment> {
        sets.iter()
            .filter_map(|set| {
                let value = self.slot_value(set, kind, target, existing)?;
                Some(Assignment {
                    key: set.key,
                    value,
                    raw_json: matches!(set.slot, ApiSlot::CatalogModels),
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
    ) -> Option<String> {
        let models = self.models(target);
        let gateway = self.route == AgentRoute::Gateway;
        match &set.slot {
            ApiSlot::Origin if gateway => Some(origin(target.base_url())),
            ApiSlot::V1 if gateway => Some(format!("{}/v1", origin(target.base_url()))),
            ApiSlot::Token if gateway => Some(target.token().to_owned()),
            ApiSlot::Literal(text) => Some((*text).to_owned()),
            ApiSlot::ModelIfUnset { from } => {
                model_if_unset(kind, existing, set.key, *from, &models)
            }
            ApiSlot::ModelTier {
                when_model_matches, ..
            } => {
                let main = self.tier_model(set, kind, existing, &models)?;
                if when_model_matches
                    .is_some_and(|needle| !main.to_ascii_lowercase().contains(needle.as_str()))
                {
                    return None;
                }
                Some(main)
            }
            ApiSlot::CatalogModels if gateway => Some(catalog_models_json(&models)),
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
}

struct Chosen<'a> {
    file: &'static str,
    path: PathBuf,
    kind: FileKind,
    sets: Vec<&'a EngineSet>,
}

#[cfg(any(test, feature = "legacy-agent-consts"))]
impl AgentAdapter for Described {
    fn id(&self) -> &str {
        self.description.id
    }

    fn display_name(&self) -> &str {
        self.description.display_name
    }

    fn detect(&self, session: &Session) -> bool {
        Engine::described(self.description, session).detect(session)
    }

    fn read(&self, session: &Session) -> Result<Vec<ConfigFile>, AgentError> {
        Engine::described(self.description, session).read()
    }

    fn plan(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, AgentError> {
        Engine::described(self.description, session).plan(target)
    }

    fn apply(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, AgentError> {
        Engine::described(self.description, session).apply(target)
    }

    fn revert(&self, session: &Session) -> Result<(), AgentError> {
        Engine::described(self.description, session).revert()
    }

    fn disconnect(&self, session: &Session, force: bool) -> Result<DisconnectOutcome, AgentError> {
        Engine::described(self.description, session).disconnect(session, force)
    }

    fn refresh_token(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<Vec<PathBuf>, AgentError> {
        Engine::described(self.description, session).refresh_token(target)
    }

    fn configured_model(&self, session: &Session) -> Result<Option<String>, AgentError> {
        Engine::described(self.description, session).configured_model()
    }

    fn status(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<ConnectionStatus, AgentError> {
        Engine::described(self.description, session).status(session, target)
    }
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

    fn configured_model(&self, session: &Session) -> Result<Option<String>, AgentError> {
        self.engine(session)?.configured_model()
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

fn origin(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_owned()
}

/// Records the bytes apply would write onto the pre-connect file.
///
/// User edits are not part of this snapshot. Disconnect compares the live file
/// with it, per key, so a later edit is not treated as something hofund wrote.
fn snapshot_wrote(
    path: &Path,
    kind: FileKind,
    assignments: &[Assignment],
) -> Result<(), AgentError> {
    let backup = read_optional(&sibling_bak(path))?;
    let text = match &backup {
        Some(bytes) => Some(utf8(path, bytes)?),
        None => None,
    };
    let wrote = render(kind, text, assignments).map_err(|error| annotate(path, error))?;
    write_atomic(&sibling_wrote(path), &wrote)
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
    for key in keys {
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
        if current_value == wrote_value {
            match original {
                Some(value) => restore.push(Assignment {
                    key,
                    raw_json: value.trim_start().starts_with('{')
                        || value.trim_start().starts_with('['),
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
const FIXTURE_TOML_AGENT: Description = Description {
    id: "fixture-toml-agent",
    display_name: "Fixture TOML Agent",
    binary: "fixture-toml",
    locations: &[Location {
        file: "config",
        kind: FileKind::Toml,
        create: true,
        resolve: fixture_toml_config,
    }],
    sets: &[
        Set {
            file: "config",
            key: "model_provider",
            slot: Slot::Literal("hofund"),
        },
        Set {
            file: "config",
            key: "model_providers.hofund.name",
            slot: Slot::Literal("hofund"),
        },
        Set {
            file: "config",
            key: "model_providers.hofund.base_url",
            slot: Slot::V1,
        },
        Set {
            file: "config",
            key: "model_providers.hofund.wire_api",
            slot: Slot::Literal("responses"),
        },
        Set {
            file: "config",
            key: "model_providers.hofund.experimental_bearer_token",
            slot: Slot::Token,
        },
    ],
    model: Some(ModelKey {
        file: "config",
        key: "model",
    }),
};

/// The private engine does not read effort keys. This retains the fixture's
/// effort file and key beside its equivalent `Description`.
#[cfg(test)]
const FIXTURE_TOML_EFFORT: ModelKey = ModelKey {
    file: "config",
    key: "model_reasoning_effort",
};

#[cfg(test)]
const FIXTURE_JSON_AGENT: Description = Description {
    id: "fixture-json-agent",
    display_name: "Fixture JSON Agent",
    binary: "fixture-json",
    locations: &[
        Location {
            file: "settings",
            kind: FileKind::Json,
            create: true,
            resolve: fixture_json_settings,
        },
        Location {
            file: "env",
            kind: FileKind::Env,
            create: true,
            resolve: fixture_json_env,
        },
        Location {
            file: "origin",
            kind: FileKind::Env,
            create: true,
            resolve: fixture_json_origin,
        },
        Location {
            file: "config",
            kind: FileKind::Json,
            create: false,
            resolve: fixture_json_config_jsonc,
        },
        Location {
            file: "config",
            kind: FileKind::Json,
            create: true,
            resolve: fixture_json_config_json,
        },
    ],
    sets: &[
        Set {
            file: "settings",
            key: "origin",
            slot: Slot::Origin,
        },
        Set {
            file: "settings",
            key: "token",
            slot: Slot::Token,
        },
        Set {
            file: "settings",
            key: "literal",
            slot: Slot::Literal("value"),
        },
        Set {
            file: "settings",
            key: "model.name",
            slot: Slot::ClaudeModel,
        },
        Set {
            file: "settings",
            key: "selected.model",
            slot: Slot::ModelIfUnset(Some("legacy_model")),
        },
        Set {
            file: "settings",
            key: "tiers.opus",
            slot: Slot::ModelTier {
                tier: "opus",
                when_model_matches: None,
            },
        },
        Set {
            file: "settings",
            key: "tiers.fable",
            slot: Slot::ModelTier {
                tier: "fable",
                when_model_matches: Some("fixture-fable-"),
            },
        },
        Set {
            file: "env",
            key: "FIXTURE_TOKEN",
            slot: Slot::Token,
        },
        Set {
            file: "env",
            key: "FIXTURE_ORIGIN",
            slot: Slot::Origin,
        },
        Set {
            file: "env",
            key: "FIXTURE_LITERAL",
            slot: Slot::Literal("value"),
        },
        Set {
            file: "origin",
            key: "FIXTURE_BASE",
            slot: Slot::Origin,
        },
        Set {
            file: "config",
            key: "models",
            slot: Slot::OpenCodeModels,
        },
    ],
    model: Some(ModelKey {
        file: "settings",
        key: "model.name",
    }),
};

#[cfg(test)]
fn fixture_toml_config(session: &Session) -> PathBuf {
    session
        .env("FIXTURE_TOML_HOME")
        .filter(|value| usable_override(value))
        .map_or_else(|| fixture_toml_dir(session), PathBuf::from)
        .join("config.toml")
}

#[cfg(test)]
fn fixture_toml_dir(session: &Session) -> PathBuf {
    if cfg!(target_os = "macos") {
        session
            .home
            .join("Library")
            .join("Preferences")
            .join("fixture-toml")
    } else if cfg!(target_os = "windows") {
        session
            .home
            .join("AppData")
            .join("Local")
            .join("fixture-toml")
    } else {
        session.home.join(".config").join("fixture-toml")
    }
}

#[cfg(test)]
fn fixture_json_settings(session: &Session) -> PathBuf {
    fixture_json_dir(session).join("settings.json")
}

#[cfg(test)]
fn fixture_json_env(session: &Session) -> PathBuf {
    fixture_json_dir(session).join("fixture.env")
}

#[cfg(test)]
fn fixture_json_origin(session: &Session) -> PathBuf {
    fixture_json_dir(session).join("origin.env")
}

#[cfg(test)]
fn fixture_json_dir(session: &Session) -> PathBuf {
    if cfg!(target_os = "macos") {
        session
            .home
            .join("Library")
            .join("Application Support")
            .join("fixture-json")
    } else if cfg!(target_os = "windows") {
        session
            .home
            .join("AppData")
            .join("Roaming")
            .join("fixture-json")
    } else {
        session.home.join(".fixture-json")
    }
}

#[cfg(test)]
fn fixture_json_config_jsonc(session: &Session) -> PathBuf {
    session
        .home
        .join(".config")
        .join("fixture-json")
        .join("config.jsonc")
}

#[cfg(test)]
fn fixture_json_config_json(session: &Session) -> PathBuf {
    session
        .home
        .join(".config")
        .join("fixture-json")
        .join("config.json")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        Described, FIXTURE_JSON_AGENT, FIXTURE_TOML_AGENT, FIXTURE_TOML_EFFORT, builtin_adapter,
        compose,
    };
    use crate::{AgentAdapter, GatewayTarget, Plan, Session, TargetOs};

    #[test]
    fn missing_compiled_codex_is_an_error_without_a_fallback() {
        fn without_codex(id: &str) -> Option<Box<dyn AgentAdapter>> {
            if id == "codex" {
                None
            } else {
                builtin_adapter(id)
            }
        }

        let error = compose(&[], without_codex).err().expect("missing codex");
        assert_eq!(
            error.to_string(),
            "built-in agent `codex` has no compiled description"
        );

        fn without_claude(id: &str) -> Option<Box<dyn AgentAdapter>> {
            if id == "claude-code" {
                None
            } else {
                builtin_adapter(id)
            }
        }

        let error = compose(&[], without_claude)
            .err()
            .expect("missing claude-code");
        assert_eq!(
            error.to_string(),
            "built-in agent `claude-code` has no compiled description"
        );

        fn without_opencode(id: &str) -> Option<Box<dyn AgentAdapter>> {
            if id == "opencode" {
                None
            } else {
                builtin_adapter(id)
            }
        }

        let error = compose(&[], without_opencode)
            .err()
            .expect("missing opencode");
        assert_eq!(
            error.to_string(),
            "built-in agent `opencode` has no compiled description"
        );

        fn without_gemini_cli(id: &str) -> Option<Box<dyn AgentAdapter>> {
            if id == "gemini-cli" {
                None
            } else {
                builtin_adapter(id)
            }
        }

        let error = compose(&[], without_gemini_cli)
            .err()
            .expect("missing gemini-cli");
        assert_eq!(
            error.to_string(),
            "built-in agent `gemini-cli` has no compiled description"
        );
    }

    #[derive(Default)]
    struct GoldenCase {
        env: BTreeMap<String, String>,
        routes: Vec<String>,
        base_url: String,
        token: String,
        configured_model: Option<String>,
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

    /// Shared cases run everywhere; OS-specific cases run where their const
    /// resolver selects the same host-OS paths as the compiled description.
    #[test]
    fn const_fixture_descriptions_replay_compiled_goldens() {
        assert_eq!(FIXTURE_TOML_EFFORT.file, "config");
        let host_directory = match TargetOs::current() {
            TargetOs::Linux => "linux",
            TargetOs::Macos => "macos",
            TargetOs::Windows => "windows",
        };
        for (id, description) in [
            ("fixture-toml-agent", &FIXTURE_TOML_AGENT),
            ("fixture-json-agent", &FIXTURE_JSON_AGENT),
        ] {
            let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins/registry/tests/fixtures/agents")
                .join(id)
                .join("fixtures");
            let (shared, per_os) = golden_layout(id, &fixtures).expect("valid fixture layout");
            let host = per_os
                .iter()
                .find(|(directory, _)| *directory == host_directory)
                .map(|(_, cases)| cases)
                .expect("host cases");
            let mut replayed = 0;
            for case in shared.iter().chain(host.iter()) {
                replay_case(&Described { description }, case, TargetOs::current());
                replayed += 1;
            }
            assert!(replayed > 0, "{id} has no host or shared cases");
        }
    }

    #[test]
    fn legacy_codex_replays_compiled_goldens_for_every_target_os() {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/agents/codex/fixtures");
        let (shared, per_os) = golden_layout("codex", &fixtures).expect("valid codex layout");
        assert!(!shared.is_empty());
        for (directory, cases) in &per_os {
            let os = match *directory {
                "linux" => TargetOs::Linux,
                "macos" => TargetOs::Macos,
                "windows" => TargetOs::Windows,
                _ => unreachable!("validated target OS"),
            };
            for case in shared.iter().chain(cases.iter()) {
                replay_case(crate::legacy::codex(), case, os);
            }
        }
    }

    #[test]
    fn legacy_claude_code_replays_compiled_goldens_for_every_target_os() {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/agents/claude-code/fixtures");
        let (shared, per_os) =
            golden_layout("claude-code", &fixtures).expect("valid claude-code layout");
        assert!(!shared.is_empty());
        for (directory, cases) in &per_os {
            let os = match *directory {
                "linux" => TargetOs::Linux,
                "macos" => TargetOs::Macos,
                "windows" => TargetOs::Windows,
                _ => unreachable!("validated target OS"),
            };
            for case in shared.iter().chain(cases.iter()) {
                replay_case(crate::legacy::claude_code(), case, os);
            }
        }
    }

    #[test]
    fn legacy_opencode_replays_compiled_goldens_for_every_target_os() {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/agents/opencode/fixtures");
        let (shared, per_os) = golden_layout("opencode", &fixtures).expect("valid opencode layout");
        assert!(!shared.is_empty());
        for (directory, cases) in &per_os {
            let os = match *directory {
                "linux" => TargetOs::Linux,
                "macos" => TargetOs::Macos,
                "windows" => TargetOs::Windows,
                _ => unreachable!("validated target OS"),
            };
            for case in shared.iter().chain(cases.iter()) {
                replay_case(crate::legacy::opencode(), case, os);
            }
        }
    }

    #[test]
    fn legacy_gemini_cli_replays_compiled_goldens_for_every_target_os() {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/agents/gemini-cli/fixtures");
        let (shared, per_os) =
            golden_layout("gemini-cli", &fixtures).expect("valid gemini-cli layout");
        assert!(!shared.is_empty());
        for (directory, cases) in &per_os {
            let os = match *directory {
                "linux" => TargetOs::Linux,
                "macos" => TargetOs::Macos,
                "windows" => TargetOs::Windows,
                _ => unreachable!("validated target OS"),
            };
            for case in shared.iter().chain(cases.iter()) {
                replay_gemini_case(crate::legacy::gemini_cli(), case, os);
            }
        }
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
    fn const_fixture_layout_rejects_unknown_top_level_entry() {
        let root = scratch();
        for directory in ["linux", "macOS", "windows"] {
            write_layout_case(&root, directory, "fresh", None);
        }
        let error = layout_error(&root);
        assert!(error.contains("macOS"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn const_fixture_layout_requires_every_os() {
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
    fn const_fixture_layout_requires_complete_per_os_cases() {
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
    fn const_fixture_layout_requires_case_toml() {
        let root = scratch();
        fs::create_dir_all(root.join("all").join("broken")).expect("case directory");
        let error = layout_error(&root);
        assert!(error.contains("must contain case.toml"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn const_fixture_layout_enforces_only() {
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
    fn const_real_fixture_trees_pass_layout_validation() {
        let agents = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/registry/tests/fixtures/agents");
        for id in ["fixture-toml-agent", "fixture-json-agent"] {
            golden_layout(id, &agents.join(id).join("fixtures"))
                .unwrap_or_else(|error| panic!("{id}: {error}"));
        }
    }

    fn replay_case(adapter: &dyn AgentAdapter, case_dir: &Path, os: TargetOs) {
        let label = format!("{} / {os:?} / {}", adapter.id(), case_dir.display());
        let case = read_case(&case_dir.join("case.toml"));
        let home = scratch();
        let before_dir = case_dir.join("before");
        if before_dir.is_dir() {
            copy_tree(&before_dir, &home);
        }
        let before = tree(&home);
        let after = tree(&case_dir.join("after"));
        let mut session = Session::new(&home);
        for (name, value) in case.env {
            session = session.with_env(name, value.replace("{tmp}", &home.to_string_lossy()));
        }
        let target = GatewayTarget::new(case.base_url, case.token, case.routes);
        let plan = adapter.plan(&session, &target).expect("const plan");
        verify_plan(&label, &home, &plan, &after);
        let applied = adapter.apply(&session, &target).expect("const apply");
        verify_plan(&label, &home, &applied, &after);
        assert_eq!(
            adapter
                .configured_model(&session)
                .expect("const configured model")
                .as_deref(),
            case.configured_model.as_deref(),
            "{label}: const configured model"
        );
        let mut expected = before;
        expected.extend(after);
        assert_eq!(managed_tree(&home), expected, "{label}: const apply tree");
        let _ = fs::remove_dir_all(&home);
    }

    fn replay_gemini_case(adapter: &dyn AgentAdapter, case_dir: &Path, os: TargetOs) {
        fn directories(root: &Path) -> std::collections::BTreeSet<PathBuf> {
            fn collect(
                root: &Path,
                directory: &Path,
                directories: &mut std::collections::BTreeSet<PathBuf>,
            ) {
                let Ok(entries) = fs::read_dir(directory) else {
                    return;
                };
                for entry in entries {
                    let path = entry.expect("directory entry").path();
                    if path.is_dir() {
                        directories.insert(
                            path.strip_prefix(root)
                                .expect("relative directory")
                                .to_path_buf(),
                        );
                        collect(root, &path, directories);
                    }
                }
            }

            let mut directories = std::collections::BTreeSet::new();
            collect(root, root, &mut directories);
            directories
        }

        fn implied_directories<'a>(
            paths: impl IntoIterator<Item = &'a PathBuf>,
        ) -> std::collections::BTreeSet<PathBuf> {
            let mut directories = std::collections::BTreeSet::new();
            for path in paths {
                let mut parent = path.parent();
                while let Some(directory) = parent
                    && !directory.as_os_str().is_empty()
                {
                    directories.insert(directory.to_path_buf());
                    parent = directory.parent();
                }
            }
            directories
        }

        let label = format!("{} / {os:?} / {}", adapter.id(), case_dir.display());
        let case = read_case(&case_dir.join("case.toml"));
        let home = scratch();
        let before_dir = case_dir.join("before");
        if before_dir.is_dir() {
            copy_tree(&before_dir, &home);
        }
        let before = tree(&home);
        let before_directories = directories(&home);
        let after = tree(&case_dir.join("after"));
        let session_for = |home: &Path| {
            let mut session = Session::new(home);
            for (name, value) in &case.env {
                session = session.with_env(name, value.replace("{tmp}", &home.to_string_lossy()));
            }
            session
        };
        let session = session_for(&home);
        let target = GatewayTarget::new(&case.base_url, &case.token, case.routes.clone());
        let plan = adapter.plan(&session, &target).expect("const plan");
        let after =
            if std::env::var_os("UPDATE_GOLDEN").as_deref() == Some(std::ffi::OsStr::new("1")) {
                let after_dir = case_dir.join("after");
                if after_dir.exists() {
                    fs::remove_dir_all(&after_dir).expect("remove old Gemini golden");
                }
                for change in plan.changes() {
                    let relative = change.path().strip_prefix(&home).expect("golden relative");
                    let path = after_dir.join(relative);
                    fs::create_dir_all(path.parent().expect("golden parent"))
                        .expect("create golden parent");
                    fs::write(path, change.after()).expect("write Gemini golden");
                }
                tree(&after_dir)
            } else {
                after
            };
        verify_plan(&label, &home, &plan, &after);
        let applied = adapter.apply(&session, &target).expect("const apply");
        verify_plan(&label, &home, &applied, &after);
        assert_eq!(
            adapter
                .configured_model(&session)
                .expect("const configured model")
                .as_deref(),
            case.configured_model.as_deref(),
            "{label}: const configured model"
        );
        let mut expected = before.clone();
        expected.extend(after.clone());
        assert_eq!(managed_tree(&home), expected, "{label}: const apply tree");
        assert_eq!(
            directories(&home),
            implied_directories(before.keys().chain(after.keys())),
            "{label}: apply directories"
        );

        let forced_home = scratch();
        copy_tree(&home, &forced_home);
        let forced = adapter
            .disconnect(&session_for(&forced_home), true)
            .expect("const forced disconnect");
        assert!(forced.changed(), "{label}: forced disconnect unchanged");
        assert_eq!(tree(&forced_home), before, "{label}: forced bytes");
        assert_eq!(
            directories(&forced_home),
            before_directories,
            "{label}: forced directories"
        );
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&forced_home);
    }

    fn verify_plan(label: &str, home: &Path, plan: &Plan, after: &BTreeMap<PathBuf, Vec<u8>>) {
        let mut planned = Vec::new();
        for change in plan.changes() {
            let relative = change.path().strip_prefix(home).expect("plan relative");
            planned.push(relative.to_path_buf());
            assert_eq!(
                change.after(),
                after.get(relative).map_or(&[][..], Vec::as_slice),
                "{label}: plan after bytes for {relative:?}"
            );
        }
        planned.sort();
        let mut golden = after.keys().cloned().collect::<Vec<_>>();
        golden.sort();
        assert_eq!(planned, golden, "{label}: plan paths");
    }

    fn read_case(path: &Path) -> GoldenCase {
        let text = fs::read_to_string(path).expect("case.toml");
        let mut case = GoldenCase::default();
        let mut in_env = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[env]" {
                in_env = true;
                continue;
            }
            let Some((key, raw)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let raw = raw.trim();
            if key == "routes" {
                case.routes = serde_json::from_str(raw).expect("case routes");
            } else if key == "base_url" {
                case.base_url = serde_json::from_str(raw).expect("case base URL");
            } else if key == "token" {
                case.token = serde_json::from_str(raw).expect("case token");
            } else if key == "configured_model" {
                case.configured_model =
                    Some(serde_json::from_str(raw).expect("case configured model"));
            } else if key == "only" {
                case.only = Some(serde_json::from_str(raw).expect("case only"));
            } else if in_env {
                case.env.insert(
                    key.to_owned(),
                    serde_json::from_str(raw).expect("case env value"),
                );
            }
        }
        case
    }

    fn managed_tree(home: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        tree(home)
            .into_iter()
            .filter(|(path, _)| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                ![".hofund.bak", ".hofund.absent", ".hofund.wrote"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
            })
            .collect()
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

    fn copy_tree(source: &Path, destination: &Path) {
        for (relative, bytes) in tree(source) {
            let path = destination.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("before parent");
            }
            fs::write(path, bytes).expect("before file");
        }
    }

    fn scratch() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hofund-const-golden-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("const golden temp dir");
        path
    }
}
