# Devin subscription upstream (R-12, R-13)

Status: R-12a ships the shared protobuf/Connect codec and Devin's text-only chat and raw model-config message tables with golden byte tests. R-12b adds unary `GetChatMessage` execution with incremental text and reasoning streams, request gzip, section 6 error mapping, live `GetCliModelConfigs` with isolated CLI and snapshot fallbacks, variant selection, and stored-credential probes. R-13a lands stream usage, `GetUserStatus` quota windows, free-tier model exclusions, and list prices. R-13b adds tool definitions, tool history and id round trips, split-call joining, interrupted results, streamed tool calls with finish usage, and inline/tool-result images. R-13b also adds structured-output prose for Chat, Responses, and Messages. Signed thinking, Messages cache marks, and keepalive remain for later R-13 slices; PKCE remains deferred to its designated slice. Keepalive stays off by default and is not exposed in product UI. The remaining R-12/R-13 sections are the contract for serving a model call on a Devin account through the same Windsurf/Codeium backend the Devin CLI uses.

The wire contract is the reference's JavaScript subscription plugin (`packages/devin/index.mjs`, tests listed in section 7). The Go provider files (`internal/provider/devin.go`, `devin_accounts.go`, `devin_images.go`) cover CLI install paths, catalog shaping, and migration readers; they do not open `GetChatMessage`. Where Go and JavaScript disagree, JavaScript is the wire contract.

Vendor names stay inside `plugins/subscriptions/devin`. Kernel changes named here are generic HTTP or shared Connect helpers. Plugins still depend only on `hofund-plugin-api` and `hofund-plugin-sdk` (`xtask/src/boundary.rs` `link_allowance`, `ALLOWED_DEPS`).

## 0. Decision, and where it is weak

Ship one unary Connect RPC per `UpstreamProvider::execute`: `POST {api_server}/exa.api_server_pb.ApiServerService/GetChatMessage` with a single framed protobuf body and a framed protobuf stream back. Part 1 (R-12) returns streamed **text and reasoning** on a real account, live **GetCliModelConfigs** (plus variant resolution against embedded snapshot families), PKCE sign-in, CLI install guidance, gzip on large bodies, metadata that matches the pinned CLI version, and **section 6 `failure` mapping on `GetChatMessage` only** (HTTP errors, Connect end JSON, empty reply, broken stream). Part 2 (R-13) adds tools, inline images, reasoning signature replay, usage and list prices, **quota windows and free-tier `notModels`**, cache keepalive, and the Messages-shaped history rules (cache marks, split calls, gemini schema cleaning). R-13 does not re-own generic chat HTTP classification—that stays in R-12.

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
| Browser PKCE start | `https://app.devin.ai/auth/cli/continue` | redirect | `AUTHORIZE`, `index.mjs:29`; `browserSignIn`, `index.mjs:945-1008`. Loopback **`RedirectHost::Ipv4`**, path **`/callback`**, redirect URL **`http://127.0.0.1:<port>/callback`** (`index.mjs:987-989`, `loopback.rs:263`). Authorize query: same `redirect_uri`, `state`, `prompt=select_account`, `code_challenge`, `code_challenge_method=S256`, `cli_pkce_marker=1` (`index.mjs:995-1001`). Verifier = base64url of **48** random bytes; challenge = base64url(SHA-256 of the **verifier string**, not raw bytes) (`index.mjs:948-950`); state = base64url of **24** bytes, verified before exchange (`index.mjs:974`). **`exchange`** POST body includes the same `redirect_uri` plus `code` and `code_verifier` (`index.mjs:908-912`); posts to fixed `SERVER` (`index.mjs:908-909`); success uses `SERVER` only and ignores returned API host URLs (`index.mjs:976-977`) |

Default API origin: `https://server.codeium.com` (`SERVER`, `index.mjs:28`). Stored accounts carry `api_server` from `credentials.toml` (`windsurf_api_key`, `api_server_url`, `devin_api_url`; `plugins/subscriptions/devin/src/creds.rs:111-147`). The reference also honors `WINDSURF_API_SERVER_URL` at request time (`live`, `index.mjs:3084`). Hofund reads the env var only during `execute` / `list_models` for the account being billed, not process-wide.

EU host `eu.codeium.com` is a stored URL choice, not a separate code path (`docs/coverage/subscription-parity.md` row m). No automatic region retry like Cursor; a wrong host fails with HTTP/`failure` mapping and the router may failover on eligible statuses.

CLI version pin sent on the wire: `3000.11.3` (`CLI_VERSION`, `index.mjs:33`; `metadata`, `index.mjs:1414-1415`).

### 1.2 Connect framing (`GetChatMessage` only)

Only **`GetChatMessage`** uses Connect framing. **`GetCliModelConfigs`** and its response are raw protobuf (section 1.3a); do not call `frame` or `frames` on that RPC.

For chat, `frame` / `frames` (`index.mjs:1138-1177`). Same 5-byte header as Cursor (`docs/cursor-upstream-design.md` section 1.1): flags, big-endian length, payload. **Flag bit 0** (`flags & 1`): gzip payload. **Flag bit 1** (`flags & 2`): stream **end** frame; payload is JSON (Connect error), not protobuf (`index.mjs:1162-1163`; end-frame tests use header byte **`2`**, `errors.test.mjs:15-16`). **Both bits may be set** on one frame: **inflate first** (bit 0), then treat the result as end JSON (bit 1). Do not use `flags & 4`—that mis-classifies end frames (`failure`, `index.mjs:1684-1705`).

