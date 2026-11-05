# Devin subscription upstream (R-12, R-13)

Status: design only. The subscription crate already imports CLI credentials, discovers agent sign-ins, writes agent TOML, and returns an upstream handle that refuses chat (`plugins/subscriptions/devin/src/upstream.rs:20-35`). This document is the contract for serving a model call on a Devin account through the same Windsurf/Codeium backend the Devin CLI uses.

The wire contract is the reference's JavaScript subscription plugin (`packages/devin/index.mjs`, tests listed in section 7). The Go provider files (`internal/provider/devin.go`, `devin_accounts.go`, `devin_images.go`) cover CLI install paths, catalog shaping, and migration readers; they do not open `GetChatMessage`. Where Go and JavaScript disagree, JavaScript is the wire contract.

Vendor names stay inside `plugins/subscriptions/devin`. Kernel changes named here are generic HTTP or shared Connect helpers. Plugins still depend only on `hofund-plugin-api` and `hofund-plugin-sdk` (`xtask/src/boundary.rs` `link_allowance`, `ALLOWED_DEPS`).

## 0. Decision, and where it is weak

Ship one unary Connect RPC per `UpstreamProvider::execute`: `POST {api_server}/exa.api_server_pb.ApiServerService/GetChatMessage` with a single framed protobuf body and a framed protobuf stream back. Part 1 (R-12) returns streamed **text** on a real account, live **GetCliModelConfigs** (plus variant resolution), PKCE sign-in, CLI install guidance, gzip on large bodies, and metadata that matches the pinned CLI version. Part 2 (R-13) adds tools, inline images, reasoning replay and thinking signatures, usage and list prices, quota windows, full error classification, cache keepalive, and the Messages-shaped history rules (cache marks, split calls, gemini schema cleaning).

This will be wrong if R-12 silently drops tools or images. A tool-bearing request encoded with an empty tool list can still make the model call a tool the client never sees. R-12 must refuse requests that need R-13 fields with a fixed, router-safe error (section 4.2), not send a partial wire shape.

This will also be wrong if R-12 implements keepalive. Keepalive sends extra billed `GetChatMessage` calls on a timer (`index.mjs:2248-2486`, `keepalive.test.mjs`). It depends on session affinity headers the gateway does not yet pass into plugins (`index.mjs:2291-2295`, `plugins#620`). Defer keepalive to R-13 and keep it **off by default** (`README.md` lines 95-96, `index.mjs:3093-3095`).

This will be wrong if hofund reuses the Cursor duplex transport extension for Devin. Devin never opens a second write path; one POST carries the full frame (`roundtrip`, `index.mjs:1818-1832`). Adding `open_duplex` for Devin would be dead code and would blur the seam audit.

Attack: treating `list_models` as a static JSON fixture. The reference refreshes from `GetCliModelConfigs` every five minutes and falls back to `devin models list` only when the RPC fails and the CLI answers for the same account (`configsFor`, `index.mjs:482-515`; `ownFamilies`, `index.mjs:862-877`). R-12 must hit the RPC first; the fixture in `plugins/subscriptions/devin/src/fixtures/models.json` is test-only.

## 1. Wire protocol

### 1.1 Endpoints and environment

| Purpose | Path | Content-Type | Source |
|---|---|---|---|
| Chat | `/exa.api_server_pb.ApiServerService/GetChatMessage` | `application/connect+proto` | `CHAT_RPC`, `index.mjs:31` |
| Model configs | `/exa.api_server_pb.ApiServerService/GetCliModelConfigs` | `application/proto` | `MODEL_CONFIGS_RPC`, `index.mjs:349` |
| Account identity / quota JSON | `/exa.seat_management_pb.SeatManagementService/GetUserStatus` | `application/json` | `userStatus`, `index.mjs:2987-2995` |
| PKCE token exchange | `/exa.seat_management_pb.SeatManagementService/ExchangeDevinCLIPKCECode` | `application/json` | `EXCHANGE`, `index.mjs:30`; `exchange`, `index.mjs:908-914` |
| Browser PKCE start | `https://app.devin.ai/auth/cli/continue` | redirect | `AUTHORIZE`, `index.mjs:29`; `browserSignIn`, `index.mjs:945-999` |

