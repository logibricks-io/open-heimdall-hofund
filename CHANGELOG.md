# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- D-227: A Claude Code request through the Claude subscription bridge that carries
  an advisor server tool (`advisor_20260301`) for a model this gateway serves
  keeps the client's instructions, offers an advisor tool beside the client's
  tools, and answers a call by asking that model through normal routing as the
  original gateway key. The client is told `server_tool_use` and
  `advisor_tool_result`. One reply may ask three times; the next is
  `max_uses_exceeded` and is not asked. A model this gateway does not serve is
  left out and those instructions are voided. One advisor call waits at most
  55 seconds. A dashed Claude id is sent as the provider spells the same
  model. Relayed Messages replies record each `usage.iterations` advisor call
  as its own usage row, kind `advisor`, priced at the advisor model. The
  executor row is unchanged. A web search on that run, when one is installed,
  uses the same original key. Refs D-227.
- D-221: Antigravity sign-in, import, and add are refused for new accounts because Google suspends accounts it sees on Code Assist through Antigravity's own client. Saved accounts keep working and show a risk line. The message points at the agy CLI route. Gemini CLI is unchanged.
- D-225: Generative Language and Vertex model paths escape each slash-separated segment and keep the slashes. An id with an empty, `.`, or `..` segment is escaped as one segment.

### Fixed

- D-226: Using a Claude setup-token account writes `CLAUDE_CODE_OAUTH_TOKEN` into Claude Code's `settings.json` `env`, records only a hash of that value, and leaves a value it did not write in place. Switching to a signed-in account or removing the setup-token account takes the recorded value out. A pasted setup-token line in the Claude import is stored as a setup-token account. The settings file is mode `0600` and written atomically. The token is not logged. Other subscriptions return unsupported for that paste, including a line of any length or bytes that starts with `sk-ant-oat`, and make no token request; when Claude is not configured, import fails without sending the token. Automatic switching does not select a setup-token account and does not move off one that is in use. `next` stays on it too. Naming another account, or signing the setup-token account out, still switches. An explicit Use still writes the env token, including when the live profile cannot be named, and leaves the credentials file unchanged. `keep_login` uses the first switchable account. A remembered return account that is not switchable is ignored. Clearing it leaves the hash record in place when the home or the value is not the one recorded. A mixed paste, including a JSON array that contains a setup-token and other entries, stores the setup-token account and then reports the first other entry, because import returns one result.
- D-229: A Copilot turn refused with "too many images" learns that model's cap, replaces the oldest images outside the latest user message with a placeholder, and sends the turn once more. Later turns for that account and model are fitted before they are sent. A turn already within the cap is unchanged. When the latest user message alone is over the cap, the refusal is returned unchanged. The cap comes from the refusal, not from the model list.
- D-234: A Claude subscription account, or an Anthropic API key whose upstream is
  `api.anthropic.com`, that the vendor has turned off rests for two hours and
  the routing group asks the next member, including that provider's other
  accounts. The same words from a relay fail over with an ordinary failure
  rest, not a restricted one. A token count, and a 200 body that is only an
  error object, rest the same way. Extra-usage and Usage Policy refusals are
  unchanged. Refs D-234.
- D-228: A Messages body whose `tool_use` and `tool_result` blocks do not pair is
  mended before it is sent to a Messages upstream. An unanswered call gets a
  synthetic error result, a result with no call becomes the text it said, a
  second answer is dropped, and a result after other content moves first. A
  body that already pairs keeps its bytes.
- D-236: That same mend moves blocks an assistant wrote after its first
  `tool_use` to before the call (thinking stays, empty text is dropped), moves
  a system or developer message that sits between a call and its results to
  after the results, and keeps two calls that share an id as two ids, each
  answered. Passthrough, `count_tokens`, and translated Messages builds use
  this one pass. A body that is already in shape is not serialized again.
- D-232: GPT-6.2 and later send Codex instructions, the client's instructions,
  and a translated system prompt as leading `developer` input items, with no
  top-level `instructions`. Older models stay unchanged. This applies to the
  Codex subscription, the ChatGPT API subscription, and translated Chat and
  Anthropic Responses encodes. When Codex's instructions are unknown or blank,
  the client's text still moves to the front of `input` on those models.
