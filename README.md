# hofund

**English** | [Simplified Chinese](README.zh-CN.md)

hofund is a local LLM gateway in the LogiBricks Heimdall series. It translates
between LLM API protocols, points coding agents at any model, and fails over
across providers and accounts.

Heimdall is the gate. Hofund is the sword that guards it.

| Repository | Role |
| --- | --- |
| [open-heimdall](https://github.com/logibricks-io/open-heimdall) | The gate between your applications and intelligence |
| [open-heimdall-kernel](https://github.com/logibricks-io/open-heimdall-kernel) | Rust gateway, hexagonal architecture |
| open-heimdall-hofund | This repository: local protocol gateway and agent router |

## Status

**Pre-alpha.** The canonical IR and the Anthropic Messages and OpenAI Chat
Completions codecs are implemented, including streaming. See
[docs/codecs.md](docs/codecs.md). Routing, upstream adapters, the HTTP server,
OpenAI Responses, Gemini, config rewriting, and the desktop app are not
implemented. The `hofund` binary prints its version and exits.

## Planned features

- Hub-and-spoke translation between LLM API protocols: Anthropic Messages,
  OpenAI Chat Completions, OpenAI Responses, and later Gemini. Each protocol is
  implemented once against a canonical IR, not as an N-by-M matrix of direct
  translators.
- Coding-agent config adapters (Claude Code, Codex, Gemini CLI, OpenCode, and
  others) that plan and apply edits so any agent can call any model.
- Routing groups, selection strategies, and failover across providers and
  accounts.
- A long-running local daemon and the `hofund` CLI.
- A menu-bar / tray application. The reserved path is `apps/desktop` (Tauri v2).
  It is not created yet.

## Architecture

See [docs/architecture.md](docs/architecture.md) for the crate graph, the
one-way dependency rule, and the rule that vendor names stay out of `gateway`
and `router`.

## Build

Install [Rust](https://rustup.rs/). The first `cargo` command in this
repository installs the compiler pinned in `rust-toolchain.toml` (Rust 1.99.1,
edition 2024).

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask help
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: standard English
everywhere, except translated `README.<lang>.md` files and future i18n language
packs. `README.md` is always English.

## Acknowledgements

hofund is an independent reimplementation inspired by
[magpie](https://github.com/yetone/magpie) (MIT). It does not use that
project's name or logo, and it does not copy its source. See
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## License

[MIT](LICENSE). Copyright (c) 2026 LogiBricks.