Default API origin: `https://server.codeium.com` (`SERVER`, `index.mjs:28`). Stored accounts carry `api_server` from `credentials.toml` (`windsurf_api_key`, `api_server_url`, `devin_api_url`; `plugins/subscriptions/devin/src/creds.rs:111-147`). The reference also honors `WINDSURF_API_SERVER_URL` at request time (`live`, `index.mjs:3084`). Hofund reads the env var only during `execute` / `list_models` for the account being billed, not process-wide.

EU host `eu.codeium.com` is a stored URL choice, not a separate code path (`docs/coverage/subscription-parity.md` row m). No automatic region retry like Cursor; a wrong host fails with transport or `unavailable` and the router may failover.

CLI version pin sent on the wire: `3000.11.3` (`CLI_VERSION`, `index.mjs:33`; `metadata`, `index.mjs:1414-1415`).

### 1.2 Connect framing (request and response)

`frame` / `frames` (`index.mjs:1138-1177`). Same 5-byte header as Cursor (`docs/cursor-upstream-design.md` section 1.1): flags, big-endian length, payload. Flag bit 0: gzip payload. Flag bit 2: stream end with JSON error body, not protobuf (`frames` yield `{ end, data }`; `failure`, `index.mjs:1684-1705`).

Request gzip threshold: `GZIP_AT = 8 << 10` (8192 bytes) (`index.mjs:1138-1146`). When set, flag byte is 1 and header `Connect-Content-Encoding: gzip` is added (`rpcHeaders`, `index.mjs:1813-1814`). The CLI does not send `Connect-Accept-Encoding` on the request (`gzip-request.test.mjs` lines 51-52). Response frames may still arrive gzip-compressed (flag bit 0); inflate before parsing.

Oversize wire body: nginx returns 413 above ~10 MiB (`README.md` lines 78-79; `gzip-request.test.mjs` lines 1-4). Hofund gzips before send when over 8192 bytes; do not send 10 MiB plain.

Malformed stream: payload length above 64 MiB throws (`index.mjs:1157`). Trailing bytes after the last frame throw `unexpected EOF` (`index.mjs:1167`). `fields` drops a malformed tail (`index.mjs:1091-1129`).

Connect error JSON at stream end: `{code, message}` or `{error:{code,message}}` (`failure`, `index.mjs:1684-1691`).

### 1.3 GetChatMessage HTTP headers

`rpcHeaders(key, body)` (`index.mjs:1813-1814`):

| Header | Value |
|---|---|
| `Content-Type` | `application/connect+proto` |
| `Connect-Protocol-Version` | `1` |
| `Authorization` | `Basic ` plus the session token (not Bearer) |
| `Connect-Content-Encoding` | `gzip` only when frame flag bit 0 is set |

Chat uses `fetch(server + CHAT_RPC, { method: "POST", body, signal })` (`index.mjs:1827-1831`). No HTTP/2 pin is required; the reference uses ordinary HTTPS POST with a full body buffer.

Timeouts: `userStatus` and `GetCliModelConfigs` use 15 s (`index.mjs:2994`, `index.mjs:492`). PKCE `exchange` uses 30 s (`index.mjs:913`). Sign-in loopback uses `SIGN_IN_TIMEOUT = 10 * 60 * 1000` (`index.mjs:37`, `browserSignIn`). Hofund should use `Call` with explicit per-RPC budgets matching those values rather than relying on the default 60 s read idle alone for unary JSON RPCs.

Non-OK HTTP before streaming: read body capped at 1 MiB (`index.mjs:1836`) and pass through `failure`.

### 1.4 Client metadata protobuf (`metadata`)

Built in `metadata(key)` (`index.mjs:1414-1415`), embedded as field 1 bytes of the chat request:

| Field | Type | Value |
|---|---|---|
| 1 | string | `devin-cli` |
| 2 | string | `CLI_VERSION` (`3000.11.3`) |
| 3 | string | session token (same as Basic auth secret) |
| 4 | string | `en` |
| 5 | string | OS name (`windows` on win32, else `process.platform`) |
| 7 | string | `CLI_VERSION` again |
| 12 | string | `chisel` |
| 28 | string | `chisel` |

`GetCliModelConfigs` sends the same metadata message as the unary body (`configsFor`, `index.mjs:488-491`).

### 1.5 GetChatMessage request (`encodeRequest`)

Outer message (`encodeRequest`, `index.mjs:1599-1632`):

