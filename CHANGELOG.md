# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `[providers.options]` and `[subscriptions.options]` are opaque TOML tables.
  A value may be a string, an integer, a boolean, an array, or a nested
  table. Vertex and Bedrock still read strings. A value of another type
  names the provider and the key.

### Deprecated

- `kind` and the short protocol names `chat`, `responses`, `messages`,
  `generative`, and `converse` still load for this release. They are rewritten
  to the codec ids `openai-chat`, `openai-responses`, `anthropic-messages`,
  `generative-language`, and `bedrock-converse`. The load warning names the
  replacement. `kind = "openai_compat"` may still select Responses. A `kind`
  that disagrees with `protocol` is an error. The next release rejects the
  old names.
- The provider keys `project`, `location`, `credentials`, `impersonate`,
  `access_key_id`, `secret_access_key`, `session_token`, and `region` still
  load at the top of a provider for this release. A non-empty value is copied
  into `[providers.options]` and the load warning names the key. The next
  release rejects the old keys.
- An empty or whitespace-only string is absent, both for those legacy keys
  and for string values in `[providers.options]` and `[subscriptions.options]`.
  It is not copied, and it is not an error. `0`, `false`, an empty array, and
  an empty table are kept. A blank string inside an array is kept.

### Fixed

- Gemini CLI connect writes `model.name`. A string `model` is moved there.
- OpenCode `models` lists each gateway route, with `name` and an empty
  `variants` object. Modalities, limits, and reasoning levels are not
  invented.
- Claude Code connect writes the Opus, Sonnet, and Haiku tier model env
  vars, and the small-fast model, from the selected model. The Fable tier
  is written only for a Fable model id. The subagent model and effort stay
  unset.

### Added

- The router speaks `Member` and `PluginError`. The daemon adapts each
  provider into a tower service with a kernel deadline and panic containment
  over the call and the whole response body, and publishes the route table
  as an `ArcSwap` snapshot at startup. A panic, including one while polling
  a stream, omits that member from the next snapshot. The body deadline is
  idle time since the last chunk. A stream that keeps sending is not cut
  off at twice the endpoint timeout. Same-protocol responses are forwarded
  as bytes. The codec reads token usage across chunk boundaries. When the
  name the client asked for is the name sent upstream, the body is forwarded
  without parsing it for a rewrite, and the typed response keeps the model
  the upstream returned. When the names differ, only the bytes of that
  protocol's model string are spliced, so numbers, escapes, and tool
  arguments stay byte-exact, and the typed response is labelled with the
  client name. The match is the field path, not the string value. A leading
  BOM is stripped. An event larger than 1 MiB splices a model string found
  in the held prefix and streams the rest. A model string not found by that
  cap, invalid JSON, trailing garbage, two JSON values, or a partial event
  flushed on an upstream error is forwarded unchanged and counted in
  `hofund_model_rewrite_skipped_total`. The contract is
  `docs/model-names.md`. A leading in-band
  failure still fails over. A later stream error is that protocol's error
  frame. Bytes still held by the rewriter are flushed and closed as an event
  first. A frame that already ends in `\r\n\r\n` or `\r\r` does not gain an
  extra blank event. The stream content type comes from the codec. Gateway
  routes, status codes, and usage provider ids are unchanged.
- The plugin seam from RFC 0001 is in `hofund-plugin-api`,
  `hofund-plugin-sdk`, and `hofund-plugin-host`, plus the three test kits.
  `hofund-provider` and `hofund-agent` re-export the new traits. Gateway,
  daemon, and CLI behavior is unchanged. Account rows are plaintext in
  `PlaintextFileAccountStore` (mode `0600`); encrypted storage is a
  prerequisite of subscription pull request H. A `HostContext` is bound to
  one plugin id. Secret reads are limited to that plugin. `StdProcess::kill`
  signals the child and reaps it on a background thread.
- Plugin manifests are parsed as TOML. A renamed `hofund-plugin-api` or
  `tokio` dependency is rejected, and a `[features]` forward enables
  `kernel` under the dependency's package name, including `api/kernel` and
  `?/kernel`. `cargo check -p` plus `cargo tree -e features` run for each
  plugin from the workspace root, and fail when `plugins/` exists but has
  no package, an unknown kind, or a manifest that cannot be read.
  `plugins/providers/fixture` is the package that job compiles. The source
  scan follows a `use` inside a function, `{self as x}`, `std::*`, raw
  identifiers, statement macros, attribute arguments, and `include!`.
  `std::net`, `libc`, `nix`, and `std::os::unix::process` are rejected.
- The Kernel Hilt is the hofund mark: an upright sword seated in an amber
  kernel, with plugin modules as the crossguard. README heroes, the desktop
  bundle icons, and the tray icon use `assets/brand`.
- RFC 0001, a microkernel and plugin design, is accepted in
  `docs/rfc/0001-microkernel-plugins.md` (2026-10-25, owner sign-off
  conditional on the architecture review). No crate moves in that pull
  request. Migration starts after it merges.
- Coverage inventories for the 65 coding agents, the 58 provider presets,
  and the 33 subscription rows are in `docs/coverage/`. The preset catalog
  pin is commit `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`.
  `internal/provider/presets.go` is unchanged since
  `5b2b3d3f50959735403dff6b77a85eec65772218`.
