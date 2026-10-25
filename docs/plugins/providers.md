# Provider plugins

This note is the contract for row E of RFC 0001 before the factories change. The plugin boundary in [boundary.md](boundary.md) still applies. This page is only about provider options, the SDK helpers those factories share, and the limits of this first slice.

## Threat model

Plugins are in-repo Rust linked into `hofundd`. A reviewer reads the diff. Nothing here is a sandbox, and nothing here accepts an out-of-tree plugin.

The config file is trusted to name an endpoint. It is not trusted to invent option keys. Each factory has a schema. A key that is not in that schema is a check error. A value of the wrong type is a check error. Both errors name the provider id and the key. They do not include the value. Option tables hold secrets (an AWS secret access key, a credentials path, a refresh token). Echoing the value in an error, a log line, or `Debug` would publish it.

`[providers.options]` is opaque in config on purpose. Config does not know which preset owns which key. The factory does. An AWS key on an OpenAI provider, or a Vertex key on Bedrock, is the same class of error as a typo.

String options are trimmed and then passed through `hofund_secrets::resolve` before the factory sees them. `env:NAME` becomes the variable's value or an error that names the variable. The resolved text is not copied into that error. Reserved kernel keys (`id`, `base_url`, `api_key`, `credential`, `quirks`, `headers`, `protocol`) are rejected if they appear inside `options`, so an option cannot overwrite the endpoint the daemon already resolved.

`token_url` and `iam_root` on the Vertex factory choose where the token request is sent. A config author who sets them is the same trust as a config author who sets `base_url`. The token request body is not logged. Production config leaves both unset, and the factory uses Google's hosts.

SigV4's result contains the access key id. That is what AWS puts in `Authorization`. The secret access key is an input and is not copied into the result. OAuth parsing returns the access token as a secret. `Debug` and `Display` redact it. Parse errors use a fixed sentence and do not include the response body.

## Non-goals

- Out-of-tree or dynamically loaded plugins.
- PKCE, a loopback callback, device-flow polling, and the fixed Codex port. Those belong to subscription plugins (row H). This slice only parses a token response and builds a refresh form.
- Moving the 58 URL presets, per-preset `/models`, or a connectivity probe. That is the next pull request.
- A live call to Google or AWS. Tests use a local mock.
- Streaming response bodies through `HttpHandle::send`. That call buffers the body. SSE and Converse streams stay on the pooled client until a later slice.
- Lifting Vertex's JWT, ADC, and impersonation mint out of `hofund-provider`. The mint calls the SDK parser. The vendor flow stays next to the adapter.
- TOML floats and datetimes. Config still has no float variant.

## Known limits

The Chat, Responses, Messages, Generative, Vertex, and Bedrock adapters still live in `hofund-provider`. The registry factories validate options and call those adapters. A later pull request can move one adapter into its own plugin crate. This slice deletes the daemon's `Vertex::new` and `Bedrock::new` arms, so every wire is `registry.factory`.

On that path the factory takes the `reqwest` client out of the host's `PooledHttp` and hands it to the adapter. Connect and idle-read timeouts are the daemon's `HttpLimits`, including Vertex token mint. `HttpHandle::send` is still the API a real plugin calls, and it still buffers. The SDK does not build a client.

An adapter constructed in a unit test, with no pooled client attached, still builds a private client. The daemon does not take that path. Vertex's private client keeps a 30 second whole-call budget for token mint. The factory path does not: mint uses `HttpLimits` like every other call.

`plan` still reads `project`, `location`, and `region` as strings so `resolve` can build a catalog URL. A wrong type fails there, with the provider id and the key, before the factory runs. The factory repeats the check for a raw table that did not come from `plan`. `google-vertex` and `bedrock` are not compiled presets. `resolve` still owns their URLs. The factory owns construction.

`list_models` stays unsupported. The paging helper only reads a JSON page. It does not send a request.

## How options reach a factory

1. Config loads `[providers.options]` as a string, integer, boolean, array, or table. An empty or whitespace-only string is omitted, including inside a nested table. `0`, `false`, an empty array, and an empty table are kept.
2. For this release, the old top-level vendor keys are copied into `options` when they are non-empty, and the load records a warning.
3. `plan` reads `project`, `location`, and `region` when it resolves a catalog URL. A missing key is empty. Any other shape is `provider `{id}` option `{key}` must be a string`.
4. The daemon builds a `RawTable`: `id`, `base_url`, the resolved `api_key`, `credential`, `quirks`, `headers`, and `protocol` (`chat` or `responses` only). It then copies `options`. Each string is trimmed and `env:`-resolved. A reserved key is `provider `{id}` option `{key}` is reserved`.
5. `factory.validate` allows the kernel keys plus that plugin's schema. Anything else is `provider `{id}` has unknown option `{key}``. A non-string on a string field is `provider `{id}` option `{key}` must be a string`.
6. `factory.build` attaches the pooled client and constructs the adapter.

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