| Field | Wire | Role |
|---|---|---|
| 1 | bytes | Client metadata (`metadata(key).done()`) |
| 3 | bytes (repeat) | Each `ChatMessagePrompt` from `encodeMsg` |
| 7 | varint | constant `5` |
| 8 | bytes | Sampling parameters (below) |
| 10 | bytes (repeat) | Tool definitions offered on the wire |
| 21 | string | Resolved model **uid** (variant id), not the family id the client asked for |

Sampling submessage (field 8) (`index.mjs:1608`):

| Field | Type | Role |
|---|---|---|
| 1 | varint | `1` |
| 2 | varint | `max_tokens` (from request; default cap 128000 in builder) |
| 3 | varint | `400` |
| 5 | double | temperature (`above0`: exact `0` becomes `1e-6`, `index.mjs:1605-1607`) |
| 7 | varint | `40` |
| 8 | double | top_p (`above0`, default 0.95) |

Tool definition (field 10 element) (`index.mjs:1614-1618`):

| Field | Type | Role |
|---|---|---|
| 1 | string | tool name |
| 2 | string | description or pointer text |
| 3 | bytes/string | JSON schema string |

Each `ChatMessagePrompt` (`encodeMsg`, `index.mjs:1639-1658`):

| Field | Type | Role |
|---|---|---|
| 1 | string | random UUID per prompt |
| 2 | varint | role: `USER=1`, `ASSISTANT=2`, `TOOL=4` (`index.mjs:1182-1184`) |
| 3 | string | text |
| 6 | bytes (repeat) | tool call: inner 1 id, 2 name, 3 args JSON |
| 7 | string | tool result answers this call id |
| 8 | bytes | cache mark: inner field 1 varint `1` (EPHEMERAL) |
| 10 | bytes (repeat) | image: inner 1 base64 data, 2 mime type |
| 11 | string | thinking text replay |
| 12 | string | thinking signature |
| 18 | string | signature type (`anthropic`, etc.) |

Keepalive ping: same bytes as the real request plus one more user prompt `"continue"` and sampling max `1` (`keepalivePing`, `index.mjs:1631-1637`; `keepalive.test.mjs` `isPing`).

### 1.6 Streamed response events (`events`)

Server frames decode to deltas (`events`, `index.mjs:1711-1789`):

| Top-level field | Wire | Maps to |
|---|---|---|
| 3 | string | text delta |
| 9 | string | reasoning delta |
| 10 | string | thinking signature delta |
| 21 | string | signature type delta |
| 6 | bytes | tool call: inner 1 id, 2 name, 3 args fragment |
| 5 | varint | stop reason: `10` → tool_calls, `3` → length |
| 7 | bytes | usage: inner 2 input, 3 output, 4 cache write, 5 cache read |

After the stream ends cleanly, if no text, reasoning, or tool call was seen, emit error status 502 with message containing `empty reply` (`index.mjs:1774-1776`; `empty-reply.test.mjs`; captured bytes in `testdata/empty-reply.bin`).

If usage reports zero output but content was seen, estimate output as `floor((said + 3) / 4)` (`index.mjs:1778`).

### 1.7 Opening the stream before success

`roundtrip` reads the first frame before returning the iterator (`index.mjs:1839-1860`):

- Immediate end frame with error JSON → map through `failure`, return error without opening a success stream (`errors.test.mjs` lines 87-91).
- Immediate end with `{}` on a metadata-only success → 502 `an empty reply` (`errors.test.mjs` lines 95-101).
- EOF before content → 502 (`errors.test.mjs` lines 81-85).
- First content event must arrive before hofund yields `StreamEvent::TextDelta` to the router (comment at `index.mjs:1851-1852`): failover before the first byte is only safe when nothing was yielded.

Hofund must use `hofund_codec` empty-reply wording for router rest rules: `EMPTY_REPLY_MESSAGE` in `crates/kernel/codec/src/responses_empty.rs:6` (`an empty reply (the model answered nothing; try again)`). The reference's shorter `"an empty reply"` string does **not** contain that substring; copying it would incorrectly rest the member (`docs/cursor-upstream-design.md` section 4.2).

### 1.8 GetCliModelConfigs response (R-12)

Unary protobuf body (`parseModelConfigs`, `parseConfigFamilies`, `parseFree`, `parsePrices`, `index.mjs:354-426`):

Repeated `client_model_configs` (top-level field **1**). Per config:

| Field | Type | Used for |
|---|---|---|
| 1 | string | display label |
| 5 | varint | `supports_images` (proto3 false omitted) |
| 18 | varint | context size |
| 22 | string | `model_uid` |
| 23 | bytes | `model_info`: inner 13 output limit, 23 family uid |
| 24 | varint | `model_cost_tier` (`FREE_TIER = 4`, `index.mjs:375`) |
| 30 | bytes | `family_metadata`: inner 1 family label |
| 32 | bytes (repeat) | price row: 1 label (`Input`, `Cached input`, `Output`), 2 float32, 3 unit (`1M tokens`) |

`parseConfigFamilies` skips families whose id is `adaptive` or `fusion` (`index.mjs:460-461`). Aliases come from the static `SNAPSHOT` table when the family id matches (`index.mjs:464-465`).

Cache: success cached `FRESH = 5 * 60 * 1000` (`index.mjs:39`); failed refresh retry after `RETRY = 60 * 1000` (`index.mjs:40`, `configsFor` `index.mjs:508-512`).

## 2. Protobuf approach

Reuse the Cursor design's direction (`docs/cursor-upstream-design.md` section 2): a **vendor-neutral Connect helper** in `hofund-plugin-sdk`, not `prost`.

| Piece | Location | Rationale |
|---|---|---|
| `fields`, malformed-tail rule | Shared SDK module | Same algorithm as `index.mjs:1091-1129` and Cursor `index.mjs:1511-1560` |
| `frame` / deframe / `frames` iterator | Shared SDK | Identical framing; Devin request gzip is chosen before `frame`, not inside it |
| `gzip` inflate for flag bit 0 | `hofund_plugin_sdk::inflate::gzip` (Cursor R-8) | `flate2` with pure backend; boundary-legal |
| Chat-specific messages | `plugins/subscriptions/devin/src/connect/` | Field numbers in section 1; no Codeium names in SDK |

Attack: copying the JavaScript `PB` class twice. Defense: SDK exports `ProtoBuf` builder and `Field` parser only; Devin and Cursor plugins each own message assembly tables with golden round trips.

Attack: `prost` code generation. Still forbidden on plugins (`xtask/src/boundary.rs`). Still no checked-in `.proto` from the vendor.

Numeric range: decode varints as `u64`. Tool stop reasons and usage counts are safe; do not copy JavaScript `Number` past 2^53.

## 3. Transport inside hofund

### 3.1 What exists

`HttpHandle::send` returns `http::Response<BodyStream>` (`crates/kernel/plugin-api/src/host.rs:48-52`). That is enough for Devin chat:

- Build the full framed body in memory (gzip optional).
- POST once.
- Read the response body as a byte stream and deframe.

No `open_duplex`, no HTTP/2 pin, no client heartbeats on the wire.

`Call::open` (`plugin-sdk/src/transport.rs:7-10`) keeps a 2xx body unread; use it for `GetChatMessage` so empty-reply and Connect end-frame errors can be handled before emitting stream events. Do **not** use `Call::send`, which buffers the entire response and hides streaming backpressure until complete—acceptable for `GetCliModelConfigs`, not for chat.

Request compression: gzip the protobuf **before** framing when `msg.len() > 8192` (`index.mjs:1139-1146`). The kernel need not expose a separate compression API; the plugin compresses with the same SDK helper Cursor uses for inflate (symmetric gzip in SDK, or `flate2` encode in the Devin plugin only if encode stays plugin-local—prefer SDK `gzip::compress` mirroring inflate to keep `flate2` in one workspace place).

Streaming read: poll `BodyStream` on the execute task. Idle timeout should follow plugin-host defaults; Devin's own stall semantics are simpler than Cursor (no quiet clock on Devin wire). If the socket goes silent mid-stream, map to `PluginErrorKind::Interrupted` with text `the reply broke off: …` (`events`, `index.mjs:1720-1722`).

### 3.2 Minimal kernel extension

**None required for R-12 chat**, if `Call::open` is available to subscription plugins.

Optional later (R-13 keepalive reporting): a host callback to record usage rows for pings that never pass the gateway—mirrors `reportPing` (`index.mjs:2435-2440`). Defer until keepalive lands; do not block R-12 on daemon usage plumbing.

Attack: enabling global HTTP/2 on the pooled client for Devin. Devin does not need it; leave the unary pool on HTTP/1.1 (`docs/cursor-upstream-design.md` section 3.1).

## 4. Request mapping

