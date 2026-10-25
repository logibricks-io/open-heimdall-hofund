# Per-preset model lists and the connectivity probe

This is the plan for the code that follows E1 ([#59](https://github.com/logibricks-io/open-heimdall-hofund/pull/59)). Magpie is pinned at `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`. Listing is `internal/provider/models.go` and `internal/catalog/live.go`. Gemini listing is `internal/catalog/gemini_list.go`. The probe is `internal/provider/test.go`. Refresh is `internal/provider/autolist.go`. Pull request #28 stays unmerged. It is not the trait.

This document targets the page helpers #59 adds (`next_cursor`, `string_items`, and `Call::json`). It does not import them. The first code pull request waits until #59 is on `main`. This pull request may merge first.

Preset status in [providers.md](../coverage/providers.md) stays as it is until a preset meets the acceptance criteria below. Feature-parity rows 2.9 and 2.10 stay Missing until that code lands.

## Two layers

Hofund builds one plugin instance per configured provider, and that instance has one wire (`crates/daemon/src/lib.rs` builds `planned.wire` and calls `build_plugin` once). `openai-compat`, `anthropic`, and `generative` are separate factories. A factory's static `ListKind` stays `Unsupported`. The host does not read it.

`UpstreamProvider::list_models` lists that one wire. It does not walk the preset's other bases, skip OpenRouter, or keep a side from a previous fetch.

Preset-level behaviour lives in a host-side models aggregator, keyed by preset id. The aggregator asks every base the preset speaks, one `list_models` call per wire, and merges the rows. The list kind is computed there from the preset row and the region the instance sits on. It is not a property of the factory.

| Kind | When |
| --- | --- |
| Catalog | The preset sets `no_list`, and the matched region does not set `lists`. `google-vertex` and `bedrock` are this kind, including when `bedrock` is built on the chat wire through `openai-compat`. The static `models` array is the list. No `GET /models`. Vertex then keeps the ids `Vertex::models_for(location)` allows. Bedrock does not call control-plane `ListFoundationModels`. |
| Deployments | Preset `azure`. See the Azure section. |
| Live | Every other preset, including `bedrock-mantle`. Also a `no_list` preset whose matched region sets `lists` (`baidu-qianfan` and `volcengine`, region `api`), and a `no_list` preset whose `models_url` is set. |

Region match is by path, as `listRegion` and `atRegion` do (`models.go` lines 974-994). The host may differ. `bedrock-mantle` is live: its preset comment says the runtime has no list and Mantle lists at `/v1/models` (`presets.go` lines 395-402). A chat or responses call still uses `MantleBase` (`provider.go` lines 1698-1708): a model is asked at `/v1` or `/openai/v1`, and the other prefix is retried when the first is refused. The list itself is asked at `/v1/models`. `/openai/v1/models` is not a list.

`GET /v1/models` and `GET /v1beta/models` on the gateway keep listing route ids (feature-parity row 1.6). Magpie's gateway `/models` is the user's exposed picks. Those are different surfaces. The models.dev catalog (row 2.8) stays a kernel cache. A plugin does not fetch it.

## What one wire's `list_models` does

The plugin asks only the base it was built with.

OpenAI-compatible and Anthropic URL order is `listURLs` (`live.go` lines 353-376):

1. `{base}/models`.
2. When the base is Anthropic, or the path has no version segment (`v` plus a digit: `v1`, `v1beta`, `v3`, `v4`), also `{base}/v1/models`.
3. For those same bases, walk the suffixes `/anthropic`, `/apps/anthropic`, `/api/anthropic`, `/v1`, `/api`, `/api/v1` and strip each one that is currently the end of the path. Several can come off. A base ending in `/api/v1` loses `/v1` and then `/api`, and the root is what remains. Then try `{root}/v1/models` and `{root}/models`.

A versioned OpenAI base (`…/v1`, `…/api/paas/v4`, `…/v1beta/openai`) is asked only at `{base}/models`. The first URL that returns a non-empty list wins for that base. An empty successful page fails that URL (`no models listed`). When every URL fails, the errors are joined.

Auth on those lists, from `fetchPage`:

- `Authorization: Bearer` and `x-api-key`, both, when a key is set.
- Anthropic bases also send `anthropic-version: 2023-06-01`. Anthropic still accepts that value. A newer version string is not what this client sends.
- A caller header overrides a default of the same name.

If the response is 401 or 403 and the body refuses both headers, the page is asked again with `x-api-key` alone and then with `Bearer` alone (`live.go` lines 530-541). The winner is kept for the rest of that list. This is listing behaviour, not a per-preset extra.

Official shapes:

- OpenAI lists with `GET /v1/models`, Bearer, and `{ "object", "data": [ { "id" } ] }` (<https://platform.openai.com/docs/api-reference/models/list>). That reference documents no paging. Every OpenAI-compatible body still goes through the pager below. A page that does not set `has_more` is the whole list.
- Anthropic lists with `GET /v1/models`, `x-api-key`, `anthropic-version: 2023-06-01`, `data[].id`, `has_more`, `first_id`, and `last_id`. The next page uses `after_id` and `limit`. `limit` is 1 to 1000 and defaults to 20. `before_id` exists on the official API and is not what the pager sends (<https://platform.claude.com/docs/en/api/models/list>).
- A compat body that uses `models` instead of `data` is read the same way. `data` wins when it is non-empty. An object contributes `id`, then `name`.

The OpenAI and Anthropic pager is `Paged` (`live.go` lines 459-489). The first request is the URL as written. The whole list has an 8 second budget. A response body is capped at 8 MiB. A page with `has_more` repeats the URL with `after_id` set to `last_id` and `limit=1000`. At most 20 pages are read. When the 20th page still sets `has_more`, the list fails and no 21st request is sent. A page that sets `has_more` without a new `last_id` fails the list. A partial list is a failure, so a later page is not mistaken for models the vendor dropped. A JSON error message is cut at 160 runes (`live.go` lines 637-662).

Gemini native follows `gemini_list.go` and `models.list` (<https://ai.google.dev/api/models#method:-models.list>):

- `GET {base}/models?pageSize=1000` with `x-goog-api-key`.
- Pages follow `nextPageToken` as `pageToken`, at most 20 pages. Each page has its own 8 second timeout.
- When the 20th page still has a token, the rows already read are returned and the call does not fail. That differs from `Paged`.
- An empty list is an error (`no models listed`).
- Each id is `name` with a leading `models/` removed.
- A row whose `supportedGenerationMethods` is non-empty is kept when that array contains `generateContent`. A row with an empty methods array is kept. Embedding (`embedContent`), Imagen (`predict`), and Veo (`predictLongRunning`) are dropped.

#59's `next_cursor` reads `next_page_token`, `nextPageToken`, `cursor`, and `next`. The Gemini walker can use it for `nextPageToken`. Gemini ids come from `name`, so the reader strips `models/` itself. `string_items` keeps objects with `id`. OpenAI and Anthropic pages use `has_more` and `last_id`, which that helper does not read. The code pull request adds an `after_id` pager and the Gemini walker to `hofund-plugin-sdk`, beside #59's helpers. Azure keeps the pager it already has.

### Azure deployments

`list_deployments` stays: `GET {root}/deployments?api-version=2022-12-01`, then `GET {base}/models`, header `api-key`. `root` is the v1 base with a trailing `/v1` removed. That is the same pair `azureModels` asks (`azure.go` lines 130-161).

Microsoft's page for the 2022-12-01 data-plane deployments list now returns 404. The replacement Microsoft documents is the management-plane deployments API, which needs ARM and Entra ID. Magpie still calls the data-plane URL, and field reports say a resource key still gets an answer. This plan keeps that call and records the risk: it can stop answering without a docs page to follow. The second URL stays. For the usual Azure base that ends in `/openai/v1`, that URL is `/openai/v1/models`. It lists base models. It lists deployment names only when the resource owner named each deployment after its model. The id a chat call must send is the deployment name, so a fallback hit is reported as base models, not as deployments. When both URLs fail, the message ends with `type your deployments' names in as model ids`, which is Azure's own suffix, not the generic one below.

### Rows that are not chat models

`fetchPage` (`live.go` lines 583-587) drops a row unless it is tagged as a drawing, a film, or a decision, when `textModel` rejects the id. On that path `textModel` is called with the id alone (`catalog.go` lines 1448-1467), so the drop is the substring test:

- The id, as the vendor spelled it, is dropped when it contains any of `embed`, `-tts`, `image`, `audio`, `-live`, `robotics`, `computer-use`, `deep-research`, `transcribe`, `realtime`, `moderation`, `whisper`, `dall-e`, or `sora`. The check does not lowercase the id first. Embedding (`embed`), TTS (`-tts`), Whisper, Sora, realtime, and moderation are in this list. `tts-1` does not contain `-tts`, so the substring test does not drop it.

`textModel` also drops a models.dev catalog row whose output modalities are present and contain no `text` (`catalog.go` line 931). A plugin does not fetch models.dev, and a live page does not pass modalities into `textModel`, so a live row is not dropped only because its output modalities omit `text`. Those modalities are still stored on the row.

Tagging, still recorded when the probe cannot use the row yet:

- Decision: the row's kind is `decision`.
- Film: the row's kind is `video`.
- Drawing: the row is not a film or a decision, and either its kind is `image` or `DrawsID` matches and the id does not contain `deep-research`. `DrawsID` lowercases the id and looks for `image`, `imagen`, `imagine`, `dall-e`, `flux`, `seedream`, `cogview`, `stable-diffusion`, `sdxl`, `wanx`, `kolors`, `hidream`, or `banana`.

A tagged drawing, film, or decision stays on the result and is marked. The chat-facing list omits it, and auto-include does not add it to chat picks. An id that fails the substring test and is not tagged is absent from both. `text-embedding-3-small` contains `embed` and is not tagged, so it is absent. `gpt-4o-mini-tts` contains `-tts` and is absent the same way. `dall-e-3` and `gpt-image-2` match `DrawsID`, so they are drawings, not chat models. An OpenAI-compatible page of `gpt-5.5`, `gpt-image-2`, `flux-kontext-pro`, and `text-embedding-3-small` yields the chat id `gpt-5.5` and the drawings `gpt-image-2` and `flux-kontext-pro`. The same filter runs on Anthropic pages. Gemini's `generateContent` filter is in addition to this one.

### Metadata one row carries

Today `ModelSpec` is an id and an optional display name. The code pull request extends it. `list_models` returns these fields, empty when the page did not send them:

| Field | Source on an OpenAI-compatible or Anthropic page |
| --- | --- |
| `id` | `id`, else `name` |
| `display_name` | `magpie_label`, else `display_name`, else `name` when that name does not contain `": "`, else the id |
| `plain` | `display_name`, only when `magpie_label` is set and the two differ |
| `context` | `context_length`, when a positive number |
| `max_output` | `max_output_tokens`, when a positive number |
| `input_modalities`, `output_modalities` | `modalities.input` and `modalities.output` |
| `image_input` | `true` or `false` when input modalities were present and include `image` or not. Absent modalities stay unknown, not `false` |
| `endpoints` | `native_endpoints` when that list is non-empty, else `supported_endpoints`, else `type_target` |
| `draws`, `films`, `decides` | the tags above |
| `efforts` | `supported_reasoning_levels` |
| `web_search` | `web_search`, only when the value is `native` or `magpie` |
| `service_tiers` | `service_tiers` |
| `price` | `magpie_price`: `input`, `output`, `cache_read`, `cache_write`, `cache_write_1h`, and `tiers`. Kept when the object is present and sane: no part is NaN, below zero, or above 1e6, and each tier's `above` is positive |

Gemini rows fill `id`, `display_name`, `context` (`inputTokenLimit`), and `max_output` (`outputTokenLimit`). Price and endpoints stay empty. Rows 1.6 and 2.8 read `context` and `max_output` from this record. They do not fetch models.dev from the plugin.

## What the aggregator adds

The aggregator calls `list_models` once per base the preset speaks, in protocol order, and merges by id.

- The same list URL, asked with the same auth, is fetched once.
- A duplicate id keeps one row. Image input follows `sharedImageInput` (`models.go` lines 1331-1344). An explicit text-only answer from either base wins. Image input is kept only when both bases report it. If either side left it unknown, the merged value stays unknown, unless the other side is explicitly text-only. `anyImageInput` (`models.go` lines 1346-1350), where either side having image input is enough, is the rule for groups. It is not the rule for two bases of one preset. A mock where one base says text-only and the other says image expects text-only.
- OpenRouter's Anthropic base is skipped when the provider also has a chat or responses base (`models.go` lines 754-763). With `anthropic-version`, that host returns its catalog under an `anthropic/` prefix. When that Anthropic base is the only base, it is asked, and a namespaced answer is an error that names `https://openrouter.ai/api/v1`.
- A base that fails keeps the models it listed last time, and only those (`keepDeadSides`, `models.go` lines 927-968), once the picks store exists. Until that store exists, a failed base contributes nothing.
- `fixV1` (`models.go` lines 1138-1167): when the list was found only at `{base}/v1/models`, the base has no version segment, and the protocol is chat or responses, the stored base gains `/v1`. A versioned base is left as written. The aggregator returns the corrected base. The refresh job writes it back onto the provider. A request to the unfixed base would miss the models the list just found.

`models_url`, when the user set it, is the only URL asked (`models.go` lines 648-660). It also makes a `no_list` preset list live (`lists`, lines 170-176). The value is an option the aggregator reads. Factories do not grow a schema key for it. A query string on that URL is redacted in every error.

`planModels` (`models.go` lines 1038-1091) runs in the aggregator after the merge:

- A region's `models` replace the preset's `models` when the instance sits on that region.
- Vertex replaces them with `vertexModels(location)`, which hofund already implements as `Vertex::models_for`.
- When the preset sets `only`, live ids are kept only when they start with that prefix. `clinepass` sets `only = "cline-pass/"`. When the filtered list is empty, the static ids are the list.
- A `no_list` preset with a static `models` array and no live rows returns those ids.
- Cline's extra free-model list is part of the Cline preset pull request, not this filter.

Per-key listing (`fetchPerKey`, `models.go` line 1174) waits for the pull request that gives one provider more than one key. Until then the single `api_key` is the whole list, and the probe uses that key.

When every base fails, the joined message ends with `type its model ids in by hand, or give the URL its list is at` (`models.go` line 807).

## Refresh

List refresh is one host-side job. Plugins do not register it. That refines RFC 0001's "plugins register jobs" for this loop: `SchedulerBackend::register` is a fixed interval and has no due time, and a job per instance cannot keep "one provider at a time" or the retry clock. The plugin implements `list_models`. The host owns the sweep.

The job follows `KeepListsFresh` (`autolist.go` lines 239-290):

- It starts 30 seconds after process start, then ticks every 1 minute.
- Each tick walks providers one after another.
- A provider with a blank interval, or `off`, `never`, `no`, `none`, `0`, or `-`, is skipped. `Nd` is a number of days. Any other value is a Go duration. Shorter than 10 minutes is a config error (`MinRefreshEvery`, line 39).
- The job skips a disabled provider, an account, a plugin-backed provider, a decide-only provider, and a preset that `lists()` says has nothing to refresh (`ownList`, lines 83-85; the `Off` check at line 260).
- Due is computed from the stored fetch time. A list younger than the provider's interval is left alone.
- A failed fetch records the try and waits `min(interval, 15 minutes)` (`listRetry`, line 231). Magpie's sweep uses only that retry. This job also honours a 429: when the response status is 429 or the response carries `Retry-After`, the next try waits at least that delay, and still at least 10 minutes.
- One fetch is capped at 1 minute.
- The job holds, per provider: last success time, last try time, the picks, the delisted ids, and the per-base sides.

Auto-include runs inside this job. A rule is a glob (`*`, the whole id, case-insensitive) or `re:` plus a regular expression matched against the whole id. A `re:` rule is case-insensitive unless the expression contains `(?-i)` (`compilePattern`, `groupmatch.go` lines 38-55). A match adds a model the new list has and the previous list lacked. A model the user left unticked stays unticked. Rules only add.

A model the previous list had and the new list lacks leaves the picks and is stored as delisted (`models.go` lines 301-358). When a later list contains it again, it returns to the picks. A hand-typed id that was never in a fetched list stays. A fetch that fails, or that returns no model, changes nothing.

## Probe

Probes run only when the user asks. The refresh job never probes. One provider speaks up to four protocols, and each probe asks for 16 output tokens, so one test can be four paid calls. A test of many models runs at most four at a time (`TestModels`, `test.go` line 245).

The host selects the model. `probe` on the plugin sends the one request the host named. The host does what `Provider.Test` does (`test.go` lines 37-94 and 405-442):

1. Refresh the list first.
2. Pick a model per protocol: the first exposed model the key can see, else the first listed model it can see. On Anthropic, prefer an id that starts with `claude`. Skip a disabled id. Skip an id `ImagesAPI` matches (`catalog.go` lines 1502-1509): the lowercased id contains `gpt-image`, `chatgpt-image`, `dall-e`, `imagen`, `imagine`, `qwen-image`, `wanx`, `wan2`, `seedream`, `cogview`, `flux`, `stable-diffusion`, `sdxl`, `kolors`, or `hidream`. A chat model whose id merely ends in `-image`, such as `gemini-2.5-flash-image`, is not in that list. OpenCode's public key tries only a free model, on chat unless the id is Claude.
3. Map the id with `UpstreamName` before it is written into the body (`test.go` line 79).
4. Call the plugin `probe` with that protocol and that upstream name.

When no model can be chosen, the error is `no model to try: expose one, or refresh the model list`, and nothing is sent. `ProbeQuery::new` still rejects an empty string with `probe model is empty`.

The body is `tinyBody` (`test.go` lines 184-206), then `tested` (`extrabody.go` lines 205-207): the preset's extra body is merged, on a session id of the probe's own, and `Prepare` runs. The extra-body hook is part of the probe code pull request. Preset-specific fields land with that preset. A preset whose probe is wrong without those fields is not accepted as probed until the fields exist. The wait is 20 seconds. `Content-Type` is `application/json`. The call uses the pooled host client, so `HttpLimits` apply inside that wait.

| Protocol | URL | Body |
| --- | --- | --- |
| Chat, other than Azure and Bedrock | `{chat}/chat/completions` | `model`, one user message `hi`, `max_tokens` 16 |
| Chat on Azure or Bedrock | `{chat}/chat/completions`. Mantle uses `MantleBase` for this model | `max_completion_tokens` 16 |
| Responses | `{responses}/responses`. Mantle uses `MantleBase` here too (`tinyBody` line 191) | `input` of one user `input_text` `hi`, `max_output_tokens` 16 |
| Anthropic | `{anthropic}/v1/messages` | `max_tokens` 16, one user message `hi`, plus `anthropic-version: 2023-06-01` |
| Gemini on Vertex | `{base}/publishers/google/models/{model}:generateContent` | Unary. `maxOutputTokens` 16. Thinking follows `vertexTest` (`vertex.go` lines 189-201): Gemini 3 and later send `thinkingLevel` `low`; `gemini-2.5-pro` sends `thinkingBudget` 128; other `gemini-2.5` ids send `thinkingBudget` 0; a generation below 3, including an id with no generation number such as `gemini-flash-latest`, omits thinking. The credential is the Google access token. |
| Gemini native | `{gemini}/models/{model}:generateContent` | `generationConfig.maxOutputTokens` 16, header `x-goog-api-key` |
| Factory Gemini | a later sign-in pull request | Factory is row H. |

`vertexTest` is unary `:generateContent`. The gateway's `:streamGenerateContent` path is a different call.

Auth follows `AuthHeaders` (`test.go` lines 457-488), unless listing already learned a single header for that host:

- Vertex, and any provider with an empty key, sends no API-key header. Vertex sends the Google access token from its mint.
- Azure on Anthropic sends `x-api-key`. Every other Azure call sends `api-key`.
- Native Gemini sends `x-goog-api-key`.
- Anthropic on a host ending in `anthropic.com`, and Bedrock's Anthropic route, send `x-api-key` alone.
- Every other Anthropic-compatible route sends both `x-api-key` and `Authorization: Bearer`.
- Every remaining route sends `Authorization: Bearer`.

A 2xx unary body is success. `streams` (`test.go` lines 573-578) reads the request body. A request whose JSON has `stream: true` is read until the first text, thought, or tool, or until an error event, and at most 1 MiB. A stream that ends with none of those still counts as answered. The stored snippet is at most `PROBE_CAP` (4096 bytes). A non-2xx response reads at most 4096 bytes, and the message shown from that body is cut at 300 runes (`apiError`, lines 826-828). The probe does not retry a 429.

The kernel wraps `list_models` and `probe` in the spans RFC 0001 describes, and redacts before a span field or a captured body is stored. The probe posts bytes through the pooled client, so that wrap sits at the kernel call site. Those spans are not on the daemon path today (RFC line 879). The code pull request adds them. Feature-parity row 2.10 also asks that a probe be logged like any other request. The host log uses the same two kinds magpie writes, `list` and `test` (`ownRequests`, `test.go` lines 492-498).

`kits/upstream` is the conformance suite for a live list's paging and for one minimal probe (RFC line 798). The code pull request extends that kit. A unit mock does not replace it.

## Errors and secrets

Per-URL failures are joined into one `PluginError`. Mapping:

| Failure | Kind | Status | `retry_after` |
| --- | --- | --- | --- |
| DNS, TCP, or TLS before the request is sent | `Connect` | none | none |
| Connect or idle-read deadline | `Timeout` | none | none |
| The connection failed after the request was sent | `Transport` | none | none |
| An HTTP status, including 401, 404, and 429 | `Status` | the status | `Retry-After` when the response sent one |
| A body that is not a model list or a completion | `Decode` | none | none |
| A catalog preset asked to call the network, or a wire with no list method | `Unsupported` | none | none |

The CLI prints `PluginError`'s message. Values are removed before the message is built:

- `Authorization`, `api-key`, `x-api-key`, `x-goog-api-key`, and every other header value.
- The query string of a URL, so a `models_url` of `https://vendor.example/v1/models?key=SECRET` becomes a path with the query redacted.
- A response body that echoes the key. The test for this is a 401 whose body contains the key. The error text must not contain the key. Stripping a header the client added is not enough, because the vendor sent the key back.

## Deferred, each with a pull request

These stay out of the first code pull request. Each one is required for that host to answer, and the pull request named here carries it.

| Behaviour | Pull request |
| --- | --- |
| Per-key lists and a probe key chosen from the keys that see the model | The pull request that stores more than one key on a provider |
| Cline, Kilo, and OpenCode client headers, Cline's streamed probe with a 1024-token cap, Zen's free-tier body, and Cline's free-model append | The pull request that moves that preset |
| `RetryZhipuCoding` and the Zhipu success envelope | The Zhipu and Z.ai preset pull request |
| Image probes (`tinyDrawing`, 2 minute cap) and Decide probes | The pull request that adds drawing or Decide. An image-only id is not sent a chat `hi` |
| Account translation for Claude, Cursor, and the other sign-in APIs | Row H |
| Factory Gemini's `/generate` probe | Row H |

Auth-header learning on a list is not in this table. It is part of the first code pull request, as specified above.

## Acceptance before a preset is Done

A preset stays partial until all of the following are true. Group presets that share a URL shape so one mock covers the group. The coverage row is updated with the CI run id in the same pull request that claims the preset.

1. A mock list test hits the preset's real base path and the URL candidates, including the `/v1` and root fallbacks. A static preset has a test that no network call is made.
2. A mock probe for each protocol the preset speaks, with the auth headers this note names, and with Mantle's prefix when the preset is `bedrock-mantle`.
3. The preset's other quirks in [providers.md](../coverage/providers.md) are closed.
4. The coverage row says done and cites the CI run.

Azure stays partial until the probe, the refresh job, and this acceptance list are done. The six presets `resolve` still rejects stay missing until `resolve` accepts them.

## Code pull requests

1. After #59 is on `main`: the `after_id` pager and the Gemini walker in `hofund-plugin-sdk`. `list_models` and `probe` on one wire for `openai-compat`, `anthropic`, `generative`, Azure, Vertex, and Bedrock. The host aggregator: kind from the preset and region, the merge rule, the non-text filter, the metadata fields, OpenRouter's skip, `models_url`, auth-header learning, `only` and region models, and `fixV1`. The host probe selector. Kernel spans, redaction, and the `list` and `test` log kinds. Conformance in `kits/upstream`. Mocks for the text-only merge, the embedding filter, the 20th page, the Gemini partial page, the 401 body that echoes a key, a static preset that does not dial out, and `bedrock-mantle` listing live while `bedrock` does not. The scheduler does not start.
2. The host refresh job, the picks store, delist and restore, auto-include, dead sides, and the 429 delay.
3. The rows in the deferred table, each inside the pull request that moves that preset or adds that capability.

#28 rebases onto `list_models` and `probe` after the first of those pull requests. It stays unmerged until then. It does not define the trait.
