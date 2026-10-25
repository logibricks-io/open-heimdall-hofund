//! Declarative descriptions of the four coding-agent adapters.
//!
//! Paths and keys were checked against each tool's own docs on 2026-10-24.
//! The citations sit on each description below.

use std::path::{Path, PathBuf};

use crate::edit::{Assignment, FileKind, get, is_empty_document, remove_keys, render, string_at};
use crate::store::{
    backup_once, clear_markers, created_here, delete_file, marker_present, read_optional,
    remove_empty_parents, revert_path, unified_diff, write_atomic,
};
use crate::{
    AgentAdapter, AgentError, ConfigFile, ConnectionStatus, FileChange, GatewayTarget, Plan,
    Session,
};

/// Claude Code user settings.
///
/// The file is `~/.claude/settings.json`. On Windows that is
/// `%USERPROFILE%\.claude\settings.json`. `CLAUDE_CONFIG_DIR` replaces the
/// `.claude` directory. The file is strict JSON; comments are a syntax error
/// in current Claude Code, and this adapter still accepts a loosened form so
/// a commented file can be edited and later restored.
///
/// `env.ANTHROPIC_BASE_URL` is the gateway origin. The Claude SDK appends
/// `/v1/messages`, so the value has no `/v1` suffix. `env.ANTHROPIC_AUTH_TOKEN`
/// is sent as `Authorization: Bearer`. The model is left alone. When settings
/// contain a string `model`, connect compares it with route names.
/// Checked 2026-10-25: <https://code.claude.com/docs/en/model-config>.
///
/// Sources: <https://code.claude.com/docs/en/settings>,
/// <https://code.claude.com/docs/en/llm-gateway-connect>,
/// <https://code.claude.com/docs/en/env-vars>.
const CLAUDE_CODE: Description = Description {
    id: "claude-code",
    display_name: "Claude Code",
    binary: "claude",
    locations: &[Location {
        file: "settings",
        kind: FileKind::Json,
        create: true,
        resolve: claude_settings,
    }],
    sets: &[
        Set {
            file: "settings",
            key: "env.ANTHROPIC_BASE_URL",
            slot: Slot::Origin,
        },
        Set {
            file: "settings",
            key: "env.ANTHROPIC_AUTH_TOKEN",
            slot: Slot::Token,
        },
    ],
    model: Some(ModelKey {
        file: "settings",
        key: "model",
    }),
};

