# Provider plugins

This note is the contract for row E of RFC 0001 before the factories change. The plugin boundary in [boundary.md](boundary.md) still applies. This page is only about provider options, the SDK helpers those factories share, and the limits of this first slice.

## Threat model

Plugins are in-repo Rust linked into `hofundd`. A reviewer reads the diff. Nothing here is a sandbox, and nothing here accepts an out-of-tree plugin.

The config file is trusted to name an endpoint. It is not trusted to invent option keys. Each factory has a schema. A key that is not in that schema is a check error. A value of the wrong type is a check error. Both errors name the provider id and the key. They do not include the value. Option tables hold secrets (an AWS secret access key, a credentials path, a refresh token). Echoing the value in an error, a log line, or `Debug` would publish it.

`[providers.options]` is opaque in config on purpose. Config does not know which preset owns which key. The factory does. An AWS key on an OpenAI provider, or a Vertex key on Bedrock, is the same class of error as a typo.

The factory validates option keys and value types before the daemon resolves any option string. Only then are strings trimmed and passed through `hofund_secrets::resolve`. `env:NAME` becomes the variable's value or an error that names the provider id, option key, and variable. The resolved text is not copied into that error. Reserved kernel keys (`id`, `base_url`, `api_key`, `credential`, `quirks`, `headers`, `protocol`) are rejected if they appear inside `options`, so an option cannot overwrite the endpoint the daemon already resolved.

`token_url` and `iam_root` on the Vertex factory choose where the token request is sent. A config author who sets them is the same trust as a config author who sets `base_url`. After `env:` resolution, each non-empty override must be an HTTPS URL without userinfo; HTTP is accepted only for a loopback host (`localhost`, `127.0.0.0/8`, or `::1`). A non-empty `credentials` value must be a full path. `config check` also requires the file to exist, but daemon startup does not: request signing reads it lazily, so a file created later is used by the next request. These value errors name the provider id and option key without copying the value. The token request body is not logged. Production config leaves both URL overrides unset, and the factory uses Google's hosts.

SigV4's result contains the access key id. That is what AWS puts in `Authorization`. The secret access key is an input and is not copied into the result. OAuth parsing returns the access token as a secret. `Debug` and `Display` redact it. Parse errors use a fixed sentence and do not include the response body.

## Non-goals

- Out-of-tree or dynamically loaded plugins.
- PKCE, a loopback callback, device-flow polling, and the fixed Codex port. Those belong to subscription plugins (row H). This slice only parses a token response and builds a refresh form. OAuth token caching, refresh, and single-flight become an SDK helper in row H; Vertex keeps its own cache until then.
- Moving the 58 URL presets into individual provider plugin crates. Per-wire `/models` and minimal probes are implemented in `hofund-provider`; host aggregation and probe model selection live in `hofund-daemon`. Refresh, persistence, and auto-include remain separate work.
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
4. The daemon builds a `RawTable`: `id`, `base_url`, the resolved `api_key`, `credential`, `quirks`, `headers`, and `protocol` (`chat` or `responses` only). It then copies `options` without resolving their strings. A reserved key is `provider `{id}` option `{key}` is reserved`.
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
