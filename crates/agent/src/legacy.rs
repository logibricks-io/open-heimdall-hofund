//! Frozen const adapters retained solely for migration equivalence tests.

use std::path::PathBuf;

use crate::AgentAdapter;
use crate::Session;
use crate::catalog::{Described, Description, Location, ModelKey, Set, Slot};
use crate::edit::FileKind;

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
