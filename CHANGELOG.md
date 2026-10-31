# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Responses continuation history (P0-1a): `/v1/responses` now resolves
  `previous_response_id` in a caller-scoped, bounded in-memory LRU before
  decoding and routing. Unary and SSE replies are captured without delaying
  streaming; translated routes receive whole history, same-protocol relays can
  send a delta with explicit `store: true`, and a refused delta is retried
  whole once. Unknown ids on translating or subscription routes return the
  OpenAI-shaped 400 error. History expires after six hours and is lost on
  restart; upstream `store: false` remains the default when omitted.

- Agent keys in the OS credential store (Z1): an agent descriptor may
  declare a `[credential]` item (URL and Windows target templates, literal
  account and label). The engine stores hofund's key there through a
  `CredentialStore` passed to the `Session`, before the config files are
  written; it is idempotent, rewritten on token refresh, reported as drift
  when missing or replaced, and removed on disconnect only while it still
  holds hofund's key. A record beside the managed file keeps a SHA-256
  digest of the key, never the key. `hofund-kit-agent` adds
  `MemoryCredentialStore` for tests. Native OS backends follow in Z2.
- Native credential stores (Z2): `hofund_plugin_host::native_credential_store()`
  keeps agent keys in the macOS keychain (internet password by URL and
  account), the Secret Service default collection (attributes `url` and
  `username`) and Windows Credential Manager (generic credential by target,
  local persistence), without child processes and with a 90 s deadline per
  call. The daemon and the desktop app use it through
  `hofund_daemon::agent_session()`.