`UpstreamProvider::execute` receives canonical `hofund_ir::Request`. The gateway has already decoded Chat, Responses, or Messages. The Devin plugin maps IR → `encodeRequest` → stream events → `Completion::Stream`.

Inbound protocol id may affect **outbound** client shape when hofund re-encodes, but the upstream wire is always `GetChatMessage`. The reference's separate `build` vs `buildMessages` (`CHAT_WIRE` / `MESSAGES`, `index.mjs:1810-1811`) is about client JSON APIs, not a second upstream.

### 4.1 Shared rules (R-12 baseline)

Model id: resolve with `variantFor(families, model, effort)` (`index.mjs:234-272`, `variantFor` tests in `request.test.mjs` / snapshot families). R-12 implements family list + effort/nearest level + fast/priority tiers (`LEVEL_RANK`, `TIERS`, `index.mjs:136-183`). Until `list_models` returns live families, `execute` may use `SNAPSHOT`-equivalent data only in tests; production must call `configsFor` first.

System and developer text: prepend to the first user message with tool description block (`build`, `index.mjs:1572-1590`). R-12 includes **no** tool descriptions because R-12 refuses tools.

Default user nudge when the transcript ends on assistant without pending calls: `"Please proceed with the task."` (`index.mjs:1570-1570`).

Sampling: map IR generation params to field 8; apply `above0` for temperature and top_p. Other params (penalties, stop, seed) have no wire home—ignore without failing.

Structured output, tool_choice, images, tool calls, thinking replay: **R-13** (sections 4.2–4.3).

Effort: map `GenerationParams::effort` through the same strings as `effortOf` (`index.mjs:1219-1227`).

### 4.2 Part 1 (R-12): text stream and model configs

**Successful execute** returns:

1. `MessageStart` with generated id and requested model id (client-facing family id, not only uid).
2. `ContentBlockStart` text, `TextDelta` for each field-3 string.
3. On clean end: `MessageDelta` stop `EndTurn`, block stop, message stop.
4. Omit usage in R-12 even if field 7 arrives (R-13).

Before `Ok(Completion::Stream)`, buffer until the first text delta or a terminal error, matching `roundtrip` (`index.mjs:1851-1858`).

**list_models (R-12)** calls `GetCliModelConfigs`, parses with `parseConfigFamilies`, merges image flags via `parseModelConfigs`, applies `listed()` catalog rules (`index.mjs:274-320`). Fallback order matches `familiesFor` (`index.mjs:856-858`): RPC → CLI JSON (`devin models list --format json`, `parseFamilies` `index.mjs:112-134`) when CLI answers for the account → static snapshot for offline tests only.

**Refusals (R-12)** before RPC when the IR contains:

- any tool definition, tool call, or tool result;
- any image part;
- thinking blocks that require signature replay (R-13).

Return `PluginError::status_error` with `without_cooldown` and fixed phrases (pattern from Cursor R-8): e.g. `tools are not served on Devin until R-13` / `images are not served on Devin until R-13`. Status 404 keeps default failover without resting the account.

Attack: passing tools with empty field 10 but tool calls still in history—still unsafe. Refuse when history contains tool calls or results, not only when `tools` is non-empty.

### 4.3 Part 2 (R-13): tools, images, reasoning, usage, errors

Implement the reference builders in full:

| Behavior | Reference |
|---|---|
| Join split assistant turns | `joinSplitCalls`, `index.mjs:1197-1209`; `split-calls.test.mjs` |
| Unanswered tool calls → `NO_RESULT` | `index.mjs:1186`, `build` `answer()` `index.mjs:1475-1477`; `request.test.mjs` |
| Tool descriptions in system block; wire tools use pointer descriptions | `toolDescriptions`, `index.mjs:1242-1246`; `encodeRequest` `index.mjs:1619` |
| Tool id sanitization `outID` / `inID` | `index.mjs:1423-1428`; `tool-ids.test.mjs` |
| Inline images and tool-result images in following user message | `inlineImage`, `FROM`/`NOTE`, `index.mjs:1443-1466`; `images.test.mjs`, `tool-images.test.mjs` |
| `objectRoot` on schemas | `index.mjs:1262-1327`; `schema.test.mjs` |
| `geminiSchema` for Gemini uids | `index.mjs:1357-1407`; `gemini-schema.test.mjs` |
| Cache marks on Messages-shaped history | `buildMessages`, `index.mjs:1964-2104`; `cache-marks.test.mjs` |
| Reasoning unsigned replay on non-Claude; signed on Claude | `build` `index.mjs:1509-1515`; `reasoning-history.test.mjs`, `thinking-signature.test.mjs` |
| `sigTypes` map for signature types | `index.mjs:1661-1673` |
| Structured output prose in system | `formatText`, `index.mjs:1435-1440`; `structured-output.test.mjs` |
| Stream tool calls and args | `events` field 6, `index.mjs:1748-1761`; map to IR tool events |
| Usage on finish | `events` field 7, `index.mjs:1763-1788`; `usage.test.mjs` |
| Free tier model set | `parseFree`, `index.mjs:380-393`; `free.test.mjs` |
| Price map | `parsePrices`, `index.mjs:401-425`; `prices.test.mjs` |
| Quota windows from `GetUserStatus` | `usage`, `index.mjs:3008-3056`; `usage.test.mjs` |
| Error mapping | `failure`, `index.mjs:1684-1705`; `errors.test.mjs` |

