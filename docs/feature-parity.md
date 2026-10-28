# Feature parity: magpie and hofund

This document compares every user-facing capability of
[yetone/magpie](https://github.com/yetone/magpie), the Go original, with
hofund, the Rust rewrite. Each row cites a source path in each repository.

- **magpie snapshot:** `641b121` (shallow clone, October 2026).
- **hofund snapshot:** `main` at `0d8cfa9` ("Accept omitted Messages caps and return Codex tool calls intact.", #14).
  Hofund line citations below were checked against that commit on 2026-10-25,
  except rows 1.1, 1.2, 1.4, 1.12, and 5.1. Those cite this change,
  which is not on `0d8cfa9` by itself.
  [#9](https://github.com/logibricks-io/open-heimdall-hofund/pull/9) is `ac6325a`,
  [#10](https://github.com/logibricks-io/open-heimdall-hofund/pull/10) is `695860d`,
  [#11](https://github.com/logibricks-io/open-heimdall-hofund/pull/11) is `77167f8`,
  [#12](https://github.com/logibricks-io/open-heimdall-hofund/pull/12) is `e650022`,
  [#13](https://github.com/logibricks-io/open-heimdall-hofund/pull/13) is `fcef574`,
  and [#14](https://github.com/logibricks-io/open-heimdall-hofund/pull/14) is `0d8cfa9`.
  Rows those pull requests finish are Done and cite the code on `main`.
- **Goal:** rebuild every core magpie capability in Rust and do each one better.
  Copy designs that are good, stable, and efficient. Redesign the parts that are unsound.
  `docs/magpie-comparison.md` records each adopt-or-redesign verdict.
  `docs/roadmap.md` is the execution order. A `Stage` column marks each row `core` (stage 1) or `auxiliary` (stage 2).

## Status key

| Status | Meaning |
| --- | --- |
| Done | hofund has the capability, with equal or wider scope for the user. |
| Partial | hofund has a subset, or it has a known defect that blocks real use. |
| Missing | hofund has nothing for it. |
| Not planned | hofund leaves it out on purpose. The reason is in the row. These rows are not counted. |

A row is **Done** only when the row itself cites three pieces of evidence: the code location, the test or tests that prove the behavior, and the CI run that executed those tests. Behavior that no real-agent or real-upstream acceptance run has exercised says `verified with mocks only` or `unverified`. A row without those citations stays Partial, including when the code and the tests exist. Item-by-item coverage inventories, derived from magpie's source, are [docs/coverage/agents.md](coverage/agents.md), [docs/coverage/providers.md](coverage/providers.md), and [docs/coverage/subscriptions.md](coverage/subscriptions.md). Work follows those inventories, in order.

| Stage | Meaning |
| --- | --- |
| core | Stage 1. Required before auxiliary work starts. |
| auxiliary | Stage 2. Do not start until stage 1 has merged. |

Unless a path says otherwise, magpie paths are relative to the magpie repository
and hofund paths to the hofund repository. `ref.md:N` means magpie's
`docs/reference.md` at line N.

## Summary

| Status | Rows |
| --- | --- |
| Done | 28 |
| Partial | 22 |
| Missing | 105 |
| Total counted | 155 |
| Not planned (not counted) | 1 |

| Stage | Counted rows |
| --- | --- |
| core | 78 |
| auxiliary | 77 |

### Where hofund is already better

1. **Auth on loopback too.** The gateway token is required on loopback as well
   (`crates/gateway/src/lib.rs`, `authorized`), and tokens are compared in constant time.
   magpie accepts any key on loopback (`ref.md:1920`, "any value works").
   That leaves it open to DNS rebinding and to any local process.
2. **Hub-and-spoke IR.** Each protocol has one codec to and from a shared IR
   (`crates/ir`, `crates/codec`). That is N codecs where pairwise translation needs N×N.
   magpie translates in many protocol-specific files under `internal/gateway`
   (`chat.go`, `responses.go`, `anthropic.go`, `gemini.go` and over 190 others).
3. **Quirks are data.** Vendor workarounds are declarative rules in `crates/quirks`.
   magpie scatters them through per-vendor files (`deepseek_schema.go`, `kimi_schema.go`,
   `grok_schema.go`, `zhipu_envelope.go`).
4. **No vendor names in core code.** A test bans vendor names in the gateway and router
   crates, and dependencies flow one way between crates (`docs/architecture.md`).
5. **Secrets out of plain files.** API keys can be `env:NAME` references
   (`crates/secrets/src/lib.rs`). magpie keeps keys in `providers.json` (`ref.md:2998`, "Files").
6. **Backpressure.** Streams are pulled through the codec, not buffered (`crates/provider/src/stream.rs`).
7. **Agent backup markers.** Backups use their own markers (`*.hofund.bak`, `*.hofund.absent`, `*.hofund.wrote`)
   and live beside the file (`crates/agent/src/catalog.rs:479`). A clean disconnect writes the pre-connect bytes back.
   A later edit stays. The backup is removed only after that write is read back and matches.
   `--force` restores the backup bytes.
8. **One config file.** A single TOML file (`crates/config/src/lib.rs`) replaces about 20
   JSON and YAML state files (`ref.md:2998`). It can be reviewed, diffed and checked
   (`hofund config check`).
9. **Release pipeline.** cargo-dist builds 5 targets plus a universal2 macOS binary,
   with checksums and optional signing (`dist-workspace.toml`, `.github/workflows/release*.yml`).
   Pull requests run `dist plan` and install the pinned Tauri CLI
   (`.github/workflows/ci.yml`). That is reproducible from CI.
10. **Per-upstream model names and reasoning effort.** Each route member names
    the model it sends (`crates/config/src/lib.rs:98`). Effort is one enum on
    the request (`crates/ir/src/effort.rs:16`), mapped by each codec. Token
    counts ask a Messages upstream and otherwise return an estimate
    (`crates/gateway/src/lib.rs:319`).

## Matrix

### 1. Gateway protocols and endpoints

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 1.1 | OpenAI Chat Completions endpoint `/v1/chat/completions` | `internal/gateway/chat.go`; `ref.md:1880` | core | Done | `crates/codec/src/openai_chat.rs:51` (`include_usage`), `:1341` (`encode_usage`), `:1431` (`client_wants_usage`), `:1468` (`merge_patch`) | Streamed Chat requests set `stream_options.include_usage` on the upstream, so the upstream reports usage. The client is sent that usage chunk only when its own request set `include_usage`. The chunk has an empty `choices` array and still carries `id` and `model`. A usage object always includes `prompt_tokens`, `completion_tokens`, and `total_tokens`. A missing input or output is 0. A missing total is their sum. A Messages stream keeps input and cache tokens from `message_start` and output tokens from `message_delta`; the usage chunk is that merge. Details stay optional. Next: golden stream tests per vendor. |
| 1.2 | OpenAI Responses endpoint `/v1/responses` (SSE) | `internal/gateway/responses.go`; `ref.md:1882` | core | Done | `crates/codec/src/openai_responses.rs:271` (`decode_custom_call`), `:601` (`decode_tools`), `:1167` (`function_call_item`), `:1219` (`restored_call`), `:1556` (`encode_usage`), `:1654` (`merge_patch`), `:1791` (`output_index`), `:2775` (`responses_id`) | Streams emit `output_text.done` and `content_part.done`, put `id` and `content` on `output_item.done`, put the full `output` array on `response.completed`, and set `sequence_number` on every event. `output_index` is the index of that item in `completed.output`, including when the inbound indexes were Chat's 0/1/2+n. A `namespace`, `web_search`, or `custom` tool is accepted. Same-protocol encode writes the original tools array and keeps an upstream item id. A namespaced call comes back as `namespace` plus the original name. A custom call comes back as `custom_tool_call` with the raw input. Response ids start with `resp_`. Item ids start with `msg_`, `fc_`, `ctc_`, or `rs_` and a unique suffix. A unary output item has `id` and `status`. Every usage object includes `input_tokens`, `output_tokens`, `total_tokens`, `input_tokens_details` (`cached_tokens` and `cache_write_tokens`), and `output_tokens_details` (`reasoning_tokens`). A missing counter is 0 and a missing total is the sum (checked 2026-10-25, <https://platform.openai.com/docs/api-reference/responses/object>). A Messages stream merges `message_start` and `message_delta` before that object is written. History `custom_tool_call` and `custom_tool_call_output` stay those types for a Responses upstream. Sent to Chat or Messages, the call is an assistant tool call named with the declared flat name and arguments `{"input": <raw>}`, and the output is a tool result with that call id. A namespaced `function_call` in history uses that same flat name. `encrypted_function_args` is not written. |
| 1.3 | Anthropic Messages endpoint `/v1/messages` | `internal/gateway/anthropic.go`; `ref.md:1883` | core | Done | `crates/codec/src/anthropic.rs:1183` (`anthropic_message_id`); `crates/provider/src/http.rs`; `crates/provider/src/billing.rs:12` | Usage is on `message_start`. `content_block_stop` comes before a single `message_delta`. Message ids start with `msg_`, including a unary reply. Extension fields pass through only when the inbound and outbound protocols match, so `metadata.user_id` and `cache_control` do not cross protocols. A system text that is only `x-anthropic-billing-header` is removed on that crossing. |
| 1.4 | Gemini `generateContent` and `streamGenerateContent` | `internal/gateway/gemini.go`; `ref.md:1885` | core | Done | `crates/codec/src/generative.rs:865` (`encode_usage`), `:1325` (`merge_patch`); `crates/gateway/src/endpoints.rs` | `usageMetadata` includes `promptTokenCount`, `candidatesTokenCount`, and `totalTokenCount` whenever usage is present. A Messages stream merges start and delta usage into that object. Cache and thoughts counts stay optional. Token counting is row 1.5. |
| 1.5 | Token counting: `/v1/messages/count_tokens` and Gemini `:countTokens` | `ref.md:1884-1885` | core | Done | `crates/gateway/src/lib.rs:139` and `:319`; `crates/gateway/src/endpoints.rs:40` (`match_count`); `crates/provider/src/anthropic.rs:53` (`count_input`), `:94` (`omit_max_tokens`); `crates/provider/src/generative.rs` (`count_input`) | A Messages upstream is asked to count. The posted body omits `max_tokens`; the count endpoint rejects that field (checked 2026-10-25, <https://platform.claude.com/docs/en/build-with-claude/token-counting>). A Generative Language upstream is asked at `{base}/models/{model}:countTokens` and the count is `totalTokens` (checked 2026-10-25, <https://ai.google.dev/api/tokens#method:-models.counttokens>). 404, 405, 501, and a 400 whose message says the endpoint is unsupported become an estimate of `ceil(chars/4)` with `estimated: true` (`crates/ir/src/estimate.rs:14`). A bad image or an unsupported tool stays an error. Counts do not write usage rows. This is not a model-family tokenizer; the catalog is not shipped. |
| 1.6 | Model list `/v1/models` and `/v1beta/models`, with reasoning levels and context sizes | `ref.md:1887`, `ref.md:1890` | core | Partial | `crates/gateway/src/lib.rs:198` (ids only) | Add context window, output limit, modalities and reasoning levels from the model catalog (row 2.8). |
| 1.7 | Codex model catalog `/v1/codex/models` | `internal/gateway/codex_catalog.go`; `ref.md:1888` | core | Missing | none | Render from the same catalog as row 1.6, in one codec-side formatter. |
| 1.8 | Ollama-compatible listing `/api/version`, `/api/tags`, `/api/show` | `internal/gateway/ollama.go`; `ref.md:1889` | auxiliary | Missing | none | Low cost. Add as an optional listing codec. |
| 1.9 | Speech to text passthrough `/v1/audio/transcriptions` and `/translations` | `internal/gateway/transcribe.go`; `ref.md:1886` | auxiliary | Missing | none | Multipart passthrough with model rewrite only. Stream the body instead of buffering it. |
| 1.10 | Embeddings | `internal/gateway/embeddings.go` | auxiliary | Missing | none | Add an `/v1/embeddings` codec over the same router. |
| 1.11 | Routes without the `/v1` prefix and behind a path-prefixed reverse proxy | `ref.md:1864-1866` | auxiliary | Missing | `crates/gateway/src/lib.rs:127` | Add a configurable base path. Never guess from headers. |
| 1.12 | Cross-protocol translation with tools, reasoning and images | `internal/gateway/ir.go`, `chat_tool_shape.go`, `image_input.go` | core | Done | `crates/ir/src/content.rs`; `crates/codec/src/openai_responses.rs:271` (`decode_custom_call`), `:601` (`decode_tools`), `:1167` (`function_call_item`); `crates/provider/src/http.rs:216` (`note_dropped`) | Already better (hub IR). A custom tool becomes a function with one string input on the way up and returns as `custom_tool_call` with that raw input. A namespace is flattened to `namespace__name` on the way up and restored to `namespace` plus the original name on the way back, for Chat and Messages upstreams, streamed and not. A history `custom_tool_call` is sent upstream as that flat function name with arguments `{"input": <raw>}`, and `custom_tool_call_output` is a tool result with the call id. A namespaced history call uses `namespace__name`, the same name as the declaration. Same-protocol Responses encode keeps `custom_tool_call`, the raw input, and `namespace` plus the original name. `web_search` has no function mapping and is dropped. The provider logs that warning once per tool name per process, and only when the upstream protocol differs. A request the codec cannot encode is HTTP 400 `invalid_request_error`, not 502. A conformance suite over every pair is still open. |
| 1.13 | Streaming with flow control | `internal/gateway/sse.go`, `whole_stream.go` | core | Done | `crates/provider/src/stream.rs` | Already better (backpressure). |
| 1.14 | Reasoning effort mapped between wires (`effort`, `reasoning_effort`, `thinking.budget_tokens`) | `internal/gateway/wire_effort.go`, `effort_update.go`; `ref.md:1291` | core | Done | `crates/ir/src/effort.rs:16`; `crates/ir/src/params.rs:42` | One enum on the request, not a process-global thread map. Same-protocol encode keeps the caller's field. Chat and Responses write `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`. Messages writes `output_config.effort` (`minimal` as `low`) and `thinking.type = disabled` when the caller asked for no reasoning. Generative Language writes `thinkingLevel`, folding `xhigh` and `max` to `high`, and `thinkingBudget: 0` for no reasoning. Levels are not clamped to a model. `budget_tokens` is not mapped. Level tables checked 2026-10-25. |
| 1.15 | Prompt cache handling (Anthropic `cache_control`, cache stays on one account) | `internal/gateway/claude_cache.go`; `ref.md:1600-1616` | core | Partial | cache tokens are parsed in `crates/codec/src/*.rs` but not stored | Same-protocol requests keep `cache_control` on the extension bag. A different outbound protocol drops it. Store cache tokens (row 9.2) and keep a session on one upstream (row 4.6). |
| 1.16 | Context compaction for Codex (native and summary) | `internal/gateway/native_compaction.go`, `compacting.go`; `internal/codexsummary` | auxiliary | Missing | none | Pass native compaction through. Add summary compaction only behind a flag. |
| 1.17 | Vendor JSON-schema fixes for tools | `internal/gateway/deepseek_schema.go`, `kimi_schema.go`, `grok_schema.go`, `oneof_schema.go` | auxiliary | Partial | `crates/quirks/src/lib.rs` (5 rules, none for schemas) | Add schema-rewrite quirk rules as data, with one fixture per vendor. |
| 1.18 | `max_tokens` default when a client omits it | `internal/gateway/maxoutput.go`; `ref.md:1420` | core | Done | `crates/config/src/lib.rs:68` (`Provider.max_output_tokens`); `crates/router/src/lib.rs:348` (`apply_wire`); `crates/codec/src/anthropic.rs:145` (`DEFAULT_MAX_OUTPUT_TOKENS`); `crates/quirks/src/lib.rs` (`DefaultMaxTokens`) | The client's cap wins. Otherwise the route member's `max_output_tokens`, then the provider's, then the `messages-default` quirk (4096), then 16384 on Messages encode only. Chat and Responses do not invent a cap. A translation failure is HTTP 400 `invalid_request_error`. The catalog can still replace 16384 with the model's published output limit. |
| 1.19 | Tool-call repair: text tool calls, undeclared tools, tool loop guard | `internal/gateway/textcalls.go`, `undeclared_tools.go`, `loop_guard.go`, `tool_loop.go` | auxiliary | Missing | none | Add as opt-in quirk rules, with metrics for how often each one fires. |
| 1.20 | Bare model ids and `provider/model` names | `internal/gateway/unknown_model.go`; `internal/provider/bare.go`; `ref.md:1924` | core | Done | `crates/router/src/lib.rs:141` (`dispatch`), `:238` (`resolve`), `:286` (`Ambiguous`) | The exact route name wins. `provider/model` selects that hop. A bare wire name works only when exactly one hop has it. Several hops is HTTP 400 and names the candidates. An unknown name is HTTP 404. |
| 1.21 | CORS for browser clients | `internal/gateway/cors.go` | auxiliary | Missing | none | Off by default. Allow listed origins only. |

### 2. Upstream providers and models

Preset-by-preset status is in [docs/coverage/providers.md](coverage/providers.md).

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 2.1 | OpenAI-compatible upstreams (Chat and Responses) | `internal/provider/provider.go` | core | Done | `crates/provider/src/openai_compat.rs`; `crates/config/src/lib.rs` (`protocol = "openai-chat"`) | Keep. |
| 2.2 | Anthropic upstream | `internal/provider/provider.go` | core | Done | `crates/provider/src/anthropic.rs` | Keep. |
| 2.3 | Google Gemini API upstream | `internal/provider/gemini_api.go`, `google.go` | core | Done | `crates/provider/src/generative.rs`; `protocol = "generative-language"`; `preset = "google"` in `crates/provider/presets.toml` | API key in `x-goog-api-key`, never as a bearer and never in the URL (checked 2026-10-25, <https://ai.google.dev/gemini-api/docs/api-key>). The model is a path segment. A unary call posts `:generateContent`; a stream posts `:streamGenerateContent?alt=sse` (<https://ai.google.dev/api/generate-content>). A bare host gains `/v1beta`. A pasted method URL is cut back to that base. `:countTokens` reads `totalTokens`. `preset = "google"` uses this adapter at `https://generativelanguage.googleapis.com/v1beta`. `protocol = "openai-chat"` on that preset keeps `.../v1beta/openai` and a bearer token (<https://ai.google.dev/gemini-api/docs/openai>). Vertex is row 2.4. Code Assist and Antigravity are row 3.4. |
| 2.4 | Google Vertex AI (ADC, impersonation, per-location models) | `internal/provider/vertex.go`, `internal/gateway/vertex.go`; `ref.md:867` | core | Done | `crates/provider/src/vertex.rs`; `preset = "google-vertex"` with `project`, `location`, `credentials`, and `impersonate` in `[providers.options]` | OAuth from an ADC file (`authorized_user`, `service_account` as an RS256 JWT, `impersonated_service_account`) plus an optional impersonation email. The path is `/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent`; a stream adds `?alt=sse`. Hosts are `aiplatform.googleapis.com` (`global`), `aiplatform.<us\|eu>.rep.googleapis.com`, and `<region>-aiplatform.googleapis.com` (checked 2026-10-25, <https://cloud.google.com/vertex-ai/generative-ai/docs/learn/locations>). No API key, and no service-account JSON in config. `Vertex::models_for` is the static per-location list; the gateway does not serve it (row 2.9). Not read: the metadata server, workload identity federation, and express-mode API keys. The body is the shared Generative Language encode, without Vertex-only thinking budgets. |
| 2.5 | AWS Bedrock (Converse and Mantle) | `internal/provider/presets.go` (`bedrock`, `bedrock-mantle`); `internal/gateway/mantle.go` | core | Done | `crates/provider/src/bedrock.rs`; `crates/provider/src/sigv4.rs`; `crates/codec/src/converse.rs`; `crates/provider/src/mantle.rs`; `crates/provider/src/openai_compat.rs`; `crates/provider/src/anthropic.rs` | `protocol = "bedrock-converse"` on preset `bedrock`, or the same protocol with `region` and `base_url` in `[providers.options]`, posts `POST {root}/model/{modelId}/converse`. A stream posts `converse-stream` and reads the AWS event stream. The body is Converse JSON. SigV4 signs the call for service `bedrock` with `access_key_id`, `secret_access_key`, and an optional `session_token`. No API key on that path. `top_k` is sent in `additionalModelRequestFields`. Reasoning effort is rejected rather than given an invented budget. Chat and Responses on the runtime stay at `/openai/v1` with a bearer token. Chat re-encoding sends `max_tokens: 64` only as `max_completion_tokens: 64`, for both Chat and Messages inbound requests and both Bedrock presets. Messages stays at `/anthropic` with `x-api-key` alone and no `Authorization`. On Bedrock Messages, configured `anthropic-beta` header values and every element in array-valued body `anthropic_beta` are stringified like the reference, comma-split, renamed from `advanced-tool-use-2025-11-20` to `tool-search-tool-2025-10-19`, stripped of `prompt-caching-scope-2026-01-05` and `redact-thinking-2026-02-12`, and deduplicated; an unchanged list preserves non-string JSON values, an empty fitted body list removes the key, and a non-array value is untouched. Top-level `metadata` is removed on execute and count, including same-protocol passthrough. Client `anthropic-beta` headers are not forwarded, so only configured provider headers reach the header filter. Mantle is detected from preset `bedrock-mantle`, including a relay base override, or from a `bedrock-mantle.<region>.api.aws` host regardless of another preset; the other-preset guard applies only to base normalization. Only `api.aws` is accepted, not `amazonaws.com` (AWS endpoints checked 2026-10-27). Address-added OpenAI bases at `/openai/v1` normalize to `/v1`, so model listing uses `/v1/models`. Because a hofund provider has one protocol and one address, the address fills the configured wire the way the reference fills the other APIs: a bare root or `/anthropic` address on Chat or Responses becomes `/v1`, and a bare root, `/v1`, or `/openai/v1` address on Messages becomes `/anthropic` (the reference keeps a non-empty URL given for that API as it is). A closed OpenAI GPT id (`openai.gpt-` and not `gpt-oss`) or an `xai.` id is posted under `/openai/v1`; every other chat or Responses model is posted under `/v1`. A nonempty model refused with a route-specific 4xx on its initial guessed prefix is tried once at the explicit other prefix and remembered per model and protocol. A refusal on the remembered prefix is not retried or forgotten. The second attempt is judged on its status line and headers before its body: a 2xx keeps the route even if the body later fails, and that body error is returned. Before a 2xx, a second route-refusal message at any status or a Connect, Timeout, or Transport error forgets the route and returns the first error; any other second error, including a decode failure, is returned with the route kept. Deliberate improvements: each request captures its first prefix before sending, fixing the reference's concurrency false refusal and duplicate-prefix race, and the connectivity probe also retries at the other prefix while the reference connection test does not. Not in this build: the instance metadata service, a shared credentials file, SSO, a Bedrock API key on Converse, documents, and guardrails (<https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html>, <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html>, <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>, <https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys-use.html>, <https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html>). |
| 2.6 | Azure OpenAI | `internal/provider/azure.go` | core | Done | Code: `crates/provider/src/azure.rs` (`list_deployments`); `crates/provider/src/models.rs` (plugin `list_models` and `base_model_fallback`); `crates/provider/src/presets.rs` (`azure_base`); `Credential::ApiKey` in `crates/provider/src/http.rs`. Tests: `crates/provider/tests/azure_deployments.rs` (`deployments_are_model_ids_and_the_key_is_api_key`, `a_refused_deployment_list_falls_back_to_v1_models`, `both_lists_refusing_the_key_names_the_deployments`); `crates/provider/tests/models.rs` (`azure_plugin_returns_deployments_then_marks_the_base_model_fallback`). CI: [run 40178604119](https://github.com/logibricks-io/open-heimdall-hofund/actions/runs/40178604119) on `73656ae` (fmt, clippy, test on Ubuntu, macOS, and Windows, release-plan, cargo-deny). The newer mock is not covered by that run. | A resource, a pasted deployment URL, or a resource name becomes `https://<host>/openai/v1`. The key is `api-key`, not a bearer token. The plugin `list_models` returns `ListKind::Deployments` from `GET {root}/openai/deployments?api-version=2022-12-01` or, if that list is refused or empty, `GET {root}/openai/v1/models`. The deployment name is the model id. The fallback is explicitly labelled as base models because those ids are deployment names only when the resource owner named them after their models. Embedding, image, and audio ids are left out. Scheduled refresh is covered by row 2.9. Verified with mocks only. |
| 2.7 | Provider presets (58 presets: vendors, relays, local servers, and one remote gateway) | `internal/provider/presets.go`; `README.md` | core | Partial | `crates/provider/presets.toml` (58 presets, source commit `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`; `presets.go` identical to `5b2b3d3f50959735403dff6b77a85eec65772218`); `crates/provider/src/presets.rs`; `preset` on `[[providers]]`; [docs/coverage/providers.md](coverage/providers.md) | The catalog is data and every endpoint parses. Chat, Responses, Messages, the Gemini preset, and Vertex build an adapter. A remote gateway and decision-only presets return a clear error. A live partner feed, model lists, and a connectivity command are not in this build. |
| 2.8 | Model catalog from models.dev: names, prices, limits, reasoning levels | `internal/catalog`; `ref.md:1291` | core | Missing | none | Cache the catalog on disk with an ETag, allow a mirror, and let local overrides win. |
| 2.9 | Live model lists from each provider, with scheduled refresh and auto-include | `internal/provider/autolist.go`, `modellist.go`; `ref.md:319` | core | Partial | Code: `crates/provider/src/models.rs`, `crates/provider/src/azure.rs`, `crates/daemon/src/models.rs`, `crates/daemon/src/models_state.rs`, `plugins/registry/src/builtin.rs`, `kits/upstream/src/conformance.rs`. Tests: `crates/provider/tests/models.rs`, `crates/provider/tests/azure_deployments.rs`, `crates/daemon/tests/models_aggregate.rs`, `crates/daemon/tests/models_refresh.rs`, `crates/daemon/tests/models_state.rs`, `plugins/registry/tests/model_conformance.rs`, `plugins/providers/relay/src/lib.rs` (`unsupported_factory_passes_list_conformance`); verified with mocks only. | Per-wire lists, static catalogs, host aggregation and merging, exact custom URLs, public drawers, region planning, redaction, and factory conformance are implemented. The host refresh job, persistent state and due times, picks, delist/restore, auto-include, dead sides, and 429 retry delay are implemented. Deferred rows in `docs/plugins/models.md` remain with their named pull requests. |
| 2.10 | Provider test and endpoint detection | `internal/provider/test.go`, `detect.go`; `internal/probelog` | core | Partial | Code: `crates/provider/src/probe.rs`, protocol `probe` methods in `crates/provider/src`, `crates/daemon/src/models.rs`, `kits/upstream/src/conformance.rs`. Tests: `crates/provider/tests/models.rs`, `crates/daemon/tests/models_probe.rs`, `plugins/registry/tests/model_conformance.rs`; verified with mocks only. | One minimal probe runs on the configured wire with bounded responses, extra-body merge, auth rules, lazy Vertex credentials, host two-pass selection, `UpstreamName` mapping, four-wide batching, spans, `provider_test` tracing logs, and factory conformance. The CLI remains #28; endpoint detection and client test-as remain with their deferred pull requests. |
| 2.11 | Custom headers and User-Agent per provider | `internal/provider/authheader.go`, `user_agent.go`; `ref.md:340` | core | Done | `crates/config/src/lib.rs` (`headers`) | Keep. |
| 2.12 | Several keys per provider, with rotation and per-key rest | `internal/provider/keys.go`; `ref.md:1560` | core | Missing | `crates/config/src/lib.rs` (one `api_key`) | Model keys as router members, so all routing strategies apply to keys too. |
| 2.13 | Model alias and wire name (the name a client sends differs from the vendor's) | `internal/provider/usagealias.go`, `rename.go`; `ref.md:1367` | core | Done | `crates/config/src/lib.rs:89` (`Route`) and `:98` (`Upstream`) | Each route member may name the model sent upstream. A string upstream still sends the client model unchanged. The reply model is rewritten to the name the client sent. This is the route's wire name, separate from a usage-statistics alias. |
| 2.14 | Model spec: context window, output limit, vision, reasoning levels | `internal/provider/modelspec.go`; `ref.md:1291` | core | Missing | none | Store the spec in the catalog (row 2.8) and use it in routing and quirks. |
| 2.15 | Hide deprecated and retired models; disable a model | `internal/provider/retired.go`; `internal/gateway/disabled.go` | core | Missing | none | Add flags in the catalog. |
| 2.16 | Extra body fields per provider or model | `internal/provider/extrabody.go` | auxiliary | Missing | none | A `set` and `unset` quirk rule with JSON-pointer paths. |
| 2.17 | Import providers from CC Switch, Claude Code, Codex, OpenCode and Alma | `internal/provider/importapps.go`, `import_codex.go`, `import_opencode.go`; `ref.md:1520` | auxiliary | Missing | none | Read-only importers that produce a TOML diff to review before writing. |
| 2.18 | `magpie://import` links | `internal/provider/importurl.go`; `ref.md:2286` | auxiliary | Missing | none | Register a `hofund://` scheme in the desktop app. Always confirm and show the diff. |
| 2.19 | Local servers (Ollama, LM Studio) | `internal/provider/presets.go` (`ollama`, `lmstudio`) | core | Partial | works as an `OpenAiCompat` provider in config | Add presets and local detection. |
| 2.20 | Vendor status pages taken into account | `internal/upstream` | auxiliary | Missing | none | Poll status pages, and show "vendor degraded" instead of a sign-in error. |
| 2.21 | Allow insecure TLS for one provider | `internal/provider/insecure_tls.go` | auxiliary | Missing | none | A per-provider custom CA option first. Insecure mode only with a loud warning. |

### 3. Subscriptions and accounts

Subscription-by-subscription status is in [docs/coverage/subscriptions.md](coverage/subscriptions.md).

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 3.1 | Codex / ChatGPT subscription as a provider | `internal/gateway/codex_backend.go`; `internal/provider/codex_*.go`; `ref.md:1779` | core | Missing | none | One `Subscription` trait: sign-in, refresh, quota and model list. Each vendor is an adapter crate behind a feature flag. |
| 3.2 | Claude subscription by driving the local `claude` binary over an MCP bridge | `internal/claudebridge`; `internal/gateway/claude_subscription.go`; `docs/subsystems/claude-subscription-bridge.md` | core | Missing | none | A normal provider the user adds in config, with the same credential storage, refresh, and quota path as the other subscription adapters. Provider docs state, in one neutral sentence, that subscription use follows the vendor's terms. Not a hidden opt-in. |
| 3.3 | GitHub Copilot subscription, including Enterprise | `internal/provider/copilot_*.go` | core | Missing | none | Adapter on the trait from row 3.1. |
| 3.4 | Gemini Code Assist and Antigravity | `internal/gateway/codeassist.go`; `internal/provider/google.go`, `antigravity_*.go` | core | Missing | none | Adapter. Warn about account suspension risk, as magpie does. |
| 3.5 | Other sign-ins: Grok, Cursor, Devin, Kiro, Qoder, Factory, Zed, WorkBuddy, Z Code, Cline | `internal/provider/grok.go`, `cursor.go`, `devin.go`, `kiro.go`, `qoder.go`, `factory.go`, `zed.go`, `workbuddy.go`, `zcode.go`, `cline.go` | core | Missing | none | Community adapters behind the same trait, maintained out of the core crates. |
| 3.6 | Account discovery from agents' own sign-ins (`MAGPIE_NO_ACCOUNTS=1` turns it off) | `internal/provider/discovery.go`; `README.md` | core | Missing | none | Opt-in, not opt-out. Read only. Never write tokens back without asking. |
| 3.7 | Several accounts per subscription; import accounts from files | `internal/provider/login_import.go`, `logins.go`; `ref.md:1830` | core | Missing | none | Accounts as router members (as in row 2.12). |
| 3.8 | Quota and allowance windows; `magpie quota`, `quota wait`, history | `internal/provider/quotas.go`, `quota_history.go`; `ref.md:2588` | core | Missing | none | One quota model (windows, resets, credits) shared by routing, the UI and the CLI. |
| 3.9 | Key balances and plan windows for relays and coding plans | `internal/provider/balance.go`, `planquota.go`, `sub2api_usage.go` | core | Missing | none | Balance URL per provider, using the same quota model. |
| 3.10 | Quota and usage alerts | `internal/provider/quota_alert.go` | core | Missing | none | Desktop notification and a CLI warning at configurable thresholds. |
| 3.11 | Daily warm-up to start allowance windows early | `internal/provider/warmup_daily.go`; `plugins/warmups` | core | Missing | none | Opt-in, with the cost shown in usage. |
| 3.12 | Daily check-in for vendors that reward it | `internal/provider/checkin.go`, `*_checkin.go` | core | Missing | none | Low priority. Plugin only. |

### 4. Routing, failover and load balancing

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 4.1 | Ordered failover before the first byte | `internal/gateway/fallback.go`; `docs/subsystems/gateway-routing.md` | core | Done | `crates/router/src/lib.rs` | Keep. |
| 4.2 | Configurable failure classes (connect errors, statuses) | `docs/subsystems/gateway-routing.md` | core | Done | `crates/config/src/lib.rs` (`failover.connect_errors`, `failover.statuses`) | Already better: explicit and typed. |
| 4.3 | Named routing groups (`group/<id>`), nested groups, copy | `internal/provider/group*.go`; `plugins/groups`; `ref.md:1433` | core | Partial | `crates/config/src/lib.rs:89` (`Route` is an unnamed ordered group) | Make a group a first-class node in a routing tree. Reject cycles when config loads. |
| 4.4 | Groups found automatically for a model that several providers serve | `internal/provider/group_serve.go`; `ref.md:1924` | auxiliary | Missing | none | Derive from the catalog. Show the result in `hofund routes`. |
| 4.5 | Strategies: smart, order, rotate, least used, weekly pace, weight | `plugins/rotation`; `internal/gateway/weighted.go`; `internal/provider/window_pace.go`; `ref.md:1452` | core | Missing | `docs/magpie-comparison.md` ("only ordered failover") | A `Strategy` trait with pure, unit-tested ordering functions over a snapshot of state. Start with order, weight, round-robin and least used. Add quota-aware ones after row 3.8. |
| 4.6 | Session stickiness for prompt cache (`stays=auto/session/turn/off/strict`) | `internal/gateway/affinity.go`, `session_route.go`; `ref.md:1600` | core | Missing | none | Key on a session id taken from each protocol. Keep the table in memory and persist it in SQLite. |
| 4.7 | Rest and cooldown: `Retry-After`, reset times, doubling backoff, caps | `docs/reference.md:1700-1745`; `internal/gateway/patience.go` | core | Partial | `crates/router/src/cooldown.rs` (per-member generic, rate, quota, credit, observation, and panic rests); `crates/router/src/lib.rs` (ready-first ordering and one cooling probe); `crates/daemon/src/membership.rs` (outcome recording and timed panic recovery); `crates/provider/src/http.rs` and `crates/plugin-sdk/src/transport.rs` (`Retry-After` parsing) | Non-positive hints are absent; header hints cap at one hour, while exhausted passive observations use stated resets capped at eight days. Counts never clear rests, and non-exhausted observations record nothing. Unlike the reference gateway, a new event never shortens an active rest, and credit honors a longer header hint above the 30-minute minimum. Expose cooldown times in the routing view. Per-provider policy knobs and success-header vendor feeds remain; the feeds belong to subscription plugins in H. |
| 4.8 | Routing rules: tokens, images, effort, agents, time, days, folders, compact | `internal/provider/grouprule.go`, `ruleparse.go`; `internal/cli/groups_rules_cli.go` | auxiliary | Missing | none | Rules as a typed expression in TOML, checked when config loads, with a `hofund route explain` command. |
| 4.9 | Intent routing with a classifier model | `internal/gateway/classify.go`, `decide.go` | auxiliary | Missing | none | Optional, with a time limit and a cache. Fail open to the default order. |
| 4.10 | Move to a larger context window at 95% fill | `ref.md:1400` | auxiliary | Missing | none | Use the catalog spec (row 2.14). |
| 4.11 | Ask another member after a safety refusal | `internal/gateway/strict_refused.go`; `ref.md:1750` | auxiliary | Missing | none | Configurable count, off by default. |
| 4.12 | Fast mode and effort per group member | `internal/gateway/groupfast.go`; `internal/provider/groupeffort.go`, `groupfast.go` | auxiliary | Missing | none | Member-level overrides in the routing tree. |
| 4.13 | Routing view: who went first and why; `/v1/magpie/route` | `internal/gui/assets/plugins/routing`; `internal/gateway/session_route.go` | core | Missing | none | Record a routing decision per request and serve it over a local API (row 12.3). |
| 4.14 | Concurrency and RPM limits | `internal/gateway/concurrency.go`, `rpm.go` | auxiliary | Missing | none | A token bucket per member and per key, built on `tower` layers. |
| 4.15 | Decision models at `/v1/systemone` | `internal/gateway/decide.go`; `ref.md:1494` | auxiliary | Missing | none | Low priority. |
| 4.16 | List only the group's models to clients (`listed=no`) | `ref.md:1486` | auxiliary | Missing | none | A `listed` flag per route. |

### 5. Agent wiring

Agent-by-agent status is in [docs/coverage/agents.md](coverage/agents.md).

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 5.1 | Supported agents (65 adapters) | `internal/agent`; `ref.md:88` | core | Partial | `crates/agent/src/catalog.rs` (`GEMINI` `model.name`, the only remaining const) and `plugins/agents/<id>/agent.toml` for Codex, Claude Code, and OpenCode. Tests: `plugins/registry/tests/codex_equivalence.rs`, `claude_code_equivalence.rs`, and `opencode_equivalence.rs` compare each shipped plugin with its frozen const; `plugins/agents/<id>/fixtures/all/` holds their golden cases. `crates/agent/tests/rewrite.rs` covers Gemini CLI model behavior. `crates/cli/src/main.rs` (`opencode_models_note`). OpenCode follows `XDG_CONFIG_HOME`. Connect warns when the configured model has no route. [docs/coverage/agents.md](coverage/agents.md) lists all 65. Verified with mocks only. | OpenCode's entry is `provider.<id>` with npm `@ai-sdk/openai-compatible`, `options.baseURL`, `options.apiKey`, and `models` (checked 2026-10-25, <https://opencode.ai/docs/providers/>). The v2 page uses `providers` plus `package` and `settings`; this follows the current page. `models` is one object per route: `name` and `variants: {}`. Modalities, context, output limits, and reasoning are not invented. Gemini CLI's model is `model.name` (<https://geminicli.com/docs/reference/configuration/>). A string `model` is moved there. Claude Code writes `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, and `ANTHROPIC_SMALL_FAST_MODEL` to the selected model, and `ANTHROPIC_DEFAULT_FABLE_MODEL` only when that id contains `claude-fable-` (<https://code.claude.com/docs/en/env-vars>, <https://code.claude.com/docs/en/model-config>). `CLAUDE_CODE_SUBAGENT_MODEL` and effort stay unset: a subagent with no model keeps the session model, and connect does not pick an effort. Describe each adapter as data, and add the next 10 by usage. |
| 5.2 | Connect and disconnect with backup and restore | `docs/subsystems/agent-wiring.md`; `internal/agent` | core | Done | `crates/agent/src/catalog.rs:479` (`disconnect`), `:702` (`merge_owned_keys`), `:782` (`restore_exact`); `crates/agent/src/store.rs:91` (`write_verified`) | If the file still equals what connect wrote, disconnect writes the pre-connect backup back, so a compact Claude or Gemini `settings.json` stays byte-identical. Otherwise each owned key returns to its pre-connect value when it still holds what hofund wrote, and is removed only when it was absent before. A value the user changed is kept, with a warning. Keys the user added stay. The backup is deleted only after the restored bytes are read back and match. `--force` writes the backup bytes. A second disconnect exits 0. Empty directories connect created are removed. |
| 5.3 | Format-preserving edits (TOML, JSONC, YAML) | `internal/edit` | core | Partial | TOML keeps comments; JSONC comments are dropped until revert (`crates/agent/src/catalog.rs`) | Use a lossless JSONC editor that keeps comments. |
| 5.4 | Drift detection | `docs/subsystems/agent-wiring.md` | core | Partial | `crates/agent/src/lib.rs:175` (`ConnectionStatus`) | Report which keys drifted, and offer to fix them. |
| 5.5 | Set an agent's model and effort from the CLI (`magpie claude opus`) | `internal/cli/main.go:38`; `ref.md:2597` | core | Missing | none | `hofund agent set <agent> model=<id> effort=<level>`, checked against the catalog. |
| 5.6 | Profiles: save and switch every agent's settings | `internal/profile`; `ref.md:2597` | core | Missing | none | Snapshots stored as TOML that are easy to diff. |
| 5.7 | Visible model list per agent | `internal/provider/visible.go`; `ref.md:340` | core | Missing | none | A per-agent filter in config. |
| 5.8 | Agent CLI version check and update | `internal/proc`; `internal/provider/install.go` | auxiliary | Missing | none | Report only. Never update without asking. |
| 5.9 | WSL wiring from Windows | `internal/wslrun`; `internal/sessions/wsl.go` | auxiliary | Missing | none | Later phase. |
| 5.10 | Status line for agents (`magpie statusline`) | `internal/cli/main.go:38` | auxiliary | Missing | none | A fast CLI that reads the local API. |

### 6. Library

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 6.1 | Instruction sets written between markers in each agent's file | `internal/library`; `docs/subsystems/library.md` | auxiliary | Missing | none | Reuse the agent adapter data (row 5.1), so the library needs no per-agent code. |
| 6.2 | MCP servers written into each agent in its own format | `docs/subsystems/library.md` ("MCP servers") | auxiliary | Missing | none | Same as row 6.1. |
| 6.3 | Skills (GitHub, git, local, skills.sh market), updates, groups | `docs/subsystems/library.md` ("Skills") | auxiliary | Missing | none | Pin each skill to a commit and verify a hash before linking. |
| 6.4 | Hooks and subagents written into agents | `docs/subsystems/library.md` ("Hooks", "Subagents") | auxiliary | Missing | none | Later phase. |
| 6.5 | RTK install and removal | `docs/subsystems/library.md` ("RTK") | auxiliary | Missing | none | Later phase. |

### 7. Built-in tools and MCP

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 7.1 | Web search for any model (Tavily, Brave, Exa, Firecrawl, You.com, SearXNG and others), several keys | `internal/gateway/search*.go`; `README.md` | auxiliary | Missing | none | One `SearchBackend` trait with an ordered list and a key pool. |
| 7.2 | Web search and fetch as an MCP server (`web_search`, `web_fetch`) | `internal/gateway/search_mcp.go`; `docs/subsystems/gateway-routing.md:642` | auxiliary | Missing | none | Serve MCP over streamable HTTP from the gateway, behind the same token. |
| 7.3 | Image recognition as an MCP server | `internal/gateway/vision_mcp.go`; `gateway-routing.md:700` | auxiliary | Missing | none | Later phase. |
| 7.4 | Image generation MCP (`magpie mcp image`) and video | `internal/imagemcp`; `internal/gateway/draw*.go`, `video.go` | auxiliary | Missing | none | Later phase. |
| 7.5 | Plugin tools offered to every agent as one MCP server | `plugins/tools`; `docs/subsystems/plugin-tools.md` | auxiliary | Missing | none | Comes with the plugin runtime (row 8.1). |
| 7.6 | Sign-in to remote MCP servers that use OAuth | `internal/mcpauth` | auxiliary | Missing | none | Store tokens in the OS keychain. |

### 8. Plugins and middleware

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 8.1 | Gateway middleware hooks `onRequest`, `onEvent`, `onResponse` (JavaScript on Bun) | `internal/middleware`; `docs/subsystems/gateway-middleware.md` | auxiliary | Missing | none | Run plugins as WebAssembly components (wasmtime) over the IR: sandboxed, fast, no Bun download, with fuel and memory limits. |
| 8.2 | Ready-made middleware: param-override, model-map, system-prompt, word-guard, think-tags | `README.md`; `ref.md:1044` | auxiliary | Partial | `crates/quirks/src/lib.rs` (temperature, top-p, max-tokens rules) | Build these in Rust as declarative rules, so they need no plugin at all. |
| 8.3 | Asynchronous request hooks (context recall) | `docs/subsystems/gateway-request-hooks.md` | auxiliary | Missing | none | Run with a time limit and fail open. |
| 8.4 | Built-in plugin registry with on and off switches (`plugins.yml`) | `internal/pluginrt`; `docs/subsystems/plugin-runtime.md` | auxiliary | Missing | none | Use Cargo features and config sections instead of runtime toggles where possible. |
| 8.5 | OpenCode provider plugins and pi packages | `internal/plugin`; `ref.md:957` | auxiliary | Missing | none | Not core. Revisit after row 8.1. |
| 8.6 | Agent plugins that add agents | `internal/agentplug`; `ref.md:1109` | auxiliary | Missing | none | Covered by data-driven adapters (row 5.1). |

### 9. Usage, cost and sessions

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 9.1 | Ledger of every request | `internal/usage`; `usage.jsonl` (`ref.md:2998`) | core | Partial | `crates/usage/src/lib.rs` (SQLite: model, upstream, tokens, latency, status) | Already a better store (SQLite instead of JSONL). Add the fields below, with migrations. |
| 9.2 | Cache read and write tokens (5-minute and 1-hour split), reasoning tokens | `ref.md:1220`; `internal/usage/block_price.go` | core | Missing | `crates/usage/src/lib.rs` has no cache columns | Carry them through IR usage (`crates/ir/src/usage.rs`) into the ledger. |
| 9.3 | Cost at effective prices: custom, provider catalog, models.dev; USD or CNY | `internal/catalog`; `internal/fx`; `ref.md:1150` | core | Missing | none | Store the price used in each row, so totals do not change when prices change. Mark calls with no price as unpriced, as magpie does. |
| 9.4 | Usage report by period and CSV export | `internal/usageapi`; `internal/cli/main.go:38` | core | Partial | `crates/cli/src/main.rs:261` (local time and a totals line); `crates/usage/src/lib.rs:169` | Add `--since`, `--group-by`, `--csv` and `--json`. |
| 9.5 | Attribution by agent, provider key and gateway key | `internal/usage/client_agents.go`; `ref.md:340` | core | Missing | none | Detect the agent from the path, headers and gateway key. Record it per row. |
| 9.6 | Sessions with cost and title; grouping of subagents | `internal/sessions`; `docs/subsystems/gateway-sessions.md` | auxiliary | Missing | none | A session id from each protocol, stored in the ledger. |
| 9.7 | Read usage from agents' own session files | `internal/sessions/calls_*.go` | auxiliary | Missing | none | Later phase. |
| 9.8 | Apps report their own usage (`POST /v1/magpie/usage/requests`) | `internal/gateway/usage_ingest.go`; `ref.md:2096` | auxiliary | Missing | none | Idempotent on a request id, as magpie does. |
| 9.9 | Output speed and time to first token | `internal/gateway/ttft.go`; `docs/subsystems/gui-shell.md` ("Streamed output speed") | core | Partial | `crates/usage/src/lib.rs` (`latency_ms` only) | Record time to first byte and tokens per second. |
| 9.10 | Usage heatmap and summary API (`/v1/magpie/usage`) | `internal/usage/heatmap.go`; `internal/gateway/usage_api.go` | auxiliary | Missing | none | Serve from SQLite queries. |
| 9.11 | Settings advice from real usage (compaction, cache TTL) | `internal/tune` | auxiliary | Missing | none | Low priority. |
| 9.12 | Gateway key limits per day, week and month (`/v1/magpie/limit`) | `internal/budget`; `internal/gwbudget`; `ref.md:1440` | core | Missing | none | Enforce in SQLite transactions, so limits hold across restarts. |

### 10. Auth, sharing and network

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 10.1 | Gateway authentication | `internal/gateway/lan.go`; `ref.md:1920` | core | Done | `crates/gateway/src/lib.rs:345` (`authorized`); `crates/secrets/src/lib.rs` (`TokenFile`, mode 0600) | Already better: a token is required on loopback too. |
| 10.2 | Accept Bearer, `x-api-key`, `x-goog-api-key` or `?key=` | `ref.md:1440` | core | Done | `crates/gateway/src/lib.rs:352` (`x-api-key`) | `x-api-key` is compared in constant time, together with the bearer token, `x-goog-api-key`, and a `key` query value. Every presented secret is checked. |
| 10.3 | Token rotation | `internal/access` | core | Done | `crates/gateway/src/lib.rs:325` (`current_token`); `crates/cli/src/main.rs` (`token rotate`) | The gateway re-reads the token file on each request and keeps the last good token when the read fails. Connected agents have their token keys rewritten. A failed rewrite prints `hofund agent connect <id>`. |
| 10.4 | Named gateway keys with model and account limits | `internal/access`; `caller-keys.json` (`ref.md:2998`) | core | Missing | `crates/secrets/src/lib.rs` (one token) | Store hashed keys only (magpie stores them readable), with scopes and limits. |
| 10.5 | LAN sharing, allowed IPs (`MAGPIE_ALLOW_IPS`), trusted proxy (`MAGPIE_TRUST_PROXY`) | `internal/gateway/lan.go`, `allow.go`; `ref.md:1850` | core | Partial | `crates/config/src/lib.rs` (`listen` can be any address) | Require named keys for any address other than loopback, plus a CIDR allow-list. |
| 10.6 | Failed-auth throttling | `internal/gateway/auththrottle.go` | auxiliary | Missing | none | Throttle per IP with a `tower` layer. |
| 10.7 | TLS serving with certificate hot reload | `internal/gateway/tls.go`; `ref.md:1440` | auxiliary | Missing | none | rustls, with the certificate reloaded when its file changes. |
| 10.8 | Public URL for reverse proxies (`MAGPIE_PUBLIC_URL`) | `ref.md:1843` | auxiliary | Missing | none | A config field used in printed examples. |
| 10.9 | Remote magpie as a provider | `internal/gateway/remote_magpie.go`; `internal/provider/remote_magpie.go` | auxiliary | Missing | none | A remote hofund is just an OpenAI or Anthropic provider with a named key. Add quota pass-through later. |
| 10.10 | Outbound proxy for vendor requests: global and per provider (http, https, socks5) | `internal/netproxy`; `internal/provider/proxy.go` | auxiliary | Partial | reqwest honors `HTTPS_PROXY` from the environment (`crates/provider/Cargo.toml`) | Add proxy settings globally and per provider, including socks5. |
| 10.11 | Secrets kept out of config files | `providers.json`, mode 0600 (`ref.md:2998`) | core | Done | `crates/secrets/src/lib.rs` (`env:NAME`) | Already better. Next, add OS keychain references (`keychain:NAME`). |

### 11. Configuration and state

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 11.1 | Starter config and validation | `internal/appdir`; `internal/settings` | core | Done | `examples/hofund.toml`; `crates/cli/src/main.rs:230` (`config check`), `:339` | `--help`, `-h`, and `help` print usage. `config check` prints `config ok:` and the path it read. `examples/hofund.toml` has `[[providers]]` and `[[routes]]`. Next: point a validation error at the exact key. |
| 11.2 | Changes apply without a restart | `internal/filememo`; app edits apply live | core | Missing | none | Watch the config file and swap the router atomically (`arc-swap`). Keep running requests on the old config. |
| 11.3 | Atomic writes, cross-process file locks, last-good fallback | `internal/filelock`, `internal/steady`, `internal/lastgood` | auxiliary | Partial | atomic writes in `crates/agent` and `crates/secrets` | Add advisory locks and keep the last good config. |
| 11.4 | Environment overrides (`MAGPIE_ADDR` and others) | `ref.md:1843`; `internal/settings/settings.go:1362-1370` | auxiliary | Partial | Code has `crates/cli/src/main.rs` (`--config` only). The L control-plane plan specifies `HOFUND_ADDR` for the daemon model listener and healthcheck (`docs/plugins/control-plane.md`), but implementation remains open. | Implement shared `HOFUND_ADDR`-then-config listen resolution, report the effective address and source, and keep an environment override from relistening on config reload. `HOFUND_` variables for config and log level remain. |
| 11.5 | Config surgery from the CLI (`provider add`, `group set`, `key`) | `internal/cli`; `ref.md:319` | core | Missing | none | Edit commands that keep comments in `hofund.toml`, built on `toml_edit`. |
| 11.6 | Provider rename (`provider set <id> id=`) that follows every reference | `internal/provider/rename.go:28-131`; `internal/agent/rename.go:14`; `internal/cli/providers_cli.go:438-459` | core | Missing | none; the L control-plane plan rejects `id=` until this row lands (`docs/plugins/control-plane.md`) | Under the config sidecar lock, rewrite every `[[routes]]` upstream, group, and fallback reference with `toml_edit`; move the picks and model-state file under the old and new state locks; rewrite agent model references through row 5.2's backup and restore. Fail atomically rather than leave a half-renamed config. |

### 12. Observability

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 12.1 | Recent request inspection with bodies | `internal/gateway/reqlog.go`, `capture.go`; `docs/subsystems/gui-shell.md` ("Recent-call bodies") | auxiliary | Missing | none | A bounded ring buffer, off by default, with secrets redacted before storage. |
| 12.2 | OTLP traces and metrics export | `internal/otlp`; `plugins/otel`; `ref.md:2782` | auxiliary | Missing | no `tracing` dependency | Use `tracing` with `opentelemetry-otlp` behind a feature. Follow the GenAI semantic conventions. |
| 12.3 | Local management API (`/v1/magpie/*`: quotas, usage, route, concurrency) | `ref.md:1855-1870` | core | Missing | none | One versioned `/hofund/v1` API, used by the desktop app, the TUI and the CLI. |
| 12.4 | Runtime profiling (heap, CPU, goroutines; `healthcheck`) | `internal/gateway/heap.go`, `cpu.go`, `goroutines.go`; `internal/cli/main.go:38` | core | Partial | `crates/gateway/src/lib.rs:126` (`GET /health`, no auth, body `{"status":"ok"}`) | Health is open and contains no secrets. Still open: `hofund healthcheck` and `tokio-console` behind a feature. |
| 12.5 | Structured logs | `internal/probelog`; `internal/gateway/trace.go` | auxiliary | Missing | none | `tracing` with JSON output and a request id on every line. |

### 13. Privacy masking

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 13.1 | Rule-based masking with placeholders restored in streams and tool arguments | `internal/redact`; `internal/masking`; `README.md` | auxiliary | Missing | none | Run as an IR pass on requests and a streaming restore pass on replies. Fuzz it across chunk boundaries. |
| 13.2 | Optional local PII model (about 1.6 GB) | `internal/pii`; `native/privacy-filter` | auxiliary | Missing | none | Later. An ONNX model behind a feature, downloaded only on request. |
| 13.3 | Mask, Warn and Block modes; never-mask list; return 503 when masking fails | `README.md`; `internal/maskrec` | auxiliary | Missing | none | Fail closed, as magpie does. Add an audit log of what was masked, with no values. |

### 14. Backup and sync

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 14.1 | Encrypted backup and restore with a passphrase | `internal/backup`; `ref.md:2672` | auxiliary | Missing | none | `age` encryption over the TOML config and SQLite. |
| 14.2 | Sync over WebDAV, S3 and an encrypted GitHub repository | `internal/davsync`; `docs/subsystems/sync-backup.md`; `ref.md:2710` | auxiliary | Missing | none | Later phase. Config is one file, so sync is far simpler than in magpie. |

### 15. Desktop app and user interfaces

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 15.1 | Menu bar tray | `internal/gui`; `magpie tray` | core | Partial | `apps/desktop/src-tauri/src/lib.rs` (start, stop, agent toggles, usage, open config, quit) | Add status, current route and quota in the menu. |
| 15.2 | App window with pages: Agents, Providers, Routing, Usage, Sessions, Library, Masking, Plugins, Gateway, Settings | `internal/gui/assets/index.html` (`data-view`); `docs/subsystems/gui-shell.md` | auxiliary | Missing | `apps/desktop/ui` (tray only) | Tauri window over the local API (row 12.3). Pages ship in impact order: Providers, Routing, Usage, Agents. |
| 15.3 | Terminal UI (`magpie tui`) | `internal/tui` | core | Missing | none | `ratatui` over the same local API. |
| 15.4 | Browser UI (`magpie web`, `--lan`) | `internal/cli/main.go:38`; `ref.md:2380` | auxiliary | Missing | none | Serve the same frontend as the desktop window, behind the gateway token. |
| 15.5 | Get started onboarding | `internal/gui/assets/i18n.js` ("Get started") | auxiliary | Missing | none | First-run wizard: add a provider, connect an agent, send a test request. |
| 15.6 | UI languages: English, Simplified Chinese, Traditional Chinese, Japanese, German | `internal/gui/assets/i18n.js` | auxiliary | Missing | none | Fluent (`.ftl`) message files, English by default. Translations live only in locale files. |
| 15.7 | README in several languages | `README.md`, `README.zh-CN.md` | core | Done | `README.md` (English), `README.zh-CN.md` | Keep, with English as the default. |
| 15.8 | Start at login | `internal/autostart` | auxiliary | Missing | none | `tauri-plugin-autostart`. |
| 15.9 | Keep the computer awake while agents work | `internal/awake` | auxiliary | Missing | none | `keepawake` crate, held only while requests are in flight. |
| 15.10 | Themes and fonts; Omarchy integration | `internal/gui/assets/theme.js`, `fonts.js`; `internal/omarchy` | auxiliary | Missing | none | Low priority. Follow the system theme first. |
| 15.11 | Windows Start menu shortcut | `internal/shortcut` | auxiliary | Missing | none | Handled by the installer. |

### 16. Distribution and operations

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 16.1 | Prebuilt binaries for macOS, Linux and Windows | `build/darwin`, `build/windows`; `ref.md:2322` | core | Done | `dist-workspace.toml`; `[profile.dist]` in `Cargo.toml`; `.github/workflows/release.yml`, `release-desktop.yml`, `release-dry-run.yml`, `ci.yml`; `docs/release.md` | Already better: checksums and optional signing. `dist build` uses profile `dist`. Desktop checksums come from the workspace target directory. A dry-run builds those artifacts without publishing. Add provenance attestation. |
| 16.2 | Self-update with `--proxy`, `--mirror` and auto-update | `internal/update`; `ref.md:2322` | auxiliary | Missing | none | Updater with signature checks (minisign) for the CLI and `tauri-plugin-updater` for the app. |
| 16.3 | Socket handoff on update, so the port never closes | `internal/handoff` | auxiliary | Missing | none | Pass the listening socket to the new process, then drain the old one. |
| 16.4 | Background service (systemd, launchd): `daemon install`, `status`, `restart` | `internal/daemon`; `ref.md:2382` | auxiliary | Missing | `crates/daemon` is a library, not a service installer | `hofund service install`, with unit files written atomically. |
| 16.5 | Docker image with HEALTHCHECK | `Dockerfile`; `ref.md:2455` | auxiliary | Missing | none | Distroless multi-arch image under 20 MB, running as non-root. |
| 16.6 | Termux and Android builds | `ref.md:2322` (`-tags nogui`) | auxiliary | Missing | none | Add an `aarch64-linux-android` target to cargo-dist. |
| 16.7 | Mirrors for GitHub and the model catalog | `internal/source`; `ref.md:2954` | auxiliary | Missing | none | A mirror base URL setting used by the updater and the catalog. |
| 16.8 | Agent CLI install helpers (`claude-code install`) | `internal/cli/main.go:38` | auxiliary | Missing | none | Low priority. |
| 16.9 | Daily anonymous usage ping (off with `DO_NOT_TRACK=1`) | `internal/stats`; `ref.md:3079` | — | Not planned | none | hofund sends no telemetry. That is a deliberate privacy improvement. |

## Top 10 gaps

Ranked by how much each one blocks real daily use. Model names per upstream
(row 2.13), `provider/model` and bare ids (row 1.20), reasoning effort
(row 1.14), and token counts (row 1.5) are Done in pull request #12 and are
not listed here. The v0.1 blockers on `main` (`ac6325a` #9, `695860d` #10)
are not gaps either: stream event order (rows 1.1 to 1.3), field leakage
across protocols (row 1.3), pre-existing keys restored on disconnect (row 5.2), token
rotation (row 10.3), `--help` and `examples/hofund.toml` (row 11.1), and
constant-time `x-api-key` (row 10.2).

1. **Usage cost and cache tokens (rows 9.2, 9.3, 9.5).** These are what users look at every day.
2. **Routing strategies, stickiness and cooldown (rows 4.5 to 4.7).** Prompt-cache stickiness
   and backoff decide both cost and reliability.
3. **Model catalog and live model lists (rows 2.8, 2.9, 2.14).** Preset endpoints for Chat, Responses, and Messages are in the catalog (row 2.7). Vertex credentials are in (row 2.4). Subscriptions and `provider test` are still open.
   Every routing, pricing and UI feature depends on these.
4. **Live model lists and provider test (rows 2.9 and 2.10).** Azure deployment listing (row 2.6) is Done and verified with mocks only. Vertex credentials (row 2.4), Gemini `generateContent` (row 2.3), and Bedrock Converse with SigV4 (row 2.5) were marked Done before this evidence rule. The host refreshes live lists and probes providers; deferred listing, endpoint-detection, and client test-as rows remain open.
5. **Named gateway keys, LAN and TLS (rows 10.4, 10.5, 10.7).** Constant-time `x-api-key` (row 10.2) is on `main`.
6. **Hot reload, CLI config editing and a local management API (rows 11.2, 11.5, 12.3).**
   The UI is blocked on these. `GET /health` (row 12.4) is only the status body.
7. **Subscriptions and quota windows (rows 3.1, 3.3, 3.8).** This is magpie's most used feature.
8. **More agent adapters, profiles and per-agent model setting (rows 5.1, 5.5, 5.6).**
9. **Several keys per provider (row 2.12).** Config holds one `api_key`. Keys should be router members so every strategy applies to them.
10. **Terminal UI (row 15.3).** It is stage 1. The first pull request is `docs/tui-design.md`. The UI is a client of the management API in item 6.

## Phase plan

The execution order is [docs/roadmap.md](roadmap.md). The table below is the original
audit grouping. Later owner decisions move the terminal UI (row 15.3) and every
subscription capability, including the Claude bridge (row 3.2), into stage 1.
A phase pull request updates the rows it touches.


| Phase | Scope (rows) | PRs | Outcome |
| --- | --- | --- | --- |
| 0 | v0.1 blockers: 1.1, 1.2, 1.3, 5.2, 10.3, 11.1, and `x-api-key` from 10.2 | #9 (`ac6325a`) and #10 (`695860d`), merged | Real agents work end to end through hofund. |
| 1 | Model name per upstream (2.13), `provider/model` and bare ids (1.20), effort mapping (1.14), `count_tokens` (1.5) | #12 | A route member names its upstream model. An ambiguous wire name is an error. Effort and counts are mapped. |
| 2 | Ledger schema v2: cache and reasoning tokens, agent, session id, time to first byte (9.1, 9.2, 9.5, 9.9) and a richer `usage` command (9.4) | 1 | Accurate usage. |
| 3 | Model catalog (2.8, 2.14, 2.15), provider presets as data (2.7), cost with stored prices (9.3), model metadata in `/v1/models` (1.6) | 2 | Cost in USD, plus presets and catalog data. |
| 4 | Routing tree: named and nested groups (4.3), `Strategy` trait with order, weight, round-robin and least used (4.5), session stickiness (4.6), circuit breaker and `Retry-After` (4.7), several keys per provider (2.12) | 2 | Load balancing and cache-aware routing. |
| 5 | Config hot reload (11.2), `tracing` with JSON logs and request ids (12.5), `/healthz` (12.4), local management API `/hofund/v1` (12.3), routing decision log (4.13) | 2 | Base for the UI and for operating hofund. |
| 6 | Vertex (2.4) and Bedrock Converse (2.5) were marked Done before the evidence rule. Azure deployment listing (2.6) is Done, verified with mocks only, and cites CI run 40178604119. Live model lists and provider test (2.9, 2.10) remain. Generative Language upstream (2.3) was marked Done before the evidence rule. | 2 | Wider provider coverage. |
| 7 | Named and hashed gateway keys with scopes and limits (10.4, 9.12), LAN allow-list (10.5), auth throttling (10.6), TLS with reload (10.7), outbound proxy (10.10) | 2 | Safe sharing. |
| 8 | Desktop window over the local API: Providers, Routing, Usage, Agents pages; onboarding; Fluent i18n; autostart (15.2, 15.5, 15.6, 15.8) | 2 | Usable without editing TOML. |
| 9 | Agent adapters as data, the next 10 agents, `agent set`, profiles, visible models (5.1, 5.3, 5.5, 5.6, 5.7), and importers (2.17) | 2 | Covers most of magpie's agent list. |
| 10 | `Subscription` trait, Codex and Copilot adapters, quota model, alerts (3.1, 3.3, 3.7, 3.8, 3.10), then quota-aware strategies (4.5) | 2 | Subscriptions as providers. |
| 11 | Self-update with signatures, background service, Docker image, socket handoff (16.2 to 16.5) | 2 | Easy to run as a service. |
| 12 | Routing rules and `route explain` (4.8), context-window move (4.10), refusal re-ask (4.11), quirk schema rules and tool repair (1.17, 1.19), OTLP (12.2) | 2 | Smarter routing and better compatibility. |
| 13 | Privacy masking IR pass with a streaming restore and fail-closed modes (13.1, 13.3); request inspection ring buffer (12.1) | 2 | Privacy. |
| 14 | WebAssembly plugin runtime with the built-in middleware as rules (8.1, 8.2, 8.3); web search backends and MCP server (7.1, 7.2) | 2 | Extensibility. |
| 15 | Library (6.1 to 6.3), encrypted backup (14.1), TUI (15.3), browser UI (15.4) | 2 | Wider scope. |
| 16 | Long tail that is still auxiliary after stage 1: sync (14.2), image and vision MCP (7.3, 7.4), WSL (5.9), local PII model (13.2), and the rest. Subscriptions, including the Claude bridge (3.2), are stage 1, not this phase. | several | Full parity. |
