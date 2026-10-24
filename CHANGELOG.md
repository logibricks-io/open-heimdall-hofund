# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Canonical chat IR in `hofund-ir`: messages, content parts (text, image,
  tool call, tool result, thinking), tool definitions and tool choice,
  generation parameters, usage, stop reasons, typed stream events, and an
  extension bag that preserves unknown JSON fields.
- `hofund-codec` implementations for Anthropic Messages and OpenAI Chat
  Completions, including SSE streaming. See [docs/codecs.md](docs/codecs.md).
- Initial Cargo workspace scaffold for the hofund local LLM gateway, with
  library and binary crates, architecture checks, CI, and repository
  hygiene files.