Messages inbound via IR: preserve block boundaries for cache marks; do not merge marked blocks (`README.md` lines 64-73; `buildMessages` comment `index.mjs:1956-1963`). Strip billing header line (`BILLING_LINE`, `index.mjs:1931`).

Outbound thinking signatures: stream signature deltas on Claude uids only (`completeMessages` stream path, `index.mjs:2210-2228`). Do not emit fake signatures.

## 5. Tokens and identity

### 5.1 Credential sources

| Source | Reference | Hofund today | R-12/R-13 |
|---|---|---|---|
| Browser PKCE | `browserSignIn`, `exchange`, `index.mjs:945-924` | Bridge import only (`lib.rs:105-127`) | R-12: loopback PKCE + `ExchangeDevinCLIPKCECode`; store token in account secret only |
| CLI `credentials.toml` | `cliSignIn`, `readCredentials`, `index.mjs:655-667` | `discover`, `creds.rs` | Done; extend with `live` semantics |
| CLI key rotation | `live` re-reads TOML when `metadata.cli` (`cli-key.test.mjs`; `index.mjs:3079-3087`) | Not on chat path | R-12: before each `execute`, if account marked CLI-sourced, re-read agent/XDG TOML under lease |

Token shape: `devin-session-token$…` (`README.md` lines 9-10). Authorization is **Basic**, not Bearer (`README.md` line 42; `rpcHeaders`).

No scheduled refresh on the chat path (`README.md` lines 32-36). `Subscription::refresh` remains a no-op load unless a future CLI expiry appears. On 401, mark account needs sign-in; do not silently delete the secret on a single failed chat (`errorResponse` `X-Magpie-Sign-In: kept`, `index.mjs:1796-1799`).

### 5.2 Identity for account labels

Prefer `GetUserStatus` JSON via `whoByKey` (`index.mjs:802-818`, `identity.test.mjs`). Request body metadata mirrors CLI fields (`index.mjs:2991-2993`). Parse `whoOfStatus` (`index.mjs:780-796`): email, plan tier name, orgId, teamId.

Fallback when RPC inconclusive: isolated CLI home under plugin cache (`homeFor`, `index.mjs:667-678`; `README.md` lines 22-28), **never** the user's real `~/.local/share/devin` for naming—run `devin auth status` with `XDG_DATA_HOME` pointed at `~/.cache/hofund-devin-auth/<hash>/` (hash of token, same idea as reference OpenCode cache path in README).

