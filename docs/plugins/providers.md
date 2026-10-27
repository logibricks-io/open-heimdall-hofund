# Provider plugins

This note is the contract for row E of RFC 0001 before the factories change. The plugin boundary in [boundary.md](boundary.md) still applies. This page is only about provider options, the SDK helpers those factories share, and the limits of this first slice.

## Preset fragments

A preset fragment is the data-only form of a catalog row. Its `[[preset]]` keys intentionally match `crates/provider/presets.toml`, so moving a URL-only row is a cut-and-paste change. The preset shape follows `reference: internal/provider/presets.go:23-66`; regions follow `reference: internal/provider/presets.go:68-100`.

A preset requires `id`, `name`, and `kind`. `kind` is `vendor`, `relay`, `local`, or `remote` (`reference: internal/provider/presets.go:13-19`). The optional keys are:

- `short`, `note`, `catalog`, `website`, `keys_url`, `site_host`, and `key_hint`;
- `no_key`, `no_list`, and `hosts` (booleans), `only`, `models`, and `header_hints`;
- `endpoint`, `endpoint_hint`, `endpoint_needed`, `region_label`, and `decide`;
- the flat wire keys `chat`, `responses`, `anthropic`, and `generative`;
- `config`, a table whose values must be strings;
- `[[preset.region]]` rows.

`generative` is a hofund extension; it is not a field in the reference preset. The other wire names stay flat, as in the reference. The earlier design note's separate `protocols` list and `[preset.urls]` table are not used: non-empty URL keys already declare the available wires, and a separate list would duplicate them and force every moved row to be rewritten.

A region requires `id`. Its optional keys are `name`, `chat`, `responses`, `anthropic`, `decide`, `lists`, `keys_url`, `website`, `site_host`, `catalog`, `models`, `embeddings`, `drawers`, and `videos`. A present region `models` array replaces the preset array, including when it is empty. There is no region `generative` override, matching `Endpoints::of` in `crates/provider/src/presets.rs:450-466`.

A fragment may also contain top-level `[[alias]]` rows with `id`, `preset`, and optional `region`, the same shape as `crates/provider/presets.toml:10-27`. An alias supplies a default region; an explicit user region wins.

Fragments have three locations:

1. Inline `[[preset]]` and `[[alias]]` rows in a provider plugin's `plugin.toml`.
2. Files relative to that plugin directory named by `presets = ["presets.toml"]` in `plugin.toml`, following RFC 0001 section 2.1.
3. `plugins/providers/<builtin-factory-id>/presets.toml` with no `Cargo.toml` or `plugin.toml`. The directory name must be `openai-compat`, `anthropic`, or `generative`. These fragments compile unconditionally because their factories are always linked.

A missing named file or a data-only directory for another id fails the build. Every `presets = [...]` entry must be a plain file name ending in `.toml`, without a slash, backslash, or `..`. Each named file is a Cargo rebuild input. Plugin directories that already follow another rule, such as the code-only `fixture` package, are unchanged.

Validation runs at build time. Preset and alias ids are kebab-case and unique across all fragments, including aliases. Presets and aliases are each sorted by id within one file. Regions retain declared order because the first region is the default (`reference: internal/provider/presets.go:68-69`); region ids are non-empty and unique within their preset and are never sorted. Names are non-empty. Every wire, `decide`, and `endpoint` URL is empty or starts with `http://` or `https://` and contains no whitespace. An alias targets a preset in the same build and, when present, one of its regions. Unknown keys are errors through `deny_unknown_fields`, an intentional improvement over the reference Go literal because a typo in a moved row cannot silently compile. Config values remain strings, but emitted Rust uses debug-string escaping rather than restricting quotes, slashes, or newlines.

A builtin-owned row lives in the directory for its default wire's factory. The default is the first non-empty preset-level URL in `generative`, `chat`, `responses`, `anthropic` order, falling back to the first region's URLs in that order when the preset level has none. Generative belongs in `generative`, Chat or Responses in `openai-compat`, and Messages in `anthropic`. A row with no wire URL also belongs in `openai-compat`. A placement error names the file, preset, and expected directory.

Endpoint selection mirrors `resolve` and `Endpoints::of` in `crates/provider/src/presets.rs:177-262,450-466`:

- A non-empty region wire replaces the preset wire; an empty one falls back to the preset. A region wire may exist even when the preset has no URL for that wire.
- With an explicit `protocol`, the selected wire must have an effective URL whenever the preset has any wire URLs. A builtin-owned row may use any protocol served by the builtin factories; another row is limited to its owning plugin's protocols.
- Without an explicit protocol, the first served URL wins in `generative`, `chat`, `responses`, `anthropic` order. Builtin-owned rows are served per wire by `openai-compat`, `anthropic`, and `generative`, with the plugin chosen from the selected wire exactly as for a catalog row.
- A non-empty provider `base_url` always overrides the selected fragment URL.
- With no wire URLs, the explicit protocol or the first served protocol is used and `base_url` is required. A decide-only row reports that it only classifies a route.