/// OpenAI Codex CLI user config.
///
/// The file is `~/.codex/config.toml`. `CODEX_HOME` replaces `~/.codex`.
/// Provider ids `openai`, `ollama`, and `lmstudio` are reserved, so the
/// provider id here is `hofund`.
///
/// `model_provider` selects that table. `base_url` includes `/v1` because
/// `wire_api = "responses"` posts to `{base_url}/responses`.
/// `experimental_bearer_token` carries the gateway token.
/// Codex documents that field as discouraged next to `env_key`, and it must
/// not be combined with `env_key` or `requires_openai_auth`. The hofund token
/// lives in a file, not in the shell, so this is the documented field that
/// connects without a separate export. `requires_openai_auth` is not set.
/// The user's `model` key is left alone. Connect compares that string with
/// route names when it is set.
///
/// Source: <https://developers.openai.com/codex/config-reference>.
const CODEX: Description = Description {
    id: "codex",
    display_name: "Codex CLI",
    binary: "codex",
    locations: &[Location {
        file: "config",
        kind: FileKind::Toml,
        create: true,
        resolve: codex_config,
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

/// Gemini CLI user settings and env file.
///
/// User settings are `~/.gemini/settings.json`. `GEMINI_CLI_HOME` is the
/// parent directory: `.gemini` is created inside it. `.gemini/.env` is loaded
/// by the CLI and is not excluded.
///
/// `security.auth.selectedType` is set to `gemini-api-key`. The string values
/// of that enum are not listed in the public docs; they come from
/// `packages/core/src/core/contentGenerator.ts` (`gemini-api-key`,
/// `oauth-personal`, `vertex-ai`, `cloud-shell`,
/// `compute-default-credentials`, `gateway`).
///
/// `.env` sets `GEMINI_API_KEY`, `GOOGLE_GEMINI_BASE_URL` (the origin; HTTP
/// to localhost is allowed), and `GEMINI_API_KEY_AUTH_MECHANISM=bearer`.
/// A string `model` in settings is compared with route names and is not
/// rewritten. Checked 2026-10-25:
/// <https://geminicli.com/docs/reference/configuration/>.
///
/// `GEMINI_API_KEY_AUTH_MECHANISM` is real. It is not listed on the public
/// configuration page. In `contentGenerator.ts` the default is
/// `x-goog-api-key`. The value `bearer` adds `Authorization: Bearer ${apiKey}`
/// only when the auth type is `gemini-api-key` or `vertex-ai` and an API key
/// is set. The `gateway` auth type does not get that header.
/// `contentGenerator.test.ts` covers it ("should pass api key as Authorization
/// Header when GEMINI_API_KEY_AUTH_MECHANISM is set to bearer").
///
/// `getAuthTypeFromEnv` returns `gateway` when `GOOGLE_GEMINI_BASE_URL` is
/// set, before it considers `GEMINI_API_KEY`. Non-interactive startup uses
/// the settings value first (`configuredAuthType || getAuthTypeFromEnv()` in
/// `packages/cli/src/validateNonInterActiveAuth.ts`), so `gemini-api-key`
/// wins. `createContentGenerator` still uses `GOOGLE_GEMINI_BASE_URL` as the
/// SDK base URL for that auth type. The SDK sends `x-goog-api-key` when an
/// API key is passed. The gateway accepts either header, compared in constant
/// time, and serves `POST /v1beta/models/{model}:generateContent` and
/// `:streamGenerateContent`.
///
/// Sources: <https://geminicli.com/docs/reference/configuration/>,
/// <https://geminicli.com/docs/get-started/authentication/>,
/// <https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/core/contentGenerator.ts>,
/// <https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/core/contentGenerator.test.ts>,
/// <https://github.com/google-gemini/gemini-cli/blob/main/packages/cli/src/validateNonInterActiveAuth.ts>.
const GEMINI: Description = Description {
    id: "gemini-cli",
    display_name: "Gemini CLI",
    binary: "gemini",
    locations: &[
        Location {
            file: "settings",
            kind: FileKind::Json,
            create: true,
            resolve: gemini_settings,
        },
        Location {
            file: "env",
            kind: FileKind::Env,
            create: true,
            resolve: gemini_env,
        },
    ],
    sets: &[
        Set {
            file: "settings",
            key: "security.auth.selectedType",
            slot: Slot::Literal("gemini-api-key"),
        },
        Set {
            file: "env",
            key: "GEMINI_API_KEY",
            slot: Slot::Token,
        },
        Set {
            file: "env",
            key: "GOOGLE_GEMINI_BASE_URL",
            slot: Slot::Origin,
        },
        Set {
            file: "env",
            key: "GEMINI_API_KEY_AUTH_MECHANISM",
            slot: Slot::Literal("bearer"),
        },
    ],
    model: Some(ModelKey {
        file: "settings",
        key: "model",
    }),
};

/// OpenCode v2 global config.
///
/// The documented global path is `~/.config/opencode/opencode.json` or
/// `opencode.jsonc`, including on Windows (the docs say `~/.config`, not
/// `%APPDATA%`). When `opencode.jsonc` already exists it is the file that is
/// edited. Otherwise `opencode.json` is edited or created. A JSONC comment
/// does not survive apply; revert puts the original bytes back.
///
/// The v2 shape is `providers.<id>.package` plus `settings`, not the older
/// `provider.<id>.options` / `npm` shape. The package is
/// `@opencode/ai/providers/openai-compatible`. `settings.baseURL` includes
/// `/v1` because the package appends `/chat/completions`. `settings.apiKey`
/// is the gateway token. No model id is invented. A string `model` is compared
/// with route names and is not rewritten. v2 documents `~/.config` and does
/// not name an override (checked 2026-10-25). When `XDG_CONFIG_HOME` is set
/// this adapter uses `$XDG_CONFIG_HOME/opencode`. Unset, the path stays
/// `~/.config/opencode`, including on Windows.
///
/// Sources: <https://opencode.ai/v2/docs/config/>,
/// <https://opencode.ai/v2/docs/providers/>.
const OPENCODE: Description = Description {
    id: "opencode",
    display_name: "OpenCode",
    binary: "opencode",
    locations: &[
        Location {
            file: "config",
            kind: FileKind::Json,
            create: false,
            resolve: opencode_jsonc,
        },
        Location {
            file: "config",
            kind: FileKind::Json,
            create: true,
            resolve: opencode_json,
        },
    ],
    sets: &[
        Set {
            file: "config",
            key: "providers.hofund.name",
            slot: Slot::Literal("hofund"),
        },
        Set {
            file: "config",
            key: "providers.hofund.package",
            slot: Slot::Literal("@opencode/ai/providers/openai-compatible"),
        },
        Set {
            file: "config",
            key: "providers.hofund.settings.baseURL",
            slot: Slot::V1,
        },
        Set {
            file: "config",
            key: "providers.hofund.settings.apiKey",
            slot: Slot::Token,
        },
    ],
    model: Some(ModelKey {
        file: "config",
        key: "model",
    }),
};

struct Description {
    id: &'static str,
    display_name: &'static str,
    binary: &'static str,
    locations: &'static [Location],
    sets: &'static [Set],
    /// File id and key holding the tool's selected model, when it has one.
    model: Option<ModelKey>,
}

#[derive(Clone, Copy)]
struct ModelKey {
    file: &'static str,
    key: &'static str,
}

struct Location {
    file: &'static str,
    kind: FileKind,
    create: bool,
    resolve: fn(&Session) -> PathBuf,
}

struct Set {
    file: &'static str,
    key: &'static str,
    slot: Slot,
}

enum Slot {
    Origin,
    V1,
    Token,
    Literal(&'static str),
}

struct Described {
    description: &'static Description,
}

const CLAUDE: Described = Described {
    description: &CLAUDE_CODE,
};
const CODEX_ADAPTER: Described = Described {
    description: &CODEX,
};
const GEMINI_ADAPTER: Described = Described {
    description: &GEMINI,
};
const OPENCODE_ADAPTER: Described = Described {
    description: &OPENCODE,
};

/// The four built-in adapters, in CLI order.
#[must_use]
pub fn adapters() -> [&'static dyn AgentAdapter; 4] {
    [&CLAUDE, &CODEX_ADAPTER, &GEMINI_ADAPTER, &OPENCODE_ADAPTER]
}

/// Finds an adapter by its stable id.
#[must_use]
pub fn find(name: &str) -> Option<&'static dyn AgentAdapter> {
    adapters().into_iter().find(|adapter| adapter.id() == name)
}

struct Chosen<'a> {
    file: &'a str,
    path: PathBuf,
    kind: FileKind,
    sets: Vec<&'a Set>,
}

impl AgentAdapter for Described {
    fn id(&self) -> &str {
        self.description.id
    }

    fn display_name(&self) -> &str {
        self.description.display_name
    }

    fn detect(&self, session: &Session) -> bool {
        binary_on_path(session, self.description.binary)
            || self
                .description
                .locations
                .iter()
                .any(|location| (location.resolve)(session).is_file())
    }

    fn read(&self, session: &Session) -> Result<Vec<ConfigFile>, AgentError> {
        let mut files = Vec::new();
        for chosen in self.chosen(session)? {
            let path = chosen.path;
            let bytes = read_optional(&path)?;
            files.push(ConfigFile { path, bytes });
        }
        Ok(files)
    }

    fn plan(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, AgentError> {
        let mut changes = Vec::new();
        for Chosen {
            path, kind, sets, ..
        } in self.chosen(session)?
        {
            let before = read_optional(&path)?;
            let text = match &before {
                Some(bytes) => Some(std::str::from_utf8(bytes).map_err(|_| {
                    AgentError::message(format!("{} is not UTF-8", path.display()))
                })?),
                None => None,
            };
            let assignments = assignments_for(sets, target);
            let after = render(kind, text, &assignments).map_err(|error| annotate(&path, error))?;
            let before_bytes = before.unwrap_or_default();
            if before_bytes != after {
                let diff = unified_diff(&path, &before_bytes, &after);
                changes.push(FileChange {
                    path,
                    before: before_bytes,
                    after,
                    diff,
                });
            }
        }
        Ok(Plan { changes })
    }

    fn apply(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, AgentError> {
        let plan = self.plan(session, target)?;
        for change in &plan.changes {
            backup_once(&change.path)?;
        }
        for change in &plan.changes {
            write_atomic(&change.path, &change.after)?;
        }
        Ok(plan)
    }

    fn revert(&self, session: &Session) -> Result<(), AgentError> {
        let mut restored = false;
        for location in self.description.locations {
            let path = (location.resolve)(session);
            if revert_path(&path)? {
                restored = true;
            }
        }
        if restored {
            Ok(())
        } else {
            Err(AgentError::message(format!(
                "nothing to restore for `{}`",
                self.description.id
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
        for chosen in self.chosen(session)? {
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
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| AgentError::message(format!("{} is not UTF-8", path.display())))?;
            for set in sets {
                let current = get(kind, text, set.key).map_err(|error| annotate(&path, error))?;
                if current.as_deref() != Some(slot_value(&set.slot, target).as_str()) {
                    connected = false;
                    break;
                }
            }
        }
        Ok(ConnectionStatus {
            id: self.description.id,
            display_name: self.description.display_name,
            installed: self.detect(session),
            connected,
            paths,
        })
    }

    fn disconnect(&self, session: &Session, force: bool) -> Result<bool, AgentError> {
        let mut changed = false;
        let preserve = preserve_dirs(session);
        for location in self.description.locations {
            let path = (location.resolve)(session);
            if !marker_present(&path) {
                continue;
            }
            if force {
                if revert_path(&path)? {
                    changed = true;
                    if !path.exists() {
                        remove_empty_parents(&path, &preserve)?;
                    }
                }
                continue;
            }
            release_owned_keys(
                &path,
                location.kind,
                owned_keys(self.description, location.file),
                &preserve,
                self.description.id,
            )?;
            changed = true;
        }
        Ok(changed)
    }

    fn refresh_token(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<Vec<PathBuf>, AgentError> {
        let mut updated = Vec::new();
        for chosen in self.chosen(session)? {
            let token_sets: Vec<&Set> = chosen
                .sets
                .iter()
                .copied()
                .filter(|set| matches!(set.slot, Slot::Token))
                .collect();
            if token_sets.is_empty() {
                continue;
            }
            let Some(before) = read_optional(&chosen.path)? else {
                continue;
            };
            let text = std::str::from_utf8(&before).map_err(|_| {
                AgentError::message(format!("{} is not UTF-8", chosen.path.display()))
            })?;
            let assignments = assignments_for(token_sets, target);
            let after = render(chosen.kind, Some(text), &assignments)
                .map_err(|error| annotate(&chosen.path, error))?;
            if before != after {
                write_atomic(&chosen.path, &after)?;
                updated.push(chosen.path);
            }
        }
        Ok(updated)
    }

    fn configured_model(&self, session: &Session) -> Result<Option<String>, AgentError> {
        let Some(model) = self.description.model else {
            return Ok(None);
        };
        for chosen in self.chosen(session)? {
            if chosen.file != model.file {
                continue;
            }
            let Some(bytes) = read_optional(&chosen.path)? else {
                return Ok(None);
            };
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                AgentError::message(format!("{} is not UTF-8", chosen.path.display()))
            })?;
            return string_at(chosen.kind, text, model.key)
                .map_err(|error| annotate(&chosen.path, error));
        }
        Ok(None)
    }
}

impl Described {
    fn chosen(&self, session: &Session) -> Result<Vec<Chosen<'_>>, AgentError> {
        let mut chosen = Vec::new();
        for file in file_ids(self.description) {
            let location = choose(self.description, session, file)
                .ok_or_else(|| AgentError::message(format!("`{file}` has no config location")))?;
            let sets = self
                .description
                .sets
                .iter()
                .filter(|set| set.file == file)
                .collect();
            chosen.push(Chosen {
                file,
                path: (location.resolve)(session),
                kind: location.kind,
                sets,
            });
        }
        Ok(chosen)
    }
}

fn file_ids(description: &Description) -> Vec<&str> {
    let mut ids = Vec::new();
    for location in description.locations {
        if !ids.contains(&location.file) {
            ids.push(location.file);
        }
    }
    ids
}

fn choose<'a>(description: &'a Description, session: &Session, file: &str) -> Option<&'a Location> {
    let mut creator = None;
    let mut fallback = None;
    for location in description
        .locations
        .iter()
        .filter(|location| location.file == file)
    {
        if (location.resolve)(session).is_file() {
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

fn assignments_for(sets: Vec<&Set>, target: &GatewayTarget) -> Vec<Assignment> {
    sets.into_iter()
        .map(|set| Assignment {
            key: set.key,
            value: slot_value(&set.slot, target),
        })
        .collect()
}

fn slot_value(slot: &Slot, target: &GatewayTarget) -> String {
    match slot {
        Slot::Origin => origin(&target.base_url),
        Slot::V1 => format!("{}/v1", origin(&target.base_url)),
        Slot::Token => target.token.clone(),
        Slot::Literal(text) => (*text).to_owned(),
    }
}

fn origin(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_owned()
}

fn owned_keys<'a>(description: &'a Description, file: &str) -> Vec<&'a str> {
    description
        .sets
        .iter()
        .filter(|set| set.file == file)
        .map(|set| set.key)
        .collect()
}

fn release_owned_keys(
    path: &Path,
    kind: FileKind,
    keys: Vec<&str>,
    preserve: &[PathBuf],
    id: &str,
) -> Result<(), AgentError> {
    let Some(before) = read_optional(path)? else {
        clear_markers(path)?;
        return Ok(());
    };
    let text = std::str::from_utf8(&before).map_err(|_| refused(id, path, "file is not UTF-8"))?;
    let after =
        remove_keys(kind, text, &keys).map_err(|error| refused(id, path, &error.to_string()))?;
    let remove_file = created_here(path)
        && is_empty_document(kind, &after)
            .map_err(|error| refused(id, path, &error.to_string()))?;
    if remove_file {
        delete_file(path)?;
    } else if before.as_slice() != after.as_slice() {
        write_atomic(path, &after)?;
    }
    clear_markers(path)?;
    if remove_file {
        remove_empty_parents(path, preserve)?;
    }
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
        dirs.push(rooted(session, path));
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

fn rooted(session: &Session, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        session.home.join(path)
    }
}

fn claude_settings(session: &Session) -> PathBuf {
    let directory = session.claude_config_dir.as_ref().map_or_else(
        || session.home.join(".claude"),
        |path| rooted(session, path),
    );
    directory.join("settings.json")
}

fn codex_config(session: &Session) -> PathBuf {
    let directory = session
        .codex_home
        .as_ref()
        .map_or_else(|| session.home.join(".codex"), |path| rooted(session, path));
    directory.join("config.toml")
}

fn gemini_root(session: &Session) -> PathBuf {
    session
        .gemini_cli_home
        .as_ref()
        .map_or_else(|| session.home.clone(), |path| rooted(session, path))
}

fn gemini_settings(session: &Session) -> PathBuf {
    gemini_root(session).join(".gemini").join("settings.json")
}

fn gemini_env(session: &Session) -> PathBuf {
    gemini_root(session).join(".gemini").join(".env")
}

fn opencode_dir(session: &Session) -> PathBuf {
    match &session.xdg_config_home {
        Some(path) => rooted(session, path).join("opencode"),
        None => session.home.join(".config").join("opencode"),
    }
}

fn opencode_json(session: &Session) -> PathBuf {
    opencode_dir(session).join("opencode.json")
}

fn opencode_jsonc(session: &Session) -> PathBuf {
    opencode_dir(session).join("opencode.jsonc")
}
