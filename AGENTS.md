# AGENTS.md

Instructions for AI coding agents working in this repository. Humans should
follow [CONTRIBUTING.md](CONTRIBUTING.md) and [docs/architecture.md](docs/architecture.md).
This file restates the rules that are easy to violate in a generated change.

## Language

Write standard English only. This covers code, identifiers, comments,
documentation, commit messages, pull request titles and descriptions, issue
templates, CI job names, and error messages.

Do not write Chinese, or any language other than English, except:

- future i18n language-pack files
- translated READMEs named `README.<lang>.md` (for example `README.zh-CN.md`)

`README.md` stays English and is the default. Never add a translation beside
the English text inside a source file.

## What this repository is

hofund is a pre-alpha local LLM gateway in the LogiBricks Heimdall series.
Crates must not grow product behavior unless the task asks for it.

Present:

- canonical IR for requests, responses, and stream events (`ir`)
- hub-and-spoke codecs for Anthropic Messages, OpenAI Chat Completions,
  OpenAI Responses, and the Generative Language API (`generateContent` and
  `streamGenerateContent`), including streaming (`codec`). The field mapping
  and lossy conversions are in [docs/codecs.md](docs/codecs.md).
- named quirk profiles (`quirks`) and the `Provider` trait with Chat,
  Responses, and Messages adapters (`provider`)
- named smart/order/rotate/usage/pace/weight/free/fast/cheap routing strategies
  and failover before the first stream event (`router`)
- several upstream API keys per provider, with per-key rests, weights,
  protocol restrictions, and key management through config, control, and CLI
- an axum HTTP server (`gateway`), TOML configuration (`config`), a bearer
  token file (`secrets`), SQLite usage rows (`usage`), and the `hofund`
  commands `serve`, `config check`, `token show`, `token rotate`, and `usage`
- coding-agent config adapters (`agent`) for Claude Code, Codex CLI, Gemini
  CLI, and OpenCode, with `hofund agent list`, `hofund agent connect`,
  `hofund agent disconnect`, and `hofund agent status`
- subscription account commands (`daemon::sign_in`, `cli`): `hofund
  subscription list`, `sign-in`, `sign-out`, `refresh`, and `quota`
- a Tauri v2 menu-bar app at `apps/desktop` (`hofund-desktop`) that starts and
  stops the gateway, lists agents from the catalog, and shows the usage table

Planned behavior is the stage plan in [docs/roadmap.md](docs/roadmap.md).
RFC 0001, the accepted microkernel and plugin design, is in
[docs/rfc/0001-microkernel-plugins.md](docs/rfc/0001-microkernel-plugins.md).
The dependency table in `docs/architecture.md` includes the plugin seam.
Later migration pull requests update that table when they add an edge.
Not present yet:

- routing affinity and i18n language packs

## Architecture rules

The normative dependency table is in [docs/architecture.md](docs/architecture.md)
and is enforced by `xtask/tests/workspace_invariants.rs`.

Spine, each crate depending only downward:

`ir ← codec ← provider ← router ← gateway ← daemon ← cli`

`apps/desktop` (`hofund-desktop`) sits beside `cli` and depends only on
`daemon` and `agent`. Nothing in `crates/` depends on it.

- `agent` depends on `config` and `plugin-api`. Never on `gateway`, `router`,
  `provider`, `codec`, or `daemon`. Only the three kits depend on
  `plugin-host`.
- Vendor names must never appear in `crates/kernel/gateway` or `crates/kernel/router`.
  Quirks live in `quirks` data and in provider adapters. Protocol names belong
  in `codec`.
- Only `gateway` may depend on axum, tower, or hyper, and only when a task
  adds the HTTP server.
- `provider` must not read the secret store. `daemon` resolves secrets and
  passes material downward.
- No runtime crate depends on `xtask`.
- Do not use the magpie name or logo as hofund branding. Credit belongs in
  `README.md`, `README.zh-CN.md`, and `THIRD_PARTY_NOTICES.md`.

When you add a workspace dependency edge, update all three of: the crate
manifest, the table in `docs/architecture.md`, and `expected_deps` in
`xtask/tests/workspace_invariants.rs`.

## Learning from the upstream project

Before building a subsystem, read how yetone/magpie implements that part.
Where that design is sound, stable, and efficient, reimplement the behavior
in idiomatic Rust. Do not copy its source. The MIT notice in
`THIRD_PARTY_NOTICES.md` stays. Where that design is unsound, redesign it:
small types, trait boundaries, explicit state passed into constructors, no
process-global mutability, and units that can be tested without the rest of
the process.

Record the choice in `docs/magpie-comparison.md` (magpie approach, adopt or
redesign, hofund approach, rationale). Update that table in the same change
that adds or changes the subsystem. The name of the upstream project belongs
in that document, in `docs/coverage/`, and in the acknowledgement files, not
in gateway or router code, and not as hofund branding.

## Feature parity

hofund reimplements every core capability of yetone/magpie in Rust, and each
one should be better than the original where that original is unsound. The
matrix is [docs/feature-parity.md](docs/feature-parity.md). The execution
order is [docs/roadmap.md](docs/roadmap.md). Item-by-item inventories are
[docs/coverage/agents.md](docs/coverage/agents.md),
[docs/coverage/providers.md](docs/coverage/providers.md), and
[docs/coverage/subscriptions.md](docs/coverage/subscriptions.md). Stage 2
(auxiliary) waits until stage 1 has merged.

Every phase pull request updates the matrix rows it touches: status,
evidence, and why the hofund behavior is better. The adopt-or-redesign
verdict still goes in `docs/magpie-comparison.md` in the same change.

## Build and verify

Toolchain pin: `rust-toolchain.toml` (Rust 1.99.1). Workspace edition: 2024.
Resolver: 3. `unsafe_code` is forbidden. Clippy warnings are errors.

```sh
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`cargo xtask help` lists local tasks. Architecture checks run as part of
`cargo test --workspace`.

## Change discipline

- Match the existing scaffold. Do not add a framework, a trait hierarchy, or a
  dependency for a feature that has no caller.
- Shared package metadata and internal path dependencies go through
  `[workspace.package]` and `[workspace.dependencies]`.
- Crates stay `publish = false` until a task says otherwise.
- Commit messages, pull request text, and review comments on code are English.
- Do not merge pull requests unless the task explicitly says to merge.
