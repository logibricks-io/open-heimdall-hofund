# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `docs/feature-parity.md` records each capability with a stage (`core` or
  `auxiliary`) and the hofund status of that row.
- `docs/roadmap.md` records the two-stage plan. The terminal UI and every
  subscription capability, including the Claude bridge, are stage 1.
- A route member may name the model sent to that provider:
  `upstreams = [{ provider = "...", model = "..." }]`. A string is still a
  provider id.
- Clients may call a route by its name, by `provider/model`, or by a wire
  model that exactly one provider serves. A wire model served by several
  providers is an error that names the candidates.
- `GenerationParams.effort` maps reasoning effort across Chat Completions,
  Responses, Messages, and Generative Language.
- `POST /v1/messages/count_tokens` and
  `POST /v1beta/models/{model}:countTokens`. A Messages upstream is asked to
  count. Otherwise the body is a character estimate with `estimated` set.
- `GET /health` returns `{"status":"ok"}` with no authentication and no secrets.
- `hofund --help`, `-h`, and `help` print usage for each command.
- `examples/hofund.toml` documents `[[providers]]` and `[[routes]]`.
- The gateway accepts `x-api-key`, compared in constant time, on every routed request.

### Changed

- The CLI default config path matches the menu-bar app: the platform config
  directory, then `HOFUND_CONFIG`, then `--config`.
- `hofund config check` prints the path it read, and `config ok: <path>` on success.
- `hofund token rotate` is picked up by a running gateway on the next request.
  Connected agents have their token keys rewritten. When that edit fails, the
  command prints `hofund agent connect <id>`.
- `hofund agent disconnect` restores a file that still matches what connect
  wrote, and otherwise restores only the values hofund wrote. A value the
  user changed is kept, with a warning. Later keys stay. `--force` restores
  the backup bytes. A second disconnect exits 0.
- `hofund usage` prints local time and a totals line.
- `hofund agent connect --dry-run` redacts the bearer token in the diff.
- Connect warns when the tool's configured model has no route.
- OpenCode follows `XDG_CONFIG_HOME` when that variable is set.
- Disconnect removes empty directories that connect created.

### Fixed

- `hofund agent disconnect` no longer deletes a key that existed before
  connect, and it no longer removes the backup before the restored file is
  read back.
- A Responses request with `namespace`, `web_search`, or `custom` tools is
  accepted. The same protocol forwards the tools unchanged. A different
  protocol maps a custom tool to a function with one string input, flattens
  a namespace, and drops a tool it cannot map after a warning.
- A locally rejected request records the model named in the body.
- Client-facing Responses ids start with `resp_`, each output item has its
  own id, and a non-streaming Messages id starts with `msg_`.
- A Chat Completions stream forwards a usage chunk only when the client set
  `stream_options.include_usage`. That chunk has an empty `choices` array,
  and every chunk keeps `id` and `model`.
- `examples/hofund.toml` points the sample backup provider at a local URL.
- A system text that is only `x-anthropic-billing-header` is removed when the
  request is sent to a different protocol.
- The extension bag is forwarded only when the inbound and outbound protocols
  are the same. A different protocol drops it, including caller identifiers
  such as `metadata.user_id`, and drops raw stream events that the decoder
  does not model.
- A streamed Chat Completions upstream body sets
  `stream_options.include_usage`, so usage is returned and recorded.
- Anthropic streams write `message_start.message.usage`, prefix message ids
  with `msg_`, and emit `content_block_stop` before the single `message_delta`
  that carries usage.
- Responses streams emit `response.output_text.done`,
  `response.content_part.done`, a `response.output_item.done` item with `id`
  and `content`, a terminal `output` array, and `sequence_number` on every
  event.
- `token rotate` updates a running gateway, which previously kept accepting
  the previous token until restart.
- Disconnect keeps edits the user made while connected, such as a Codex
  `sandbox_mode` key.

## [0.1.0] - 2026-10-25

### Added

- Canonical chat IR in `hofund-ir`: messages, content parts (text, image,
  tool call, tool result, thinking), tool definitions and tool choice,
  generation parameters, usage, stop reasons, typed stream events, and an
  extension bag that preserves unknown JSON fields.
- `hofund-codec` implementations for Anthropic Messages, OpenAI Chat
  Completions, and OpenAI Responses, including SSE streaming. See
  [docs/codecs.md](docs/codecs.md).
- `hofund-quirks` profiles, `hofund-provider` adapters for Chat, Responses,
  and Messages, and `hofund-router` ordered failover before the first stream
  event.
- Local HTTP server on `127.0.0.1:3425`, TOML configuration, a bearer token
  file, SQLite usage rows, and the `serve`, `config check`, `token show`,
  `token rotate`, and `usage` commands.
- Coding-agent adapters for Claude Code, Codex CLI, Gemini CLI, and OpenCode.
  `hofund agent list`, `hofund agent connect`, `hofund agent disconnect`, and
  `hofund agent status` point an agent at the local gateway. Each edit is
  backed up once, written by rename, and restored byte for byte.
- Streamed Responses `encrypted_content` is kept on the thinking part and
  written back on encode.
- `GenerativeLanguage` codec for `generateContent` and
  `streamGenerateContent`, including function calls, system instructions,
  thinking parts, thought signatures, and usage metadata. The gateway serves
  `POST /v1beta/models/{model}:generateContent`,
  `POST /v1beta/models/{model}:streamGenerateContent`, and
  `GET /v1beta/models`. Authentication accepts the bearer token,
  `x-goog-api-key`, or a `key` query value, compared in constant time.
- Menu-bar app `hofund-desktop` (Tauri v2) at `apps/desktop`. It starts and
  stops the local gateway, shows the listen address, toggles each catalog
  agent through `hofund-agent`, prints the usage table, opens the config
  file, and quits. The config path is `HOFUND_CONFIG` or
  `hofund/hofund.toml` under the platform config directory.
- Release workflow for the `hofund` CLI (cargo-dist: macOS arm64, x86_64, and
  universal; Linux x86_64 and aarch64; Windows x86_64) and for Tauri bundles.
  Archives carry SHA-256 checksums. Code signing and notarization run only
  when the repository secrets are set.
- Initial Cargo workspace scaffold for the hofund local LLM gateway, with
  library and binary crates, architecture checks, CI, and repository
  hygiene files.