Request gzip threshold: `GZIP_AT = 8 << 10` (8192 bytes) (`index.mjs:1138-1146`). When set, flag byte is 1 and header `Connect-Content-Encoding: gzip` is added (`rpcHeaders`, `index.mjs:1813-1814`). The CLI does not send `Connect-Accept-Encoding` on the request (`gzip-request.test.mjs` lines 51-52). Response frames may still arrive gzip-compressed (flag bit 0); inflate through the shared SDK helper (`docs/cursor-upstream-design.md` section 1.1) with **decompressed output capped at 64 MiB** (same bound as compressed frame length at `index.mjs:1157`; `gunzipSync` alone does not cap expanded size).

Oversize wire body: nginx returns 413 above ~10 MiB (`README.md` lines 78-79; `gzip-request.test.mjs` lines 1-4). Hofund gzips before send when over 8192 bytes; do not send 10 MiB plain.

Malformed stream: payload length above 64 MiB throws (`index.mjs:1157`). Trailing bytes after the last frame throw `unexpected EOF` (`index.mjs:1167`). `fields` drops a malformed tail (`index.mjs:1091-1129`).

Connect error JSON at stream end: `{code, message}` or `{error:{code,message}}` (`failure`, `index.mjs:1684-1691`).

### 1.3a GetCliModelConfigs (raw protobuf)

Request (`configsFor`, `index.mjs:488-495`; Go `devin_images.go:50-57`):

| Item | Value |
|---|---|
| `Content-Type` | `application/proto` |
| `Connect-Protocol-Version` | `1` |
| Body | Unframed protobuf: outer field **1** (bytes) wraps the same `metadata(key)` message as chat field 1 |
| `Authorization` | **Omitted** — session token appears only in metadata field **3** |
| Gzip / Connect frame | **None** |

Response: read the full HTTP body as protobuf; parse with `parseModelConfigs` / `parseConfigFamilies` (`index.mjs:354-426`). Golden bytes in `packages/devin/testdata/model-configs.bin` and `model-configs-1015.bin` are **raw response bodies**, not Connect frames (`keepalive.test.mjs` loads `model-configs.bin` the same way).

### 1.3 GetChatMessage HTTP headers

`rpcHeaders(key, body)` (`index.mjs:1813-1814`):

| Header | Value |
|---|---|
| `Content-Type` | `application/connect+proto` |
| `Connect-Protocol-Version` | `1` |
| `Authorization` | Literal `Basic ` concatenated with the raw session token (not Bearer, not RFC 7617 base64; `opencode2.test.mjs` expects `Basic account-a`, `index.mjs:1814`) |
| `Connect-Content-Encoding` | `gzip` only when frame flag bit 0 is set |

Chat uses `fetch(server + CHAT_RPC, { method: "POST", body, signal })` (`index.mjs:1827-1831`). No HTTP/2 pin is required; the reference uses ordinary HTTPS POST with a full body buffer. Only the caller **`signal`** bounds idle time on the wire (no separate 60 s idle cap in the reference).

Per-RPC budgets (section 3.2) replace the pooled client’s fixed **60 s** read idle (`crates/kernel/plugin-host/src/pool.rs:26`, `70-74`) for Devin calls. Until then, a thinking model silent for more than 60 s before the first frame would surface as `Interrupted` from the pool (`pool.rs:309-314`), not as Devin’s own `failure` mapping.

Non-OK HTTP before streaming: read body capped at **1 MiB** (`index.mjs:1836`), then classify with section 6’s `failure` table—not `Call::open`’s generic `status_failure` (`plugin-sdk/src/transport.rs:290-297`, `461-480`, **256**-char snippet).

### 1.4 Client metadata protobuf (`metadata`)

Built in `metadata(key)` (`index.mjs:1414-1415`), embedded as field 1 bytes of the chat request:

| Field | Type | Value |
|---|---|---|
| 1 | string | `devin-cli` |
| 2 | string | `CLI_VERSION` (`3000.11.3`) |
| 3 | string | session token (same as Basic auth secret) |
| 4 | string | `en` |
| 5 | string | OS name from `osName`: **`darwin`** on macOS, **`windows`** on Windows, **`linux`** on Linux (`process.platform`; `index.mjs:1410-1415`) |
| 7 | string | `CLI_VERSION` again |
| 12 | string | `chisel` |
| 28 | string | `chisel` |

On **`GetChatMessage`**, this message is field 1 of the chat request and the session token is also sent as HTTP Basic auth (`rpcHeaders`, `index.mjs:1813-1814`). On **`GetCliModelConfigs`**, the same metadata is field 1 of the unframed POST body; there is no Basic header (section 1.3a).

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
| 5 | double | temperature: default **1** when absent; `above0` maps exact **0** to **1e-6** (`index.mjs:1605-1607`) |
| 7 | varint | `40` |
| 8 | double | top_p: default **0.95** when absent; same `above0` rule (`index.mjs:1606-1607`) |

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

After the stream ends cleanly, the reference treats the reply as empty when **`said` stays zero and no tool call was counted** (`!said && tools < 0`, `index.mjs:1774-1776`) and maps to HTTP 502 with a short message (hofund uses the full **`EMPTY_REPLY_MESSAGE`**, `crates/kernel/codec/src/responses_empty.rs:6`; `empty-reply.test.mjs`; wire bytes in `testdata/empty-reply.bin` are **Connect frames**, not raw model-config protobuf). Only **fields 3 and 9** increment `said` (`index.mjs:1735-1740`); field **6** increments **`tools`**, not `said`.

If usage reports zero output but content was seen, estimate output as `floor((said + 3) / 4)` (`index.mjs:1778`).

### 1.7 Opening the stream before success

`roundtrip` reads the first frame before returning the iterator (`index.mjs:1839-1860`):