Windows: do not trust CLI status for a browser token when CLI reads `%APPDATA%\devin\` regardless of home (`cliAnswersFor`, `index.mjs:821-840`; `identity.test.mjs`).

### 5.3 Lease

Any PKCE exchange, CLI TOML re-read for `live`, or refresh that touches secrets must take `accounts().lease(id, 30s)` first (`HostContext` pattern in `docs/cursor-upstream-design.md` section 5.1). Serialize with the reference's single-flight CLI refresh intent (`cli-key.test.mjs`).

### 5.4 Keepalive (R-13 only)

When enabled via subscription config (mirror `cacheKeepalive`, `keepaliveSettings`, `index.mjs:2279-2288`):

- Schedule from **request sent** time, not reply end (`index.mjs:2446-2453`; `keepalive.test.mjs`).
- Stop on real request for same `keepaliveKey` (`index.mjs:3122-3129`).
- Stop on 401, 402, 429; two consecutive cache misses; max pings (`index.mjs:2478-2482`).
- Pings bypass the gateway (`README.md` lines 136-137).

Hofund lacks session headers on `Request` today. Until the IR or host exposes session affinity, keepalive must remain off and undocumented in product UI, or keys will be wrong (`keepaliveKey`, `index.mjs:2316-2340`).

### 5.5 CLI install guidance (R-12)

Match Go install hints (`internal/provider/install.go:56-58`): curl install.sh / PowerShell setup.ps1. Optional `devin setup` note: interactive (`install.go:119-123`). Hofund does not run setup during sign-in; document in sign-in challenge text only.

### 5.6 What is never written

Account store is the only durable secret. Chat does not write `credentials.toml` except `write_agent_sign_in`. Do not log Basic auth or field-3 metadata tokens. Temp dirs for CLI runs must be removed on drop (Cursor design section 5.3 pattern).

## 6. Error classification

Map using `failure` / `statusOf` order (`index.mjs:1684-1705`) and `errors.test.mjs`. Append hofund-specific sign-in hint **without** naming the reference product: e.g. `sign in to Devin again` instead of copying the plugin suffix (`index.mjs:1695`).

| Condition | HTTP / kind | Router rest notes |
|---|---|---|
| `unauthenticated`, HTTP 401 | 401 / `Auth` | Account kept; user must re-sign-in |
| `resource_exhausted`, quota/rate limit words | 429 / `Quota` | Message prefix `usage limit reached:` |
| content policy | 400 / `Status` | No special failover phrase |
| too long / context length | 400 / `Status` | Prefix `input is too long for the model's context:` |
| `invalid_argument` | 400 / `Status` | Includes bad Gemini schema requests |
| `unavailable` | 503 / `Connect` or `Transport` | |
| empty reply before stream | 502 / `Status` with `EMPTY_REPLY_MESSAGE` | `rests` false; failover allowed |
| stream break mid-flight | 502 / `Status` | `the reply broke off:` → may rest |
| HTTP 402 | 402 / `Status` | Payment Required text |
| metadata-only success | 502 empty reply | Same as reference |

Free-tier holds: when quota windows say exhausted but model uid is in `parseFree` set, router should still allow SWE-2-class models (`usage` `notModels`, `index.mjs:3041-3047`; R-13).

Model not allowed: surface as 400/403 with message intact; classify as `Status`, not `Auth`, unless text says unauthenticated.

## 7. Testing

### 7.1 Local fake server

TCP or `mockito` HTTP server in `devin` plugin tests:

- Assert path, headers (`Connect-Protocol-Version`, Basic auth, gzip header when body flag bit 0).
- Return canned frame bytes from `packages/devin/testdata/` (`empty-reply.bin`, `model-configs.bin`, `model-configs-1015.bin`).
- Stream multi-frame replies built from `PB` golden vectors.

No network in CI.

### 7.2 Golden frames

Port assertions from reference tests:

| Test file | Property |
|---|---|
| `gzip-request.test.mjs` | 8083 plain vs 8333 gzipped threshold |
| `request.test.mjs` | Tool history encoding |
| `empty-reply.test.mjs` | Usage-only body → 502 before stream |
| `errors.test.mjs` | Connect end JSON → status mapping |
| `cache-marks.test.mjs` | Field 8 on marked prompts |
| `keepalive.test.mjs` | Ping timing, `continue` tail prompt |
| `tool-ids.test.mjs` | `dv_` encoding |
| `thinking-signature.test.mjs` | Fields 11–12–18 round trip |

Mutation-worthy: change a field number in decode and expect golden test failure; truncate a frame and expect `fields` partial parse behavior.

### 7.3 Subscription conformance

Extend `plugins/subscriptions/devin/src/conformance.rs` and `tests.rs`:

- `a2b_*` lifecycle stays.
- Add upstream probe success against fake server (R-12).
- Run `hofund-kit-subscription` scenarios shared with Codex/Cursor plugins where applicable.

### 7.4 Workspace verify

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 8. Work breakdown

### 8.1 R-12 (estimated ~2.5k–3.5k LOC, high wire risk)

