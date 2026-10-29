# hofund

**English** | [Simplified Chinese](README.zh-CN.md)

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-dark.svg">
    <img alt="hofund" src="assets/brand/lockup-light.svg" width="320">
  </picture>
</p>

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

**0.1.0 is prepared.** The canonical IR, the Anthropic Messages, OpenAI Chat
Completions, OpenAI Responses, and Generative Language codecs, quirk profiles,
upstream adapters, ordered failover, the local HTTP server, config adapters
for Claude Code, Codex CLI, Gemini CLI, and OpenCode, and the menu-bar app
are implemented. See [docs/codecs.md](docs/codecs.md). This tree does not
publish a GitHub release; the tag step is described in
[docs/release.md](docs/release.md).

`hofund serve` listens on `127.0.0.1:3425` unless the config says otherwise.
Every `/v1` request needs the bearer token from `hofund token show`. The
commands are `serve`, `config check`, `token show`, `token rotate`, `usage`,
`agent list`, `agent connect`, `agent disconnect`, `agent status`, and
`subscription list`, `sign-in`, `sign-out`, `refresh`, and `quota`.

## Planned features

- Hub-and-spoke translation between LLM API protocols: Anthropic Messages,
  OpenAI Chat Completions, OpenAI Responses, and the Generative Language API.
  Each protocol is implemented once against a canonical IR, not as an N-by-M
  matrix of direct translators.
- Coding-agent config adapters (Claude Code, Codex, Gemini CLI, OpenCode, and
  others) that plan and apply edits so any agent can call any model.
- Routing groups, selection strategies, and failover across providers and
  accounts.
- A long-running local daemon and the `hofund` CLI.

## Install

Build the `hofund` binary from this tree:

```sh
cargo install --path crates/cli --locked
```

`cargo install --path` uses the lockfile in this repository. The crates stay
`publish = false`, so there is no crates.io package.

A version tag such as `v0.1.0` is what the release workflow publishes. That
workflow is in the repository and has not been run for 0.1.0. When a tag is
published, the assets are:

- `hofund-cli-installer.sh` and `hofund-cli-installer.ps1`, which download
  the CLI archive for the current machine (one architecture)
- per-target archives and `.sha256` checksums for macOS (arm64, x86_64, and
  a universal archive built with `lipo`), Linux (x86_64 and aarch64), and
  Windows (x86_64)
- menu-bar bundles from `apps/desktop`, with their own `SHA256SUMS`

The release steps, including optional code signing, are in
[docs/release.md](docs/release.md).

The menu-bar app is `hofund-desktop`. On Linux the build needs WebKitGTK 4.1
and the Ayatana AppIndicator headers:

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev patchelf libxdo-dev
cargo build -p hofund-desktop
```

The CLI and the menu-bar app both read `HOFUND_CONFIG` when it is set.
Otherwise they use `hofund/hofund.toml` under the platform config directory:
`XDG_CONFIG_HOME` or `~/.config` on Unix, `~/Library/Application Support` on
macOS, and `%APPDATA%` on Windows. `--config` selects another file. The app
creates a starter file on Start or Open config.

## Configuration

[`examples/hofund.toml`](examples/hofund.toml) is a complete file: two
`[[providers]]` tables and a `[[routes]]` table whose `upstreams` list is the
failover order. `api_key` may be a literal or `env:NAME`. Check it in place:

```sh
hofund config check --config examples/hofund.toml
```

`hofund config check` prints `config ok:` and the path it read when the file
loads. When it does not, the message starts with that path. `hofund --help`
and `hofund <command> --help` print that command's usage.

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