- After a **5-byte empty message frame**, an end frame with error JSON → map through `failure`, return error without opening a success stream (`errors.test.mjs:87-91`; immediate end-frame quota example `errors.test.mjs:43-47`).
- Immediate end with `{}` on a metadata-only success → 502 with **`EMPTY_REPLY_MESSAGE`** (`errors.test.mjs` lines 95-101).
- EOF before content → 502 (`errors.test.mjs` lines 81-85).
- Before returning `Ok(Completion::Stream)`, wait until **`events`** yields **text (field 3) or reasoning (field 9)**—the only fields that increment `said` (`index.mjs:1735-1740`). **Do not open a success stream on field 6 (tool call), field 10 (signature delta), field 21 (signature type), or field 7 (usage)**; skip them and keep reading (`index.mjs:1741-1747`, `1763-1771`). If the stream ends with **tool calls only** (`tools >= 0` in the reference counter, `index.mjs:1748-1760`) and **`said` is still zero**, fail **before** `Ok` with **`PluginError::status_error(404, …).without_cooldown()`** using the same fixed phrase as inbound tool refusal (section 4.2)—not `EMPTY_REPLY_MESSAGE` and not a success stream (tools are R-13). Only after the first field **3** or **9** delta may hofund emit stream events; failover before the first yielded client byte is only safe when nothing was yielded (`roundtrip`, `index.mjs:1851-1858`).

Both empty-reply paths (post-stream `events` check and metadata-only end frame) use **`EMPTY_REPLY_MESSAGE`** exactly. The reference plugin's shorter `"an empty reply"` substring would incorrectly **rest** the member (`decision.rs` `rests`, `docs/cursor-upstream-design.md` section 4.2).

### 1.8 GetCliModelConfigs response fields (R-12)

Response body fields (`parseModelConfigs`, `parseConfigFamilies`, `parseFree`, `parsePrices`, `index.mjs:354-426`). Request wire is section 1.3a.

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
| `gzip` inflate for flag bit 0 | `hofund_plugin_sdk::inflate::gzip` (Cursor R-8) | `flate2` with pure backend; **64 MiB** decompressed cap (section 1.2) |
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

**Do not use `Call::open` for Devin.** On status ≥ 400 it reads the full body without a cap (`transport.rs:1258-1260`) and maps through generic `status_failure` (`transport.rs:290-297`, `461-480`), which truncates messages to **256** characters and never runs the Devin **`failure`** table (section 6).

Use **`Call::open_raw(http, 1)`** (`transport.rs:307-311`) for **`GetChatMessage`** and for **`GetCliModelConfigs`**, **`GetUserStatus`**, and PKCE **`exchange`** responses. Read bodies up to **1 MiB** where the reference does (`index.mjs:1836`; **`exchange`** truncates at **300** chars on failure, `index.mjs:916`). Map through **section 6 only for `GetChatMessage`** (HTTP errors, Connect end JSON on the chat stream, empty reply, broken stream). **`GetCliModelConfigs` / `configsFor`:** every failure is caught and logged (`index.mjs:500-503`): timeout, connect failure, non-2xx, and **2xx whose body fails parse** (for example **`parseModelConfigs` returns no uid**, `index.mjs:366`, `497`). In hofund terms, **`list_models` / `execute` must treat the same RPC failures as `configsFor` does**—including **`PluginErrorKind::Timeout`**, **`Connect`**, **`Transport`** (post-send reset before status, `pool.rs:295-306`), and **`Interrupted`** (body read failure after **2xx**, before parse completes, `pool.rs:309-314`)—by setting **`failed`** and continuing **`familiesFor`** (CLI, then **`familiesOf(SNAPSHOT)`**). **None** of those kinds may be returned to the router from this RPC (only chat uses section 6 for transport-shaped errors). Success cached **`FRESH = 5` min** (`index.mjs:39`, `508-512`), retry after **`RETRY = 60` s** (`index.mjs:40`). **`parseConfigFamilies` empty with images present** is not a 5-minute success; fall through to CLI (`index.mjs:498-499`, `863-873`). CLI subprocess runs unless **`cliAnswersFor === false`**; **`undefined`** from **`cliAnswersFor`** (for example **`auth status` throws**, `index.mjs:831-832`) still attempts **`devin models list`** (`index.mjs:872-873`). **`GetUserStatus` / `whoByKey`** (`index.mjs:802-818`, `780-782`; **`success`** `932-936`; **`identity`** `754-762`) has **three outcomes**. **`null` is a rejection**, not a stored signed-out account: **do not call the CLI** for **`null`**, and **do not persist** an account when **`success`** throws (`935-936`). **`success`** calls **`identity`** (`devin auth status` only) **only when `who === undefined`** (`933-934`); **`who === null`** (from **`whoByKey`** or **`identity`** when **`signedOut`**, `757-760`) **throws** and writes **no** secret. When **`whoByKey`** and **`identity`** are both **`undefined`**, sign-in succeeds with label **`"Devin"`** (`936`). **(a) `null` (rejected key)** when **`res.status === 401`** or **`failure(res.status, text).status === 401`** (`810`), including **HTTP 401** with a **`resource_exhausted`** body. **(b) `undefined` → `identity` CLI fallback** when the **`try`** fails before a usable body (**timeout, connect**, or **`res.text()`** failure after **2xx**—in hofund **`PluginErrorKind::Interrupted`**, `pool.rs:309-314`, `807-808`), when **`!res.ok`** and **`failure` status is not 401** (`811`), when JSON parse fails (`815-816`), or when **`whoOfStatus`** has no usable email/name (`780-782`). Map those **`Interrupted`** cases to **(b)**, **never** return **`Interrupted`** to the router or sign-in flow. **(c) HTTP 200** with **`whoOfStatus(u)`**—return that identity; **do not** run the CLI. Sign-in transport errors are **never** surfaced as **`Timeout` / `Connect` / `Transport` / `Interrupted`** to the router. **PKCE `exchange` failure:** sign-in **`PluginError`** with truncated body; account row may not exist yet (section 5.3)—never quota routing. On **2xx** chat, stream with `open_raw` and deframe; buffer-unary bodies for model-config RPCs.