- D-223: A Codex Responses Lite turn (an `additional_tools` input item, or
  `reasoning.context` of `all_turns`, and no client instructions) is sent
  without added instructions and without `reasoning.summary`. A Lite turn on
  GPT-6.2 keeps those Lite rules. A socket replay removes `x-codex-turn-state`
  from `client_metadata` once it is a header, and that socket keeps the last
  reply's state for the same turn id. A per-account store for pooled turns is
  not implemented.
- D-231: ChatGPT model-list asks, the `version` header, the User-Agent, and
  device sign-in name the newest plain `x.y.z` among a `0.169.1` floor, npm's
  latest `@openai/codex` release (read at most every six hours on the host
  monotonic clock), and a newer
  Codex client already seen on a routed request. A model list asked at an
  older client version is fetched again. Refs the reference.
- D-233: Claude bridge plans set `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0` and `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=0` unless the process environment already sets that name, and a WSL plan lists both on `WSLENV`, so a client tool is waited on instead of being moved to the background or aborted at the idle limit. One-off runs are unchanged.

## [0.2.0-rc.2] - 2026-11-18

This is a second preview (prerelease) of v0.2.0. It exists so the tag
workflows can publish the CLI/daemon archives, shell and PowerShell
installers, and the universal macOS archive that `v0.2.0-rc.1` missed.
The `v0.2.0-rc.1` tag and GitHub Release are left in place.

Real-account smoke tests for subscriptions are still pending and remain
Palmer's work: #304, #58, and #310. Request serving on this cut is verified
with mocks and loopback tests only; do not treat it as live-verified.

Held work is not in this cut: true 4.2–4.4 session recording, Library writes,
and MCP execution (awaiting a second security review); R-47b; R-47 k
credit-reset / `quota reset` / automatic consume; and the settings UI.
Desktop bundles (macOS `.dmg`/`.app`, Linux `.deb`/AppImage, and a Windows
NSIS installer) are produced by the existing tag workflow. The Windows MSI is
omitted from this prerelease because WiX requires a numeric-only prerelease
identifier. Signing and notarization run only when repository secrets are
set; otherwise the artifacts are unsigned.

### Changed

- The detailed `[0.2.0-rc.1]` change list moved to
  `docs/changelog-v0.2.0-rc.1.md`, so cargo-dist announcement bodies stay
  well under Linux `MAX_ARG_STRLEN` (~128 KiB).
- The Release workflow reads GitHub Release notes from `dist-manifest.json`
  with `jq` into a file, and no longer passes the announcement body through
  an environment variable or process argument. The plan job also writes a
  slim manifest (artifact matrix only) to `GITHUB_OUTPUT`.
- The Release desktop attach job sets `GH_REPO`, surfaces `gh` stderr, waits
  for the CLI release, and creates a prerelease stub if that workflow did
  not, then uploads desktop bundles and a combined `SHA256SUMS`.

## [0.2.0-rc.1] - 2026-11-18

This is a preview (prerelease) of v0.2.0. Real-account smoke tests for
subscriptions are still pending and remain Palmer's work: #304, #58, and
#310. Request serving on this cut is verified with mocks and loopback tests
only; do not treat it as live-verified.

Held work is not in this cut: true 4.2–4.4 session recording, Library writes,
and MCP execution (awaiting a second security review); R-47b; R-47 k
credit-reset / `quota reset` / automatic consume; and the settings UI.
Desktop bundles (macOS `.dmg`/`.app`, Linux `.deb`/AppImage, and a Windows
NSIS installer) are produced by the existing tag workflow. The Windows MSI is
omitted from this prerelease because WiX requires a numeric-only prerelease
identifier. Signing and notarization run only when repository secrets are
set; otherwise the artifacts are unsigned.

The cargo-dist Release workflow did not attach CLI/daemon archives on this
tag, because the original change list in this section exceeded Linux
`MAX_ARG_STRLEN`. The full change list is in
[docs/changelog-v0.2.0-rc.1.md](docs/changelog-v0.2.0-rc.1.md). The
`v0.2.0-rc.1` tag and GitHub Release were left in place; later prereleases
starting with v0.2.0-rc.2 carry the CLI archives.

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