An unknown region error lists valid ids in declared order, unlike the catalog's shorter error in `crates/provider/src/presets.rs:483-489`. Naming a region on a regionless preset is a separate error. Runtime registry tests check builtin-owned wires against the combined builtin protocol set and other wires against their owning factory, and require every fragment id to differ from the remaining catalog ids; these checks need factory data that the build script cannot use.

For a builtin-owned row, credential placement comes from `hofund_provider::credential_for` with the final base URL (the region-resolved URL, or a provider `base_url` override when one is set), the same derivation used for catalog rows. The model-list path, which has no override, uses the region-resolved URL in `crates/provider/src/presets.rs:340-365`. Other plugins keep the default credential. A builtin-owned row with no wire URLs defaults to chat, as `resolve` does for endpoint-style catalog rows; the builtin protocol set is ordered chat, responses, messages, generative for that reason. There is deliberately no fragment `credential` key: host- and preset-sensitive derivation already matches current behavior and the reference behavior, while a key would add a second source of truth.

## Threat model

Plugins are in-repo Rust linked into `hofundd`. A reviewer reads the diff. Nothing here is a sandbox, and nothing here accepts an out-of-tree plugin.

The config file is trusted to name an endpoint. It is not trusted to invent option keys. Each factory has a schema. A key that is not in that schema is a check error. A value of the wrong type is a check error. Both errors name the provider id and the key. They do not include the value. Option tables hold secrets (an AWS secret access key, a credentials path, a refresh token). Echoing the value in an error, a log line, or `Debug` would publish it.

`[providers.options]` is opaque in config on purpose. Config does not know which preset owns which key. The factory does. An AWS key on an OpenAI provider, or a Vertex key on Bedrock, is the same class of error as a typo.

The factory validates option keys and value types before the daemon resolves any option string. Only then are strings trimmed and passed through `hofund_secrets::resolve`. `env:NAME` becomes the variable's value or an error that names the provider id, option key, and variable. The resolved text is not copied into that error. Reserved kernel keys (`id`, `base_url`, `api_key`, `credential`, `quirks`, `headers`, `protocol`, `preset`) are rejected if they appear inside `options`, so an option cannot overwrite the endpoint the daemon already resolved. The daemon passes `preset`, the catalog preset id, to builtin factories; a user's `[providers.options] preset` is rejected as reserved.

`token_url` and `iam_root` on the Vertex factory choose where the token request is sent. A config author who sets them is the same trust as a config author who sets `base_url`. After `env:` resolution, each non-empty override must be an HTTPS URL without userinfo; HTTP is accepted only for a loopback host (`localhost`, `127.0.0.0/8`, or `::1`). A non-empty `credentials` value must be a full path. `config check` also requires the file to exist, but daemon startup does not: request signing reads it lazily, so a file created later is used by the next request. These value errors name the provider id and option key without copying the value. The token request body is not logged. Production config leaves both URL overrides unset, and the factory uses Google's hosts.

SigV4's result contains the access key id. That is what AWS puts in `Authorization`. The secret access key is an input and is not copied into the result. OAuth parsing returns the access token as a secret. `Debug` and `Display` redact it. Parse errors use a fixed sentence and do not include the response body.

## Non-goals

- Out-of-tree or dynamically loaded plugins.
- PKCE, a loopback callback, device-flow polling, and the fixed Codex port. Those belong to subscription plugins (row H). This slice only parses a token response and builds a refresh form. OAuth token caching, refresh, and single-flight become an SDK helper in row H; Vertex keeps its own cache until then.
- Moving the 58 URL presets into individual provider plugin crates. Per-wire `/models` and minimal probes are implemented in `hofund-provider`; host aggregation, probe model selection, refresh, persistent state, and auto-include live in `hofund-daemon`.
- A live call to Google or AWS. Tests use a local mock.
- Moving the Chat, Responses, Messages, Generative, and Converse transports into the SDK and streaming response bodies through `HttpHandle::send`. That call currently buffers the body. The target is the first row E PR that moves an adapter into its own plugin crate (E3, Vertex).
- Lifting Vertex's JWT, ADC, and impersonation mint out of `hofund-provider`. The mint calls the SDK parser. The vendor flow stays next to the adapter.
- TOML floats and datetimes. Config still has no float variant.

## Known limits