Set **`Authorization`** via `Call::sensitive_header` with the literal `Basic ` + raw token (`CredentialStyle` has no matching preset; `index.mjs:1814`).

Request compression: gzip the protobuf **before** framing when `msg.len() > 8192` (`index.mjs:1139-1146`). Prefer SDK `gzip::compress` mirroring the shared inflate helper (section 2).

Streaming read: poll `BodyStream` on the execute task. With **`Call::read_timeout`**, a timeout **before** response headers arrive maps to **`PluginErrorKind::Timeout`** (`pool.rs:295-306`). Any body read failure **after** a **2xx** status—including idle timeout, connection reset, and other read errors—surfaces as **`PluginErrorKind::Interrupted`** via **`map_body`** (`pool.rs:309-314`). The Devin **chat** plugin must **never** return `Interrupted` to the router: rewrite **every** such error on a 2xx body to **`PluginError::status_error(502, "the reply broke off: …")`** (same wording as `events` / `roundtrip`, `index.mjs:1720-1722`, `1843-1844`). **`allows` does not treat `Interrupted` like a mapped 502** (`decision.rs:173-176`). If bytes were already yielded, **rest** applies and **no** failover; if the break happens before any client byte, **rest** still applies (`decision.rs:259-277`) **and** failover may apply (section 6 table).

### 3.2 Kernel extension (shared with Cursor R-8)

Devin R-12 **depends** on the same vendor-neutral unary transport extension as Cursor (`docs/cursor-upstream-design.md` section 3.2): optional **`Call::read_timeout(Duration)`** (and `plugin-host` honouring it instead of the pool’s fixed **60 s** idle, `pool.rs:26`, `70-74`). Land that API in the SDK/host **with or before** Devin R-12; do not ship Devin upstream on today’s uncapped pool idle alone.

| RPC | Read budget | Why |
|---|---|---|
| `GetCliModelConfigs` | **15 s** | Matches reference `AbortSignal.timeout(15_000)` (`index.mjs:492`) |
| `GetUserStatus` | **15 s** | `index.mjs:2994` |
| PKCE `exchange` | **30 s** | `index.mjs:913` |
| `GetChatMessage` (2xx stream) | **300 s** per-call idle | Reference follows only the caller **`signal`** (`index.mjs:1827-1831`); the pool’s **60 s** idle would cut off long “thinking” gaps before the first frame and mis-map to `Interrupted` / false “reply broke off” 502s |

Sign-in loopback still uses **`SIGN_IN_TIMEOUT = 10 * 60 * 1000`** (`index.mjs:37`, `browserSignIn`). Duplex `open_duplex` remains Cursor-only; Devin does not use it.

Optional later (R-13 keepalive reporting): record ping usage **until the daemon exposes a usage hook**—mirrors `reportPing` (`index.mjs:2435-2440`). Defer until keepalive lands; do not block R-12 on daemon usage plumbing.

Attack: enabling global HTTP/2 on the pooled client for Devin. Devin does not need it; leave the unary pool on HTTP/1.1 (`docs/cursor-upstream-design.md` section 3.1).

## 4. Request mapping

`UpstreamProvider::execute` receives canonical `hofund_ir::Request`. The gateway has already decoded Chat, Responses, or Messages. The Devin plugin maps IR → `encodeRequest` → stream events → `Completion::Stream`.

Inbound protocol id may affect **outbound** client shape when hofund re-encodes, but the upstream wire is always `GetChatMessage`. The reference's separate `build` vs `buildMessages` (`CHAT_WIRE` / `MESSAGES`, `index.mjs:1810-1811`) is about client JSON APIs, not a second upstream.

### 4.1 Shared rules (R-12 baseline)

Model id: resolve with `variantFor(families, model, effort)` (`index.mjs:234-272`, `variantFor` tests in `request.test.mjs` / snapshot families). R-12 implements family list + effort/nearest level + fast/priority tiers (`LEVEL_RANK`, `TIERS`, `index.mjs:136-183`). **`execute` and `list_models` use the same `familiesFor` chain** (`index.mjs:856-858`): live RPC → CLI JSON when **`cliAnswersFor !== false`** → **`familiesOf(SNAPSHOT)`** when both fail.

**Production snapshot data:** embed the reference **`SNAPSHOT`** table (`index.mjs:51-106`) as **plugin source data** (for example `plugins/subscriptions/devin/src/snapshot_families.rs`), **separate from** `plugins/subscriptions/devin/src/fixtures/models.json` (tests only). Build **`SNAPSHOT_FAMILIES`** once from that table; **`list_models` and `execute` share it**. After resolving against the live family list, if the requested id is not a known variant uid, run the same second pass as **`variantFor`** against **`SNAPSHOT_FAMILIES`** when `families !== SNAPSHOT_FAMILIES` (`index.mjs:250-251`) so a family id such as **`swe-2`** becomes **`swe-2-high`** (via `defaultModel`), never the bare family id on field 21.

Sampling: map IR generation params to field 8; default temperature **1** and top_p **0.95** when absent; apply `above0` (`index.mjs:1605-1607`). Other params (penalties, stop, seed) have no wire home—ignore without failing.

Effort for **`variantFor`**: **Chat** and **Responses** inbound → map **`GenerationParams::effort`** through **`effortOf`** (`index.mjs:1219-1227`). **`None` (absent on the IR request) → `""`** → **`defaultModel(f).id`** (for example **`swe-2-high`**). **`Some(Effort::None)`** → wire string **`"none"`** for nearest-level picking (`usage.test.mjs` effortOf/variantFor cases).

