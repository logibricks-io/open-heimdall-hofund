# Per-preset model lists and the connectivity probe

This is the plan for the code that follows E1 ([#59](https://github.com/logibricks-io/open-heimdall-hofund/pull/59)). This pull request changes no Rust. Preset status stays as recorded in [providers.md](../coverage/providers.md) until that code lands and its mocks pass.

Magpie is pinned at `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`. Listing is `internal/provider/models.go` `fetchOne` and `internal/catalog/live.go` `FetchAtListed`. Gemini listing is `internal/catalog/gemini_list.go`. The probe is `internal/provider/test.go` `tinyBody`, `AuthHeaders`, and `probe`. Refresh is `internal/provider/autolist.go`. Pull request #28 stays unmerged. It is not the trait. `UpstreamProvider::list_models` and `probe` already have defaults on `crates/plugin-api`.

## What moves a preset toward Done

A preset is done only when it has an upstream model list, a connectivity test, and every other gap in its coverage row is closed. The work below supplies the list and the probe. A remaining quirk (schema rewrite, balance, client headers, Decide) keeps the preset partial. The six presets `resolve` still rejects stay missing. Azure already lists deployments. It still needs this probe and this refresh, so it stays partial until both exist.

`GET /v1/models` and `GET /v1beta/models` keep listing route ids (feature-parity row 1.6). Magpie's gateway `/models` is the user's exposed picks. Hofund's route list is a different surface. Upstream lists are what `list_models` returns. A later pull request can show them in settings. These routes stay as they are.

The models.dev catalog (feature-parity row 2.8) is a kernel cache. A plugin does not fetch it. Live lists and that catalog stay separate.

## Listing

`list_models` asks every protocol base the preset speaks and merges by id. One list URL is fetched once. Image input on a duplicate id is kept when either side reported it. A base that fails keeps the models it listed last time, once a store exists. Until that store exists, a failed base contributes nothing and the bases that answered are the result. An empty successful page fails that URL (`no models listed`), and the next candidate URL is tried. When every URL fails, the error joins those failures and tells the caller to type the model ids in.

### URL order

OpenAI-compatible and Anthropic follow `listURLs` in `internal/catalog/live.go`:

1. `{base}/models`.
2. When the base is Anthropic, or the path has no version segment (`v` plus a digit: `v1`, `v1beta`, `v3`, `v4`), also `{base}/v1/models`.
3. For those same bases, strip one trailing suffix from `/anthropic`, `/apps/anthropic`, `/api/anthropic`, `/v1`, `/api`, `/api/v1`, then try `{root}/v1/models` and `{root}/models`.

A versioned OpenAI base (`…/v1`, `…/api/paas/v4`, `…/v1beta/openai`) is asked only at `{base}/models`. The first URL that returns a non-empty list wins for that base.

Auth on those lists, from `fetchPage`:

- `Authorization: Bearer` and `x-api-key`, both, when a key is set.
- Anthropic bases also send `anthropic-version: 2023-06-01`.
- A caller header overrides a default of the same name.

Official shapes:

- OpenAI lists with `GET /v1/models`, Bearer, and `{ "data": [ { "id" } ] }` (<https://platform.openai.com/docs/api-reference/models/list>).
- Anthropic lists with `GET /v1/models`, `x-api-key`, `anthropic-version: 2023-06-01`, `data[].id`, `has_more`, `last_id`, and a next page of `after_id` plus `limit` (<https://platform.claude.com/docs/en/api/models/list>).
- A compat body that uses `models` instead of `data` is read the same way. Magpie takes `data` when it is non-empty, and otherwise `models`. An object contributes `id`, then `name`.

Gemini native follows `internal/catalog/gemini_list.go` and Google's `models.list` (<https://ai.google.dev/api/models#method:-models.list>):

- `GET {base}/models?pageSize=1000` with `x-goog-api-key`.
- Pages follow `nextPageToken` as `pageToken`, at most 20 pages.
- Each id is `name` with a leading `models/` removed.
- A row whose `supportedGenerationMethods` is non-empty is kept when that array contains `generateContent`. A row with an empty methods array is kept.
- Each page has its own 8 second timeout.

OpenAI and Anthropic pages follow `Paged`. The first request is the URL as written. The whole list has an 8 second budget. A page with `has_more` repeats the URL with `after_id` set to `last_id` and `limit=1000`. The cap is 20 pages. A page that sets `has_more` without a new `last_id` fails the list (`only a page of the model list`). A partial list is a failure, so a later page is not mistaken for models the vendor dropped.

Azure stays `list_deployments` in `crates/provider/src/azure.rs`: `GET {root}/openai/deployments?api-version=2022-12-01`, then `GET {base}/openai/v1/models`, header `api-key`, pages of `has_more` and `last_id` (<https://learn.microsoft.com/en-us/rest/api/azureopenai/data-plane/deployments/list?view=rest-azureopenai-dataplane-2022-12-01>, <https://learn.microsoft.com/en-us/azure/foundry/openai/api-version-lifecycle>). That pager stays in the Azure function.

E1's page helper reads `next_page_token`, `nextPageToken`, `cursor`, and `next`. Gemini can use it for `nextPageToken`. Gemini ids come from `name`, so the Gemini reader strips `models/` itself. OpenAI, Anthropic, and Azure follow `has_more` and `last_id`. Each vendor keeps the pager its docs and magpie already use.

### Presets with a static list

`no_list = true`, with no region that sets `lists = true`, returns the preset's static `models` and makes no network call. `ListKind::Catalog`.

- `google-vertex` returns those ids filtered by `Vertex::models_for(location)`, so a location keeps the models that location serves (feature-parity row 2.4). Magpie does not call the publisher models list. This plan does the same. The probe still posts `:generateContent`.
- `bedrock` returns the static ids in `presets.toml`. Magpie treats the Bedrock runtime as a vendor with no list to ask. The control-plane `ListFoundationModels` call is left unused.
- `baidu-qianfan` and `volcengine` return static ids, except the `api` region, which sets `lists = true` and uses the live URL rules. Qianfan's pay-as-you-go root answers `/v2/models`. The token-plan paths answer nothing.

`ListKind` becomes `Live` for `openai-compat`, `anthropic`, `generative`, and Azure, and `Catalog` for Vertex and Bedrock. A factory that has not implemented a list keeps `Unsupported` and the default error `this upstream has no model list`.

### OpenRouter's Anthropic base

When the provider also has a chat or responses base, the Anthropic base on `openrouter.ai` is skipped. With `anthropic-version`, that host returns its catalog under an `anthropic/` prefix, and those ids are ones it does not serve (magpie, 2026-10-06). When that Anthropic base is the only base, it is asked, and a namespaced answer is an error that points the caller at `https://openrouter.ai/api/v1`.

### Refresh

The numbers are `autolist.go` at the pin (`MinRefreshEvery` line 39, `listRetry` line 231, `KeepListsFresh` line 239). The job is registered on `HostContext::scheduler`. The plugin does not spawn its own task.

- A blank interval, or `off`, `never`, `no`, `none`, `0`, or `-`, means no refresh.
- `Nd` is a number of days. Any other value is a Go duration. Shorter than 10 minutes is a config error.
- The floor is 10 minutes.
- A failed fetch retries after 15 minutes, or after the provider's own interval when that interval is shorter.
- The loop starts 30 seconds after process start, then every 1 minute. One provider runs at a time. One fetch times out at 1 minute.
- Refresh runs for the user's own provider. Accounts, plugins, and decide-only providers are left idle.
- A `no_list` preset with no listing region is left idle.
- Auto-include: a rule is a glob (`*`, case-insensitive, the whole id) or `re:` plus a regular expression matched against the whole id. A match adds a model the new list has and the previous list lacked. A model the user left unticked stays unticked. A model the previous list had and the new list lacks leaves the picks. A hand-typed id that was never in a fetched list stays. A fetch that fails, or that returns no model, changes nothing.

The picks store and auto-include are the second code pull request. The first implements the fetch and returns the ids. It rejects an interval under 10 minutes. The background loop starts when the picks store exists, so a refresh cannot drop a pick it cannot name.

## Probe

One request for each protocol the preset speaks, among chat, responses, anthropic, and gemini. The model is the one the caller passes. When the caller passes none, the error is `no model to try: expose one, or refresh the model list`. `ProbeQuery::new` still rejects an empty string with `probe model is empty`.

The body is magpie's `tinyBody`. The output cap is 16 tokens. The wait is 20 seconds (`testWait`). `Content-Type` is `application/json`. The call uses the pooled host client, so `HttpLimits` apply inside that 20 second wait. The body is posted as these bytes. `ProbeQuery::minimal_request` is an uncapped IR `hi`, and the chat codec's field name differs for Azure and Bedrock, so the probe owns the JSON below.

| Protocol | URL | Body |
| --- | --- | --- |
| Chat, other than Azure and Bedrock | `{chat}/chat/completions` | `model`, one user message `hi`, `max_tokens` 16 |
| Chat on Azure or Bedrock | `{chat}/chat/completions`, with Mantle's model prefix from magpie's `mantleBase` | `max_completion_tokens` 16 |
| Responses | `{responses}/responses` | `input` of one user `input_text` `hi`, `max_output_tokens` 16 |
| Anthropic | `{anthropic}/v1/messages` | `max_tokens` 16, one user message `hi`, plus `anthropic-version: 2023-06-01` |
| Gemini on Vertex | `{base}/publishers/google/models/{model}:generateContent` | Unary. `maxOutputTokens` 16. Thinking follows magpie's `vertexTest`: Gemini 3 and later send `thinkingLevel` `low`; `gemini-2.5-pro` sends `thinkingBudget` 128; other `gemini-2.5` ids send `thinkingBudget` 0; earlier generations omit thinking. The credential is the Google access token. |
| Gemini native | `{gemini}/models/{model}:generateContent` | `generationConfig.maxOutputTokens` 16, header `x-goog-api-key` |
| Factory Gemini | later | Factory is a sign-in surface (row H). |

`vertexTest` is unary `:generateContent` (`VertexWholePath`). The gateway's `:streamGenerateContent` path is a different call.

Auth follows `AuthHeaders` (`internal/provider/test.go` line 457):

- Vertex, and any provider with an empty key, sends no API-key header. Vertex sends the Google access token from its mint.
- Azure on Anthropic sends `x-api-key`. Every other Azure call sends `api-key`.
- Native Gemini sends `x-goog-api-key`.
- Anthropic on a host ending in `anthropic.com`, and Bedrock's Anthropic route, send `x-api-key` alone.
- Every other Anthropic-compatible route sends both `x-api-key` and `Authorization: Bearer`, until that preset records a single header.
- Every remaining route sends `Authorization: Bearer`.

A 2xx unary body is success. A 2xx body that contains `"stream":true` is read until the first text, thought, or tool, or until an error event, and at most 1 MiB. A stream that ends with none of those still counts as answered. The stored snippet is at most `PROBE_CAP` (4096 bytes). A non-2xx response keeps at most 4096 bytes of the body in the error. The key is absent from that error. The first code slice sends unary bodies. The stream reader is covered by a canned SSE fixture, and Cline and Zen use it when their presets land.

### Carried by the preset that needs them

These stay out of the first code pull request. Each one is required for that host to answer. The row E pull request that moves the preset carries it. A generic probe that omitted the header would record a false failure.

- `RetryOneAuth` for Anthropic-compatible vendors that refuse one of the two key headers. The first request still sends both, as `AuthHeaders` does.
- `RetryZhipuCoding` and the Zhipu success envelope, for Zhipu and Z.ai.
- Cline, Kilo, and OpenCode client headers, Cline's streamed probe with a 1024-token cap, and Zen's free-tier body. Those hosts answer 403, or an empty body, without them.
- Image probes (`tinyDrawing`, 2 minute cap) and Decide probes. The gateway does not draw or decide yet. An image-only id is not sent a chat `hi`.
- Account translation for Claude, Cursor, and the other sign-in APIs. Those are subscription plugins (row H).

## Tests the code pull request ships

Mocks only. No live call to Google, AWS, or a vendor.

- An OpenAI `{base}/models` page of two ids is returned. A page with `has_more` is followed with `after_id`. A twenty-first page fails the list.
- An Anthropic list sends `anthropic-version` and `x-api-key`. A probe error for a 401 contains the status and the capped vendor message, and contains no key.
- A Gemini list keeps a `generateContent` model, drops an embedding, and follows `nextPageToken`.
- Azure still uses `list_deployments`.
- Vertex and Bedrock return the static catalog and do not GET `/models`.
- A Vertex probe posts `:generateContent` with `thinkingLevel` `low` for a Gemini 3 id, carries the mock Google token, and sends no API-key header.
- A Bedrock chat probe sends `max_completion_tokens`. Bedrock's Anthropic route sends `x-api-key` alone.
- An unknown option still fails in `check` before a list or a probe runs.
- A probe with no model returns `no model to try: expose one, or refresh the model list` and does not call the network.

## Code pull requests

1. `list_models` and `probe` on `openai-compat`, `anthropic`, `generative`, Azure, Vertex, and Bedrock. `ListKind` as above. The mock tests in this note. The scheduler rejects an interval under 10 minutes and does not start the loop.
2. The picks store, the refresh loop, auto-include, and removal of models the vendor no longer lists.
3. The per-preset extras in the table above, each inside the pull request that moves that preset.

#28 rebases onto these methods after the first of those pull requests. It stays unmerged until then. It does not define the trait.