- A release dry-run builds local cargo-dist artifacts and the desktop
  bundles, then writes checksums, without creating a GitHub release. Pull
  requests that change the release config build the Linux x86_64 CLI.
  `workflow_dispatch` builds every local target.
- `preset = "google-vertex"` posts Vertex AI `generateContent` with an OAuth
  token minted from Application Default Credentials. `project` is required.
  `location`, `credentials`, and `impersonate` are optional. No API key is
  stored. A service-account key stays in the credentials file.
- `preset = "bedrock-mantle"` posts a closed OpenAI GPT id or an `xai.` id
  under `/openai/v1`, and every other chat or Responses model under `/v1`.
  A refusal that the model is not supported on that route is tried once at
  the other prefix. Messages stay on `/anthropic`.
- Azure OpenAI deployment names are listed from the resource. The
  data-plane list `GET /openai/deployments?api-version=2022-12-01` is asked
  with `api-key`. A refusal falls back to `GET /openai/v1/models`. Those
  names are the model ids.
- `protocol = "converse"` on preset `bedrock`, or `kind = "converse"`, posts
  Bedrock Converse and ConverseStream. The call is signed with SigV4
  (`access_key_id`, `secret_access_key`, optional `session_token`). No API
  key is sent on that path. Chat and Messages on the same preset still use
  a Bedrock API key.

### Fixed

- `dist build` uses Cargo profile `dist`. `[profile.dist]` inherits
  `release` and sets thin LTO, which is what `dist init` writes. Without
  that profile the tag build stops with `profile dist is not defined`.
- Desktop checksums and Windows signing read the bundle directory from
  `cargo metadata`. A workspace build writes bundles under the workspace
  target directory.
- A version tag can get past `dist host --steps=create`. `allow-dirty = ["ci"]`
  keeps the hand-edited release workflow (macOS repack, Windows signing, and
  the universal archive). The desktop workflow installs `@tauri-apps/cli`
  2.12.4. That package has no 2.12.5; the `tauri` crate is 2.12.5 and the CLI
  published with it is 2.12.4. Every pull request runs `dist plan` and
  installs that CLI.

## [0.1.0] - 2026-10-25

Early preview of the local gateway. The notes below are everything on `main`
at this tag.

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
- Provider presets. `[[providers]]` may set `preset` instead of `kind`, with
  optional `region`, `protocol` (`chat`, `responses`, `messages`, or
  `generative`), and `base_url`. The catalog is `crates/provider/presets.toml`.
- `kind = "generative"` posts the Generative Language API:
  `models/{model}:generateContent`, `streamGenerateContent?alt=sse`, and
  `countTokens`. The API key is sent as `x-goog-api-key`. `preset = "google"`
  uses that adapter. `protocol = "chat"` on that preset keeps the
  OpenAI-compatible root.

### Changed

- Responses usage objects no longer include `cache_write_tokens`. The counter
  stays on the internal usage object. `input_tokens_details` carries
  `cached_tokens` only.
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
- OpenCode writes `provider.hofund` with npm `@ai-sdk/openai-compatible`,
  `options.baseURL`, `options.apiKey`, and `models`.
- A provider and a route member may set `max_output_tokens`. Messages encode
  uses 16384 when the client, the route, and the quirk profile all omit a cap.
- A Responses usage object always includes `input_tokens`, `output_tokens`,
  `total_tokens`, `input_tokens_details`, and `output_tokens_details`. A
  missing counter is 0.
- Generative Language `usageMetadata` always includes `promptTokenCount`,
  `candidatesTokenCount`, and `totalTokenCount` when usage is present.
- `function_call` items no longer include an empty `encrypted_function_args`
  array.
- `hofund agent connect opencode` says that `models` stays empty because this
  build does not ship a model catalog and does not copy route names.

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
- A Messages upstream accepts a request that omitted `max_tokens`. A request
  the codec cannot encode is HTTP 400 `invalid_request_error`.
- A namespaced tool call comes back as `namespace` plus the original name.
  A custom tool call comes back to a Responses client as `custom_tool_call`
  with the raw input.
- A Responses stream uses the same `output_index` as
  `response.completed.output`.
- `POST /v1/messages/count_tokens` does not send `max_tokens`.
- Responses item ids are unique. A same-protocol reply keeps the upstream
  item id. A unary output item has `id` and `status`.
- A dropped tool is warned once per name per process.
- Chat usage includes `prompt_tokens`, `completion_tokens`, and `total_tokens`
  whenever a usage object is written. A missing input or output is 0. A
  missing total is their sum.
- A Messages stream's usage is merged across `message_start` and
  `message_delta` when it is encoded for Responses, Chat Completions, or
  Generative Language. An output-only delta no longer drops the input tokens.
- Responses history items `custom_tool_call` and `custom_tool_call_output`
  sent to Chat or Messages keep the call id, the raw input, and the output.
  The call is an assistant tool call with arguments `{"input": <raw>}`. The
  output is a tool result.
- A namespaced `function_call` in history is sent upstream under the flattened
  name declared for that tool.

### Known limitations

- Tested only against mock upstreams. This release has no live OpenAI,
  Anthropic, or Gemini runs.
- Gemini CLI and OpenCode were verified via config only.
- The Codex `apply_patch` path was verified via HTTP only.
- Binaries are unsigned.
- The desktop tray app was not smoke-tested.
- Stage-1 parity is incomplete. See [docs/feature-parity.md](docs/feature-parity.md).