**Messages** inbound (`ProtocolId::AnthropicMessages`): **do not use `GenerationParams::effort` for variant pick.** Recompute like **`anthropicEffort`** from **`request.extensions`** (`thinking`, `output_config`), matching **`buildMessages`** (`index.mjs:1947-1954`): if **`thinking.type === "disabled"`** → **`""`** (default model) **before** any **`output_config.effort`**; else if **`output_config.effort`** → **`effortOf(...)`**; else for **`enabled` / `adaptive`**, map **`thinking.budget_tokens`**: **`<= 0` → `""`**, **`<= 4096` → `low`**, **`<= 12000` → `medium`**, **`<= 24000` → `high`**, else **`xhigh`**. The hofund codec stores disabled thinking as **`Some(Effort::None)`** on **`Request::params.effort`** but reads **`output_config.effort` first** when decoding Messages (`crates/kernel/codec/src/anthropic.rs:358-374`); **`thinking` stays in `extensions`**—**`execute` must apply the `anthropicEffort` order on extensions**, not **`params.effort` alone**. Pin with **`cache-marks.test.mjs`** (anthropicEffort cases), **`usage.test.mjs`**, **`request.test.mjs`**, **`free.test.mjs`** (no-effort default variant).

System and developer text: prepend to the first user message (`build`, `index.mjs:1572-1590`). R-12 includes **no** tool-description block because R-12 refuses tools.

Default user nudge when the transcript ends on assistant without pending calls: `"Please proceed with the task."` (`index.mjs:1570-1570`).

Structured output and other R-13-only wire fields: section 4.2 refusals (not a field on `hofund_ir::Request`, `crates/kernel/ir/src/request.rs:16-40`).

### 4.2 Part 1 (R-12): text stream and model configs

**Successful execute** returns:

1. `MessageStart` with generated id and the **requested** model id from the inbound request (`chat.model` in the reference stream, `index.mjs:1887`), not the resolved wire uid alone.
2. `ContentBlockStart` text and/or thinking blocks: **`TextDelta` for field 3**; **R-12 forwards field-9 reasoning** (same `said` accounting as text, `index.mjs:1738-1740`) using the gateway’s reasoning stream shape for the inbound protocol. **For the whole stream**, do **not** emit IR events for fields **6, 10, or 21** (section 1.7)—not only before `Ok(Completion::Stream)`.
3. On clean end after text or reasoning was sent: map stop varint **3** to `StopReason::MaxTokens`; otherwise use **`StopReason::EndTurn`**. **Never** `StopReason::ToolUse` in R-12—even if the wire reports tool stop varint **10** or the reference would finish with `tool_calls` (`index.mjs:1781`). Tool-only replies (no field 3/9) stay section 1.7 **404** before `Ok`.
4. R-12 omitted usage. R-13a consumes field 7 and attaches its counters to the finish event, estimating zero output from content bytes; field 7 alone still cannot open a success stream.

Before `Ok(Completion::Stream)`, buffer until the first **`events`** output or a terminal error (`roundtrip`, `index.mjs:1851-1858`; section 1.7).

**list_models (R-12)** calls `GetCliModelConfigs` (section 1.3a), parses with `parseConfigFamilies`, merges image flags via `parseModelConfigs`, applies `listed()` catalog rules (`index.mjs:274-320`). Fallback order matches **`familiesFor`** (section 3.1, `index.mjs:856-858`). Locate the CLI like **`cliPath`**: on **Windows**, search **`PATH` for `devin.exe`** (`index.mjs:619-621`); on other platforms search **`PATH` for `devin`**. Then the three fixed paths **`~/.local/bin/devin`**, **`/usr/local/bin/devin`**, **`/opt/homebrew/bin/devin`** (bare name on all platforms, `index.mjs:622`). Resolve to an **absolute** `program` path. **`SpawnRequest::new`** requires **absolute** `program` and **`cwd`** (`host.rs:418-430`). CLI subprocesses use `HostContext::spawn_process().spawn_piped` (`host.rs:570-575`) with **`XDG_DATA_HOME`** (Unix) and **`APPDATA`** (Windows) set to the **root of a per-call temporary directory** (mode **`0o700`**). Write credentials at **`{temp}/devin/credentials.toml`** (mode **`0o600`**) with the same keys as **`credentials()`** (`windsurf_api_key`, `api_server_url`, `devin_webapp_host`, `devin_api_url`; `index.mjs:643-651`, path layout `cliCredentialsPath` `index.mjs:635-637`). Remove the **entire** temp directory on **every** exit path including failure. **`spawn_piped` has no host timeout**—kill the child after **30 s** to match **`runCLI`** (`index.mjs:693`). Plugin source must not use `std::process` or `std::net` (`xtask/plugin-boundary`, `clippy.toml`). **`spawn_piped` does not truncate oversized stdout**: a line longer than **1 MiB** without a newline stops with **`PluginError::io("child output line is longer than 1 MiB")`** (`piped.rs:113-117`; test **`oversized_line_errors`** asserts **`PluginErrorKind::Io`**, `piped.rs:292-299`). Treat that **`Io`** like any other CLI subprocess failure and fall through **`familiesFor`** to **`familiesOf(SNAPSHOT)`** (`index.mjs:856-858`)—do not parse truncated JSON.

**Refusals (R-12)** before RPC when the IR or leftover protocol JSON requests structured output or other R-13-only shapes:

- any tool definition, tool call, or tool result;
- any image part;
- thinking blocks that require signature replay (R-13);
- **structured output**, detected from **`request.extensions`** (there is no top-level `response_format` on `Request`, `request.rs:16-40`):
  - **Chat** inbound: extension key **`response_format`** with `type` **`json_object`** or **`json_schema`** (codec leaves the rest of the Chat object in extensions, `openai_chat.rs:586-592`). **`type: "text"`** or absent type is allowed (`formatText` returns `""`, `index.mjs:1435-1437`).
  - **Responses** inbound: extension object **`text`** with nested **`format`** requesting JSON schema or JSON object (remaining top-level fields after decode, `openai_responses.rs:320`).
  - **Messages** inbound: extension **`output_config`** whose **`format`** sub-object requests structured output (`json_schema` / `json_object`). Do **not** reject **`output_config` that carries only `effort`** (`anthropic.rs:358-366`).

