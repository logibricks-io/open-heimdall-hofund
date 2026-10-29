# Per-preset model lists and the connectivity probe

This is the plan for the code that follows E1 ([#59](https://github.com/logibricks-io/open-heimdall-hofund/pull/59), merged to `main` as `2277360`). Magpie is pinned at `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`. Listing is `internal/provider/models.go` and `internal/catalog/live.go`. Gemini listing is `internal/catalog/gemini_list.go`. The probe is `internal/provider/test.go`. Refresh is `internal/provider/autolist.go`. Pull request #28 stays unmerged. It is not the trait.

The code pull requests use the page helpers #59 added (`next_cursor` and `string_items` in `crates/plugin-sdk/src/page.rs` lines 43 and 65, and `Call::json` in `crates/plugin-sdk/src/transport.rs` line 186). This document does not change them.

Preset status in [providers.md](../coverage/providers.md) stays as it is until a preset meets the acceptance criteria below. Feature-parity rows 2.9 and 2.10 are Partial for the landed subset and retain their remaining work.

## Two layers

One configured `[[providers]]` entry has an `id`, `preset`, `protocol`, `base_url`, `api_key`, `headers`, and `options` (`crates/config/src/lib.rs` lines 121-149). Hofund builds exactly one plugin instance for it, on `planned.wire` (`crates/daemon/src/lib.rs` lines 259-306 and 315-344). `openai-compat`, `anthropic`, `generative`, and `bedrock` (Bedrock Converse) are separate kernel factories (`plugins/registry/src/builtin.rs`). The first three specs say `ListKind::Live`; Bedrock says `Catalog`. Since E9b, preset `google-vertex` is its own plugin, `google-vertex` (`plugins/providers/google-vertex`), whose spec says `Catalog`. A factory spec's `ListKind` is an upper bound on what the plugin can do, and the conformance kit keys on it (RFC 0001 lines 313 and 798). Code pull request 1 sets those values and adds `Deployments` to the non-exhaustive `ListKind` enum in `crates/plugin-api/src/upstream.rs` for Azure. Since PR E8c, preset `azure` is its own plugin, `azure` (`plugins/providers/azure`), whose spec also says `Live`.

The explicit capability rank is `Unsupported < Catalog < Deployments < Live`. Code pull request 1 implements `ListKind::rank()`; it must not derive `Ord`, because the current declaration order `Live`, `Catalog`, `Unsupported` is the reverse (`crates/plugin-api/src/upstream.rs` lines 84-94). `Deployments` dials the vendor but returns only the account's deployment names, so it is narrower than a general live list. An Azure Chat or Responses instance on the `azure` spec (`Live`) therefore resolves to `Deployments` without exceeding its bound, and an Anthropic-wire one stays `Live`. The host resolves each instance's kind from its preset, region, and options. A preset that resolves to `Catalog` is not dialled even when the spec says `Live`.

`ListKind` is `#[non_exhaustive]`, so adding `Deployments` is an additive plugin-api change. Plugin and host `rank()` matches must treat an unknown future variant as `Unsupported`. `hofund-plugin-api` has no version of its own; it uses the shared workspace version (`Cargo.toml` lines 31-36; `crates/plugin-api/Cargo.toml` lines 1-8). RFC 0001's semver policy asks for a reviewed public-API snapshot diff, not a version bump, and stage 1 has no out-of-tree plugins (RFC 0001 lines 129-134). Code pull request 1 includes that snapshot diff and a `CHANGELOG.md` line.

Since #59, Vertex and Bedrock Converse are registry factories (`plugins/registry/src/builtin.rs` lines 27-36 and 202-300), so the earlier precondition that they move behind a factory is met. Their `list_models` returns the compiled static catalog with no network call. Since E9b, Vertex is the `google-vertex` plugin, whose `list_models` returns the ids its location serves.

`UpstreamProvider::list_models` lists that one wire at that instance's base. It does not walk the preset's other bases, skip OpenRouter, or keep a side from a previous fetch. A plugin stays encapsulated behind its own base.

Preset-level behaviour lives in a host-side models aggregator, keyed by the configured provider id. It never merges two configured providers, even when they share a preset and have different keys. The list kind is computed for each configured instance from its preset row, resolved region, and options. It is not a property of the factory.

Which bases to ask, how to merge them, OpenRouter's skip, `fixV1`, and dead sides are host policy. Compiled `presets.toml` data drives that policy. This is RFC 0001's kernel-owns-mechanism split: the plugin lists its base, and the host composes the results.

| Kind | When |
| --- | --- |
| Catalog | The preset sets `no_list`, and the matched region does not set `lists`. `google-vertex` and `bedrock` are this kind, including when `bedrock` is built on the chat wire through `openai-compat`. The static `models` array is the list. No `GET /models`. The `google-vertex` plugin's `list_models` returns the ids its location serves. Bedrock does not call control-plane `ListFoundationModels`. |
| Deployments | Preset `azure`, or a custom chat or responses base whose host `AzureHost` recognizes, matching magpie's `IsAzure` (`azure.go` lines 27-53). The Azure plugin's `list_models` owns deployments, then the base-model `/models` fallback; the host aggregator only consumes the rows. See the Azure section. |
| Live | Every other preset, including `bedrock-mantle`. Also a `no_list` preset whose matched region sets `lists` (`baidu-qianfan` and `volcengine`, region `api`), and a `no_list` preset whose `models_url` is set. An Anthropic-wire Azure instance is Live on its own `/models` and `/v1/models` candidates: magpie's deployment branch requires chat or responses, and its Anthropic base otherwise uses the ordinary live path (`models.go` lines 661-667 and 697-758; `azure.go` lines 126-160). |

Region match is by path, as `listRegion` and `atRegion` do (`models.go` lines 974-994). The host may differ. `bedrock-mantle` is live: its preset comment says the runtime has no list and Mantle lists at `/v1/models` (`presets.go` lines 395-402). A chat or responses call still uses `MantleBase` (`provider.go` lines 1698-1708): a model is asked at `/v1` or `/openai/v1`, and the other prefix is retried when the first is refused. The list itself is asked at `/v1/models`. `/openai/v1/models` is not a list.

`GET /v1/models` and `GET /v1beta/models` on the gateway keep listing route ids (feature-parity row 1.6). Magpie's gateway `/models` is the user's exposed picks. Those are different surfaces. The models.dev catalog (row 2.8) stays a kernel cache. A plugin does not fetch it.

## What one wire's `list_models` does

The plugin asks only the base it was built with.

OpenAI-compatible and Anthropic URL order is `listURLs` (`live.go` lines 353-376):

1. `{base}/models`.
2. When the base is Anthropic, or the path has no version segment (`v` plus a digit: `v1`, `v1beta`, `v3`, `v4`), also `{base}/v1/models`.
3. For those same bases, make one ordered pass over `/anthropic`, `/apps/anthropic`, `/api/anthropic`, `/v1`, `/api`, `/api/v1`. At each suffix, strip it once if the current root ends with it; do not revisit an earlier suffix. A base ending in `/api/v1` loses `/v1` and then `/api`, and the root is what remains. Then try `{root}/v1/models` and `{root}/models`.

A versioned OpenAI base (`…/v1`, `…/api/paas/v4`, `…/v1beta/openai`) is asked only at `{base}/models`. The first URL that returns a non-empty list wins for that base. An empty successful page fails that URL (`no models listed`). When every URL fails, the errors are joined.

Auth on those lists, from `fetchPage`:

- `Authorization: Bearer` and `x-api-key`, both, when a key is set.
- Anthropic bases also send `anthropic-version: 2023-06-01`. Anthropic still accepts that value. A newer version string is not what this client sends.
- A caller header overrides a default of the same name.

Only an Anthropic-wire list for a `BothAuth` provider retries and learns: a third-party Anthropic-compatible host with a key, not an account. It is off for `anthropic.com`, an empty Anthropic base, Bedrock, Azure, Vertex, accounts, and every OpenAI-compatible list. `fetchPage` retries only with the context supplied by `listAuth`, and `listAuth` installs it only for `BothAuth` (`internal/catalog/live.go` lines 530-541; `internal/provider/authheader.go` lines 101-106; `internal/provider/models.go` lines 648-655 and 737-740). When a 401 body refuses both headers, the page is asked again with `x-api-key` alone and then with `Bearer` alone. The winner is learned per configured provider and persisted in host state. It affects only later Anthropic-wire lists and Anthropic probes for that provider; it never changes OpenAI-compatible lists or chat/Responses probes. A user-pinned header in `headers` always wins over a learned one. Refusal detection matches Chinese and English refusal phrases (`internal/catalog/authheader.go` lines 20-41), and the code pull request must keep the non-English markers.

Official shapes:

- OpenAI lists with `GET /v1/models`, Bearer, and `{ "object", "data": [ { "id" } ] }` (<https://platform.openai.com/docs/api-reference/models/list>). That reference documents no paging. Every OpenAI-compatible body still goes through the pager below. A page that does not set `has_more` is the whole list.
- Anthropic lists with `GET /v1/models`, `x-api-key`, `anthropic-version: 2023-06-01`, `data[].id`, `has_more`, `first_id`, and `last_id`. The next page uses `after_id` and `limit`. `limit` is 1 to 1000 and defaults to 20. `before_id` exists on the official API and is not what the pager sends (<https://platform.claude.com/docs/en/api/models/list>).
- A compat body that uses `models` instead of `data` is read the same way. `data` wins when it is non-empty. An object contributes `id`, then `name`.

The OpenAI and Anthropic pager is `Paged` (`live.go` lines 459-489). The first request is the URL as written. Only HTTP 200 is a list response; another 2xx status fails that candidate with its status. Each candidate list URL has its own 8 second budget across its pages (`live.go` lines 422-437); it is not one budget across every candidate. A response body is capped at 8 MiB. A page with `has_more` repeats the URL with `after_id` set to `last_id` and `limit=1000`. At most 20 pages are read. When the 20th page still sets `has_more`, the list fails and no 21st request is sent. A page that sets `has_more` without a new `last_id` fails the list. A partial list is a failure, so a later page is not mistaken for models the vendor dropped. A JSON error message is cut at 160 runes (`live.go` lines 637-662).

Gemini native follows `gemini_list.go` and `models.list` (<https://ai.google.dev/api/models#method:-models.list>):

- `GET {base}/models?pageSize=1000` with `x-goog-api-key`.
- Pages follow `nextPageToken` as `pageToken`, at most 20 pages. Each page has its own 8 second timeout.
- A token on the 20th page returns the rows already read without another request and without failing. Hofund deliberately also returns the partial list on a repeated token; the reference has no repeated-token check. This early stop differs from `Paged` and from the reference.
- An empty list is an error (`no models listed`).
- Each id is `name` with a leading `models/` removed.
- A row whose `supportedGenerationMethods` is non-empty is kept when that array contains `generateContent`. A row with an empty methods array is kept. Embedding (`embedContent`), Imagen (`predict`), and Veo (`predictLongRunning`) are dropped. Magpie applies only this filter to Gemini pages (`gemini_list.go` lines 60-106); hofund's additional common classification is below.

#59's `next_cursor` reads `next_page_token`, `nextPageToken`, `cursor`, and `next`. The Gemini walker can use it for `nextPageToken`. Gemini ids come from `name`, so the reader strips `models/` itself. `string_items` keeps objects with `id` and bare string elements (`crates/plugin-sdk/src/page.rs` lines 65-90). OpenAI and Anthropic pages use `has_more` and `last_id`, which that helper does not read. The code pull request adds an `after_id` pager and the Gemini walker to `hofund-plugin-sdk`, beside #59's helpers. Azure keeps the pager it already has.

### Azure deployments

The Azure plugin's `list_models` owns the whole sequence and returns `ListKind::Deployments`: `GET {root}/deployments?api-version=2022-12-01`, then the base-model fallback `GET {base}/models`, header `api-key`. The host aggregator only consumes the returned rows; it does not issue the fallback. `root` is the v1 base with a trailing `/v1` removed. That is the same pair `azureModels` asks (`azure.go` lines 130-161). The plugin is `plugins/providers/azure/src/lib.rs`; on the Anthropic wire it lists `Live` from `{base}/models`, `{base}/v1/models`, `{root}/v1/models`, and `{root}/models` with `x-api-key`, and its error has no Azure suffix.

On 2026-10-26, `https://learn.microsoft.com/en-us/rest/api/azureopenai/deployments/list?view=rest-azureopenai-2022-12-01` and the same URL without `?view` returned HTTP 404, while `https://learn.microsoft.com/en-us/rest/api/aiservices/accountmanagement/deployments/list` returned HTTP 200. The replacement Microsoft documents is the management-plane deployments API, which needs ARM and Entra ID. Magpie still calls the data-plane URL, and field reports say a resource key still gets an answer. This plan keeps that call and records the risk: it can stop answering without a docs page to follow. The second URL stays. For the usual Azure base that ends in `/openai/v1`, that URL is `/openai/v1/models`. It lists base models. It lists deployment names only when the resource owner named each deployment after its model. The id a chat call must send is the deployment name, so the plugin labels a fallback hit as base models, not deployments. That explicit label is a deliberate difference: magpie returns the fallback rows without distinguishing them (`azure.go` lines 130-154). When both URLs fail, the message ends with `type your deployments' names in as model ids`, which is Azure's own suffix, not the generic one below.

When the first failure is classified as `Cancelled`, the plugin stops without requesting the second URL. No current transport path in the built-in model-list client produces `Cancelled`: its send failures become connect, timeout, or transport errors, and its response-read failures become timeout or transport errors. There is no separate cancellation token or signal in this API; caller cancellation is dropping the call future, which prevents the fallback loop from continuing.

### Rows that are not chat models

`fetchPage` (`live.go` lines 583-587) drops a row unless it is tagged as a drawing, a film, or a decision, when `textModel` rejects the id. On that path `textModel` is called with the id alone (`catalog.go` lines 1448-1467), so the drop is the substring test:

- The id, as the vendor spelled it, is dropped when it contains any of `embed`, `-tts`, `image`, `audio`, `-live`, `robotics`, `computer-use`, `deep-research`, `transcribe`, `realtime`, `moderation`, `whisper`, `dall-e`, or `sora`. The check does not lowercase the id first. Embedding (`embed`), TTS (`-tts`), Whisper, Sora, realtime, and moderation are in this list. `tts-1` does not contain `-tts`, so the substring test does not drop it.

`textModel` also drops a models.dev catalog row whose output modalities are present and contain no `text` (`catalog.go` line 931). A plugin does not fetch models.dev, and a live page does not pass modalities into `textModel`, so a live row is not dropped only because its output modalities omit `text`. Those modalities are still stored on the row. In magpie, the live-page filter and tags run only on OpenAI-compatible and Anthropic pages (`live.go` lines 571-613); `geminiPage` does not call them. The models.dev path is the other `textModel` caller.

Tagging, still recorded when the probe cannot use the row yet:

- Decision: the row's kind is `decision`.
- Film: the row's kind is `video`.
- Drawing: the row is not a film or a decision, and either its kind is `image` or `DrawsID` matches and the id does not contain `deep-research`. `DrawsID` lowercases the id and looks for `image`, `imagen`, `imagine`, `dall-e`, `flux`, `seedream`, `cogview`, `stable-diffusion`, `sdxl`, `wanx`, `kolors`, `hidream`, or `banana`.

A tagged drawing, film, or decision stays on the result and is marked. The chat-facing list omits it, and auto-include does not add it to chat picks. An id that fails the substring test and is not tagged is absent from both. `text-embedding-3-small` contains `embed` and is not tagged, so it is absent. `gpt-4o-mini-tts` contains `-tts` and is absent the same way. `dall-e-3` and `gpt-image-2` match `DrawsID`, so they are drawings, not chat models. An OpenAI-compatible page of `gpt-5.5`, `gpt-image-2`, `flux-kontext-pro`, and `text-embedding-3-small` yields the chat id `gpt-5.5` and the drawings `gpt-image-2` and `flux-kontext-pro`. The same filter runs on Anthropic pages.

Hofund deliberately applies the text filter and tags to Gemini rows too, after the generation-method filter. One classification rule for every wire means a drawing or speech id never becomes a chat route because a Gemini base listed it. When the same id arrives from several bases, a drop from any base wins; otherwise the drawing, film, and decision tags are the union of every base's tags. This conservative rule matches the AND image-input rule. Mock group T covers a Gemini/OpenAI-compatible duplicate where one side drops the id and another tags it.

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

The `magpie_label` and `magpie_price` fields, the `magpie` `web_search` value, and the dependent `plain` field are deferred to the remote-gateway preset pull request. Those reference-named wire fields cannot appear in code or tests in this branch; no alias or spelling workaround is used.

Gemini rows fill `id`, `display_name`, `context` (`inputTokenLimit`), and `max_output` (`outputTokenLimit`). Price and endpoints stay empty. Rows 1.6 and 2.8 read `context` and `max_output` from this record. They do not fetch models.dev from the plugin. Catalog decoration is deferred to the models.dev catalog pull request for row 2.8: the aggregator will then apply `Decorate` to live rows and `Named` after it, filling names, efforts, context, and output; Azure gets `Decorate` without `Named`, preserving deployment names (`live.go` lines 781-822; `models.go` lines 83-97).

## What the aggregator adds

For the configured wire, the aggregator calls `list_models` on the configured instance registered by the daemon (issue #86). It falls back to a fresh build only when no instance is registered, as with a one-shot host, or when the registered instance predates a newly learned `fixV1` base. For every other base the region-resolved preset row declares for chat, responses, Anthropic, or generative, code pull request 1 adds a daemon list-only build path through `DynUpstreamFactory::validate` followed by `validate_and_build` for daemon-owned factories or `validate_and_build_with` for listed factories. It follows the order `build_plugin` uses since #59: validate the table with option values unresolved, resolve `env:` option values, then build. A listed factory receives the selected protocol in `BuildContext`; no factory schema key is needed.

Each list-only `RawTable` contains exactly:

- `id`: `<provider id>#list:<wire>`. It cannot collide with the configured instance, never reaches the router, and is never logged as a route; host logs use the configured provider id.
- `base_url`: the region-resolved preset row's base for that wire.
- `protocol`: `chat` or `responses` only; Anthropic and generative factories take no protocol key.
- `preset`: the catalog preset id the daemon passes to builtin factories. It is a reserved kernel key, so a user's `[providers.options] preset` is rejected.
- `credential`: the credential for that wire.
- `api_key`: the configured key after secret resolution. The same key also reaches the plugin through `runtime_host` (`crates/daemon/src/lib.rs` lines 335-341; `crates/plugin-host/src/lib.rs` lines 82-103).
- `headers`: the configured headers.
- `quirks`: only when the preset row names a profile.
- Configured options are not an `options` key in the `RawTable`. Only for a list-only instance of the configured provider's own plugin, `insert_options` flattens them into top-level keys guarded by `RESERVED_OPTIONS`, and `resolve_options` resolves their `env:` values (`crates/daemon/src/lib.rs` lines 393-432). A list-only instance of another plugin gets no configured options.

The kernel keys are the ones every builtin factory accepts (`is_kernel`, `plugins/registry/src/builtin.rs` lines 355-360). Since #59 each factory also accepts its own option names and rejects any other key with `unknown option` (`reject_options` and `read_endpoint`, lines 334-353 and 391-396): `google-vertex` takes `VERTEX_OPTIONS`, `bedrock` takes `BEDROCK_OPTIONS`, and the other three take none (lines 73-87). That is why options never cross to another plugin. The host-owned `[providers.models]` keys below are never put in a `RawTable`, so they cannot trip that check. A list-only instance is never a router member, is never sent `execute`, and is dropped after the sweep.

The host-owned listing keys live in a `[providers.models]` table that `hofund-config` adds beside `options`: string `refresh`, string array `auto_include`, and string `url`. They are kernel keys, not vendor settings. RFC 0001 keeps kernel keys in `hofund-config` and leaves the opaque `options` table to the plugin, which validates it (RFC 0001 lines 319-321; `crates/config/src/lib.rs` lines 117-149). Putting host keys in `options` would make the host read inside a plugin-owned table. Code pull request 1 adds the table with all three keys; the refresh job that reads `refresh` and `auto_include` arrives in code pull request 2. This document calls them `models_url`, `models_refresh`, and `models_auto_include` below.

The name `[providers.models]` stays. That config table holds listing settings; a preset's `models` in `crates/provider/presets.toml` is the compiled static id list used by the `Catalog` source (for example, line 68). They live in different files and types and never merge. The Rust config field may be called `model_list` only if needed. There is no existing key collision: `RawProvider` is `deny_unknown_fields` and has no `models` field, so code pull request 1 adds it explicitly (`crates/config/src/lib.rs` lines 791-831).

When the user set `base_url` on the provider, only the configured wire is listed. The key must not go to the preset's other hosts after the user pointed the provider elsewhere. This is a deliberate difference from magpie. Magpie exposes chat, responses, Anthropic, Gemini, and Decide as separate fields (`internal/provider/provider.go` lines 84-104), and `fetchOne` walks the bases that are set (`internal/provider/models.go` lines 640-660 and 697-758).

The aggregator merges by id, in protocol order, within that one configured provider.

A plugin records the answering list URL in `ModelList::source_url` with `with_source_url`; the aggregator uses that sanitized URL as part of its deduplication key. The builder strips userinfo and fragments and replaces every query with `<redacted>`, so the record is safe for its derived `Debug` output. Error redaction masks standalone secret tokens of at least eight characters, while URL query values are removed at any length.

- The same list URL, asked with the same auth, is fetched once.
- A duplicate id keeps one row. Image input follows `sharedImageInput` (`models.go` lines 1331-1344). An explicit text-only answer from either base wins. Image input is kept only when both bases report it. If either side left it unknown, the merged value stays unknown, unless the other side is explicitly text-only. `anyImageInput` (`models.go` lines 1346-1350), where either side having image input is enough, is the rule for groups. It is not the rule for two bases of one preset. A mock where one base says text-only and the other says image expects text-only. Drops and tags follow the cross-wire precedence above.
- OpenRouter's Anthropic base is skipped when the provider also has a chat or responses base (`models.go` lines 754-763). With `anthropic-version`, that host returns its catalog under an `anthropic/` prefix. When that Anthropic base is the only base, it is asked, and a namespaced answer is an error that names `https://openrouter.ai/api/v1`.
- When at least one base answers and another fails, the failed base keeps the rows in its own saved side, and only those (`keepDeadSides`, `models.go` lines 927-968). A base absent from a non-empty saved-sides map keeps nothing. With no saved sides, rows from the previous list that no answering base lists are kept once and are not assigned to the failed base. Sides are saved only when more than one base was asked; an answering base keeps its side even when that side is empty.
- `fixV1` (`models.go` lines 1138-1167): when the list was found only at `{base}/v1/models`, the base has no version segment, and the protocol is chat or responses, the stored base gains `/v1`. A versioned base is left as written. The corrected base is stored per provider and wire in host state, not in the user's TOML; only the configured wire's entry can change its configured base (issue #84). A request to the unfixed base would miss the models the list just found.

`models_url`, when the user set it, is the only URL asked (`models.go` lines 648-660). It also makes a `no_list` preset list live (`lists`, lines 170-176). It supports `env:NAME` resolution through the same resolver as `api_key` (`crates/secrets/src/lib.rs` lines 26-45; `crates/daemon/src/lib.rs` line 272), because the URL may carry a query secret. The resolved value is a secret: redact it in errors, logs, spans, and probelog, including wherever it appears as a URL or query value.

After a normal or `models_url` fetch, the aggregator applies the public-drawer rule for an AIHubMix base host. It fetches `https://aihubmix.com/api/v1/models` without a key, keeps matching image rows as drawings, and appends ids the keyed list omitted. A public-feed failure adds nothing (`public.go` lines 14-94; `models.go` lines 659 and 816-824).

The unsaved add-provider preview (`Provider.List`, `models.go` lines 408-433) is deferred to the desktop add-provider pull request. That pull request adds a dry-run over an unsaved provider table through the same aggregator, with no picks or refresh-state writes.

`planModels` (`models.go` lines 1038-1091) runs in the aggregator after the merge:

- A region's `models` replace the preset's `models` when the instance sits on that region.
- Vertex replaces them with `vertexModels(location)`, which the `google-vertex` plugin's catalog `list_models` returns.
- When the preset sets `only`, live ids are kept only when they start with that prefix. `clinepass` sets `only = "cline-pass/"`. When the filtered list is empty, the static ids are the list.
- When `only` is empty and either the live list is non-empty or there are no static preset or region models, `planModels` returns the live rows early and ignores static models (`models.go` lines 1050-1051). A preset with a static `models` array and no live rows falls through and returns those ids.
- Only the path past that early return fills limits from the kernel's models.dev cache: `ContextOf` when `catalog.Window(context) == 0`, and `OutputOf` when `max_output == 0`, after stripping the `only` prefix for lookup (`models.go` lines 1077-1088). Early-return live rows instead get limits through `Decorate` in the deferred models.dev catalog pull request (`internal/catalog/live.go` lines 781-822; `internal/provider/models.go` lines 83-97). Plugins do not fetch models.dev.
- Region embedding, drawing, and video lists are deferred to the embeddings, drawing, and video pull requests.
- Cline's extra free-model list is part of the Cline preset pull request, not this filter.

Per-key listing (`fetchPerKey`, `models.go` line 1174) waits for the pull request that gives one provider more than one key. Until then the single `api_key` is the whole list, and the probe uses that key.

When every base fails, the joined message ends with `type its model ids in by hand, or give the URL its list is at` (`models.go` line 807).

## Refresh

List refresh is one host-side job. Plugins do not register it. That refines RFC 0001's "plugins register jobs" for this loop: `SchedulerBackend::register` is a fixed interval and has no due time, and a job per instance cannot keep "one provider at a time" or the retry clock. The plugin implements `list_models`. The host owns the sweep. RFC 0001 line 642 leaves the 10 minute and 15 minute numbers to "the plugin pull request that needs them". This plan chooses them here for the host job.

The job follows `KeepListsFresh` (`internal/provider/autolist.go` lines 239-290):

- It starts 30 seconds after process start, then ticks every 1 minute.
- Each tick walks providers one after another.
- A provider with a blank `models_refresh`, or `off`, `never`, `no`, `none`, `0`, or `-`, is skipped. `Nd` is a number of days and accepts fractions, such as `0.5d`. Any other value is a Go duration. Shorter than 10 minutes is a config error (`MinRefreshEvery`, `internal/provider/autolist.go` lines 37-65). Hofund rejects an unparseable value at config load. Deliberate difference: magpie's `RefreshInterval` swallows that parse error and treats refresh as off at runtime (`internal/provider/autolist.go` lines 68-78); a config error keeps a typo from silently disabling refresh.
- Every configured `[[providers]]` entry is in scope. Every hofund provider is already a plugin instance, so none is skipped for being plugin-backed. There is no hofund equivalent of a magpie external plugin provider today. Subscription members from `[[subscriptions]]` are refreshed by their subscription under row H, not by this job. A disabled provider, a decide-only provider, and a preset with nothing to refresh are skipped. This replaces magpie's `ownList` account and external-plugin exclusions (`internal/provider/autolist.go` lines 70-85) while keeping its `Off` skip (`internal/provider/autolist.go` lines 255-274).
- Due is computed from the stored fetch time. A list younger than the provider's interval is left alone.
- A failed fetch records the try and waits `min(interval, 15 minutes)` (`listRetry`, `internal/provider/autolist.go` lines 222-234 and 266-274). A `Retry-After` delay raises that wait when it is longer, but hofund honours at most 24 hours so a broken or hostile upstream cannot persist an arbitrarily distant retry time. This cap is a hofund choice; the reference gateway does not read `Retry-After` for list refresh. A 429 without that header uses the normal retry wait. Every retry still waits at least 10 minutes.
- One fetch is capped at 1 minute.

The host-owned file is `<root>/host/models/<slug>-<h>.json`, where `<root>` is `.hofund` beside the config (`crates/daemon/src/lib.rs` lines 335-341 and 653-663). It is not a plugin `state_dir` (`crates/plugin-host/src/lib.rs` lines 681-692). Its versioned JSON stores `version` and the exact `id`; optional `fetched_at`, `tried_at`, and `retry_at` Unix-millisecond times; the successful merged `list` and per-base `sides` as rows with all host-used model metadata; `picks`, `picked_from`, and `delisted`; the optional learned `auth` choice; and per-wire corrected `bases`. Sides are saved only when more than one base was asked. Due and retry times survive a restart, so a restart does not refetch every provider 30 seconds after start.

The filename never uses the raw id. `slug` is the provider id lowercased, with every character outside `[a-z0-9-]` replaced by `-`, then truncated to 40 characters. `h` is the first 16 lowercase hexadecimal characters of SHA-256 of the exact UTF-8 id. The JSON also stores the exact id; the loader ignores, and does not delete or overwrite, a file whose stored id does not match. Provider ids are only checked non-empty and unique by exact string (`crates/config/src/lib.rs` lines 383-390), so they can contain `/`, `..`, `\`, `:`, or reserved names, and can differ only by case on case-insensitive filesystems such as macOS and Windows. The safe slug keeps state inside `host/models`; the hash keeps such ids and case variants in different files. A file that is not valid provider-state JSON is moved to `<filename>.corrupt-<unix_ms>` before fresh state is saved. If it cannot be moved, it is preserved and fresh state remains in memory only.

`picked_from` records the list ids at pick time. Both `set_picks` and auto-include write it now so the picks-serving PR can distinguish newly listed models from models the user saw and left unpicked, without a state migration. Picks currently shape the probe pool; they are not yet served to agents.

Neither `fixV1` nor auth learning rewrites the user's TOML. This is a deliberate difference from magpie, which writes `AuthHeader` and the fixed base back into its config (`internal/provider/authheader.go` lines 80-100; `internal/provider/models.go` lines 1138-1167): the user's TOML is the user's, and the daemon never edits it. A user-pinned header wins over host state.

Auto-include runs on every successful list, including manual lists and this job, using `models_auto_include`. Every provider on this host path owns its aggregated list; account- and plugin-owned lists do not exist here, so the reference's `ownList` guard has no hofund equivalent (`internal/provider/models.go` lines 202-204,216-235). A rule is a glob (`*`, the whole id, case-insensitive) or `re:` plus a regular expression matched against the whole id. A `re:` rule is case-insensitive unless the expression contains `(?-i)` (`compilePattern`, `groupmatch.go` lines 38-55). A match adds a model the new list has and the previous list lacked. A model the user left unticked stays unticked. Rules only add. With no picks and no delisted ids, the whole list is served, so auto-include adds nothing; the reference's first-slice cap is not ported here.

A model the previous list had and the new list lacks leaves the picks and is stored as delisted (`models.go` lines 301-358). When a later list contains it again, it returns to the picks. Drops are applied before restores within one successful fetch, so dropping one model does not lose the restoration of another. A hand-typed id that was never in a fetched list stays. A fetch that fails, or that returns no model, changes nothing.

## Probe

Probes run only when the user asks. The refresh job never probes. A configured provider has one wire, so its normal connectivity probe is one paid call for 16 output tokens. A host batch tests at most four models at a time, as `TestModels` does (`internal/provider/test.go` lines 239-257).

The host selects the model. `probe` on the plugin sends the one request the host named. The host does what `Provider.Test` does (`internal/provider/test.go` lines 37-94 and 405-449):

1. Refresh the configured provider's aggregated list first.
2. Pick a model for the configured wire from exposed models, then listed models. The first pass skips an id `ImagesAPI` matches (`internal/catalog/catalog.go` lines 1499-1509): the lowercased id contains `gpt-image`, `chatgpt-image`, `dall-e`, `imagen`, `imagine`, `qwen-image`, `wanx`, `wan2`, `seedream`, `cogview`, `flux`, `stable-diffusion`, `sdxl`, `kolors`, or `hidream`. A chat model whose id merely ends in `-image`, such as `gemini-2.5-flash-image`, is not in that list. Each pool keeps its plain order; endpoint knowledge does not move a row ahead of an earlier row. A row with endpoint metadata is accepted only when it includes the configured wire. Without endpoints, Anthropic prefers `claude` after stripping any namespace through the last `/`, and `NamedOnlyOn` applies. An `anthropic` endpoint on Messages is sufficient without a `claude` id. The second pass accepts any otherwise eligible id, including an image id. Both passes skip disabled and Decide models and enforce key visibility. OpenCode's public key tries only a free model, on an API the id serves (`test.go` lines 405-449).
3. Map the id with `UpstreamName` before it is written into the body (`internal/provider/test.go` line 79; `internal/provider/modelprefs.go` lines 908-941).
4. Call `probe` with its wire and that upstream name on the daemon's registered configured instance (issue #86), with the same fresh-build fallback as listing when no current instance is registered.

A probe always runs on the configured provider's one wire, through that configured instance. It never uses a list-only instance, so no paid call goes to a base the user did not configure as a member. For a named model, when the aggregated list's endpoint metadata names protocols and does not include the configured wire, the host returns an error saying the model is not served on this protocol and sends nothing. Multi-key selection and drawing or Decide branches stay with their deferred pull requests.

This is a deliberate difference from magpie. Magpie's `Test` probes every protocol one provider speaks, and `testOne` can select any of those protocols from the list's endpoint metadata (`internal/provider/test.go` lines 34-94 and 290-356). In hofund each protocol is a separate configured provider, so a full parity check is one probe per configured provider.

Endpoint detection is deferred to the endpoint-detection pull request. That pull request carries `Detect`, `DetectBase`, `DetectModels`, `DetectDecide`, and the `api_detect` log kind (`internal/provider/detect.go` lines 28-120 and 179-348; `internal/probelog/probelog.go` lines 21-27), completing feature-parity row 2.10 rather than only its ordinary probe.

Test-as Codex and Claude Code is deferred to the client test-as pull request. It owns `TestAs`, `asClient`, `clientHeaders`, and `SawClient`, including persisted client header shapes and streamed client-shaped bodies (`internal/provider/test_as.go` lines 33-65 and 101-216).

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

Auth follows `AuthHeaders` (`test.go` lines 457-488). Only an Anthropic probe on the same `BothAuth` provider may use a header learned by listing. A learned header never changes chat/Responses probes, which stay Bearer; native Gemini stays `x-goog-api-key`, and non-Anthropic Azure stays `api-key`. A user-pinned header still wins:

- Vertex, and any provider with an empty key, sends no API-key header. Vertex sends the Google access token from its mint.
- Azure on Anthropic sends `x-api-key`. Every other Azure call sends `api-key`.
- Native Gemini sends `x-goog-api-key`.
- Anthropic on a host ending in `anthropic.com`, and Bedrock's Anthropic route, send `x-api-key` alone.
- A third-party `BothAuth` Anthropic-compatible route sends both `x-api-key` and `Authorization: Bearer`, or the one header learned on the Anthropic wire.
- Every remaining route sends `Authorization: Bearer`.

Only an Anthropic probe on a `BothAuth` provider that sent both headers retries with one header when a 401 body refuses the pair. Retry and learning are off for `anthropic.com`, an empty Anthropic base, Bedrock, Azure, Vertex, and accounts; chat/Responses probes never do this retry. The winner is remembered per provider in host state only for later Anthropic-wire lists and Anthropic probes. A user header that replaced a default disables that retry. This is `RetryOneAuth` (`internal/provider/authheader.go` lines 35-74 and 101-106; `internal/provider/test.go` lines 529-538).

A 2xx unary body is success. `streams` (`test.go` lines 573-578) reads the request body. A request whose JSON has `stream: true` is read until the first text, thought, or tool, or until an error event, and at most 1 MiB. A stream that ends with none of those still counts as answered. The stored snippet is at most `PROBE_CAP` (4096 bytes). A non-2xx response reads at most 4096 bytes, and the message shown from that body is cut at 300 runes (`apiError`, lines 826-828). The probe does not retry a 429.

The daemon wraps `list_models` and `probe` in the spans RFC 0001 describes, and redacts before a span field or a captured body is stored. The probe posts bytes through the pooled client, so that wrap sits at the kernel call site. Feature-parity row 2.10 also asks that a probe be logged like any other request. The host log uses magpie's `model_list` and `provider_test` kinds (`internal/probelog/probelog.go` lines 21-27; `ownRequests`, `test.go` lines 492-498). Detection adds `api_detect` in its deferred pull request.

`kits/upstream` keys its cases on the factory spec's `ListKind`. `check_list_and_probe` in `kits/upstream/src/conformance.rs` runs the paging cases against local mocks and one minimal probe for a `Live` spec. An Azure `Deployments` instance on that `Live` spec is not reachable from the spec alone, so the kit also exports `check_deployments` (deployment pages, the base-model fallback, both lists empty, and the probe), which `plugins/registry/tests/model_conformance.rs` (`azure_factory_passes_deployment_and_probe_conformance`) runs against the `azure` plugin; a `Deployments` spec gets the same cases from `check_list_and_probe`. The host aggregator's Azure mock group D still covers the host side. A unit mock does not replace the kit.

## Errors and secrets

Per-URL failures are joined into one `PluginError`. Mapping:

| Failure | Kind | Status | `retry_after` |
| --- | --- | --- | --- |
| DNS, TCP, or TLS before the request is sent | `Connect` | none | none |
| Connect or pre-header idle-read deadline | `Timeout` | none | none |
| The connection failed after the request was sent and before a status arrived | `Transport` | none | none |
| A 2xx body failed or stalled after the status arrived | `Interrupted` | none | none |
| An HTTP status, including 401, 404, and 429, even when its body read fails | `Status` | the status | `Retry-After` when the response sent one |
| A body that is not a model list or a completion | `Decode` | none | none |
| A catalog preset asked to call the network, or a wire with no list method | `Unsupported` | none | none |

The `Status` message keeps magpie's three `APIError` hint cases: an edge-block page adds `BlockedHint`, a Google account-verification refusal adds its HTTPS verification link, and Command Code's Go-plan refusal adds `CommandCodeGoHint` (`internal/provider/test.go` lines 633-726). Hofund rewrites the hint wording for hofund. In particular, `CommandCodeGoHint` points at a magpie-specific plugin, so that text is not copied. The kind and status stay as the table says; only the displayed message gains the hint. Mock group E covers all three.

The CLI prints `PluginError`'s message. Values are removed before the message is built:

- `Authorization`, `api-key`, `x-api-key`, `x-goog-api-key`, and every other header value.
- The query string of a URL, so a `models_url` of `https://vendor.example/v1/models?key=SECRET` becomes a path with the query redacted.
- A response body that echoes the key. The test for this is a 401 whose body contains the key. The error text must not contain the key. Stripping a header the client added is not enough, because the vendor sent the key back.

## Magpie behaviour inventory

Every listing, probing, and refresh behaviour has one disposition here. A deferred row is required for parity. Its named pull request carries it. A `Deliberate difference` status is allowed when hofund intentionally keeps different behaviour.

| Behaviour | Magpie source (file:lines) | Hofund status | Section of this doc |
| --- | --- | --- | --- |
| `listURLs` candidates and the single ordered suffix pass | `internal/catalog/live.go:351-376` | Covered: code PR 1 | What one wire's `list_models` does |
| `Versioned` detects a version path segment | `internal/catalog/live.go:378-396` | Covered: code PR 1 | What one wire's `list_models` does |
| `Paged`, its 20-page cap, and its `has_more` and `last_id` rules | `internal/catalog/live.go:407-489` | Covered: code PR 1 | What one wire's `list_models` does |
| The 8 second list budget per URL and 8 MiB body cap | `internal/catalog/live.go:422-437,495-524`; `internal/catalog/gemini_list.go:60-80` | Covered: code PR 1 | What one wire's `list_models` does |
| The 160-rune list error cap | `internal/catalog/live.go:637-662` | Covered: code PR 1 | What one wire's `list_models` does |
| Gemini's pager and partial result at a repeated token or 20 pages | `internal/catalog/gemini_list.go:23-49` | Deliberate difference: hofund returns partial rows on a repeated token, while the reference has no such check; the 20-page cap is covered by code PR 1 | What one wire's `list_models` does |
| Gemini's generation-method-only filter | `internal/catalog/gemini_list.go:60-106` | Covered: code PR 1 | What one wire's `list_models` does |
| Anthropic-wire-only single-header list auth learning for `BothAuth` providers, with `BothAuthRefused` and `OneAuthOrder`, Chinese and English refusal phrases, per-provider state, and user-pinned override | `internal/catalog/live.go:495-541`; `internal/catalog/authheader.go:9-73`; `internal/provider/authheader.go:78-106`; `internal/provider/models.go:648-655,737-740` | Covered: code PR 1 | What one wire's `list_models` does |
| Learned auth and `fixV1` write back to the config file | `internal/provider/authheader.go:80-100`; `internal/provider/models.go:1138-1167` | Deliberate difference: host state holds them because the user's TOML is the user's and the daemon never edits it | Refresh |
| The `textModel` substring filter | `internal/catalog/live.go:571-588`; `internal/catalog/catalog.go:931,1448-1467` | Covered: code PR 1 | Rows that are not chat models |
| `Draws`, `Films`, and `Decides` tagging | `internal/catalog/live.go:580-613`; `internal/catalog/catalog.go:1485-1497` | Covered: code PR 1 | Rows that are not chat models |
| Applying the text filter and tags to Gemini rows | `internal/catalog/gemini_list.go:60-106`; `internal/catalog/live.go:571-613` | Deliberate difference: one classification rule for every wire, so a drawing or speech id never becomes a chat route because it was listed by a Gemini base | Rows that are not chat models |
| `X-Magpie-Drawers` keeps every row from another gateway's list, including rows the normal filter would drop | `internal/catalog/live.go:580-587`; `internal/provider/remote_magpie.go:21-25,50-64,109-123` | Deferred: remote-gateway preset PR; its `resolve` implementation is missing today | Rows that are not chat models |
| Row metadata, modalities, endpoints, limits, efforts, and tiers | `internal/catalog/live.go:589-632,664-739` | Covered: code PR 1 | Metadata one row carries |
| `magpie_label`, `magpie_price`, the `magpie` `web_search` value, and `plain` | `internal/catalog/live.go:589-632,664-739` | Deferred: remote-gateway preset PR | Metadata one row carries |
| `Decorate` and `Named`, with Azure decorated without `Named` | `internal/catalog/live.go:781-822`; `internal/provider/models.go:83-97` | Deferred: models.dev catalog PR (row 2.8) | Metadata one row carries |
| `sharedImageInput` for bases versus `anyImageInput` for groups | `internal/provider/models.go:672-683,1331-1360` | Covered: code PR 1 | What the aggregator adds |
| One fetch per list URL and auth, with `ErrAsked` for nothing new | `internal/catalog/live.go:230-349`; `internal/provider/models.go:687-696,764-780` | Covered: code PR 1 | What the aggregator adds |
| OpenRouter's Anthropic skip and namespaced-list error | `internal/provider/models.go:741-763,837-880` | Covered: code PR 1 | What the aggregator adds |
| `keepDeadSides` | `internal/provider/models.go:809-815,900-968` | Covered: code PR 2 | What the aggregator adds |
| `fixV1` | `internal/provider/models.go:782-793,1129-1167` | Covered: code PR 1 | What the aggregator adds |
| `ModelsURL` is the only URL, and `lists()` lets it override `NoList` | `internal/provider/models.go:168-176,646-660` | Covered: code PR 1 | What the aggregator adds |
| AIHubMix public drawers with `PublicDrawers` and `WithDrawers` | `internal/catalog/public.go:14-94`; `internal/provider/models.go:659,816-824` | Covered: code PR 1 | What the aggregator adds |
| Unsaved add-provider preview `List` | `internal/provider/models.go:408-433` | Deferred: desktop add-provider PR | What the aggregator adds |
| `planModels`: region models, `vertexModels`, `only`, static fallback, and models.dev limits | `internal/provider/models.go:996-1091` | Covered: code PR 1 | What the aggregator adds |
| Cline's separate feed and free-model append | `internal/provider/models.go:380-385,1053-1076`; `internal/provider/cline.go:56-128` | Deferred: Cline preset PR | What the aggregator adds |
| Kilo's paged list, free-only public view, and tool and output filters | `internal/provider/models.go:387-395`; `internal/provider/kilo.go:60-99,149-173` | Deferred: Kilo preset PR | What one wire's `list_models` does |
| OpenCode skips a separate Gemini list and uses the other APIs' shared list | `internal/provider/models.go:699-708` | Not applicable: today's OpenCode preset rows declare no generative base; row H owns account-added bases | What the aggregator adds |
| Decision-list variants and their static or key-check fallbacks | `internal/provider/models.go:254-260,361-365`; `internal/provider/decide.go:768-899` | Deferred: Decide PR | Refresh |
| Region embedding, drawing, and video model lists | `internal/provider/models.go:1093-1127` | Deferred: embeddings, drawing, and video PRs | What the aggregator adds |
| `fetchPerKey`, per-model key visibility, and `keyFor` | `internal/provider/models.go:1169-1209`; `internal/provider/test.go:316-385` | Deferred: multi-key PR | What the aggregator adds; Probe |
| Delist and relist | `internal/provider/models.go:297-358` | Covered: code PR 2 | Refresh |
| `KeepListsFresh`: 30 second start, 1 minute tick, sequential sweep, `Fetched` due, `listsTried` retry, 1 minute cap, and `Off` skip | `internal/provider/autolist.go:222-290` | Covered: code PR 2 | Refresh |
| Persistent fetch time, list, and per-base sides | `internal/provider/models.go:127-155`; `internal/catalog/live.go:73-109,160-184` | Covered: code PR 2 | Refresh |
| `MinRefreshEvery` and valid interval parsing, including fractional days | `internal/provider/autolist.go:37-65` | Covered: code PR 2 | Refresh |
| `RefreshInterval` treats an unparseable refresh value as off | `internal/provider/autolist.go:68-78` | Deliberate difference: hofund rejects the value at config load so a typo cannot silently disable refresh | Refresh |
| Auto-include glob and `re:` rules, including `(?-i)` | `internal/provider/autolist.go:114-219`; `internal/provider/groupmatch.go:25-57` | Covered: code PR 2 | Refresh |
| `withNewlyListed` and `PickedFrom`: picks that held every listed model gain newly listed models at serve time | `internal/provider/models.go:1535-1583` | Deferred: picks-serving PR; hofund stores `picked_from`, but picks currently shape only the probe pool | Refresh |
| `manyModels` caps the first slice at 24 when nothing is picked | `internal/provider/models.go:27,1490-1501` | Deferred: picks-serving PR | Refresh |
| Azure detection by preset or `AzureHost` | `internal/provider/azure.go:27-53` | Covered: code PR 1 | Two layers |
| Azure `azureModels`: the plugin's `list_models` owns deployments, then the base-model `/models` fallback, and returns `Deployments`; the host only consumes the rows | `internal/provider/azure.go:121-161` | Covered: code PR 1 | Azure deployments |
| Labelling the Azure `/models` fallback as base models | `internal/provider/azure.go:130-154` | Deliberate difference: the fallback does not prove that its ids are deployment names | Azure deployments |
| Bedrock's static list instead of a runtime list call | `internal/provider/models.go:374-379`; `internal/provider/presets.go:380-392` | Covered: code PR 1 | Two layers |
| Mantle lists at `/v1/models`, not `/openai/v1/models` | `internal/provider/presets.go:393-409`; `internal/provider/provider.go:1692-1715` | Covered: code PR 1 | Two layers |
| `Test` refreshes the list before selecting a model | `internal/provider/test.go:34-80` | Covered: code PR 1 | Probe |
| `testModel` two-pass selection, endpoint preference, namespace handling, disabled, Decide, and image-API skips, `NamedOnlyOn`, and Zen's public-key rule | `internal/provider/test.go:387-449`; `internal/catalog/catalog.go:1499-1509` | Covered: code PR 1 | Probe |
| Per-model protocol selection from listed endpoints | `internal/provider/test.go:290-356` | Deliberate difference: one configured provider is one wire | Probe |
| Test probes every protocol a provider speaks | `internal/provider/test.go:34-94` | Deliberate difference: one configured provider is one wire | Probe |
| Endpoint detection with `Detect`, `DetectBase`, `DetectModels`, `DetectDecide`, and `api_detect` | `internal/provider/detect.go:28-120,179-348`; `internal/probelog/probelog.go:21-27` | Deferred: endpoint-detection PR | Probe |
| Test-as Codex and Claude Code with `TestAs`, `asClient`, `clientHeaders`, and `SawClient` | `internal/provider/test_as.go:33-65,101-216` | Deferred: client test-as PR | Probe |
| `UpstreamName` mapping | `internal/provider/test.go:57,79,352`; `internal/provider/modelprefs.go:908-941` | Covered: code PR 1 | Probe |
| `tinyBody` for each protocol, including both Mantle prefixes | `internal/provider/test.go:175-206` | Covered: code PR 1 | Probe |
| `tested`, the extra body, a fresh session, and `Prepare` | `internal/provider/extrabody.go:202-207`; `internal/provider/test.go:57-58,79-80` | Covered: code PR 1 | Probe |
| `testWait` is 20 seconds | `internal/provider/test.go:208-213` | Covered: code PR 1 | Probe |
| `AuthHeaders`, with learned single-header auth confined to Anthropic probes on `BothAuth` providers; chat/Responses stay Bearer, Gemini stays `x-goog-api-key`, and Azure stays `api-key` | `internal/provider/test.go:452-488`; `internal/provider/authheader.go:23-33,99-106` | Covered: code PR 1 | Probe |
| Request-body stream detection and the 1 MiB stream read | `internal/provider/test.go:555-631` | Covered: code PR 1 | Probe |
| Probe errors read 4096 bytes and `apiError` cuts the message to 300 runes | `internal/provider/test.go:567-578,649-667,789-830` | Covered: code PR 1 | Probe |
| `APIError` edge-block, Google verification, and Command Code Go-plan hints, with hofund wording rather than the magpie-specific plugin text | `internal/provider/test.go:633-726` | Covered: code PR 1 | Errors and secrets |
| `TestModels` concurrency is four | `internal/provider/test.go:239-257` | Covered: code PR 1 | Probe |
| `probelog` kinds `model_list` and `provider_test` | `internal/probelog/probelog.go:21-27`; `internal/provider/models.go:238-240`; `internal/provider/test.go:37-38,490-498` | Covered: code PR 1 | Probe |
| The final list error suffix | `internal/provider/models.go:801-807` | Covered: code PR 1 | What the aggregator adds |
| Azure's deployment-name error suffix | `internal/provider/azure.go:149-160` | Covered: code PR 1 | Azure deployments |
| Anthropic-only probe `RetryOneAuth` on `BothAuth` providers, affecting only later Anthropic-wire lists and probes | `internal/provider/test.go:529-538`; `internal/provider/authheader.go:35-74,99-106` | Covered: code PR 1 | Probe |
| `RetryZhipuCoding` and the Zhipu success-envelope error | `internal/provider/test.go:539-547`; `internal/provider/zhipu_coding.go:61-110`; `internal/provider/zhipu_envelope.go:67-112` | Deferred: Zhipu and Z.ai preset PR | Probe |
| Cline's streamed 1024-token probe and the Cline, Kilo, and OpenCode client variants | `internal/provider/test.go:96-134,387-394,517-525` | Deferred: Cline, Kilo, and OpenCode preset PRs | Probe |
| `tinyDrawing`, its 2 minute cap, and drawing fallback | `internal/provider/test.go:208-237,334-365` | Deferred: drawing PR; an image-only id is not sent a chat `hi` | Probe |
| Decide probes | `internal/provider/test.go:39-40,90-92,163-173,265-279`; `internal/provider/decide.go:1066-1082` | Deferred: Decide PR | Probe |
| Account probes, including Claude and translated Code Assist | `internal/provider/test.go:60-65,284-289`; `internal/provider/translated_probe.go:28-62` | Deferred: row H | Probe |
| Factory Gemini's `/generate` probe | `internal/provider/test.go:195-203` | Deferred: row H | Probe |

## Acceptance before a preset is Done

A preset stays partial until all of the following are true. The coverage row is updated with the CI run id in the same pull request that claims the preset.

1. A mock list test hits the preset's real base path and the URL candidates, including the `/v1` and root fallbacks where those candidates apply. A static preset has a test that no network call is made.
2. A mock probe covers each protocol the preset speaks, with the auth headers this note names. Mantle's model-dependent prefix is included.
3. The preset's other quirks in [providers.md](../coverage/providers.md) are closed.
4. The coverage row says done and cites the CI run.

Azure stays partial until the probe, the refresh job, and this acceptance list are done. The six presets `resolve` still rejects stay missing until `resolve` accepts them.

### Mock groups

| Group | Behaviour to mock |
| --- | --- |
| V | A versioned OpenAI-compatible chat or responses base is asked only at `{base}/models`. |
| U | An unversioned or suffixed OpenAI-compatible base uses `{base}/models`, `{base}/v1/models`, then the root `/v1/models` and `/models` fallback chain. This also covers a custom `base_url`. |
| A | An Anthropic-protocol base uses its own `/models`, `/v1/models`, and suffix-stripped root candidates. On a third-party `BothAuth` host, a refused pair retries with `x-api-key`, then Bearer, and learns the winner for later Anthropic lists and probes. An OpenAI-compatible 401 and an `anthropic.com` 401 do not retry. A learned header does not change a chat probe, which stays Bearer. An Anthropic-wire Azure base stays on this live path without auth learning. |
| G | The Gemini native pager uses `pageSize=1000` and `pageToken`. The method filter, common text filter and tags, and partial result at 20 pages are included. |
| T | The same id arrives from Gemini and OpenAI-compatible bases. A drop on either side wins; otherwise tags union. |
| I | A list-only build has the exact allowed `RawTable`, a non-route id, and no host-owned keys. Only a list-only instance of the configured plugin gets configured options, flattened into guarded top-level keys rather than an `options` key; another plugin gets none and does not fail with `unknown option`. Vertex and Bedrock static catalogs need no network. |
| S | A static catalog returns the compiled ids without a network call. Region and Vertex location filtering are included where they apply. |
| D | Azure is recognized by preset and by Azure resource host. The Azure plugin's `list_models` owns deployments, then `/openai/v1/models`; the host only consumes the rows, the plugin returns `Deployments`, and the plugin labels the fallback as base models. This group, not the conformance kit, covers a `Deployments` instance on a `Live` spec. |
| M | Mantle lists at `/v1/models`. A test proves that `/openai/v1/models` is not asked. |
| R | A `no_list` preset is static outside its `lists = true` region and live in that region. Both outcomes are tested. |
| P | AIHubMix's public feed adds only matching image rows not already keyed, and a feed failure adds nothing. |
| E | Probe errors produce the edge-block, Google verification, and Command Code Go-plan hints in hofund wording without changing error kind or status. |
| J | Refresh state survives a restart, preserves due times and learned host state, and never writes TOML. Two ids differing only by case get different files and do not share state. An id containing `/` and one containing `..` stay inside `host/models`. A file with a mismatched stored id is ignored and not deleted. |
| Q | Probe selection covers both passes, endpoint preference, namespaced Claude ids, `NamedOnlyOn`, and disabled and Decide skips. Azure cases also prove the plugin's `list_models` owns deployments then the base-model fallback and the host only consumes the rows. |
| L | The models.dev catalog PR mocks `Decorate`, `Named`, and Azure decoration without `Named`. |
| O | The desktop add-provider PR mocks an unsaved dry-run and proves that it writes no picks or refresh state. |
| N | The endpoint-detection PR mocks Chat, Responses, Anthropic, Gemini, and Decide detection and the `api_detect` log kind. |
| W | The client test-as PR mocks Codex and Claude Code headers and streamed bodies. A header learned on the Anthropic wire does not change a chat probe, which stays Bearer and does no single-header retry. |
| C | Cline's recommendation feed, its fallback to the ordinary list, and free-model handling are mocked. |
| K | Kilo's paged list, free-only public view, and tool and output filters are mocked. |
| X | `resolve` rejects the preset today. The named blocker must land before list or probe acceptance can run. |

### Preset matrix

The list kind is per configured instance. `V + A` means one mock for each declared base shape. Region names in the last column require the same mocks at every region's real paths.

| Preset | List kind | Mock group(s) per base | Probe protocols to mock | Extra criteria or blocker |
| --- | --- | --- | --- | --- |
| `anthropic` | Live | A | Anthropic | None. |
| `openai` | Live | V | Chat, Responses | The shared URL and auth are fetched once. |
| `google` | Live | V + G + T | Chat, Gemini native | Merge chat and Gemini rows with the common filter and cross-wire precedence. |
| `google-vertex` | Catalog | S | Vertex Gemini | Filter static ids by location. Test unary thinking rules. |
| `deepseek` | Live | V + A | Chat, Responses, Anthropic | None. |
| `xai` | Live | V + A | Chat, Responses, Anthropic | None. |
| `moonshot` | Live | V + A | Chat, Anthropic | None. |
| `moonshot-cn` | Live | V + A | Chat, Anthropic | None. |
| `kimi-code` | Live | V + A | Chat, Anthropic | None. |
| `kimi-code-cn` | Live | V + A | Chat, Anthropic | None. |
| `zhipu` | Live | V + A | Chat, Anthropic; Responses in `coding` | Test `api` and `coding`. Retry and success-envelope probes wait for the Zhipu and Z.ai preset PR. |
| `zai` | Live | V + A | Chat, Anthropic; Responses in `coding` | Test `api` and `coding`. Retry and success-envelope probes wait for the Zhipu and Z.ai preset PR. |
| `minimax` | Live | V + A | Chat, Anthropic | None. |
| `minimax-cn` | Live | V + A | Chat, Anthropic | None. |
| `stepfun` | Live | V + A | Chat, Anthropic | Test `plan` and `api` base paths. |
| `stepfun-cn` | Live | V + A | Chat, Anthropic | Test `plan` and `api` base paths. |
| `xiaomi` | Live | V + A | Chat, Responses, Anthropic | Test `plan-cn`, `plan-sgp`, `plan-ams`, and `api`. |
| `baidu-qianfan` | Catalog by default and in `personal` and `team`; Live in `api` | S + R; V + A in `api` | Chat, Responses, Anthropic | Test the default and all three regions, including the static models. |
| `tencent-cloud` | Live | V + A | Chat, Anthropic; Responses outside `plan` | Test `plan`, `cn`, `intl`, `enterprise`, and `enterprise-intl`, including region models. |
| `huaweicloud` | Live | V + A | Chat, Anthropic | Test `plan` and `api`. |
| `volcengine` | Catalog by default and in `coding` and `agent`; Live in `api` | S + R; V + A in `api` | Chat, Responses, Anthropic; no Anthropic in `api` | Test the default and all three regions, including region models. |
| `qwen` | Live | V + A | Chat, Anthropic | None. |
| `qwen-cn` | Live | V + A | Chat, Anthropic | None. |
| `qwen-token-plan` | Live | V + A | Chat, Anthropic | None. |
| `bailian-token-plan` | Live | V + A | Chat, Anthropic | Decide listing and probe wait for the Decide PR. |
| `mistral` | Live | V | Chat | None. |
| `groq` | Live | V | Chat, Responses | None. |
| `bedrock` | Catalog | S | Chat, Responses, Anthropic | Test every region. No control-plane list call. |
| `bedrock-mantle` | Live | M + A | Chat, Responses, Anthropic | Test every region and model-dependent Chat and Responses prefixes. |
| `azure` | Deployments on chat and responses; Live on Anthropic | D; A on Anthropic | Chat, Responses; Anthropic on a custom Anthropic base | Match Azure resource hosts. Keep the retired-data-plane risk and base-model fallback result. |
| `ollama-cloud` | Live | V + A | Chat, Anthropic | Decide listing and probe wait for the Decide PR. |
| `nous` | Live | V | Chat | None. |
| `openrouter` | Live | V + A | Chat, Anthropic | Test the Anthropic skip and only-base namespaced error. Decide waits for the Decide PR. |
| `opencode-go` | Live | V + A | Chat, Responses, Anthropic | Client variants wait for the OpenCode preset PR. |
| `clinepass` | Live with `only` and static fallback | V; C with the Cline preset PR | Chat | Test `cline-pass/`, fallback ids, free-model append, and the streamed 1024-token probe. The last three wait for the Cline preset PR. |
| `opencode-zen` | Live | V + A | Chat, Responses, Anthropic | Public-key model selection is covered. Client and free-body variants wait for the OpenCode preset PR. Decide waits for the Decide PR. |
| `kilo` | Live | K with the Kilo preset PR | Chat | Custom list and client variants wait for the Kilo preset PR. |
| `commandcode` | Live | V + A | Chat, Responses, Anthropic | E covers the Go-plan hint. |
| `together` | Live | V | Chat | None. |
| `fireworks` | Live | V | Chat | None. |
| `siliconflow` | Live | V | Chat | Test `cn` and `intl`. |
| `nvidia` | Live | V | Chat | None. |
| `modelscope` | Live | V | Chat, Responses | None. |
| `aihubmix` | Live | V + A + P | Chat, Anthropic | Public image drawer feed adds rows the keyed list omits. |
| `pipellm` | Live | V + A | Chat, Responses, Anthropic | Test the different Chat and Responses base paths. |
| `302ai` | Live | V + A | Chat, Anthropic | None. |
| `cherryin` | Live | V + A | Chat, Responses, Anthropic | None. |
| `yylx` | Live | V + A | Chat, Anthropic | Test `auto`, `global`, and `cn`. |
| `typesafe` | Blocked by `resolve` | X | Decide after the Decide PR | `resolve` and Decide support are blockers. |
| `vercel-jev` | Blocked by `resolve` | X | Decide after the Decide PR | `resolve` and Decide support are blockers. |
| `cloudflare-jev` | Blocked by `resolve` | X | Decide after the Decide PR | `resolve` and Decide support are blockers. |
| `bailian-decision` | Blocked by `resolve` | X | Decide after the Decide PR | Test all three regions after `resolve` and Decide support land. |
| `qwen-decision` | Blocked by `resolve` | X | Decide after the Decide PR | `resolve` and Decide support are blockers. |
| `ollama` | Live | V + A | Chat, Anthropic | Decide listing and probe wait for the Decide PR. |
| `lmstudio` | Live | V | Chat | None. |
| `omlx` | Live | V + A | Chat, Responses, Anthropic | Decide listing and probe wait for the Decide PR. |
| `mlx-serve` | Live | V + A | Chat, Responses, Anthropic | Decide listing and probe wait for the Decide PR. |
| `remote-gateway` | Blocked by `resolve` | X | None until the remote-gateway preset PR | The implementation is missing. That PR owns the gateway list headers and the all-rows filter exception. |

### Testing order

Mock upstreams run in CI first for every preset. The generic mocks, the matrix, and `kits/upstream` conformance must pass before a coverage row can become done.

Live vendor tests come last. The owner runs them by hand with test keys the owner supplies. They never run in CI. A live result is recorded separately from the CI run id. It does not replace the mock tests or the CI evidence.

## Code pull requests

1. The `after_id` pager and the Gemini walker in `hofund-plugin-sdk`. `ListKind::Deployments`, explicit `rank()`, unknown-future-variant handling, the public-API snapshot diff, and a `CHANGELOG.md` line. `list_models` and `probe` on one wire for `openai-compat`, `anthropic`, `generative`, and Azure; and `Catalog` `list_models` plus `probe` for the `google-vertex` and `bedrock` factories. Azure's plugin owns deployments then the base-model fallback. The host aggregator: configured-provider identity; the exact list-only `RawTable` build, with flattened options only for the configured plugin; host-owned `models_url`, `models_refresh`, and `models_auto_include`, with `env:` resolution and full secret redaction for the URL; the custom-`base_url` restriction; the ordered kind bound and Azure host detection; merge, drop, and tag precedence; metadata; OpenRouter's skip; AIHubMix public drawers; Anthropic-only `BothAuth` header learning; `only`, region models, and conditional models.dev limits; and `fixV1` in host state. The two-pass host probe selector and four-wide model-test batch. `APIError` hints in hofund wording. Kernel spans, redaction, and the `model_list` and `provider_test` log kinds. Conformance in `kits/upstream`. Mocks A, D, I, T, P, E, and Q, plus the text-only merge, embedding filter, 20th page, Gemini partial page, 401 body that echoes a key, static preset that does not dial out, Azure host and fallback cases, and `bedrock-mantle` listing live while `bedrock` does not. The scheduler does not start.
2. The host refresh job, persistent host-owned state and due times, the safe state filenames and exact-id check, the picks store, delist and restore, auto-include, dead sides, and the 429 delay. Mock J proves restart persistence, case-distinct and contained state files, mismatched-id handling, and that learned auth and `fixV1` never rewrite TOML.
3. Each `Deferred` row in the behaviour inventory, inside the pull request named there, with mocks L, O, N, and W where those rows apply.

#28 rebases onto `list_models` and `probe` after the first of those pull requests. It stays unmerged until then. It does not define the trait.
