# Contributing

Thanks for contributing to hofund. This repository is pre-alpha. The scaffold
is in place; product behavior is not.

## Language

Everything in this repository is written in standard English. That includes
code, identifiers, comments, documentation, commit messages, pull request
titles and descriptions, issue templates, CI job names, and error messages.

Chinese, and any language other than English, is forbidden except in two
places:

1. Future i18n language-pack files.
2. Translated READMEs named `README.<lang>.md`, such as `README.zh-CN.md`.

`README.md` is always English and is the default. Do not put a translation in
`README.md`. Do not add a second language inside source files, issue
templates, or CI configuration.

## Architecture

Read [docs/architecture.md](docs/architecture.md) before adding a crate or a
dependency.

- Dependencies on the spine run one way only:
  `ir ← codec ← provider ← router ← gateway ← daemon ← cli`.
- `agent` depends only on `config`. It must never depend on `gateway`.
- Vendor names must never appear in `gateway` or `router` code. Quirks live in
  data (`quirks`) and in provider adapters.
- Do not use the magpie name or logo as hofund branding. The acknowledgement
  belongs in `README.md`, `README.zh-CN.md`, and `THIRD_PARTY_NOTICES.md`.

`cargo test --workspace` checks the dependency table and the vendor-name rule.
If you change an edge, update the manifest, `docs/architecture.md`, and
`xtask/tests/workspace_invariants.rs` together.

## Development

The pinned toolchain is `rust-toolchain.toml` (Rust 1.99.1, edition 2024).
`rustup` installs it on the first build.

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Formatting is `rustfmt` with its default edition 2024 style. Clippy warnings
are errors in CI. `unsafe` is forbidden by the workspace lint.

Workspace crates set `publish = false` until there is a stable API. Do not
publish them.

## Pull requests

- Write the title, the description, and the commit messages in English.
- Describe what changed and how you tested it.
- Keep the change scoped to the task. This tree does not yet contain product
  behavior; do not add a protocol implementation inside a scaffold-only change
  unless the issue asks for it.
- CI must be green: formatting, Clippy (`-D warnings`), tests on Linux, macOS,
  and Windows, and `cargo deny` (advisories, licenses, bans, and sources).

## Reporting issues

Use the issue templates. Write the issue in English. Report security problems
privately, as described in [SECURITY.md](SECURITY.md), instead of opening a
public issue.

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