Return `PluginError::status_error(404, …).without_cooldown()` with one fixed phrase for tools, one for images, one for structured output. Wording must **not** contain the too-long substring (`hofund_ir::too_long`, `error.rs:12-14`) or match **`UNSERVED_WORDS`** (`decision.rs:49-51`)—for example: **"structured output is not served on Devin until R-13"** (no **model** + **not supported** pattern). Status 404 keeps default failover without resting the account.

Attack: passing tools with empty field 10 but tool calls still in history—still unsafe. Refuse when history contains tool calls or results, not only when `tools` is non-empty.

### 4.3 Part 2 (R-13): tools, images, reasoning, usage, quota

R-12 already implements **section 6 for `GetChatMessage` only** (section 3.1). **`GetCliModelConfigs` failures** (non-2xx, parse errors, **`Timeout` / `Connect` / `Transport` / `Interrupted`**) are recorded as **`failed`** and use the **section 3.1 `familiesFor` fallback** (CLI, then **`familiesOf(SNAPSHOT)`**)—**never** **`quota_status`**, **503**, or resting the account. R-13 adds tool/image stream behavior and **quota-window / free-tier** logic on top; it does not replace R-12’s **`failure`** table for chat turns.

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
| Tool/image-specific server errors | Same section 6 constructors as R-12 where the full `build` path hits them |

Messages inbound via IR: preserve block boundaries for cache marks; do not merge marked blocks (`README.md` lines 64-73; `buildMessages` comment `index.mjs:1956-1963`). Strip billing header line (`BILLING_LINE`, `index.mjs:1931`).

Outbound thinking signatures: stream signature deltas on Claude uids only (`completeMessages` stream path, `index.mjs:2210-2228`). Do not emit fake signatures.

## 5. Tokens and identity

### 5.1 Credential sources

| Source | Reference | Hofund today | R-12/R-13 |
|---|---|---|---|
| Browser PKCE | `browserSignIn`, `exchange`, `index.mjs:908-924`, `945-1008` | Bridge import only (`lib.rs:105-127`) | R-12: loopback PKCE + `ExchangeDevinCLIPKCECode`; store token in account secret only |
| CLI `credentials.toml` | `cliSignIn`, `readCredentials`, `index.mjs:655-662`, `1011-1033` | `discover`, `creds.rs` | Done; CLI sign-in sets `metadata.cli = "true"` (`index.mjs:1027`) so `live` re-reads TOML |
| CLI key rotation | `live` re-reads TOML when `metadata.cli` (`cli-key.test.mjs`; `index.mjs:3079-3087`) | Not on chat path | R-12: before each `execute`, if account marked CLI-sourced, re-read agent/XDG TOML under lease |

Token shape: `devin-session-token$…` (`README.md` lines 9-10). Authorization is **Basic**, not Bearer (`README.md` line 42; `rpcHeaders`).

No scheduled refresh on the chat path (`README.md` lines 32-36). `Subscription::refresh` remains a no-op load unless a future CLI expiry appears. On 401, surface `status_error(401, …)` and mark the account needs sign-in; **do not delete the secret** on a single failed chat (the reference keeps the account via a sign-in-kept response header; `errorResponse`, `index.mjs:1796-1799`).

### 5.2 Identity for account labels

Prefer `GetUserStatus` JSON via `whoByKey` (`index.mjs:802-818`, `identity.test.mjs`). Request body metadata mirrors CLI fields (`index.mjs:2991-2993`). Parse `whoOfStatus` (`index.mjs:780-796`): email, plan tier name, orgId, teamId.

Fallback when **`whoByKey`** is **`undefined`**: run **`devin auth status`** through **`identity`** (`index.mjs:754-762`) via **`spawn_piped`** with the same **CLI temp layout** and **30 s kill** as section 4.2 (`runCLI`, `index.mjs:693`): set **`XDG_DATA_HOME` / `APPDATA` to the temp root**, credentials at **`{temp}/devin/credentials.toml`** (`0o600`, parent `devin/` and temp root **`0o700`**), fields from **`credentials()`** (`index.mjs:643-651`, `635-637`). Remove the whole temp dir on every exit path including failure. **`devin models list`** is **not** an identity fallback—it belongs only to **`familiesFor` / `ownFamilies`** (section 3.1, `index.mjs:862-873`). Do **not** keep a long-lived cache copy like the reference’s **`homeFor`** under `XDG_CACHE_HOME` (`index.mjs:667-675`); hofund uses ephemeral temp dirs only.