The Chat, Responses, Messages, Generative, Vertex, and Bedrock adapters still live in `hofund-provider`. The registry factories validate options and call those adapters. A later pull request can move one adapter into its own plugin crate. This slice deletes the daemon's `Vertex::new` and `Bedrock::new` arms, so every wire is `registry.factory`.

On that path the factory takes the `reqwest` client out of the host's `PooledHttp` and hands it to the adapter. The raw-client method is gated behind the kernel-only feature and is removed in row E, the first PR that moves an adapter into its own plugin crate (E3, Vertex). Connect and idle-read timeouts are the daemon's `HttpLimits`. A connection-establishment failure is retried once because the request was not sent; an error after a possible write is not retried. Hyper separately retries requests it cancelled on an idle socket before writing them. `HttpHandle::send` is still the API a real plugin calls, and it still buffers. The SDK does not build a client.

An adapter constructed in a unit test, with no pooled client attached, still builds a private client. The daemon does not take that path. Vertex token mint has a 30 second total deadline on both private and pooled clients, and concurrent cache misses share one refresh.

`Bedrock`'s `UpstreamSpec` credential is `SigV4`. The adapter rejects an API key and signs each request.

`plan` still reads `project`, `location`, and `region` as strings so `resolve` can build a catalog URL. A wrong type fails there, with the provider id and the key, before the factory runs. The factory repeats the check for a raw table that did not come from `plan`. `google-vertex` and `bedrock` are not compiled presets. `resolve` still owns their URLs. The factory owns construction.

`list_models` and `probe` use the adapter's pooled endpoint client. The SDK pager owns cursor rules; the provider transport enforces body caps while reading. The relay remains unsupported. For `[providers.models].url`, the daemon fetches that exact URL through `HostContext` and the SDK pager rather than treating it as a plugin base. It redacts the resolved URL, its query, and every query value.

The daemon probe selector uses route mappings and listed endpoint metadata. `NamedOnlyOn` applies to OpenCode's Generative wire when that metadata exists. There is no models.dev cache yet, so catalog-only API decoration and the fallback limits cache are absent; `ModelsHost::with_catalog_limits` is the seam for row 2.8. Disabled-model state and per-key visibility do not exist in this slice. The daemon emits `model_list` and `provider_test` tracing events because there is no separate probe request-log store.

## How options reach a factory

1. Config loads `[providers.options]` as a string, integer, boolean, array, or table. An empty or whitespace-only string is omitted, including inside a nested table. `0`, `false`, an empty array, and an empty table are kept.
2. For this release, the old top-level vendor keys are copied into `options` when they are non-empty, and the load records a warning.
3. `plan` reads `project`, `location`, and `region` when it resolves a catalog URL. A missing key is empty. Any other shape is `provider `{id}` option `{key}` must be a string`.
4. The daemon builds a `RawTable`: `id`, `base_url`, the resolved `api_key`, `credential`, `quirks`, `headers`, `protocol` (`chat` or `responses` only), and `preset` (the catalog preset id for builtin factories). It then copies `options` without resolving their strings. A reserved key, including a user-supplied `preset`, is `provider `{id}` option `{key}` is reserved`.
5. `factory.validate` checks the unresolved table against the kernel keys plus that plugin's schema. Anything else is `provider `{id}` has unknown option `{key}``. A non-string on a string field is `provider `{id}` option `{key}` must be a string`.
6. Only after validation, each option string is trimmed and `env:`-resolved. Resolution errors name the provider id and option key. `config check` then calls `factory.preflight` on the resolved table; startup skips this step.
7. The factory validates the resolved table again, and `factory.build` attaches the pooled client and constructs the adapter.

| Factory | Option keys, all strings, all optional |
| --- | --- |
| `openai-compat`, `anthropic`, `generative` | none |
| `google-vertex` | `project`, `location`, `credentials`, `impersonate`, `token_url`, `iam_root` |
| `bedrock` | `region`, `access_key_id`, `secret_access_key`, `session_token` |

`config_schema` lists those option keys so a form can render them. Kernel keys are not options.

## Adversarial cases shipped with the change

- An unknown key on OpenAI, and an AWS key on OpenAI, name the provider and the key and do not include the value.
- A non-string `project` on Vertex names the provider and the key. A table value's nested sentinel is not in the error.
- A Vertex key on Bedrock is an unknown option.
- A whitespace-prefixed `env:` AWS key is trimmed before lookup, so an unset variable is reported as unset.
- SigV4's `Authorization` value does not contain the secret access key.
- A token parse error and `Debug` of the parsed token do not contain the access token or the response body.
- A page that is not JSON produces an error that does not contain the body.
- Bedrock Converse and Vertex `generateContent` run through `registry.factory` against a local mock.
- A built-in call with a short `connect_timeout_ms` fails against an unroutable address without waiting on the adapter's old private timeout.
