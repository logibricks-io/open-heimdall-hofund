//! Frozen const adapters retained solely for migration equivalence tests.

use std::path::PathBuf;

use crate::AgentAdapter;
use crate::Session;
use crate::catalog::{Described, Description, Location, ModelKey, Set, Slot};
use crate::edit::FileKind;

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
/// is sent as `Authorization: Bearer`. A string `model` already in the file
/// stays. When the file has none and the gateway has one route, connect
/// writes that route as `model`. Opus, Sonnet, Haiku, and
/// `ANTHROPIC_SMALL_FAST_MODEL` then name that same model. Fable is written
/// only when the model id contains `claude-fable-`; a non-Fable id in
/// `ANTHROPIC_DEFAULT_FABLE_MODEL` makes Claude Code treat the main model as
/// Fable. `CLAUDE_CODE_SUBAGENT_MODEL` stays unset so a subagent that names
/// no model keeps the session model. `effortLevel` and
/// `CLAUDE_CODE_EFFORT_LEVEL` stay unset because connect does not pick a
/// level. Checked 2026-10-25: <https://code.claude.com/docs/en/model-config>,
/// <https://code.claude.com/docs/en/env-vars>.
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
        Set {
            file: "settings",
            key: "model",
            slot: Slot::ClaudeModel,
        },
        Set {
            file: "settings",
            key: "env.ANTHROPIC_DEFAULT_OPUS_MODEL",
            slot: Slot::ClaudeTier("opus"),
        },
        Set {
            file: "settings",
            key: "env.ANTHROPIC_DEFAULT_SONNET_MODEL",
            slot: Slot::ClaudeTier("sonnet"),
        },
        Set {
            file: "settings",
            key: "env.ANTHROPIC_DEFAULT_HAIKU_MODEL",
            slot: Slot::ClaudeTier("haiku"),
        },
        Set {
            file: "settings",
            key: "env.ANTHROPIC_DEFAULT_FABLE_MODEL",
            slot: Slot::ClaudeTier("fable"),
        },
        Set {
            file: "settings",
            key: "env.ANTHROPIC_SMALL_FAST_MODEL",
            slot: Slot::ClaudeTier("fast"),
        },
    ],
    model: Some(ModelKey {
        file: "settings",
        key: "model",
    }),
};

pub(crate) const CLAUDE: Described = Described {
    description: &CLAUDE_CODE,
};

/// Frozen copy of the pre-F2 Rust const, used only as a test oracle for the
/// compiled claude-code; not part of the public API; removed once F1-F4 are done.
#[doc(hidden)]
#[must_use]
pub fn claude_code() -> &'static dyn AgentAdapter {
    &CLAUDE
}

fn claude_settings(session: &Session) -> PathBuf {
    let directory = session
        .claude_config_dir
        .as_ref()
        .map_or_else(|| session.home.join(".claude"), |path| path.clone());
    directory.join("settings.json")
}

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

pub(crate) const CODEX_ADAPTER: Described = Described {
    description: &CODEX,
};

/// Frozen copy of the pre-F1 Rust const, used only as a test oracle for the
/// compiled codex; not part of the public API; removed once F1-F4 are done.
#[doc(hidden)]
#[must_use]
pub fn codex() -> &'static dyn AgentAdapter {
    &CODEX_ADAPTER
}

fn codex_config(session: &Session) -> PathBuf {
    let directory = session
        .codex_home
        .as_ref()
        .map_or_else(|| session.home.join(".codex"), |path| path.clone());
    directory.join("config.toml")
}
