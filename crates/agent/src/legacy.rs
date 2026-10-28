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

/// OpenCode global config.
///
/// The documented global path is `~/.config/opencode/opencode.json` or
/// `opencode.jsonc`, including on Windows (the docs say `~/.config`, not
/// `%APPDATA%`). When `opencode.jsonc` already exists it is the file that is
/// edited. Otherwise `opencode.json` is edited or created. A JSONC comment
/// does not survive apply; revert puts the original bytes back.
///
/// The provider entry is `provider.<id>` with `npm`
/// `@ai-sdk/openai-compatible`, `options.baseURL`, `options.apiKey`, and
/// `models`. `options.baseURL` includes `/v1` because that package appends
/// `/chat/completions`. `models` has one entry per route: `name` is the
/// route id and `variants` is `{}`. Modalities, context, output limits, and
/// a reasoning flag are omitted; this build has no catalog values for them.
/// An empty variants object keeps OpenCode from adding low, medium, and high
/// on its own. A string `model` is compared with route names and is not
/// rewritten. `hofund agent connect opencode` says how many routes were
/// written. The
/// v2 docs describe `providers` plus `package` and `settings`; this adapter
/// follows the current providers page instead. Both pages document
/// `~/.config` and do not name an override (checked 2026-10-25). When
/// `XDG_CONFIG_HOME` is set this adapter uses `$XDG_CONFIG_HOME/opencode`.
/// Unset, the path stays `~/.config/opencode`, including on Windows.
///
/// Sources: <https://opencode.ai/docs/providers/>,
/// <https://opencode.ai/docs/config/>. The v2 shape is
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
            key: "provider.hofund.name",
            slot: Slot::Literal("hofund"),
        },
        Set {
            file: "config",
            key: "provider.hofund.npm",
            slot: Slot::Literal("@ai-sdk/openai-compatible"),
        },
        Set {
            file: "config",
            key: "provider.hofund.options.baseURL",
            slot: Slot::V1,
        },
        Set {
            file: "config",
            key: "provider.hofund.options.apiKey",
            slot: Slot::Token,
        },
        Set {
            file: "config",
            key: "provider.hofund.models",
            slot: Slot::OpenCodeModels,
        },
    ],
    model: Some(ModelKey {
        file: "config",
        key: "model",
    }),
};

pub(crate) const OPENCODE_ADAPTER: Described = Described {
    description: &OPENCODE,
};

/// Frozen copy of the pre-F4 Rust const, used only as a test oracle for the
/// compiled opencode; not part of the public API; removed once F1-F4 are done.
#[doc(hidden)]
#[must_use]
pub fn opencode() -> &'static dyn AgentAdapter {
    &OPENCODE_ADAPTER
}

fn opencode_dir(session: &Session) -> PathBuf {
    match &session.xdg_config_home {
        Some(path) => path.join("opencode"),
        None => session.home.join(".config").join("opencode"),
    }
}

fn opencode_json(session: &Session) -> PathBuf {
    opencode_dir(session).join("opencode.json")
}

fn opencode_jsonc(session: &Session) -> PathBuf {
    opencode_dir(session).join("opencode.jsonc")
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

/// Gemini CLI user settings and env file.
///
/// User settings are `~/.gemini/settings.json`. `GEMINI_CLI_HOME` is the
/// parent directory: `.gemini` is created inside it. `.gemini/.env` is loaded
/// by the CLI and is not excluded. Gemini CLI's `homedir()` in
/// `packages/core/src/utils/paths.ts` returns `GEMINI_CLI_HOME` when set, and
/// `getGlobalGeminiDir` in `packages/core/src/config/storage.ts` joins that
/// home with `.gemini`. Both files deliberately follow that resolution instead
/// of always using the user home (`reference: internal/agent/gemini.go:28-31`);
/// trace discovery uses the same override
/// (`reference: internal/sessions/traces_gemini.go:17-22`).
///
/// `security.auth.selectedType` is set to `gemini-api-key`. The string values
/// of that enum are not listed in the public docs; they come from
/// `packages/core/src/core/contentGenerator.ts` (`gemini-api-key`,
/// `oauth-personal`, `vertex-ai`, `cloud-shell`,
/// `compute-default-credentials`, `gateway`).
///
/// `.env` sets `GEMINI_API_KEY`, `GOOGLE_GEMINI_BASE_URL` (the origin; HTTP
/// to localhost is allowed), and `GEMINI_API_KEY_AUTH_MECHANISM=bearer`.
/// The selected model is `model.name`. A string `model` is moved there.
/// An existing `model.name` is left as the user wrote it. When neither is
/// set and the gateway has one route, connect writes that route.
/// Checked 2026-10-25:
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
        Set {
            file: "settings",
            key: "model.name",
            slot: Slot::GeminiModel,
        },
    ],
    model: Some(ModelKey {
        file: "settings",
        key: "model.name",
    }),
};

pub(crate) const GEMINI_ADAPTER: Described = Described {
    description: &GEMINI,
};

/// Frozen copy of the pre-F3 Rust const, used only as a test oracle for the
/// compiled gemini-cli; not part of the public API; removed once F1-F4 are done.
#[doc(hidden)]
#[must_use]
pub fn gemini_cli() -> &'static dyn AgentAdapter {
    &GEMINI_ADAPTER
}

fn gemini_root(session: &Session) -> PathBuf {
    session
        .gemini_cli_home
        .as_ref()
        .map_or_else(|| session.home.clone(), |path| path.clone())
}

fn gemini_settings(session: &Session) -> PathBuf {
    gemini_root(session).join(".gemini").join("settings.json")
}

fn gemini_env(session: &Session) -> PathBuf {
    gemini_root(session).join(".gemini").join(".env")
}