Windows: do not trust CLI status for a browser token when CLI reads `%APPDATA%\devin\` regardless of home (`cliAnswersFor`, `index.mjs:821-840`; `identity.test.mjs`).

### 5.3 Lease

**PKCE first exchange:** `accounts().lease` on a **not-yet-stored** account returns **`PluginError` config** (“account is not stored”, `host.rs:185-186`). The initial **`ExchangeDevinCLIPKCECode`** completes **before** the account row exists—**do not lease** for that exchange; **persist the secret only after `success` returns without throwing** (`index.mjs:932-942`).

**After the account exists:** any CLI TOML re-read for `live`, token refresh, or other secret mutation must take **`accounts().lease(id, 30s)`** first (`HostContext` pattern in `docs/cursor-upstream-design.md` section 5.1). Serialize with the reference's single-flight CLI refresh intent (`cli-key.test.mjs`).

### 5.4 Keepalive (R-13 only)

When enabled via subscription config (mirror `cacheKeepalive`, `keepaliveSettings`, `index.mjs:2279-2288`; defaults **`KEEPALIVE_INTERVAL = 285` s**, **`KEEPALIVE_MAX = 6`**, **`KEEPALIVE_KEPT = 32` concurrent sessions**, `index.mjs:2265-2267`):

- Schedule from **request sent** time, not reply end (`index.mjs:2446-2453`; `keepalive.test.mjs`).
- Schedule pings only when the reply had cache **read or write** (`keptAfter`, `index.mjs:2492-2496`).
- Stop on real request for same `keepaliveKey` (`index.mjs:3122-3129`).
- Stop on 401, 402, 429; after **two consecutive pings** that fail the hit test `status == 200 && cacheRead > 0 && cacheRead * 2 >= input + cacheRead + cacheWrite` (`index.mjs:2480-2482`); or when ping count reaches max (`index.mjs:2478-2482`).
- Pings bypass the gateway (`README.md` lines 136-137).

Hofund lacks session headers on `Request` today. Until the IR or host exposes session affinity, keepalive must remain off and undocumented in product UI, or keys will be wrong (`keepaliveKey`, `index.mjs:2316-2340`).

### 5.5 CLI install guidance (R-12)

Match Go install hints (`internal/provider/install.go:56-58`): curl install.sh / PowerShell setup.ps1. Optional `devin setup` note: interactive (`install.go:119-123`). Hofund does not run setup during sign-in; document in sign-in challenge text only.

### 5.6 What is never written

Account store is the only **persistent** secret. Per-call CLI temp dirs (sections 4.2 and 5.2) are the sole exception. Chat does not write `credentials.toml` except `write_agent_sign_in`. Do not log Basic auth, metadata field 3, or the **`apiKey` field in `GetUserStatus` JSON** (`index.mjs:2991-2993`). Temp dirs for CLI runs must be removed on drop (sections 4.2 and 5.2).

## 6. Error classification (R-12, `GetChatMessage` only)

**Section 6 applies only to `GetChatMessage`** (non-OK HTTP before stream, Connect end JSON on the chat body, empty reply, broken stream). **R-12** implements this table on the **`execute`** chat path. **`GetCliModelConfigs`**, **`GetUserStatus`**, and PKCE **`exchange`** use the fallbacks in section 3.1—not these router shapes. **R-13** adds **quota window** routing, **`notModels` / free-tier** holds, and tool/image-specific cases on top of R-12 chat mapping.

Map using **`failure`** (`index.mjs:1684-1705`) and `errors.test.mjs`, in the **plugin** after reading chat error bodies via **`Call::open_raw`** (section 3.1). There is no `statusOf` in the Devin plugin. Append a hofund sign-in hint without naming the reference product: e.g. `sign in to Devin again` (`index.mjs:1695`). **`failure` parses the body before status-specific shortcuts:** only Connect code **`unauthenticated`** forces **401** (`index.mjs:1695`); raw HTTP **401** with a JSON body still runs the full table—a body whose parsed code is **`resource_exhausted`** or whose text matches quota/rate rules becomes **429** even when HTTP status was 401 (`index.mjs:1694-1696`).

Follow the same **`PluginError` shapes and router rules** as `docs/cursor-upstream-design.md` section 6 (`allows`, `decision.rs:171-178`; `failure_kind`, `cooldown.rs`). Do **not** use `PluginError::auth` on the chat path—401 uses `status_error(401, …)` so failover can try another account while the secret stays stored. On **`GetChatMessage`**, never return raw **`Interrupted`** from a **2xx** body (section 3.1). **`Connect` / `Transport` / `Interrupted`** before a successful HTTP status are not section 6 rows.

Classification order matches `failure` (`index.mjs:1694-1705`):

| Reference outcome | HTTP | Hofund | Rest / failover |
|---|---|---|---|
| Connect code **`unauthenticated`** (including end-frame JSON) | 401 | `status_error(401, …)`; do not delete secret | Generic ladder; 401 failovers (`allows` treats `Status`, not `Auth`) |
| Connect code **`resource_exhausted`** | 429 | **`quota_status(429, "usage limit reached: …")`** always—even when the message text says **rate limit** (`errors.test.mjs:87-91`, `index.mjs:1696`) | Quota rest + failover |
| message contains **quota**, or **rate limit** when code is **not** `resource_exhausted` and text does **not** contain **quota** | 429 | **`quota_status`** only on the quota branch; plain **rate limit** → `status_error(429, …)` **without** the **`usage limit reached:`** prefix and **without** `quota_status` | Quota rest vs rate ladder + failover |
| content policy | 400 | `status_error(400, …)` | |
| too long / context length / too many tokens | 400 | `status_error(400, "input is too long for the model's context: …")` | `overflowed`; no status-list failover |
| `invalid_argument` (other) | 400 | `status_error(400, …).without_cooldown()` when text is not quota-shaped | |
| `unavailable` | 503 | `status_error(503, …)` (`index.mjs:1703`) | Generic ladder + failover |
| any other **non-empty Connect code** | 502 | `status_error(502, …)` (`index.mjs:1704`) | |
| HTTP error with no Connect code (e.g. 402) | same | `status_error(status, statusText…)` (`index.mjs:1705`; `errors.test.mjs` 402) | |
| empty reply (both paths) | 502 | `status_error(502, EMPTY_REPLY_MESSAGE)` | `rests` false; failover yes |
| stream break before any byte returned from `execute` | 502 | `status_error(502, "the reply broke off: …")` | **Rest** true; failover yes (`errors.test.mjs:81-85`) |
| stream break after bytes yielded | 502 | same wording | **rest** only; no failover |

Free-tier holds: when quota windows say exhausted but model uid is in `parseFree` set, router should still allow SWE-2-class models (`usage` `notModels`, `index.mjs:3041-3047`; R-13).

Model not allowed: preserve server text; `status_error` with 400/403/404/422 as mapped; not `Auth` unless text says unauthenticated.

## 7. Testing

### 7.1 Local fake server

The HTTP fake lives in **`hofund-kit-subscription`** (shared subscription test kit), not in `plugins/subscriptions/devin` tests: plugin crates must not use `std::net::TcpListener` (`xtask/plugin-boundary`, `clippy.toml`). Dev plugin tests depend only on the kit and **`tempfile`**; do not add `mockito`.

The kit server asserts path and headers for each RPC kind:

- **`GetChatMessage`**: `Connect-Protocol-Version`, Basic auth, optional `Connect-Content-Encoding: gzip`; response bodies use **Connect frame** bytes (`empty-reply.bin` from `packages/devin/testdata/`).
- **`GetCliModelConfigs`**: `Content-Type: application/proto`, no Basic auth; response bodies are **raw protobuf** (`model-configs.bin`, `model-configs-1015.bin`).

Plugin unit tests submit captured bytes without opening sockets in the plugin crate.

### 7.2 Golden frames

Port assertions from reference tests:

| Test file | Property |
|---|---|
| `gzip-request.test.mjs` | 8083 plain vs 8333 gzipped threshold |
| `request.test.mjs` | Tool history encoding |
| `empty-reply.test.mjs` | Usage-only body → 502 before stream |
| `errors.test.mjs` | Connect end JSON → status mapping |
| `identity.test.mjs` (add) | **`GetUserStatus` HTTP 401** with body **`resource_exhausted`** → **`whoByKey` `null`**, not CLI fallback |
| `cache-marks.test.mjs` | Field 8 on marked prompts; **`anthropicEffort`** |
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
| Shared Connect + read_timeout | `plugin-sdk` `Call::read_timeout`, `plugin-host` `pool.rs` | **Blocker**: same API as Cursor R-8 section 3.2; Devin R-12 must not ship on 60 s pool idle alone |
| Upstream execute + probe | `upstream.rs`, `stream.rs` | Replace `unsupported()` |
| Model list + variant picker | `models.rs`, `snapshot_families.rs`, `discover.rs` touch | Wire `list_models` to RPC; embedded **`SNAPSHOT`** (`index.mjs:51-106`) |
| PKCE sign-in | `sign_in.rs` (new), `lib.rs` | Loopback server via host loopback handle |
| CLI install copy | `lib.rs` or `sign_in.rs` | Strings only |
| `live` + CLI re-read | `creds.rs`, `session.rs` (new) | Lease guarded |
| Tests + golden bytes | plugin tests + kit fake server | Port JS binaries; frames vs raw proto per section 7.1 |
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
| Error taxonomy tests | `errors.rs` | Extends R-12 section 6 for tool/image paths; base chat mapping ships in R-12 |
| Feature parity + comparison table rows | docs only in same PRs | Evidence links |

**Deferred beyond R-13:** OpenCode v2 `LanguageModelV3` adapter (reference `devinLanguage`, `index.mjs:2662+`); usage ping reporting **until the daemon exposes a usage hook**; Anthropic `/devin/v1/messages` as a separate HTTP API (reference still uses `GetChatMessage` for Messages-shaped clients—hofund IR path only).

### 8.3 Doc and parity updates (each PR)

- `docs/feature-parity.md` row 3.5 evidence (upstream design link).
- `docs/coverage/subscription-parity.md` Devin rows e, g, l, q, o as PRs land.
- the adopt-or-redesign document Devin upstream row when behavior changes (implementation PRs, not this design PR).

## 9. Adversarial checklist (design author)

1. **Session-less keepalive** will ping the wrong cache or spam six full-context reads—keep off until session metadata exists.
2. **R-12 tool refusal** must inspect conversation history, not only `Request::tools`.
3. **Shared SDK Connect** without Cursor R-8 merged first duplicates gzip policy—sequence SDK PR before or with Devin R-12.
4. **CLI live re-read** without lease races two executes on rotated TOML—always lease.
5. **Windows CLI naming** must not overwrite browser account labels—keep `cliAnswersFor` logic.
6. **Empty reply string** must match `EMPTY_REPLY_MESSAGE` exactly on both paths.
7. **GetCliModelConfigs** must not use Connect frames or Basic auth—only chat does.
8. **Hand-written codec drift**—require golden binaries from reference testdata in CI (frame vs raw).
9. **`Call::open` vs `open_raw`**—chat error bodies must use `open_raw` + 1 MiB cap + section 6 `failure` on **`GetChatMessage` only**; model-config and identity RPCs fall back, not router quota/503.

---

## Summary

Devin chat is one Connect+proto POST per turn on `{api_server}/…/GetChatMessage`, literal Basic auth, optional gzip request bodies over 8 KiB, and a framed protobuf stream of text and reasoning (R-12) plus tools, images, usage, quota, errors, and optional cache keepalive (R-13). Reuse a vendor-neutral Connect codec and **`Call::read_timeout`** in `hofund-plugin-sdk` (Cursor R-8) with Devin-specific message tables in the plugin. Use **`Call::open_raw`**, not `Call::open`, so **`GetChatMessage`** HTTP and stream errors map through section 6; other RPCs use the fallbacks in section 3.1. Tokens come from PKCE or CLI TOML; identity from `GetUserStatus` with isolated CLI fallback; serialize renewal with `accounts().lease`. Test with the kit fake server and golden bytes from the reference plugin tests (Connect frames for chat, raw protobuf for model configs). R-12 ships text/reasoning streaming and live model configs; R-13 ships the rest of the reference request mapper and operational features.
