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
- ordered model routes and failover before the first stream event (`router`)
- an axum HTTP server (`gateway`), TOML configuration (`config`), a bearer
  token file (`secrets`), SQLite usage rows (`usage`), and the `hofund`
  commands `serve`, `config check`, `token show`, `token rotate`, and `usage`
- coding-agent config adapters (`agent`) for Claude Code, Codex CLI, Gemini
  CLI, and OpenCode, with `hofund agent list`, `hofund agent connect`,
  `hofund agent disconnect`, and `hofund agent status`
- a Tauri v2 menu-bar app at `apps/desktop` (`hofund-desktop`) that starts and
  stops the gateway, lists agents from the catalog, and shows the usage table

Planned behavior, not present yet:

- further routing strategies, and i18n language packs

## Architecture rules

The normative dependency table is in [docs/architecture.md](docs/architecture.md)
and is enforced by `xtask/tests/workspace_invariants.rs`.

Spine, each crate depending only downward:

`ir ← codec ← provider ← router ← gateway ← daemon ← cli`

`apps/desktop` (`hofund-desktop`) sits beside `cli` and depends only on
`daemon` and `agent`. Nothing in `crates/` depends on it.

- `agent` depends only on `config`. Never on `gateway`, `router`, `provider`,
  `codec`, `ir`, or `daemon`.
- Vendor names must never appear in `crates/gateway` or `crates/router`.
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
in that document and in the acknowledgement files, not in gateway or router
code, and not as hofund branding.

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