| Area | Files | Notes |
|---|---|---|
| Connect codec (Devin messages) | `plugins/subscriptions/devin/src/connect/{mod,chat,configs}.rs` | Field tables in section 1 |
| Shared Connect helpers (if not from Cursor merge) | `crates/kernel/plugin-sdk/src/connect/{frame,proto,gzip}.rs` | Coordinate with Cursor R-8 to avoid duplicate `flate2` policy |
| Upstream execute + probe | `upstream.rs`, `stream.rs` | Replace `unsupported()` |
| Model list + variant picker | `models.rs`, `discover.rs` touch | Wire `list_models` to RPC |
| PKCE sign-in | `sign_in.rs` (new), `lib.rs` | Loopback server via host loopback handle |
| CLI install copy | `lib.rs` or `sign_in.rs` | Strings only |
| `live` + CLI re-read | `creds.rs`, `session.rs` (new) | Lease guarded |
| Tests + fake server | `upstream/tests.rs`, golden blobs under `src/testdata/` | Port JS binaries |
| Config schema | `plugin.toml`, optional `cache_keepalive` deferred | |

**Deferred from R-12 to R-13:** tools, images, reasoning signatures, usage in stream, quota RPC, keepalive, free/prices enrichment on quota, full Messages cache path, gemini/objectRoot schema fixes, structured output.

**Risks:** wrong model uid breaks silently (400 invalid_argument); gzip off-by-one breaks nginx; Basic vs Bearer mis-auth looks like 401 storm; empty-reply wording breaks failover rests.

### 8.2 R-13 (estimated ~3k–4k LOC, high behavioral risk)

| Area | Files | Notes |
|---|---|---|
| IR → full `build`/`buildMessages` | `map/chat.rs`, `map/messages.rs` | Single IR entry calling both paths internally |
| Tool + image streaming | `upstream.rs` | Field 6 events |
| Reasoning + signatures | `thinking.rs` | Global `sigTypes` with cap 512 |
| Quota + usage | `quota.rs` | `GetUserStatus` JSON |
| Keepalive scheduler | `keepalive.rs` | Requires session key from host; off default |
| Error taxonomy tests | `errors.rs` | Parity with `errors.test.mjs` |
| Feature parity + comparison table rows | docs only in same PRs | Evidence links |

**Deferred beyond R-13:** OpenCode v2 `LanguageModelV3` adapter (reference `devinLanguage`, `index.mjs:2662+`); magpie-only usage ping reporting until daemon exposes a hook; Anthropic `/devin/v1/messages` as a separate HTTP API (reference still uses `GetChatMessage` for Messages-shaped clients—hofund IR path only).

### 8.3 Doc and parity updates (each PR)

- `docs/feature-parity.md` row 3.5 evidence (upstream design link).
- `docs/coverage/subscription-parity.md` Devin rows e, g, l, q, o as PRs land.
- `docs/magpie-comparison.md` Devin upstream row when behavior changes (implementation PRs, not this design PR).

## 9. Adversarial checklist (design author)

1. **Session-less keepalive** will ping the wrong cache or spam six full-context reads—keep off until session metadata exists.
2. **R-12 tool refusal** must inspect conversation history, not only `Request::tools`.
3. **Shared SDK Connect** without Cursor R-8 merged first duplicates gzip policy—sequence SDK PR before or with Devin R-12.
4. **CLI live re-read** without lease races two executes on rotated TOML—always lease.
5. **Windows CLI naming** must not overwrite browser account labels—keep `cliAnswersFor` logic.
6. **Empty reply string** must match `EMPTY_REPLY_MESSAGE` exactly.
7. **estimate output tokens** when usage zero (`index.mjs:1778`) can skew billing—R-13 should prefer wire usage when present; document deviation when absent.
8. **Hand-written codec drift**—require golden binaries from reference testdata in CI.

---

## Summary

Devin chat is one Connect+proto POST per turn on `{api_server}/…/GetChatMessage`, Basic auth, optional gzip request bodies over 8 KiB, and a framed protobuf stream of text (R-12) plus tools, images, reasoning, usage, quota, errors, and optional cache keepalive (R-13). Reuse a vendor-neutral Connect codec in `hofund-plugin-sdk` (as Cursor R-8) with Devin-specific message tables in the plugin. `HttpHandle::send` plus `Call::open` suffices; no duplex API. Tokens come from PKCE or CLI TOML; identity from `GetUserStatus` with isolated CLI fallback; serialize renewal with `accounts().lease`. Test with a local fake server and golden frames from the reference plugin tests. R-12 ships text streaming and live model configs; R-13 ships the rest of the reference request mapper and operational features.