- Zed agent (Z3): `hofund agent connect zed` writes the OpenAI-compatible
  provider `language_models.openai_compatible.hofund` (gateway `/v1`, one
  model per route with a 128000-token window and tool calling) to Zed's
  `settings.json` (`~/.config/zed` on Linux and macOS,
  `%APPDATA%\Zed` on Windows) and puts hofund's key in the OS credential
  store where Zed looks it up by the provider URL. A pick writes
  `agent.default_model` with provider `hofund`; clearing it puts back the
  user's choice. Golden conformance now runs credential agents against an
  in-memory store and checks the item through connect, refresh and both
  disconnects. Not yet: XDG and APPDATA overrides (#411), catalog windows
  and reasoning effort (#459), the restart notice (#460), keeping
  JSONC comments while connected (#461).
- Goose agent (G14): `hofund agent connect goose` writes hofund's own
  custom provider, `custom_providers/hofund.json` beside Goose's
  `config.yaml` (`~/.config/goose`, or `$XDG_CONFIG_HOME/goose` when it is
  absolute, on Linux and macOS; `%APPDATA%\Block\goose\config` on Windows,
  where `XDG_CONFIG_HOME` is not followed): engine `openai`, gateway `/v1`,
  hofund's key as the `Authorization` header, `skip_canonical_filtering`,
  one `models[]` entry per route, and `dynamic_models: false` only while a
  route is listed (Goose rejects an empty static list it may not
  discover). Two deviations from the reference, by design: the provider
  file is written at connect rather than only on a pick, so Goose lists
  one more provider until disconnect removes it; and clearing the model
  puts back the user's `active_provider` instead of deleting it. A pick
  writes `active_provider: hofund` and `providers.hofund.{enabled,
  configured, model}` (the bare route). User keys in `hofund.json` (extra
  headers, `timeout_seconds`) are kept. Not yet: the legacy
  `GOOSE_PROVIDER`/`GOOSE_MODEL` layout (#450), per-model reasoning and
  `context_limit` (#451), `GOOSE_THINKING_EFFORT` (#452), a pick changed
  in Goose (#453), WSL homes (#454), telling the pressly migration tool
  apart (#455), and dropping a stale `dynamic_models` once every route is
  gone (#456); YAML comments are lost on a pick (#414).
- agent.toml: `[[file]] env_os = ["linux", "macos"]` limits `env_override`
  to the named systems, and `env_replaces` is checked only against their
  paths (part of #411); a `json` slot with `with_models = true` is written
  only while at least one visible route is listed.
- Grok Build agent (G13): `hofund agent connect grok` writes one
  `[model."hofund/<route>"]` table per route (gateway `/v1`, hofund's key,
  `api_backend = "chat_completions"`) to `~/.grok/config.toml` (or under an
  absolute `GROK_HOME`), leaving the user's own model tables alone. A model
  pick writes `[models] default` and turns xAI's remote campaigns off
  (`[features] campaigns = false`), since a launch campaign would override the
  default; clearing it drops the default (#433) and puts back the user's
  campaigns setting. No catalog display names, context windows or reasoning
  efforts yet (#447), and WSL homes are not wired (#448).
- Kimi Code agent (G12): `hofund agent connect kimi` writes
  `[providers.hofund]` (type `kimi`, `base_url` with `/v1`, `api_key`) and
  one `[models."hofund/<route>"]` table per route (128000 context, tool
  use) to `~/.kimi-code/config.toml` (or under an absolute
  `KIMI_CODE_HOME`), leaving the user's own providers and models alone. A
  model pick writes `default_model`; clearing it drops the key (#433) and
  disconnect puts back the user's. No
  per-model catalog values yet (#441), a user comment right above `default_model` is lost when it is cleared (#444), and the older kimi-cli's `~/.kimi` is
  not written (#442). The conformance kit's TOML user edit is now a
  top-level key, so it no longer lands inside a table hofund owns.

- Cline agent (G11): `hofund agent connect cline` lists each route as
  `hofund/<route>` under `providers.openai-compatible.models` in
  `~/.cline/data/settings/models.json` (or under an absolute `CLINE_DIR`),
  leaving the user's own models alone. A model pick writes hofund's whole
  `openai-compatible` entry in `providers.json` (gateway `/v1`, token, model,
  `updatedAt`) and sets `lastUsedProvider`; clearing the model or
  disconnecting puts back the user's entry and provider. A token refresh
  rewrites only the key, and only while the entry still points at the
  gateway. No VS Code extension settings (#437), fixed model capabilities
  and no reasoning effort (#438), and `CLINE_DATA_DIR` is not followed (#439).
- Agent descriptions can write a `model_pair` as a typed JSON value
  (`json = '...'`) and may put `{token}` in it; a token refresh rewrites a
  pair that holds `{token}` while the picked model is a route. Clearing the
  model or disconnecting now restores a number, flag, or object the user had
  as that type, and a file hofund created and a clear emptied is removed on
  disconnect.
- VS Code agent (G10): `hofund agent connect vscode` appends one
  `customendpoint` group named `hofund` to `chatLanguageModels.json` beside
  the default profile's `settings.json` (VS Code Stable only), each route at
  the gateway's `/v1/chat/completions` with the token as its `Authorization`
  header. A model pick writes `chat.defaultModel` as the route id; clearing
  the model drops it (#433) and disconnect puts back the user's value.
- Agent descriptions can write gateway text and nested objects inside model
  entries (`{ key, template }`, `{ key, object = [...] }`), and a key part
  may be quoted to hold a dot (`'"chat.defaultModel"'`). A file nothing is
  written to is now left as it is, and a missing one is not created.

- Qwen Code agent (G9): `hofund agent connect qwen` appends one entry per
  route to `modelProviders.openai` in `~/.qwen/settings.json` (or under an
  absolute `QWEN_HOME`), after the user's entries, each reading its key from
  `env.HOFUND_QWEN_API_KEY` in the same file. A model pick writes
  `model.name`, points `model.baseUrl` at the gateway's `/v1`, and sets
  `security.auth.selectedType` to `openai`; clearing the model puts back the
  user's values. Connect alone leaves the auth type alone, so a user on
  another sign-in keeps it until they pick a hofund route.

- `model_pair` values fill in `{origin}` and `{v1}` like `template` does.

- Droid agent (G8): `hofund agent connect droid` appends one entry per route
  to `customModels` in `~/.factory/settings.json` (or under an absolute
  `FACTORY_HOME_OVERRIDE`), after the user's entries, each on Chat
  Completions at the gateway's `/v1`. A model pick writes
  `sessionDefaultSettings.model` as `custom:hofund/<route>`.

- Agent descriptions can fill gateway values into text and pair a provider
  with the model (G6, #418): `slot = { template = "Bearer {token}" }` writes
  the text with `{origin}`, `{v1}`, and `{token}` filled in and is rewritten
  on token refresh. `slot = { model_pair = { value, unless } }` writes
  `value` beside the model key whenever the model is set to a route, and on
  connect when the model is a route and neither this key nor an `unless`
  key is set; clearing the model drops it.
- Clearing the model of an agent whose description pairs a provider with it
  now puts back the provider the user had picked before connect, instead of
  dropping it (G7). Cline uses this to hand `lastUsedProvider` back.
- Agent descriptions can write a model map (G5, #417): `slot = { model_map =
  { prefix, fields } }` writes one entry per visible route model, named
  prefix plus model id, into a JSON, YAML, or TOML map. Entries without the
  prefix are the user's and are kept; an empty prefix owns the whole map and
  disconnect restores the original. TOML entries are written as typed tables.
- Agent descriptions can edit YAML files (G1): `kind = "yaml"` in an
  `agent.toml` file entry. Edits re-emit the file, so comments and layout are
  not kept; disconnecting restores the original bytes from the backup.
- Hermes Agent (G3): `hofund agent connect hermes` writes `providers.hofund`
  (name, `base_url` with `/v1`, `api_key`, `api_mode` `chat_completions`, and
  one `models` row per route) to `~/.hermes/config.yaml`
  (`%LOCALAPPDATA%\hermes\config.yaml` on Windows). It does not pick a model
  or honor `HERMES_HOME` yet (#411, #412), and YAML comments are not kept
  (#414).
- Pi agent (G3): `hofund agent connect pi` writes `providers.hofund` (name,
  `baseUrl` with `/v1`, `api` `openai-completions`, `apiKey`, session affinity
  headers on, and one `models` entry per route) to `~/.pi/agent/models.json`,
  or `$PI_CODING_AGENT_DIR/models.json` when that is an absolute path. It does
  not pick a model; choose hofund in Pi's `/model` picker.
- Crush agent (G3): `hofund agent connect crush` writes `providers.hofund`
  (type `openai`, `base_url` with `/v1`, `api_key`, and one `models` entry per
  route) to `~/.config/crush/crush.json`, or
  `%LOCALAPPDATA%\crush\crush.json` on Windows. It does not pick a model;
  choose hofund in Crush's model picker.
- Two agent slots for JSON and YAML files (G2): `{ json = "<json>" }` writes a
  fixed typed value such as `false`, and `{ model_array = { fields = [...] } }`
  writes one object per distinct visible model, each field holding the model
  id (`value = "id"`), a fixed string (`literal`), or a fixed JSON value
  (`json`). A model array is a gateway slot, so a `route = "none"` agent
  cannot use it.
- Shared-array append slot for JSON and YAML files (G4):
  `{ append = { each = "model" | "once", owner = [...], fields = [...] } }`
  adds hofund's objects after the user's entries in an existing array (key
  `"."` is the document root) instead of replacing the array. An entry is
  hofund's when it is an object whose string fields pass every `owner` test
  (`equals` or `prefix`); each test must hold for the written objects, so a
  second connect replaces rather than duplicates them. Connect, token refresh,
  and status touch only those entries; disconnect removes them, keeps every
  other entry, including ones added after connect, and drops the key only
  when it was absent before and no entry is left. Fields gain `value = "v1"`,
  `"origin"`, and `"token"`, `prefixed = "<text>"` (the prefix and the model
  id), and nested `models = [...]` (one object per model). Append is a
  gateway slot.
- Codex accounts on Codex's ChatGPT backend (S6d): a
  `POST /backend-api/codex/responses` for one of Codex's own models, say
  `gpt-5`, is served in process as `codex/gpt-5` when the gateway lists a
  `codex/gpt-5` route, so the turn runs on hofund's Codex subscriptions and
  compacts like a hofund model's. An `sk-` bearer, `/responses/compact`, and a
  model list that cannot be had leave the turn to the relay. A relayed turn
  that carries `x-hofund-account` is a 400 instead of going out on Codex's own
  sign-in.
- Compaction for hofund's models on Codex's ChatGPT backend (S6c): a
  `POST /backend-api/codex/responses` for a hofund model with a
  `compaction_trigger` asks the model, in process and without tools, for a
  summary with Codex's compaction prompt, and answers it as one `compaction`
  item in a Responses stream. Sent back later, to any model, that item is
  opened as a user message with Codex's summary prefix. A refused summary
  request keeps its status; a failed, cut-short or empty reply is a 502 that
  says why. For Codex's own models, `/responses` and `/responses/compact`
  bodies have their input put right before they are relayed: call item ids
  get the `tsc_` or `ctc_` prefix, a `call_id` over 64 bytes becomes its hex
  SHA-256, item ids are made letters, digits, `_` and `-`, and reasoning
  written elsewhere is dropped. A body with nothing to put right goes byte
  for byte.
- Codex's ChatGPT backend on the gateway (S6b): the `codex-catalog` plugin
  also serves `GET` and `POST /backend-api/codex`, so a Codex signed in to
  ChatGPT can point its built-in provider at the gateway and keep that
  sign-in. `GET /models` is the backend's list for that sign-in followed by
  the gateway's models (3 s wait; a 401 or 403 comes back as sent). A
  `POST /responses` naming a model with a `/` runs in process as
  `/v1/responses`, and `/responses/compact` for one is a 400. Everything else
  is relayed unchanged but for hop-by-hop headers, `Host` and
  `Accept-Encoding`, to the ChatGPT backend, or to the API for an `sk-` key.
  A websocket upgrade is a 426.
- `SurfaceHost::http()` (S6a): an inbound surface can relay a client's
  request on the kernel's pooled client, with the client's own credential.
  The daemon gives surfaces a pool with its connect and idle-read limits;
  `Gateway::with_surface_http` sets it, and plugin-host adds
  `surface_host_with_http`. Without it the handle is `None`. `HttpHandle` is
  now `Debug` and `Eq` (same pool).
- Speech to text in the `retrieval` inbound plugin (S5):
  `POST /v1/audio/transcriptions` and `POST /v1/audio/translations` take a
  `multipart/form-data` form with a file and a `model` field and forward it,
  byte for byte but for the `model` fields, to the providers of that model.
  A body that is not such a form, or names no model, is a 400 before any
  provider is called. Every retrieval answer now keeps the provider's
  content type (JSON when it sends none), so `text`, `srt` and `vtt`
  transcripts come back as they were sent. Gateway surfaces take bodies up
  to 32 MiB (codec routes keep 8 MiB) and have 180 seconds instead of 60.
- The `retrieval` inbound plugin (S4): `POST /v1/embeddings` and
  `POST /v1/rerank`, forwarded as they are to the providers of the model the
  body names. Members that do not serve the path (no OpenAI-style route, or a
  404, 405 or 501) are skipped in route order; the answer keeps its status,
  and each call writes a usage row with the input tokens the answer reports.
  Adds `Forward`, `RawBody` and `RawAnswer` to the plugin API,
  `GatewayHandle::forward`, `UpstreamProvider::forward` (unsupported by
  default) and plugin-sdk `wire::forward`.
- The `codex-catalog` inbound plugin (S3): `GET /v1/codex/models`, the
  model catalog a Codex whose model provider is this gateway reads from its
  `model_catalog_url`. Each model `/v1/models` lists becomes a `models.json`
  entry with its own base instructions, text input, no effort levels, and
  Codex's default window. Entries past the 1 MiB Codex reads are left out,
  the last first, and `x-hofund-left-out` says how many.
- The `muse` inbound plugin (S2): `GET /muse-code/models`, the model list
  Muse Code asks for before a session. Each model `/v1/models` lists becomes
  a row with `metadata["muse-code"]`. Until the kernel has a model catalog,
  every model is told a 128000-token window, 32000-token replies, text in
  and out, and tool calls.
- The `ollama` inbound plugin (S1): `GET /api/version`, `GET /api/tags`,
  `POST /api/show`, `POST /api/chat`, and `POST /api/generate`, for clients
  that take an Ollama server. Chats run on the gateway's Chat Completions
  route in process and come back as Ollama NDJSON, or as one object when
  `stream` is false. `BodyStream::unfold` and
  `hofund_plugin_sdk::time::format_rfc3339` support it.
- `SurfaceHost::gateway()` (S0): an inbound surface runs the gateway's codec
  routes and model lists in process, for example a translated chat on
  `POST /v1/chat/completions`. The call is authenticated with the gateway
  token, whatever credential the surface sets, and surface routes are not
  reachable from it. `SurfaceHost::from_parts` takes the backend;
  `hofund_plugin_host::surface_host_with_gateway` builds one.
- `hofund agent set <agent> model=<id|default>` and
  `POST /v1/hofund/agents/{id}/model` (L8e2). Setting a route model connects
  the agent when needed; `model=default` drops the model hofund chose. The id
  must be a route model the agent can see, or the call fails with 422.
  `disconnect` still restores the agent's original model. `effort=` is not
  supported yet.
- `hofundd subscription list`, `sign-in`, `sign-out`, `refresh`, and `quota`
  (L8e). `hofund subscription` now runs them in `hofundd`, so the CLI no
  longer links the daemon crate or the plugin registry.

- `hofund provider add`, `set`, `key`, `rm` (aliases `remove`, `delete`),
  `refresh-every`, and `auto-include` (L8c). They edit the config with
  `toml_edit`, keeping comments, and apply it with its disk revision through
  the running daemon or one-shot `hofundd`, retrying a stale revision up to
  three times. `provider set <id> id=...` is refused before any edit; renaming
  waits for feature-parity row 11.6. `add` refuses an existing id and does not
  fetch models; `rm` does not reseat agents.
- `hofund events tail [--requests]` (L8d): prints daemon events as JSON
  lines. With no daemon running it starts `hofundd` in the background,
  logging to `.hofund/daemon-<config-id>.log` beside the config; that daemon
  keeps running after the tail exits until `hofund shutdown`. The tail
  reconnects after a token rotation without repeating the snapshot and exits
  0 when the daemon shuts down.
- `hofund provider list`, `show`, `test`, `refresh` (alias `fetch`), and
  `models`, and `hofund model list` and `show` (L8b). They use the running
  daemon or, with none, one-shot `hofundd`. Pick edits retry a stale
  revision up to three times; one-shot pick edits and refetches take the
  per-provider model state lock, and a pick edit that cannot be saved exits
  nonzero without printing a revision.
- `hofundd` (`apps/hofundd`, package `hofund-daemon-bin`): the daemon
  binary. It links the plugin registry, serves with local control on, and
  logs through `tracing` with the `log` bridge; `HOFUND_LOG` sets the level
  (default `info`). Releases ship it as its own archives and installers next
  to the CLI's; install both into one directory (#277).
- Remote gateway provider plugin (`plugins/providers/remote-gateway`, E13):
  preset `remote-gateway` now works. It forwards Chat, Responses, and
  Messages requests to another gateway in the protocol the client spoke,
  with the gateway key as Bearer (Messages also sends `x-api-key`). The
  address may be typed with or without a scheme or a known path suffix.
  `list_models` reads the other gateway's `/v1/models`; the probe asks
  Chat. The row moved out of the kernel catalog, which is now empty.
- Codex subscription (`plugins/subscriptions/codex`): requests are shaped as
  Codex CLI sends them (stream only, `store: false`, no sampling or output
  caps, ids and item references dropped, call ids over 64 characters hashed,
  orphan tool outputs told as notes, `system` items sent as `developer`, a
  cached `web_search` tool declared for replayed searches, encrypted reasoning
  included) with `session-id` and `thread-id` headers naming the
  conversation. Each model's own instructions come from `/codex/models`,
  cached in `codex-prompts.json` and fetched at most every 10 minutes per
  account; the client's instructions ride first as a developer message.
  `list_models` reads `/codex/models`.
- Codex subscription: usage limits. Each reply's `x-codex-primary-*`,
  `x-codex-secondary-*`, and `x-codex-credits-*` headers, and a
  `codex.rate_limits` event in its first 64 KiB, become quota observations
  in basis points, so a spent window rests the account until it resets
  unless the account runs on credits. `quota` reads `/wham/usage`: the
  primary and secondary windows, named by length (`5 hours`, `7 days`),
  and non-blocking while the account has credits or unlimited use. A
  request for one reply is sent as a stream and folded into one response
  (#297).
- SDK: `wire::Sent::headers`, `wire::Sent::watch` (`wire::Watch`), and
  `wire::Sent::fold`; `HeaderMap` is re-exported.
- SDK: `wire::Target::rewrite` (`wire::Rewrite`), a hook that edits a JSON
  request body after encoding and adds headers to the call.
- Gemini subscription: requests go to Code Assist `v1internal` as Gemini CLI
  sends them, wrapped as `{model, project, user_prompt_id, request}` with
  Gemini CLI's user agent, and each reply, unary or streamed, is lifted out of
  its `response` envelope. The project is the one Code Assist set up, else the
  configured one, else set up on first use. A 401 refreshes the token once.
  `quota` and `list_models` read `retrieveUserQuota`: one window per model
  (scoped to it, used out of 10000 from `remainingFraction`, with its reset
  time), skipping `_vertex` and repeated models. Token counting is not
  offered on Code Assist.
- SDK: `wire::Target::method_root` posts Generative Language calls to
  `{base}/{root}:{method}` with the model in the body; `wire::Target::unwrap`
  (`wire::Unwrap`) rewrites each JSON value of a 2xx reply, the whole unary
  body or each `data:` line of a stream; a Google `RetryInfo.retryDelay` in an
  error body sets the error's retry-after when no `Retry-After` header did.
  `time::parse_rfc3339` reads RFC 3339 times (moved from the Copilot plugin).
- Gemini CLI (Code Assist) subscription plugin `gemini`
  (`hofund-plugin-subscription-gemini`, registry feature `gemini`, on by
  default): browser sign-in with PKCE on an ephemeral IPv4 loopback port at
  `/oauth2callback`, using Gemini CLI's installed-app client and asking for
  offline access; the account is keyed by the Google email from userinfo.
  Only the refresh token and the Code Assist project are stored: Google does
  not rotate refresh tokens, so the access token lives in memory, refreshed
  2 minutes before expiry with one request per account at a time. Sign-in
  sets the account up as Gemini CLI does (`loadCodeAssist`, then
  `onboardUser` for the default tier, asked up to 5 times); the optional
  `project` option supplies a Google Cloud project for tiers that need one,
  and a failed setup is retried on refresh. A refused refresh marks the
  account as needing a new sign-in. Requests, quota, and the model list land
  with H4.
- SDK: `json` (re-exported `serde_json`), `sleep`,
  `oauth::client_code_exchange_form`, and `oauth::refresh_secret_form`, so a
  plugin with a client secret needs no other crate.

- Amazon Bedrock Mantle plugin `bedrock-mantle`
  (`plugins/providers/bedrock-mantle`): preset `bedrock-mantle` moved out of
  the built-in catalog, unchanged (17 regions), and now builds this plugin.
  Chat and Responses pick `/v1` or `/openai/v1` per model id and retry once at
  the other prefix on a route refusal, remembering the answer for up to 256
  models. Messages sends `x-api-key` alone and fits beta headers and body
  `anthropic_beta`. Models are listed live; the probe retries the other prefix
  like a call. The built-in catalog now holds only `remote-gateway`.
- SDK: `hofund_plugin_sdk::bedrock` (`fit_headers`, `fit_request`), the Bedrock
  Messages beta fitting both Bedrock plugins share, moved from the `bedrock`
  plugin; `wire::send` returns a `wire::Sent` once the status is a 2xx, so a
  caller can judge the status before reading the body.
- Amazon Bedrock plugin `bedrock` (`plugins/providers/bedrock`): preset
  `bedrock` and every `bedrock-converse` provider now build this plugin
  instead of the kernel adapter. Converse is signed with SigV4 through the SDK
  (`hofund_plugin_sdk::sigv4::AwsSigner`, `wire::Target::signer`,
  `wire::converse_call`); Chat, Responses, and Messages go through the SDK
  `wire` module with the stored API key. `list_models` returns the static
  catalog with no network call. A stored API key on Converse is now refused
  when a request or probe is sent, not when the provider is built.

- Google Vertex AI plugin `google-vertex` (`plugins/providers/google-vertex`):
  preset `google-vertex` now builds this plugin instead of the kernel adapter.
  The base URL comes from `project` and `location`, so the preset needs no
  `base_url` (new preset field `options_base`). OAuth tokens are minted from an
  ADC file (`authorized_user`, `service_account` as an RS256 JWT,
  `impersonated_service_account`) or an impersonation email and cached until
  shortly before expiry. Generate, stream, and count go through the SDK
  Generative Language transport; `list_models` returns the static ids the
  location serves; `probe` sends one unary request.
- SDK helpers: `hofund_plugin_sdk::jwt` (RS256 compact JWT signing),
  `hofund_plugin_sdk::probe` (probe bodies and outcome classification, moved
  from the kernel), `wire::Target::model_root` (the Generative Language model
  path root, `publishers/google/models` on Vertex), and a public
  `transport::Call::open_raw`.

- SDK Generative Language transport: `hofund_plugin_sdk::wire::execute`
  posts a Generative Language target to `{base}/models/{model}:generateContent`,
  or `:streamGenerateContent?alt=sse` for a stream, with a leading `models/`
  dropped and the model escaped as one path segment; an empty model is status
  400 and sends nothing. `wire::count_input` posts to
  `{base}/models/{model}:countTokens` and reads `totalTokens`. The builtin
  `generative` factory now sends `execute` and `count_input` through the SDK
  on the host HTTP client, at the base the adapter normalized. Requests,
  credentials (`x-goog-api-key` by default), and outcomes match the adapter,
  checked by a differential test.
- Azure OpenAI plugin `azure` (`plugins/providers/azure`): preset `azure` now
  builds this plugin instead of the kernel adapter. Chat and Responses send
  `api-key` and list the resource's deployments, then fall back to base models
  (`/models`); when both fail the error says to type deployment names in as
  model ids. Messages sends `x-api-key` and lists live models. A user header
  named like the key header replaces the stored key.
- A custom Chat or Responses provider whose base is on an Azure resource host
  (`*.openai.azure.com`, `*.cognitiveservices.azure.com`,
  `*.services.ai.azure.com`) now builds the `azure` plugin too, so it lists
  deployments, falls back to base models, and sends `api-key` the same way as
  preset `azure`. Messages on an Azure host stays on the Anthropic adapter
  with `x-api-key`.
- `hofund_kit_upstream::check_deployments`, and a `Deployments` case in
  `check_list_and_probe`: deployment pages, the base-model fallback, both
  lists empty, and the minimal probe.

- Copilot model list and per-model routing: `list_models` reads the account's
  `/models` list (chat models only; internal ids, vendor-experimental models
  and models without a picker or category are dropped; a model needing terms
  is listed). Each request goes to the protocol the model supports: the
  inbound protocol when listed, otherwise the first of the model's endpoints;
  a model with no endpoints, or an unlisted model, uses Chat Completions.
  Responses and Messages go to `/responses` and `/v1/messages`. A model that
  needs terms has its policy enabled once before the first request; only a
  2xx counts as accepted.
- SDK Messages transport: `hofund_plugin_sdk::wire::execute` posts a Messages
  target to `{base}/v1/messages` with the `anthropic-version` header (a
  configured header of that name replaces it), and `wire::count_input` posts
  to `{base}/v1/messages/count_tokens`, treating 404, 405, 501, or a 400 that
  says the endpoint is not supported as unsupported. The builtin `anthropic`
  factory now sends `execute` and `count_input` through the SDK on the host
  HTTP client for every endpoint that is not Bedrock or Mantle. Requests,
  credentials (`x-api-key`, or both headers for a third-party host), and
  outcomes match the adapter, checked by a differential test.
- GitHub Copilot subscription plugin `copilot`
  (`hofund-plugin-subscription-copilot`, registry feature `copilot`, on by
  default): GitHub device sign-in with the Copilot editors' OAuth app on
  github.com or an enterprise `<name>.ghe.com` host (the `host` config key or
  sign-in option). The device page shown is derived from the host, never from
  the reply. Sign-in checks the user has a Copilot seat and stores the GitHub
  token, host and login; the account is keyed by host and user id. Short-lived
  Copilot session tokens are traded single-flight and again within two minutes
  of expiry; the session's `endpoints.api` is used only when it is an https
  `githubcopilot.com` origin (or `copilot-api.<name>.ghe.com`). A refused trade
  (401/403) marks the account as needing a new sign-in unless a newer sign-in
  replaced the token. Requests go to Chat Completions with the editor headers,
  `X-Initiator`, and `Copilot-Vision-Request` for images; a 401 trades a new
  session and retries once. Quota reads chat, completions and premium request
  snapshots from `copilot_internal/user`.
- Claude Code subscription plugin `claude`
  (`hofund-plugin-subscription-claude`, registry feature `claude`, on by
  default): sign-in runs Claude Code's own `claude auth login --claudeai`
  with no shell, an environment cleared to the host allowlist plus
  `CLAUDE_CONFIG_DIR`, and a private per-session config directory. The
  printed sign-in link is shown to the user, and the pasted `code#state` is
  checked and written to the CLI's standard input; waiting for the CLI's
  own browser callback also works. The `.credentials.json` and the
  `oauthAccount` profile are stored in the account store, keyed by the
  account UUID (or the email when Claude Code gives none) and labelled with
  the email. Refresh lays the credentials out in a private account
  directory, runs `claude auth status --json`, stores rotated credentials
  once, removes the file again, and marks the account as needing a new
  sign-in when Claude Code says it is signed out. The optional `command`
  option names the binary by absolute path; otherwise it is found on
  `PATH`. Requests through Claude Code, quota, and models land with H8;
  the macOS Keychain is not read.
- SDK: `json` (re-exported `serde_json`), `sleep`, `timeout`,
  `process::find_program` (a `PATH`/`PATHEXT` lookup that never runs
  anything), `oauth::secret_text`, `oauth::client_code_exchange_form`, and
  `oauth::refresh_secret_form`, so a subscription plugin with a client
  secret needs no other crate.
- Host test backend: scripted piped children (`PipedScript`,
  `PipedSpawn`) on `RecordingProcess`, re-exported by the subscription kit.

- ChatGPT (Codex) subscription plugin `codex`
  (`hofund-plugin-subscription-codex`, registry feature `codex`, on by
  default): browser sign-in with PKCE on the loopback port 1455 Codex CLI is
  registered with, requiring ID, access, and refresh tokens; the account id
  comes from the ID token's `sub` and ChatGPT account, so signing in again
  replaces the account; the label is the email plus the workspace kind for a
  shared plan. The access token is refreshed 5 minutes before its `exp`
  under the account lease, rechecked after the lease is taken, and run as a
  detached job so a cancelled request still stores the rotated token; a
  refused refresh marks the account as needing a new sign-in. The upstream
  posts streaming Responses calls to the ChatGPT Codex backend with Codex
  CLI's headers, drops the sampling and storage keys the backend refuses,
  and retries a `token_expired` 401 once after a forced refresh. Quota and
  the model list land with H2.
- SDK: `oauth::code_exchange_form`, `oauth::SecretObject` (a JSON account
  blob whose `Debug` shows field names only), `oauth::same_secret` and
  `oauth::constant_time_eq`, `oauth::detached` (a one-shot job its caller
  cannot cancel), and `transport::Call::fetch` (one send, any status).
  `RecordingHttp::enqueue_response` scripts a status and headers, and the
  subscription kit gains `block_on`, `join_all`, `poll_once`, and
  `yield_times`.

- `hofund subscription` commands: `list [--json]`, `sign-in <id> [--host H]
  [--label L]`, `sign-out <id> <account>`, `refresh <id> [<account>]`, and
  `quota [<id>] [--json]`. Sign-in never opens a browser: it prints the
  address or device code, then waits for the loopback callback, a pasted
  address or token, Ctrl-C (exit 130), or a 10-minute deadline. Every exit
  path cancels the plugin's sign-in and closes the loopback listener. The
  JSON listing carries ids, labels, the flow, and account status only.
- SDK OAuth helpers for subscription plugins: `oauth::device::{start, poll}`
  runs an RFC 8628 device sign-in (`authorization_pending` keeps polling,
  `slow_down` adds 5 s, a 1 s interval floor, five failed polls in a row or
  `expires_in` end it); `TokenSet` and `parse_token_set` read access,
  refresh, and ID tokens with an absolute expiry; `unverified_claims` reads
  JWT claims without checking the signature; `TokenCache` refreshes once per
  account for concurrent callers, inside a margin, remembers a failure for
  5 s, and drops a value only when the caller's stale check matches;
  `token_error` maps `invalid_grant`, `invalid_client`, 400, and 401 to
  `Auth` without copying `error_description`; and `account_id_from` derives
  `a-` plus 16 hex digits from stable provider facts.

- Subscription accounts as route members: a route hop
  `{ subscription = "id" }` expands to one member per signed-in account,
  ordered by label then id, with `account:{subscription}:{account}` member
  ids. The daemon rereads accounts every 5 s and on each publication,
  polls quota every 5 minutes per member with jitter, and cools a member
  whose blocking quota window is spent. One id may not be both a provider and
  a subscription in one route, and an unlinked subscription id now fails at
  startup and in `config check`.

- Control-plane request topic and remaining live emissions: the gateway reports
  `request_started` / `request_finished` (ids, route, member, status class,
  latency, usage tokens only) and shares an `in_flight` counter with status and
  the event snapshot; config publication emits `snapshot_published`, model
  refresh emits `models_refreshed`, cooldown maintenance emits `member_cooled`
  and `member_recovered`, and agent connect/disconnect emit `agent_changed`.

- Live configuration apply and reload on the control plane: `GET /config`,
  `POST /config/validate`, `/config/apply`, and `/config/reload` run under a
  sidecar lock with revision checks, atomic byte-exact writes that keep
  symlinks, mode, and owner, and publish a new router snapshot without a
  restart. Failures emit `config_rejected` and change nothing; publications
  emit `config_reloaded` and report restart-only and not-yet-applied fields.
  A reload reuses provider instances whose identity is unchanged, so learned
  routes survive; the model refresh loop follows the published configuration;
  and agent plans use the bound model listener address and live route models.
  Hand edits to the configuration file are followed too: the daemon watches
  the config and symlink-target directories, debounces for 400 ms, ignores
  sibling files, falls back to a 2 s poll, skips its own writes, and keeps the
  last good configuration when an edit is invalid.
- Live model relisten: a published `listen` change moves the model listener
  without a restart. A port change binds the new address before closing the
  old one; a same-port host change closes first and rebinds the old address if
  the new bind fails. A bind failure keeps the rest of the configuration live,
  reports `listen` in `not_applied`, and emits `listener_relisten_failed`; an
  explicit reload retries. If the old address cannot be restored, the daemon
  exits non-zero. `HOFUND_ADDR` overrides `listen` for the daemon and local
  one-shot commands, never relistens, and is reported as `env_override`;
  status shows the configured, effective, and running addresses and the
  source.
- Control event stream (`GET /v1/hofund/events`): per-topic buses and replay
  rings, composite `Last-Event-ID` resume, targeted resyncs, token-checked
  heartbeats, and stream close on token rotation or shutdown. The typed client
  gains `events` and the config methods; the config routes return 503 until
  they are implemented.
- SDK Chat and Responses transport (`hofund_plugin_sdk::wire`) on the host HTTP
  handle. The builtin OpenAI-compatible factory uses it for every endpoint that
  is not a Bedrock Mantle root, so a streamed reply reaches the client as the
  upstream sends it. A reset after the request was written is not replayed and
  does not fail over.
- Piped child processes for plugins: `ProcessHandle::spawn_piped` returns a
  child with line-framed stdout (1 MiB line cap), stdin writes and close, an
  environment cleared to a path, home, locale, and proxy allowlist, and kill
  and reap on drop.
- Loopback sign-in callbacks: plugins bind a kernel-owned listener on
  `127.0.0.1` through `HostContext::loopback()`, on a fixed or ephemeral port.
  The first request with the expected `state` and a `code` completes the
  sign-in, `error=` fails it, and `/cancel` cancels it. A busy fixed port
  carries on paste-only, and a pasted URL must match the redirect's host, port,
  path and `state`. The SDK gains `oauth::Pkce`, `oauth::new_state` and
  `oauth::authorize_url`.
- Account refresh leases: a per-account cross-process lock that reads the stored
  secret past the cache and commits a refreshed one, with bounded waits for
  refreshes and upserts. Plugins also gain one-shot background jobs, blocking
  and model-scoped quota windows, sign-in sessions and options, deferred
  sign-in proofs, and sign-in cancel and account status hooks.
- Control picks, model refresh, and provider test routes. Picks edits check a
  revision equal to the state file hash under the state lock, roll back on a
  failed save, and follow the reference edit forms. Refresh has a 30 s timeout
  and reports added, removed, and delisted ids. Provider tests report per-model
  latency and status with 30 s or 90 s timeouts.
- Control agent routes: list, status, redacted plans with a content revision, connect
  and disconnect under a shared gate with stale-plan 409s, and token rotation that
  refreshes connected agents, bumps a token generation once, and scrubs tokens from
  errors and logs.
- Encrypted account storage with native OS credential entries, an authenticated
  encrypted-file fallback, secret-free backend selection, and crash-safe migration
  from the legacy plaintext metadata file. Reads route by stored reference and
  load secrets per row, while kind-scoped v2 entries prevent cross-kind collisions.
  Fallback keys publish without overwrite, and Linux links libdbus statically.
- Control usage summaries for today, week, month, or all time from local midnight,
  with SQL totals over the whole period and the newest 10,000 rows
  (`rows_truncated` marks a cap). `POST /shutdown` now drains in-flight model
  requests, closes idle control connections, removes the socket, and releases
  the daemon lock; later mutations return 503.
- Full typed control route and client surface, with ordered request defenses,
  bounded bodies, error envelopes, no-store responses, and drain guards. The daemon
  now serves secret-free provider summaries, cached models without upstream calls,
  and route models over control TCP and Unix sockets.
- Per-config control identities, single-instance locks, atomic token-free discovery,
  secure runtime-directory fallback, and same-user Unix socket transport with ordered
  client discovery. Windows keeps authenticated loopback TCP until named-pipe support lands.
- Opt-in loopback TCP control health and status with strict Bearer, Host, Origin,
  and mutation defenses, live disk/runtime revisions, and a typed TCP control client.
  Model `/health` now identifies the service as hofund.
- `DaemonRegistry` injection and shared daemon runtime state. Cooldown books now
  survive candidate publication, prune removed members and changed provider
  fingerprints, expire idle history, bound eviction, and record one recovery per rest.
- CI tests every workspace package with only its default features and bounds
  every Cargo cache step so a stalled cache restore cannot hang a job.
- Plugin API `BuildContext` and protocol-aware upstream build methods let listed
  provider plugins receive the selected protocol. The daemon now accepts any
  protocol a listed plugin declares instead of restricting it to the first.
- `hofund-control-proto` 0.1.0 with versioned DTOs, events, composite SSE ids,
  bounded decoding, and committed JSON and public-API snapshots.
- A workspace invariant that permits an unsafe-code opt-out only in the future
  isolated Windows platform crate.
- Stable daemon mappings from internal provider failures and cooldown causes
  to control wire values.
- Provider preset fragments now carry the full catalog schema, regions, and
  aliases, with builtin data-only directories, catalog-equivalent wire and
  credential selection, and build-time validation for row-E data migrations.
- Docs: control plane (row L) plan.
- Public `PluginErrorKind::Interrupted` and `ProviderErrorKind::Interrupted`
  variants classify a 2xx reply whose body failed while being read.
- Per-member cooldowns with generic, rate-limit, quota, credit, observation,
  and panic policies. Routing tries ready members first and retains only the
  soonest cooling member as a probe; shared state survives router republishes.
  Counts never clear a cooldown and cool only on 429 or transport-class failures.
- A passive observation backend bound to one member, so plugins can report
  quota and retry hints without selecting which member receives them. Only an
  observation whose window is exhausted can rest the member.
- `Retry-After` delta-seconds and HTTP-date parsing on provider and SDK status
  errors. Non-positive hints are ignored. Transport parsing does not cap hints;
  the cooldown policy caps header hints at one hour for every failure cause.
- Timed panic cooldowns that temporarily omit a failed member and republish it
  when its rest expires, with newer panics extending the exclusion.
- Compiled agent descriptions now support dotenv files, ordered file-location
  candidates, environment path overrides, conditional model writes, and model
  catalogs. `hofund-agent` can compose const built-ins with static compiled
  descriptions through the same editing engine. The registry compiles
  validated data-only `agent.toml` descriptions and delivers them through the
  daemon to the CLI and desktop.
- Agent descriptors can declare `model_prefix` so route validation uses the
  agent's model spelling without agent-specific CLI rules (#183).
- `hofund-kit-agent` now runs per-OS golden conformance for planning, apply and
  backup safety, token refresh, and disconnect drift. Shared `all/` cases run
  on every OS, OS-specific cases run only on their own OS, and regeneration
  deduplicates identical three-OS trees. Status and configured models are
  checked throughout the connection lifecycle (#108, #111). Registry fixtures
  replay the same committed goldens through compiled and const descriptions.
- Agent registration rejects ids reserved by the built-in catalog, mixed file
  kinds under one file id, empty model-source keys, and empty or duplicate
  visible-model entries. The reserved-id list is checked against the built-in
  catalog by a daemon test (#111).
- The plugin API's preflight hook lets `config check` validate external
  resources named by a resolved provider configuration without starting them.
- The SDK's `IamAccessToken` retains an IAM access token and its optional
  absolute expiry; `parse_iam_access_token` returns this type.
- `ListKind::Deployments`, explicit `ListKind::rank()` capability ordering,
  and answering-URL metadata on `ModelList`, with the additive plugin API
  surface recorded in the public-API snapshot.
- SDK `after_id` and Gemini model-list walkers with page limits, bounded
  response bodies, and list deadlines. Gemini returns partial rows for a
  repeated token or a token on the twentieth page.
- Per-wire live model lists, Azure deployments, static Vertex and Bedrock
  catalogs, and minimal connectivity probes in the built-in provider plugins.
  List and probe bodies are bounded, credentials are redacted, and Anthropic
  single-header authentication can be learned and reported to the host.
- Daemon model aggregation across preset wires with answering-URL deduplication,
  exact custom list URLs, learned bases and authentication, two-pass probe
  selection, and four-wide batch probes.
- Host model-list refresh with persistent state under `.hofund/host/models`,
  including due and retry times, picks, delist/restore, auto-include, dead-side
  retention, and 429-aware retry delays.
- `hofund-kit-upstream` conformance for live paging and minimal probes, static
  catalogs without network access, and unsupported model lists.
- Provider plugins share SigV4, OAuth token parsing, form bodies, and JSON
  page cursors from `hofund-plugin-sdk`. Parse errors do not include the
  response body. Token `Debug` is redacted.

### Changed

- Daemon integration tests now bound every HTTP request (60 s, no idle
  pooled connections) and every daemon shutdown (60 s), so a stalled
  request fails with a message instead of hanging the Windows runner
  (#409, #462, #467). The feature parity snapshot header names the current
  reference baseline.

- Disconnect now warns when it removes hofund's entries from a shared array
  or map that were edited after connect (G8). Such entries are still removed
  whole, token included.

- The CLI crate (`hofund-cli`, binary `hofund`) moved from `crates/cli` to
  `apps/cli` (RFC 0001 row J). Install with
  `cargo install --path apps/cli --locked`. Nothing else changed.
- The kernel crates moved under `crates/kernel/` (RFC 0001 row J): `agent`,
  `codec`, `config`, `daemon`, `gateway`, `ir`, `plugin-api`, `plugin-host`,
  `plugin-sdk`, `provider`, `quirks`, `router`, `secrets` and `usage`.
  `control-proto` and `control-client`, shared by the kernel and the shells,
  stay in `crates/`. Package names are unchanged.
- The vendor-name scan covers every kernel crate under `crates/` except a
  short, checked list of crates that still hold vendor code (I). A crate on
  that list fails the scan's own test once it stops naming a vendor, so the
  list only shrinks. Kernel crates may not ship a `presets.toml`.
- `hofund serve` runs `hofundd`: the one beside `hofund` first, then the
  first on `PATH`, with `--config` set to the resolved config path. On Unix
  the CLI process becomes `hofundd`; on Windows the CLI waits and exits with
  its code. `hofund serve --in-process` keeps the old in-process server for
  one release.
- A Chat, Responses, or Messages provider whose base is on a Bedrock host no
  longer uses the kernel adapter: a `bedrock-mantle.<region>.api.aws` base
  builds the `bedrock-mantle` plugin and a
  `bedrock-runtime.<region>.amazonaws.com` base builds the `bedrock` plugin.
  Such a runtime base without the `bedrock` preset now lists the static
  catalog instead of asking `/models`. The kernel adapters lose their Mantle
  and Bedrock paths: `OpenAiCompat::direct_base` and `Anthropic::direct_base`
  return `&str`, and a kernel Chat probe always sends `max_tokens`.

- The local presets `ollama`, `lmstudio`, `omlx` and `mlx-serve` moved from
  the built-in catalog into the `openai-compat` preset fragment, unchanged:
  same localhost endpoints, Decide URLs and no key. A Messages call on
  localhost still sends both `x-api-key` and Bearer. The built-in catalog now
  holds `google-vertex`, `bedrock`, `bedrock-mantle` and `remote-gateway`.

- A listed provider plugin (one compiled from a preset fragment) now receives
  the provider's `headers`; a plugin whose config takes no `headers` rejects
  them as an unknown key.

- Breaking (plugin API, N9): `SignInFlow::Device` now carries a `DeviceFlow`
  built from validated options instead of a static URL, so a device-code
  subscription can serve an enterprise host. `DeviceFlow::new` takes a bare
  DNS name or `https://` plus one and rejects other schemes, userinfo, ports,
  paths, queries and fragments; hosts are lowercased. `SubscriptionSpec::flow`
  returns the new `SignInFlowKind`, `SignInFlow` is no longer `Copy`, and
  building a subscription whose instance flow kind differs from its spec now
  fails. `StaticUrl` is removed.

- The pooled plugin HTTP client streams a success body as the upstream sends
  it instead of buffering the whole response (row H0g). A read failure after
  the headers is one `Interrupted` chunk that ends the stream; error bodies
  are still buffered. `Call::send` in the SDK still reads the full body, so its
  retry rules do not change.
- Plugin ids are now unique per kind rather than globally, allowing an agent and
  subscription to share an id without changing existing config or CLI commands.
  Registry contributions and host secrets, accounts, cache directories, and state
  directories are keyed by kind and id; legacy account rows remain subscriptions.
- Passive quota observations now distinguish one-hour retry hints from
  eight-day window resets, rest at 98% used only when the window reset is
  known, and exempt accounts spending credits. `Cooldowns::observe` now takes
  `&QuotaObservation` instead of separate positional fields.
- `agent.toml` rejects `create` on single-location files, where the field had
  no observable effect; multi-location candidate selection is unchanged (#177).
- Removed the frozen agent const engine, migration-only oracle feature, static
  legacy catalog lookups, reserved agent id list, and legacy-only test seams.
  Shipped behavior remains covered by unchanged goldens and shipped-plugin
  regression tests (#147, #180, #130, #167).
- The first ten relay preset rows now live in
  `plugins/providers/openai-compat/presets.toml`. Registry selection, credential
  derivation, and model planning preserve their catalog behavior.
- The second batch of ten vendor preset rows now lives in
  `plugins/providers/openai-compat/presets.toml`, with catalog-equivalent
  registry selection, credential derivation, and model planning.
- The third batch of nine vendor and relay preset rows, including their regions
  and credential metadata, now lives in `plugins/providers/openai-compat/presets.toml`
  with catalog-equivalent registry behavior.
- The fourth batch of seven vendor and relay preset rows, including their model
  metadata and decision endpoints, now lives in
  `plugins/providers/openai-compat/presets.toml` with catalog-equivalent registry
  selection and model planning.
- The fifth batch of four vendor preset rows and their four aliases, including
  regional endpoints and model metadata, now lives in
  `plugins/providers/openai-compat/presets.toml` with catalog-equivalent registry
  selection, credential derivation, and model planning.
- The sixth batch of five decide-only vendor and relay preset rows, including
  regional decision endpoints, now lives in
  `plugins/providers/openai-compat/presets.toml` with catalog-equivalent registry
  selection errors, credential derivation, and model planning.
- The seventh batch moves OpenAI and OpenRouter into
  `plugins/providers/openai-compat/presets.toml`, Google into
  `plugins/providers/generative/presets.toml`, and Anthropic into
  `plugins/providers/anthropic/presets.toml`, preserving registry selection,
  credential derivation, and model planning.
- `ProviderModels` is now non-exhaustive so future aggregation fields do not
  break downstream construction at compile time.
- Plugin-host responses now strip every RFC 9110 hop-by-hop header, including
  headers named by `Connection`, while preserving `Set-Cookie` for plugins.
  Truncated non-2xx responses retain the partial body bytes read so far.
- Regression coverage now pins RFC 9110 stripping of `Retry-After` nominated by
  `Connection`, exhaustive daemon mappings for every `CooldownCause`, and
  recursive daemon registry-call scanning; `control_map` is crate-private.
- Body breaks after a 2xx reply are now classified as `Interrupted` instead of
  `Connect` on the provider path and `Transport` on the plugin-host path.
  Plugin-host body breaks now fail over across members before a completion or
  first stream event is returned; previously they did not fail over.
- Codex CLI is now a compiled data-only agent at
  `plugins/agents/codex/agent.toml`; its production Rust const is removed.
  Frozen const oracles remain behind the non-default `legacy-agent-consts`
  feature for equivalence tests. Catalog order is unchanged, and
  `RESERVED_AGENT_IDS` now lists only const-backed agent ids.
- Claude Code is now a compiled data-only agent at
  `plugins/agents/claude-code/agent.toml`; its production Rust const is removed
  and its frozen oracle uses the renamed test-only `legacy-agent-consts`
  feature. Catalog order remains `claude-code`, `codex`, `gemini-cli`,
  `opencode`.
- Gemini CLI is now a compiled data-only agent at
  `plugins/agents/gemini-cli/agent.toml`; its production Rust const is removed
  and its frozen oracle remains behind `legacy-agent-consts`. Catalog order is
  unchanged. An absolute `GEMINI_CLI_HOME` still places both files under
  `$GEMINI_CLI_HOME/.gemini`, while empty and relative values still use
  `$HOME/.gemini`.
- OpenCode is now a compiled data-only agent at
  `plugins/agents/opencode/agent.toml`; its production Rust const is removed
  and its frozen oracle remains behind `legacy-agent-consts`. Catalog order is
  unchanged.
- No const-backed built-in agent remains. `RESERVED_AGENT_IDS` is empty, and
  static `adapters()` is empty and `find()` always returns `None`; use
  `Catalog::find`. The const engine types are compiled only for tests and
  `legacy-agent-consts`.
- Coding-agent path overrides now treat empty and relative values as unset for
  process environments, explicit environment pairs, and named setters. Const
  and compiled adapters therefore use the same default paths.
- Agent golden layouts reject unknown top-level entries, missing OS coverage,
  incomplete per-OS case sets, and case directories without `case.toml`.
  OS-specific cases can declare their exact targets with `only` (a non-empty,
  duplicate-free subset that is not every OS; shared cases use `all/`), and golden
  regeneration is enabled only by `UPDATE_GOLDEN=1`.
- Agent model-match needles are required to be lowercase; tier matching itself
  remains ASCII case-insensitive. Compiled descriptions can explicitly identify
  model and effort files, unsupported file kinds are errors, and `Session`
  debug output lists environment names without exposing their values.
- Model lists require HTTP 200, display names fall back to ids, verification
  hints require verification evidence, and Azure double failures retain the
  last HTTP status. A per-URL Azure timeout still tries the second URL; a
  cancelled call does not.
- `fixV1` host state is keyed per provider and wire, so only the configured
  wire's learned base affects its requests (#84). Model lists and probes reuse
  the daemon's configured instance when its base is current (#86).
- `models.auto_include` glob and regular-expression rules are validated when
  configuration loads.
- Built-in factory specs advertise `Live` lists for OpenAI-compatible,
  Anthropic, and Generative wires, and `Catalog` lists for Vertex and Bedrock.
- `[providers.options]` is forwarded to the plugin factory. Each factory has
  an option schema. An unknown key or a wrong type is a check error that
  names the provider and the key and does not include the value.
- Relay with no `base_url` now fails at plan time with
  `preset \`relay\` needs base_url`.
- Vertex and Bedrock are built through `registry.factory`, like the other
  wires. The daemon no longer constructs those adapters itself.
- Built-in adapters use the pooled host client, so `connect_timeout_ms` and
  `read_timeout_ms` apply to them, including Vertex token mint.
- `[providers.options]` and `[subscriptions.options]` are opaque TOML tables.
  A value may be a string, an integer, a boolean, an array, or a nested
  table. A value of the wrong type names the provider and the key.
- `config check` verifies that an explicitly configured Vertex credentials file
  exists. Daemon startup no longer fails merely because that file is missing.

### Removed

- The kernel preset catalog (I). `crates/provider/presets.toml` is gone, and
  `hofund-provider` no longer exports `resolve`, `Resolve`, `Resolved`,
  `PresetError`, `model_plan`, `catalog_ids`, or `catalog_commit`. Every
  preset row is a compiled plugin fragment; the registry still lists the same
  59 presets (the 58 reference presets plus the `relay` plugin's row) and four
  aliases. A preset id no fragment owns still fails with
  ``unknown preset `<id>` ``.
- `hofund serve --in-process` (L8e). `hofund serve` always runs `hofundd`.

### Security

- An inbound surface never sees the gateway token (S6b). Before a surface
  runs, the gateway removes each presented credential that matches it: the
  bearer, `x-api-key`, `x-goog-api-key`, and any `key` query pair. A
  credential that does not match is the caller's own and stays.
- Model-list redaction masks only whole secret tokens of at least eight characters,
  strips URL userinfo, and removes every URL query value through one shared helper.
  This applies to every URL in an error message, including pagination URLs and
  URLs quoted in transport errors.
- Token URLs containing userinfo are rejected.

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
  an empty table are kept. A blank string inside an array is kept. A blank
  `protocol` is absent, the same as a blank `kind`. `protocol` is trimmed
  before a short name is rewritten.

### Fixed

- Raw SSE model rewriting now covers a complete JSON payload in an
  unterminated final event, and the 1 MiB oversize decision is independent of
  upstream chunk boundaries (#33, #34).
- Codex retries a 401 identified only by `error.code = token_expired`. Gemini
  refresh jobs mark a refused sign-in lapsed even when their caller is
  cancelled, and token replies without `expires_in` use a one-hour lifetime
  with the existing refresh margin (#298, #331, #332).
- Anthropic host checks match `anthropic.com` and its subdomains only, so a
  lookalike host such as `evilanthropic.com` no longer gets official Anthropic
  credential placement or model-list handling.
- The test temp-path invariant also catches `env::temp_dir()`, imported or
  grouped `temp_dir` imports, and paths read from `TMPDIR`, `TEMP` or `TMP`.
- Test fixtures now retain temporary-directory guards until test completion,
  including panic paths, and a workspace invariant rejects hand-built process
  temp paths. Existing host model-state directories are tightened to `0700` on
  Unix, and unreadable or unmovable state files are preserved instead of being
  overwritten (#212, #101, #102).
- Quota refusals are now recognized from the response body with the reference's
  rule order: a 402 naming a used-up limit, a 429 naming its reset time or a
  planned allowance, a 5xx quota code, or another 4xx naming a used-up limit.
  Per-minute and short-wait rate limits, `insufficient_quota`, and credit
  refusals stay out. Quota errors keep their HTTP status for failover.
- `Retry-After` keeps fractional seconds, and `*ratelimit*reset*` headers in
  RFC 3339, Go duration, or Unix-time form are read when it is absent; the
  longest wins and the router still caps the wait.
- The rate-limit ladder now continues only when the previous rest was a rate
  rest, and a kept longer rest keeps its own cause and failure count.
- Vertex token mint failures are shared with concurrent waiters and negatively
  cached for five seconds. Mantle learned-route maps are bounded to 256 entries
  per provider instance with oldest-entry eviction. Invalid local plugin header
  values now report `Config` instead of `Auth` (#65, #137, #118).
- SigV4 form and STS tests now use the literal AWS suite inputs. Release and
  desktop-bundle apt installs now have bounded steps, retries, and HTTP timeouts.
- Control request bodies between 2 MiB and the protocol request limit now reach
  their handler instead of getting the HTTP framework's plain-text 413 without
  an error envelope or `Cache-Control: no-store`. The control pre-route check is
  the only body size gate.
- Over-long Unix control socket paths now warn and fall back to TCP instead of
  preventing daemon startup. Socket tests use short paths on macOS, and the
  Windows single-instance test no longer requires session debug output.
- Wrong-method control responses now use the `method_not_allowed` error kind
  while retaining HTTP 405 (#206).
- Decide-only presets with an explicit wire now report the same missing-endpoint
  message through catalog and registry selection (#170).
- Provider and SDK transports share one Retry-After parser. Model-list error
  joining preserves homogeneous Auth, Quota, Io, Config, Unsupported, and
  Internal kinds instead of collapsing them to Decode (#119).
- Default control error envelopes now round-trip with a valid request-id
  placeholder, validation messages use their limit constants, CI workflow
  parsing covers both cache-step forms and missing per-package test jobs, and
  accepted L1 fallback decisions are documented (#189, #194, #196, #197).
- Control response decoding now validates event boot ids and topic membership
  and rejects oversized or invalid error-envelope fields instead of accepting
  unbounded wire content (#184, #190).
- OpenCode's configured `hofund/<route>` model no longer produces an unrouted
  model warning; other provider prefixes and bare model names still warn.
- Non-2xx body-read failures now preserve the original `Status` and
  `Retry-After`, including live model pages, Vertex token responses, and the
  plugin host.
- The plugin host now passes upstream response headers, including
  `Retry-After`, through to plugins.
- Mantle route learning no longer replays a refusal on the remembered prefix or
  forgets that prefix, and an empty model is never rerouted. Each request now
  captures its initial prefix and retries at the explicit other prefix, avoiding
  concurrent false refusals. A second route-refusal message at any status or a
  transport-class failure before response headers forgets the learned route and
  returns the first error. Once the second attempt returns a 2xx, its route stays
  learned and any body failure is returned instead of the first error.
- Mantle detection honors the preset on relay overrides and detects
  `bedrock-mantle.<region>.api.aws` hosts regardless of another preset. Base
  normalization still preserves another preset's URL and uses `/v1/models` for
  address-added Mantle listing.
- Bedrock Messages sends `x-api-key` alone and comma-splits, renames, drops, and
  deduplicates beta values on configured `anthropic-beta` headers and array-valued
  body `anthropic_beta` lists. Body elements are stringified like the reference:
  null becomes an empty string, while objects and arrays use compact JSON. An
  unchanged list preserves non-string JSON values. Top-level metadata is removed
  on execute and count requests, including same-protocol passthrough.
- Bedrock Chat sends the token cap only as `max_completion_tokens`, without
  `max_tokens`, for Chat and Messages inbound requests.
- Windows agent environment names now compare ASCII-case-insensitively for
  `PATH`, path overrides, and compiled environment lookups.
- Empty agent environment overrides now mean unset everywhere, including the
  named `Session` setters and const adapters, so const and compiled plans use
  the same default paths (#109). Relative overrides are also treated as unset;
  see Changed.
- Agent conformance compares the exact trimmed origin and the current and
  refreshed tokens after disconnect, including a dedicated origin-only
  dotenv golden file, so surviving gateway values cannot pass unnoticed (#110).
- Agent golden conformance compares directories as well as file contents, so
  extra and missing empty directories no longer pass unnoticed (#148).
- Non-forced Gemini CLI disconnect restores an original string `model` after
  unrelated user edits without demoting an original object `model` on connect or
  disconnect. Invalid `model` and non-string `model.name` shapes produce
  actionable errors that name the key to repair (#176).
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

### Removed

- The kernel Azure adapter (`hofund_provider::list_deployments`,
  `AzureDeployment`, `azure_host`, and the Azure branches of the Chat list and
  probe). The `azure` plugin owns that path.

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
